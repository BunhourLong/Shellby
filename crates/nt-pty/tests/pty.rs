//! Integration tests for nt-pty (T05 tests 1-18, 20, 21; 19 lives in `fd_leak.rs`).
//! Children are always `/bin/sh -c '<script>'`, never the user's shell.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use nt_pty::{Pty, PtyReader, PtySize, PtyWriter, SpawnError, SpawnSpec};

const DEADLINE: Duration = Duration::from_secs(5);

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols,
        rows,
        px_w: 0,
        px_h: 0,
    }
}

fn sh_spec(script: &str) -> SpawnSpec {
    SpawnSpec {
        program: "/bin/sh".into(),
        arg0: None,
        args: vec!["-c".into(), script.into()],
        cwd: None,
        env: vec![],
    }
}

enum Msg {
    Data(Vec<u8>),
    Eof,
    Err(io::Error),
}

/// Runs the reader on a thread; after EOF it checks that further reads keep returning `Ok(0)`.
fn pump(mut reader: PtyReader) -> Receiver<Msg> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    for _ in 0..2 {
                        match reader.read(&mut buf) {
                            Ok(0) => {}
                            Ok(n) => {
                                let e = io::Error::other(format!("{n} bytes after EOF"));
                                let _ = tx.send(Msg::Err(e));
                                return;
                            }
                            Err(e) => {
                                let _ = tx.send(Msg::Err(e));
                                return;
                            }
                        }
                    }
                    let _ = tx.send(Msg::Eof);
                    return;
                }
                Ok(n) => {
                    if tx.send(Msg::Data(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Msg::Err(e));
                    return;
                }
            }
        }
    });
    rx
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A spawned child with its output pumped into a buffer; `pos` is the consumed prefix.
struct Session {
    pty: Pty,
    writer: PtyWriter,
    rx: Receiver<Msg>,
    acc: Vec<u8>,
    pos: usize,
}

impl Session {
    fn new(spec: &SpawnSpec, sz: PtySize) -> Self {
        let (pty, reader, writer) = Pty::spawn(spec, sz).expect("spawn");
        Self {
            pty,
            writer,
            rx: pump(reader),
            acc: Vec::new(),
            pos: 0,
        }
    }

    fn sh(script: &str) -> Self {
        Self::new(&sh_spec(script), size(80, 24))
    }

    fn tail(&self) -> String {
        let start = self.acc.len().saturating_sub(300);
        String::from_utf8_lossy(&self.acc[start..]).into_owned()
    }

    /// Reads until `marker` appears after the consumed prefix; returns the bytes consumed up to
    /// and including the marker. Panics on EOF, read error or deadline.
    fn until(&mut self, marker: &str) -> Vec<u8> {
        let marker = marker.as_bytes();
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(i) = find(&self.acc[self.pos..], marker) {
                let end = self.pos + i + marker.len();
                let out = self.acc[self.pos..end].to_vec();
                self.pos = end;
                return out;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Msg::Data(d)) => self.acc.extend_from_slice(&d),
                Ok(Msg::Eof) | Err(RecvTimeoutError::Disconnected) => {
                    panic!(
                        "EOF before {:?}; tail: {:?}",
                        String::from_utf8_lossy(marker),
                        self.tail()
                    )
                }
                Ok(Msg::Err(e)) => {
                    panic!("read error {e:?} before marker; tail: {:?}", self.tail())
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!(
                        "deadline waiting for {:?}; tail: {:?}",
                        String::from_utf8_lossy(marker),
                        self.tail()
                    )
                }
            }
        }
    }

    /// Reads to EOF (which must be `Ok(0)`, not an error) and returns the unconsumed output.
    fn read_to_eof(&mut self) -> Vec<u8> {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(Msg::Data(d)) => self.acc.extend_from_slice(&d),
                Ok(Msg::Eof) => break,
                Ok(Msg::Err(e)) => panic!("read error at EOF: {e:?}; tail: {:?}", self.tail()),
                Err(RecvTimeoutError::Disconnected) => panic!("reader thread vanished"),
                Err(RecvTimeoutError::Timeout) => {
                    panic!("deadline waiting for EOF; tail: {:?}", self.tail())
                }
            }
        }
        let out = self.acc[self.pos..].to_vec();
        self.pos = self.acc.len();
        out
    }
}

/// Polls `try_wait` until the child is reaped or the deadline passes.
fn wait_exit(pty: &mut Pty) -> std::process::ExitStatus {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(st) = pty.try_wait().expect("try_wait") {
            return st;
        }
        assert!(
            Instant::now() < deadline,
            "child did not exit before the deadline"
        );
        thread::sleep(Duration::from_millis(5)); // polling, not synchronisation
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The text between the first `[` and the following `]`.
fn bracketed(out: &str) -> &str {
    let start = out.find('[').expect("no '['") + 1;
    let len = out[start..].find(']').expect("no ']'");
    &out[start..start + len]
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nt-pty-it-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// 1
#[test]
fn echo_round_trip() {
    let mut s = Session::sh(r#"read l; printf "got:%s\n" "$l""#);
    s.writer.write_all("héllo ✓\n".as_bytes());
    s.until("got:héllo ✓");
}

// 2
#[test]
fn utf8_split_across_reads() {
    let mut s = Session::sh("yes ✓ | head -n 10000");
    let out = s.read_to_eof();
    let text = std::str::from_utf8(&out).expect("output must be valid UTF-8");
    assert_eq!(text.matches('✓').count(), 10_000);
}

// 3
#[test]
fn initial_size_visible() {
    let mut s = Session::new(&sh_spec("stty size"), size(100, 30));
    s.until("30 100");
}

// 4
#[test]
fn resize_observed() {
    let mut s = Session::sh("trap 'stty size' WINCH; echo ready; while :; do sleep 0.05; done");
    s.until("ready");
    s.pty.resize(size(120, 40)).expect("resize");
    s.until("40 120");
}

// 5
#[test]
fn resize_rejects_zero() {
    let s = Session::sh("sleep 5");
    for bad in [size(0, 10), size(10, 0)] {
        assert_eq!(
            s.pty.resize(bad).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    for bad in [size(0, 10), size(10, 0), size(0, 0)] {
        assert!(matches!(
            Pty::spawn(&sh_spec("true"), bad),
            Err(SpawnError::InvalidSize)
        ));
    }
}

// 6
#[test]
fn exit_status() {
    let mut s = Session::sh("exit 7");
    s.read_to_eof();
    let st = wait_exit(&mut s.pty);
    assert_eq!(st.code(), Some(7));
    // The status is cached; asking again is fine.
    assert_eq!(s.pty.try_wait().unwrap().and_then(|s| s.code()), Some(7));
}

// 7
#[test]
fn eof_not_eio() {
    let (mut pty, mut reader, _writer) = Pty::spawn(&sh_spec("true"), size(80, 24)).unwrap();
    wait_exit(&mut pty);
    let mut buf = [0u8; 64];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => panic!("EOF must be Ok(0), got {e:?}"),
        }
    }
    for _ in 0..3 {
        assert_eq!(reader.read(&mut buf).unwrap(), 0);
    }
}

// 8
#[test]
fn spawn_missing_program() {
    let spec = SpawnSpec {
        program: "/nonexistent".into(),
        arg0: None,
        args: vec![],
        cwd: None,
        env: vec![],
    };
    match Pty::spawn(&spec, size(80, 24)) {
        Err(SpawnError::Exec { program, source }) => {
            assert_eq!(program, Path::new("/nonexistent"));
            assert_eq!(source.kind(), io::ErrorKind::NotFound);
        }
        Err(other) => panic!("expected Exec, got {other:?}"),
        Ok(_) => panic!("spawn of a missing program succeeded"),
    }
}

// 9
#[test]
fn bad_cwd_falls_back() {
    let mut spec = sh_spec(r#"echo "[$(pwd)]""#);
    spec.cwd = Some("/nonexistent/dir".into());
    let mut s = Session::new(&spec, size(80, 24));
    s.until("]");
    let out = text(&s.acc);
    let got = bracketed(&out);
    let canon = |p: &str| std::fs::canonicalize(p).ok();
    let ok = match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => {
            got == home || (canon(got).is_some() && canon(got) == canon(&home))
        }
        _ => got == "/",
    };
    assert!(ok, "pwd was {got:?}, expected $HOME or /");
}

// 10
#[test]
fn cwd_applied() {
    let dir = TempDir::new();
    let mut spec = sh_spec(r#"echo "[$(pwd)]""#);
    spec.cwd = Some(dir.0.clone());
    let mut s = Session::new(&spec, size(80, 24));
    s.until("]");
    let out = text(&s.acc);
    let got = std::fs::canonicalize(bracketed(&out)).unwrap();
    assert_eq!(got, std::fs::canonicalize(&dir.0).unwrap());
}

// 11(b)
#[test]
fn env_set_and_scrubbed() {
    let mut spec = sh_spec("env");
    spec.env = vec![("NEWTERM_PANE_ID".into(), "42".into())];
    let mut s = Session::new(&spec, size(80, 24));
    let out = text(&s.read_to_eof());
    let lines: Vec<&str> = out.lines().map(str::trim_end).collect();
    assert!(lines.contains(&"NEWTERM_PANE_ID=42"), "env was: {out}");
    assert!(lines.contains(&"TERM=xterm-256color"), "env was: {out}");
}

// 12
#[test]
fn controlling_tty() {
    let mut s = Session::sh(r#"echo "[$(tty)]""#);
    s.until("]");
    let out = text(&s.acc);
    let tty = bracketed(&out);
    assert!(tty.starts_with("/dev/"), "tty printed {tty:?}");
    assert!(!tty.contains("not a tty"));
    #[cfg(target_os = "linux")]
    assert!(tty.starts_with("/dev/pts/"), "tty printed {tty:?}");
}

// 12 (Linux: session and foreground group)
#[cfg(target_os = "linux")]
#[test]
fn session_leader_owns_foreground_group() {
    let mut s = Session::sh("echo pid=$$; cat /proc/$$/stat");
    let out = text(&s.read_to_eof());
    let pid: u32 = out
        .lines()
        .find_map(|l| l.trim_end().strip_prefix("pid="))
        .expect("pid line")
        .parse()
        .unwrap();
    let stat = out.lines().find(|l| l.contains(") ")).expect("stat line");
    // Fields after the comm: state ppid pgrp session tty_nr tpgid
    let f: Vec<&str> = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    let (pgrp, sid, tpgid): (u32, u32, u32) = (
        f[2].parse().unwrap(),
        f[3].parse().unwrap(),
        f[5].parse().unwrap(),
    );
    assert_eq!((pgrp, sid, tpgid), (pid, pid, pid));
    assert_eq!(pid, s.pty.child_pid());
}

// 13
#[test]
fn sigpipe_default_in_child() {
    let mut s = Session::sh(r#"yes | head -n1; echo "rc=$?""#);
    s.until("rc=0");
    let out = text(&s.acc);
    assert!(out.lines().any(|l| l.trim_end() == "y"), "output: {out:?}");
}

// 14
#[test]
fn writer_never_blocks() {
    let (pty, _reader, writer) = Pty::spawn(&sh_spec("sleep 5"), size(80, 24)).unwrap();
    let chunk = vec![b'x'; 64 * 1024];
    let t = Instant::now();
    for _ in 0..256 {
        writer.write_all(&chunk);
    }
    let wrote = t.elapsed();
    assert!(
        wrote < Duration::from_millis(50),
        "16 MiB write took {wrote:?}"
    );
    let t = Instant::now();
    drop(pty);
    let dropped = t.elapsed();
    assert!(
        dropped < Duration::from_millis(500),
        "drop took {dropped:?}"
    );
}

// 15
#[test]
fn writer_order_preserved() {
    let mut s = Session::sh("stty -echo; echo ready; cat");
    s.until("ready\r\n");
    for i in 1..=10_000u32 {
        s.writer.write_all(format!("{i}\n").as_bytes());
    }
    let out = text(&s.until("10000\r\n"));
    let got: Vec<u32> = out
        .lines()
        .map(|l| l.trim().parse().expect("number"))
        .collect();
    assert_eq!(got, (1..=10_000).collect::<Vec<_>>());
}

// 16
#[test]
fn foreground_pid_is_session_leader() {
    let mut s = Session::sh("echo r; exec sleep 3");
    s.until("r\r\n");
    assert_eq!(s.pty.foreground_pid(), Some(s.pty.child_pid()));
}

// 17
#[cfg(target_os = "linux")]
#[test]
fn no_zombie_after_drop() {
    let (pty, _reader, _writer) = Pty::spawn(&sh_spec("sleep 30"), size(80, 24)).unwrap();
    let pid = pty.child_pid();
    drop(pty);
    let deadline = Instant::now() + DEADLINE;
    loop {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => return, // reaped and gone
            Ok(stat) => {
                let state = stat.rsplit_once(") ").and_then(|(_, r)| r.chars().next());
                if state != Some('Z') {
                    return;
                }
            }
        }
        assert!(Instant::now() < deadline, "pid {pid} is still a zombie");
        thread::sleep(Duration::from_millis(5)); // polling, not synchronisation
    }
}

// 18
#[test]
fn kill_is_idempotent() {
    let (mut pty, _reader, _writer) = Pty::spawn(&sh_spec("sleep 30"), size(80, 24)).unwrap();
    pty.kill().unwrap();
    pty.kill().unwrap();
    assert!(
        pty.try_wait().unwrap().is_some(),
        "child must be reaped after kill"
    );
}

// 20
#[cfg(target_os = "linux")]
#[test]
fn master_cloexec() {
    let mut s = Session::sh("ls -l /proc/self/fd");
    let out = text(&s.read_to_eof());
    let mut seen = 0;
    for line in out.lines().filter(|l| l.contains(" -> ")) {
        let (left, target) = line.trim_end().split_once(" -> ").unwrap();
        let fd: u32 = left.split_whitespace().last().unwrap().parse().unwrap();
        seen += 1;
        // fds 0-2 are the tty; the only other fd is the directory `ls` is listing.
        assert!(
            fd <= 2 || target.starts_with("/proc/"),
            "leaked fd {fd} -> {target}"
        );
    }
    assert!(seen >= 3, "expected fds 0-2 in: {out}");
}

// 21
#[test]
fn reader_outlives_pty() {
    let (pty, mut reader, writer) = Pty::spawn(&sh_spec("sleep 30"), size(80, 24)).unwrap();
    drop(pty);
    let mut buf = [0u8; 64];
    assert_eq!(reader.read(&mut buf).unwrap(), 0);
    drop(writer);
}

// arg0 (login-shell marker, D-T05-1) reaches the child as argv[0].
#[test]
fn arg0_applied() {
    let mut spec = sh_spec(r#"echo "argv0=[$0]""#);
    spec.arg0 = Some("-nt-sh".into());
    let mut s = Session::new(&spec, size(80, 24));
    s.until("argv0=[-nt-sh]");
}
