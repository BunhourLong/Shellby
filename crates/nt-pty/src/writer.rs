//! Never-blocking, never-dropping, coalescing PTY writer backed by one thread per pane.

use std::fmt;
use std::io;
use std::mem;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rustix::io::Errno;

/// Pending size above which `write_all` logs once (D-T03-1 #3).
const WARN_PENDING: usize = 64 << 20;

struct State {
    /// Bytes queued by `write_all`, not yet taken by the writer thread.
    pending: Vec<u8>,
    /// No more bytes are accepted: shutdown was requested or the fd failed.
    closed: bool,
    /// The writer thread has left its loop (set last, with `notify_all`).
    exited: bool,
    /// The 64 MiB warning has been logged.
    warned: bool,
}

impl fmt::Debug for State {
    // Manual impl: printing a multi-MiB paste in a panic message helps nobody.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("State")
            .field("pending_len", &self.pending.len())
            .field("closed", &self.closed)
            .field("exited", &self.exited)
            .finish_non_exhaustive()
    }
}

/// One condvar serves both directions: before `closed` only the writer thread waits on it
/// (woken by `write_all`); after `closed` `write_all` never notifies and only `shutdown`
/// waits (woken by the thread's exit), so `notify_one` in `write_all` cannot be stolen.
#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // No code panics while holding the lock, but a poisoned queue is still consistent bytes.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Queues bytes for the child. `write_all` never blocks beyond a short mutex hold and never
/// drops bytes while the PTY is open; after the PTY closes, writes are silently discarded.
#[derive(Clone, Debug)]
pub struct PtyWriter {
    shared: Arc<Shared>,
}

impl PtyWriter {
    /// Appends `bytes` to the queue (coalesced with pending bytes) and wakes the writer thread.
    pub fn write_all(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut s = self.shared.lock();
        if s.closed {
            return;
        }
        let was_empty = s.pending.is_empty();
        s.pending.extend_from_slice(bytes);
        let warn = s.pending.len() > WARN_PENDING && !mem::replace(&mut s.warned, true);
        let len = s.pending.len();
        drop(s);
        if was_empty {
            self.shared.cv.notify_one();
        }
        // shortcut: unbounded queue capped only by a warning; add backpressure to the paste UI (T10) if a real user hits it
        if warn && std::env::var_os("NT_LOG").is_some() {
            #[allow(clippy::print_stderr, reason = "no logger until the T06 log decision")]
            {
                eprintln!("nt-pty: writer queue at {len} bytes; child is not reading");
            }
        }
    }
}

/// Owner-side handle of the writer thread, held by `Pty`.
#[derive(Debug)]
pub(crate) struct WriterHandle {
    shared: Arc<Shared>,
    thread: JoinHandle<()>,
}

impl WriterHandle {
    /// Marks the queue closed, wakes the thread, waits up to `grace` for it to exit and joins it;
    /// if it is still blocked in `write(2)` after `grace`, detaches it (it exits when the write
    /// fails). Never blocks longer than `grace`.
    pub(crate) fn shutdown(self, grace: Duration) {
        let mut s = self.shared.lock();
        s.closed = true;
        self.shared.cv.notify_all();
        let (s, _) = self
            .shared
            .cv
            .wait_timeout_while(s, grace, |s| !s.exited)
            .unwrap_or_else(PoisonError::into_inner);
        let exited = s.exited;
        drop(s);
        if exited {
            // The thread has left its loop, so this join returns at once. A panic in the thread
            // would have prevented `exited`; nothing to propagate here.
            let _ = self.thread.join();
        }
        // Otherwise the thread is blocked in write(2) on a peer that is not reading. Dropping
        // the JoinHandle detaches it: it owns its fd dup and an Arc<Shared>, so nothing dangles,
        // and it exits once the write fails (EIO when the last master/slave fd closes) and then
        // sees `closed`. Joining here would let a stuck child hang the UI thread's Drop.
    }
}

/// Starts the writer thread ("nt-pty-writer") writing to `fd` (a dup of the master).
pub(crate) fn start(fd: OwnedFd) -> io::Result<(PtyWriter, WriterHandle)> {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            pending: Vec::new(),
            closed: false,
            exited: false,
            warned: false,
        }),
        cv: Condvar::new(),
    });
    let thread_shared = Arc::clone(&shared);
    let thread = thread::Builder::new()
        .name("nt-pty-writer".into())
        .spawn(move || run(&fd, &thread_shared))?;
    Ok((
        PtyWriter {
            shared: Arc::clone(&shared),
        },
        WriterHandle { shared, thread },
    ))
}

fn run(fd: &OwnedFd, shared: &Shared) {
    // Ping-pong with `State::pending`: both buffers keep their capacity across rounds.
    let mut buf = Vec::new();
    loop {
        {
            let s = shared.lock();
            let mut s = shared
                .cv
                .wait_while(s, |s| s.pending.is_empty() && !s.closed)
                .unwrap_or_else(PoisonError::into_inner);
            // Closed means the child is gone (Pty::drop kills it first): flushing is pointless.
            if s.closed {
                break;
            }
            mem::swap(&mut s.pending, &mut buf);
        }
        if write_fully(fd, &buf).is_err() {
            break;
        }
        buf.clear();
        // Drop a paste-sized allocation instead of keeping it for the pane's lifetime.
        buf.shrink_to(64 * 1024);
    }
    let mut s = shared.lock();
    s.closed = true;
    s.exited = true;
    s.pending = Vec::new();
    drop(s);
    shared.cv.notify_all();
}

/// Writes all of `buf`, retrying `EINTR` and short writes. Any other error ends the PTY.
fn write_fully(fd: &OwnedFd, mut buf: &[u8]) -> Result<(), Errno> {
    while !buf.is_empty() {
        match rustix::io::write(fd, buf) {
            Ok(0) => return Err(Errno::IO),
            Ok(n) => buf = &buf[n..],
            Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    const DEADLINE: Duration = Duration::from_secs(5);

    fn pair() -> (PtyWriter, WriterHandle, UnixStream) {
        let (ours, peer) = UnixStream::pair().unwrap();
        let (w, h) = start(OwnedFd::from(ours)).unwrap();
        (w, h, peer)
    }

    /// Waits for the writer thread to exit; the deadline is a failure bound only.
    fn wait_exited(shared: &Shared) {
        let s = shared.lock();
        let (s, res) = shared
            .cv
            .wait_timeout_while(s, DEADLINE, |s| !s.exited)
            .unwrap();
        assert!(!res.timed_out() && s.exited, "writer thread did not exit");
    }

    /// Waits until the thread has taken the queue (it is then writing / blocked in write).
    fn wait_drained(shared: &Shared) {
        let end = Instant::now() + DEADLINE;
        while !shared.lock().pending.is_empty() {
            assert!(Instant::now() < end, "writer thread never took the queue");
            thread::yield_now();
        }
    }

    #[test]
    fn ordering_preserved_across_threads() {
        const THREADS: u8 = 4;
        const PER: u32 = 2500;
        let (w, h, mut peer) = pair();
        let producers: Vec<_> = (0..THREADS)
            .map(|t| {
                let w = w.clone();
                thread::spawn(move || {
                    for seq in 0..PER {
                        let mut rec = [t; 5];
                        rec[1..].copy_from_slice(&seq.to_le_bytes());
                        w.write_all(&rec);
                    }
                })
            })
            .collect();
        peer.set_read_timeout(Some(DEADLINE)).unwrap();
        let mut got = vec![0u8; 5 * usize::from(THREADS) * PER as usize];
        peer.read_exact(&mut got).unwrap();
        for p in producers {
            p.join().unwrap();
        }
        let mut next = [0u32; THREADS as usize];
        for rec in got.as_chunks::<5>().0 {
            let t = usize::from(rec[0]);
            let seq = u32::from_le_bytes(rec[1..].try_into().unwrap());
            assert_eq!(seq, next[t], "thread {t} out of order");
            next[t] += 1;
        }
        assert!(next.iter().all(|&n| n == PER));
        h.shutdown(Duration::from_millis(100));
    }

    #[test]
    fn large_write_to_stalled_peer_does_not_block() {
        let (w, h, peer) = pair();
        let data = vec![b'x'; 16 << 20];
        let t0 = Instant::now();
        w.write_all(&data);
        let took = t0.elapsed();
        assert!(took < Duration::from_millis(50), "write_all took {took:?}");
        let shared = Arc::clone(&h.shared);
        h.shutdown(Duration::from_millis(10));
        drop(peer);
        wait_exited(&shared);
    }

    #[test]
    fn shutdown_is_bounded_while_thread_blocked() {
        let (w, h, peer) = pair();
        w.write_all(&vec![b'x'; 16 << 20]);
        wait_drained(&w.shared);
        let grace = Duration::from_millis(100);
        let t0 = Instant::now();
        h.shutdown(grace);
        let took = t0.elapsed();
        assert!(
            took < grace + Duration::from_millis(50),
            "shutdown took {took:?}"
        );
        assert!(!w.shared.lock().exited, "thread should still be blocked");
        drop(peer);
        wait_exited(&w.shared);
    }

    #[test]
    fn writes_after_peer_close_are_discarded() {
        let (w, h, peer) = pair();
        drop(peer);
        w.write_all(b"hello");
        wait_exited(&w.shared);
        w.write_all(b"discarded");
        assert_eq!(w.shared.lock().pending, b"");
        h.shutdown(Duration::from_millis(100));
    }
}
