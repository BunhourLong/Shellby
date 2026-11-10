//! One pane: its PTY, its shared terminal, and the reader thread that parses into it
//! (00-architecture §2).

use std::io::{self, Read};
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nt_pty::{Pty, PtyReader, PtySize, PtyWriter, SpawnError, SpawnSpec};
use nt_term::{
    FairMutex, Frame, SharedTerminal, TermConfig, TermEvent, TermModes, TermSize, Terminal,
};

use crate::app::UserEvent;
use crate::geometry::GridSize;

/// Pane identity; becomes `nt_mux::id::PaneId` when T12 lands (same shape).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PaneId(pub(crate) u64);

/// Delivers an event to the UI thread; returns false once the UI is gone.
pub(crate) type Sink = Box<dyn FnMut(UserEvent) -> bool + Send>;

/// What the reader thread reads from: the PTY in the app, a fake in tests.
pub(crate) trait Source: Read {
    /// Blocks until readable or `timeout`; true if readable.
    fn wait_readable(&self, timeout: Duration) -> bool;
}

impl Source for PtyReader {
    fn wait_readable(&self, timeout: Duration) -> bool {
        PtyReader::wait_readable(self, timeout)
    }
}

/// A running pane. Owned by the main thread.
pub(crate) struct PaneRuntime {
    pub(crate) id: PaneId,
    pty: Pty,
    writer: PtyWriter,
    terminal: SharedTerminal,
    needs_wake: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    /// Reused render input; owned by the main thread.
    pub(crate) frame: Frame,
    /// Last size sent to the PTY and terminal.
    pub(crate) grid: GridSize,
    cell: (u16, u16),
}

impl std::fmt::Debug for PaneRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaneRuntime")
            .field("id", &self.id)
            .field("pty", &self.pty)
            .field("grid", &self.grid)
            .finish_non_exhaustive()
    }
}

impl PaneRuntime {
    /// Spawns `spec` on a PTY of `grid` cells and starts the reader thread, which reports
    /// through `sink` (`Wake`, `Term`, then exactly one `PaneExited`).
    pub(crate) fn spawn(
        id: PaneId,
        spec: &SpawnSpec,
        grid: GridSize,
        cell: (u16, u16),
        config: &TermConfig,
        sink: Sink,
    ) -> Result<Self, SpawnError> {
        let (pty, reader, writer) = Pty::spawn(spec, pty_size(grid, cell))?;
        let terminal: SharedTerminal =
            Arc::new(FairMutex::new(Terminal::new(term_size(grid, cell), config)));
        let needs_wake = Arc::new(AtomicBool::new(false));
        let thread = {
            let (terminal, wake, replies) = (terminal.clone(), needs_wake.clone(), writer.clone());
            std::thread::Builder::new()
                .name(format!("nt-reader-{}", id.0))
                .spawn(move || {
                    reader_loop(reader, &terminal, &wake, sink, id, |b| replies.write_all(b));
                })
                .map_err(SpawnError::Io)?
        };
        Ok(Self {
            id,
            pty,
            writer,
            terminal,
            needs_wake,
            reader: Some(thread),
            frame: Frame::default(),
            grid,
            cell,
        })
    }

    /// Copies the terminal's damage into `self.frame`.
    pub(crate) fn fill_frame(&mut self) {
        fill(&self.needs_wake, &self.terminal, &mut self.frame);
    }

    /// Applies a new grid or cell size; a no-op when neither changed, so the PTY never
    /// sees per-pixel resizes.
    pub(crate) fn resize(&mut self, grid: GridSize, cell: (u16, u16)) {
        if grid == self.grid && cell == self.cell {
            return;
        }
        self.grid = grid;
        self.cell = cell;
        self.terminal.lock().resize(term_size(grid, cell));
        if let Err(e) = self.pty.resize(pty_size(grid, cell)) {
            log::warn!("pane {}: pty resize failed: {e}", self.id.0);
        }
    }

    /// Queues input bytes for the child; never blocks.
    pub(crate) fn write(&self, bytes: &[u8]) {
        self.writer.write_all(bytes);
    }

    /// The terminal's input modes right now.
    pub(crate) fn modes(&self) -> TermModes {
        self.terminal.lock().modes()
    }

    /// Hangs up the child (SIGHUP, then SIGKILL after 100 ms, then reap: `Pty::kill`) and
    /// waits up to 200 ms for the reader thread, then detaches it. Returns the exit status.
    /// After `PaneExited` the reader is already done, so this returns at once.
    pub(crate) fn shutdown(&mut self) -> Option<ExitStatus> {
        if let Err(e) = self.pty.kill() {
            log::warn!("pane {}: kill failed: {e}", self.id.0);
        }
        // The reader sees EOF once every holder of the slave is gone. A background job in
        // its own process group can keep it open forever: bounded wait, then detach.
        if let Some(handle) = self.reader.take() {
            let deadline = Instant::now() + Duration::from_millis(200);
            // shortcut: 1 ms sleep polling, the only sleep in the app; use a channel or
            // condvar if multi-pane close latency (T15) matters.
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            if handle.is_finished() {
                if handle.join().is_err() {
                    log::error!("pane {}: reader thread panicked", self.id.0);
                }
            } else {
                log::warn!(
                    "pane {}: reader still blocked after hangup; detaching",
                    self.id.0
                );
            }
        }
        self.pty.try_wait().ok().flatten()
    }
}

/// Main-thread side of wake coalescing: clear the flag, then fill.
///
/// Ordering argument: the flag is cleared *before* the lock is taken. Any bytes the reader
/// parses after this store set the flag again (false→true) and send a fresh `Wake`, so they
/// are picked up by the next fill even if this fill misses them. Clearing *after* the fill
/// would lose a wake for bytes parsed between the unlock and the store.
pub(crate) fn fill(needs_wake: &AtomicBool, terminal: &SharedTerminal, frame: &mut Frame) {
    needs_wake.store(false, Ordering::Release);
    terminal.lock().fill_frame(frame);
}

/// The reader thread: read ≤ 64 KiB, parse under the lock, route side effects, wake the UI
/// at most once per pending wake. Exits on EOF, read error, or when the UI is gone, and
/// always ends with `PaneExited`.
pub(crate) fn reader_loop<R: Source>(
    mut rd: R,
    terminal: &FairMutex<Terminal>,
    needs_wake: &AtomicBool,
    mut sink: Sink,
    id: PaneId,
    mut reply: impl FnMut(&[u8]),
) {
    let mut buf = vec![0u8; 64 * 1024].into_boxed_slice();
    let mut events: Vec<TermEvent> = Vec::with_capacity(16);
    let mut sync_deadline: Option<Instant> = None;
    'read: loop {
        if let Some(deadline) = sync_deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            if !rd.wait_readable(left) {
                let mut t = terminal.lock_unfair();
                t.end_expired_sync(Instant::now(), &mut events);
                sync_deadline = t.sync_deadline();
                drop(t);
                if !deliver(&mut events, &mut sink, &mut reply, needs_wake, id) {
                    break 'read;
                }
                continue;
            }
        }
        let n = match rd.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                log::warn!("pane {}: pty read failed: {e}", id.0);
                break;
            }
        };
        {
            let mut t = terminal.lock_unfair(); // held for one read's worth of bytes
            t.advance(&buf[..n], &mut events);
            sync_deadline = t.sync_deadline();
        }
        if !deliver(&mut events, &mut sink, &mut reply, needs_wake, id) {
            break;
        }
    }
    sink(UserEvent::PaneExited(id));
}

/// Routes drained events (`PtyWrite` straight to the PTY so DA/CPR replies never wait for
/// a frame; the rest to the UI), then wakes the UI if no wake is pending. False = UI gone.
fn deliver(
    events: &mut Vec<TermEvent>,
    sink: &mut Sink,
    reply: &mut impl FnMut(&[u8]),
    needs_wake: &AtomicBool,
    id: PaneId,
) -> bool {
    for ev in events.drain(..) {
        match ev {
            TermEvent::PtyWrite(bytes) => reply(&bytes),
            ev => {
                if !sink(UserEvent::Term(id, ev)) {
                    return false;
                }
            }
        }
    }
    if !needs_wake.swap(true, Ordering::AcqRel) {
        return sink(UserEvent::Wake(id));
    }
    true
}

fn term_size(grid: GridSize, cell: (u16, u16)) -> TermSize {
    TermSize {
        cols: grid.cols,
        rows: grid.rows,
        cell_w_px: cell.0,
        cell_h_px: cell.1,
    }
}

fn pty_size(grid: GridSize, cell: (u16, u16)) -> PtySize {
    PtySize {
        cols: grid.cols,
        rows: grid.rows,
        px_w: grid.cols.saturating_mul(cell.0),
        px_h: grid.rows.saturating_mul(cell.1),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread;
    use std::time::Duration;

    use nt_pty::SpawnSpec;

    use super::*;
    use crate::app::UserEvent;

    const GUARD: Duration = Duration::from_secs(5);

    struct FakeSource {
        chunks: Receiver<Vec<u8>>,
        read_called: Sender<()>,
    }

    impl Read for FakeSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let _ = self.read_called.send(());
            match self.chunks.recv() {
                Ok(chunk) => {
                    buf[..chunk.len()].copy_from_slice(&chunk);
                    Ok(chunk.len())
                }
                Err(_) => Ok(0),
            }
        }
    }

    impl Source for FakeSource {
        fn wait_readable(&self, _timeout: Duration) -> bool {
            true
        }
    }

    struct Reader {
        chunks: Option<Sender<Vec<u8>>>,
        reads: Receiver<()>,
        replies: Receiver<Vec<u8>>,
        terminal: Arc<FairMutex<Terminal>>,
        needs_wake: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
        done: Receiver<()>,
        reads_seen: usize,
    }

    fn event_channel() -> (Sink, Receiver<UserEvent>) {
        let (tx, rx) = mpsc::channel();
        (Box::new(move |ev| tx.send(ev).is_ok()), rx)
    }

    fn spawn_reader() -> (Reader, Receiver<UserEvent>) {
        let (sink, events) = event_channel();
        (spawn_reader_with(sink), events)
    }

    fn spawn_reader_with(sink: Sink) -> Reader {
        let (chunk_tx, chunk_rx) = mpsc::channel();
        let (read_tx, read_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let terminal = Arc::new(FairMutex::new(Terminal::new(
            TermSize {
                cols: 20,
                rows: 5,
                cell_w_px: 8,
                cell_h_px: 16,
            },
            &TermConfig::default(),
        )));
        let needs_wake = Arc::new(AtomicBool::new(false));
        let source = FakeSource {
            chunks: chunk_rx,
            read_called: read_tx,
        };
        let thread_terminal = terminal.clone();
        let thread_wake = needs_wake.clone();
        let handle = thread::spawn(move || {
            reader_loop(
                source,
                &thread_terminal,
                &thread_wake,
                sink,
                PaneId(1),
                |bytes| {
                    let _ = reply_tx.send(bytes.to_vec());
                },
            );
            let _ = done_tx.send(());
        });
        Reader {
            chunks: Some(chunk_tx),
            reads: read_rx,
            replies: reply_rx,
            terminal,
            needs_wake,
            handle: Some(handle),
            done: done_rx,
            reads_seen: 0,
        }
    }

    impl Reader {
        fn send(&self, bytes: &[u8]) {
            self.chunks
                .as_ref()
                .expect("input is open")
                .send(bytes.to_vec())
                .expect("send chunk");
        }

        /// Waits until the read that follows the `sent`-th chunk has started, which proves
        /// that chunk was parsed and routed.
        fn wait_processed(&mut self, sent: usize) {
            while self.reads_seen < sent + 1 {
                self.reads
                    .recv_timeout(GUARD)
                    .expect("reader did not call read");
                self.reads_seen += 1;
            }
        }

        fn close_input(&mut self) {
            self.chunks = None;
        }

        fn fill_into(&self, frame: &mut Frame) {
            fill(&self.needs_wake, &self.terminal, frame);
        }

        fn join(&mut self) {
            self.close_input();
            self.done
                .recv_timeout(GUARD)
                .expect("reader thread did not exit");
            if let Some(handle) = self.handle.take() {
                handle.join().expect("reader thread panicked");
            }
        }
    }

    impl Drop for Reader {
        fn drop(&mut self) {
            self.chunks = None;
            if self.handle.is_some() {
                let _ = self.done.recv_timeout(GUARD);
                if let Some(handle) = self.handle.take() {
                    let _ = handle.join();
                }
            }
        }
    }

    fn drain(events: &Receiver<UserEvent>) -> Vec<UserEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = events.try_recv() {
            out.push(ev);
        }
        out
    }

    fn wake_count(events: &[UserEvent]) -> usize {
        events
            .iter()
            .filter(|ev| matches!(ev, UserEvent::Wake(_)))
            .count()
    }

    #[test]
    fn three_chunks_before_fill_coalesce_to_one_wake() {
        let (mut reader, events) = spawn_reader();
        for (i, chunk) in [&b"a"[..], b"b", b"c"].into_iter().enumerate() {
            reader.send(chunk);
            reader.wait_processed(i + 1);
        }
        let got = drain(&events);
        assert_eq!(wake_count(&got), 1, "got {got:?}");
    }

    #[test]
    fn fill_clears_the_flag_so_the_next_chunk_wakes_again() {
        let (mut reader, events) = spawn_reader();
        reader.send(b"a");
        reader.wait_processed(1);
        assert_eq!(wake_count(&drain(&events)), 1);

        reader.fill_into(&mut Frame::default());
        reader.send(b"b");
        reader.wait_processed(2);
        let got = drain(&events);
        assert_eq!(wake_count(&got), 1, "got {got:?}");
    }

    #[test]
    fn fill_frame_reaches_the_grid() {
        let (mut reader, _events) = spawn_reader();
        reader.send(b"ab");
        reader.wait_processed(1);
        let mut frame = Frame::default();
        reader.fill_into(&mut frame);
        assert_eq!(frame.row(0)[0].ch, 'a');
        assert_eq!(frame.row(0)[1].ch, 'b');
    }

    #[test]
    fn da_query_replies_to_the_pty_and_not_to_the_ui() {
        let (mut reader, events) = spawn_reader();
        reader.send(b"\x1b[c");
        reader.wait_processed(1);
        let reply = reader.replies.recv_timeout(GUARD).expect("no DA reply");
        assert_ne!(reply, Vec::<u8>::new());
        assert!(reply.starts_with(b"\x1b[?"), "reply {reply:?}");
        let got = drain(&events);
        assert!(
            !got.iter().any(|ev| matches!(ev, UserEvent::Term(..))),
            "got {got:?}"
        );
    }

    #[test]
    fn osc_title_is_routed_as_a_term_event() {
        let (mut reader, events) = spawn_reader();
        reader.send(b"\x1b]0;hello\x07");
        reader.wait_processed(1);
        let got = drain(&events);
        let title = got.iter().find_map(|ev| match ev {
            UserEvent::Term(_, TermEvent::Title(s)) => Some(s.as_str()),
            _ => None,
        });
        assert_eq!(title, Some("hello"), "got {got:?}");
    }

    #[test]
    fn eof_sends_pane_exited_once_last_and_joins() {
        let (mut reader, events) = spawn_reader();
        reader.send(b"x");
        reader.wait_processed(1);
        reader.close_input();
        reader.join();
        let got = drain(&events);
        assert_eq!(
            got.iter()
                .filter(|ev| matches!(ev, UserEvent::PaneExited(_)))
                .count(),
            1,
            "got {got:?}"
        );
        assert!(
            matches!(got.last(), Some(UserEvent::PaneExited(PaneId(1)))),
            "got {got:?}"
        );
    }

    #[test]
    fn dropped_sink_stops_the_reader_and_the_thread_joins() {
        let mut reader = spawn_reader_with(Box::new(|_| false));
        reader.send(b"x");
        reader.join();
    }

    #[test]
    fn shell_output_and_exit_status_reach_the_pane() {
        let (tx, rx) = mpsc::channel();
        let spec = SpawnSpec {
            program: "/bin/sh".into(),
            arg0: None,
            args: vec!["-c".into(), "printf hi; exit 3".into()],
            cwd: None,
            env: vec![],
        };
        let mut pane = PaneRuntime::spawn(
            PaneId(7),
            &spec,
            GridSize { cols: 20, rows: 5 },
            (8, 16),
            &TermConfig::default(),
            Box::new(move |ev| tx.send(ev).is_ok()),
        )
        .expect("spawn pane");

        loop {
            if let UserEvent::PaneExited(id) = rx.recv_timeout(GUARD).expect("pane events stalled")
            {
                assert_eq!(id, PaneId(7));
                break;
            }
        }

        pane.fill_frame();
        assert_eq!(pane.frame.row(0)[0].ch, 'h');
        assert_eq!(pane.frame.row(0)[1].ch, 'i');
        let status = pane.shutdown().expect("exit status");
        assert_eq!(status.code(), Some(3));
    }
}
