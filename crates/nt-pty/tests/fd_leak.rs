//! Test 19: spawning and dropping must not leak fds. Its own binary so no other test's fds
//! show up in `/proc/self/fd`.
#![cfg(target_os = "linux")]

use std::io::Read;

use nt_pty::{Pty, PtySize, SpawnSpec};

fn fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

fn spawn_true_to_eof() {
    let spec = SpawnSpec {
        program: "/bin/sh".into(),
        arg0: None,
        args: vec!["-c".into(), "true".into()],
        cwd: None,
        env: vec![],
    };
    let size = PtySize {
        cols: 80,
        rows: 24,
        px_w: 0,
        px_h: 0,
    };
    let (pty, mut reader, writer) = Pty::spawn(&spec, size).unwrap();
    let mut buf = [0u8; 256];
    while reader.read(&mut buf).unwrap() != 0 {}
    drop(writer);
    drop(pty);
    drop(reader);
}

#[test]
fn no_fd_leak() {
    // Warm-up: lazily opened process-wide fds must not count as leaks.
    spawn_true_to_eof();
    let before = fd_count();
    for _ in 0..100 {
        spawn_true_to_eof();
    }
    let after = fd_count();
    assert_eq!(before, after, "fds leaked across 100 spawn/drop cycles");
}
