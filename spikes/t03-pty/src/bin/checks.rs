//! `checks <rustix|portable|alacritty|all> [--force-fcntl-loop] [--alloc-noise]`
//! Prints `backend Cn PASS|FAIL detail` for C3-C6, C8-C14. Child output markers drive sync.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use t03_pty::{BACKENDS, Spawned, contains, read_until, spawn, tail, winsize, write_all};

const T: Duration = Duration::from_secs(10);
type Check = fn(&'static str, bool) -> Result<String, String>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let which = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map_or("all", String::as_str);
    let force = args.iter().any(|a| a == "--force-fcntl-loop");
    dirty_parent();
    if args.iter().any(|a| a == "--alloc-noise") {
        thread::spawn(alloc_noise);
    }
    let checks: [(&str, Check); 11] = [
        ("C3", c3),
        ("C4", c4),
        ("C5", c5),
        ("C6", c6),
        ("C8", c8),
        ("C9", c9),
        ("C10", c10),
        ("C11", c11),
        ("C12", c12),
        ("C13", c13),
        ("C14", c14),
    ];
    for b in BACKENDS.iter().filter(|b| which == "all" || **b == which) {
        for (name, f) in checks {
            // Watchdog on top of the per-read 10 s timeouts: a stuck check is a FAIL, not a hang.
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || drop(tx.send(f(b, force))));
            let line = match rx.recv_timeout(Duration::from_secs(120)) {
                Ok(Ok(d)) => format!("PASS {d}"),
                Ok(Err(d)) => format!("FAIL {d}"),
                Err(mpsc::RecvTimeoutError::Timeout) => "FAIL watchdog 120s (thread leaked)".into(),
                Err(mpsc::RecvTimeoutError::Disconnected) => "FAIL check panicked".into(),
            };
            println!("{b} {name} {line}");
        }
    }
}

/// Make the parent look like a GUI app launched by `nohup`/a script, so C4 proves the reset:
/// SIGUSR2 blocked in the mask (inherited by all threads spawned later), INT/QUIT/TSTP/HUP ignored.
fn dirty_parent() {
    // SAFETY: called once at the start of main, before any thread exists; plain libc calls on a
    // local, initialised sigset.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGUSR2);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        for s in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP, libc::SIGHUP] {
            libc::signal(s, libc::SIG_IGN);
        }
    }
}

fn alloc_noise() {
    loop {
        for sz in [16usize, 256, 4096, 65_536, 1 << 20] {
            std::hint::black_box(vec![1u8; sz]);
        }
    }
}

fn run(b: &str, prog: &str, args: &[&str], force: bool) -> Result<Spawned, String> {
    spawn(b, prog, args, winsize(24, 80), force).map_err(|e| format!("spawn: {e}"))
}

/// Spawn, read all output until EIO, close (reap). Returns (output, exit code).
fn run_to_eof(
    b: &str,
    prog: &str,
    args: &[&str],
    rows: u16,
    cols: u16,
    force: bool,
) -> Result<(String, Option<i32>), String> {
    let s = spawn(b, prog, args, winsize(rows, cols), force).map_err(|e| format!("spawn: {e}"))?;
    let mut out = Vec::new();
    let r = read_until(s.fd(), &mut out, T, |_| false);
    let exit = s.close(T)?;
    r?;
    Ok((String::from_utf8_lossy(&out).into_owned(), exit.code))
}

fn c3(b: &'static str, force: bool) -> Result<String, String> {
    let s = run(b, "/bin/sh", &["-c", "tty; cat /proc/$$/stat"], force)?;
    let pts = rustix::pty::ptsname(s.fd(), Vec::new()).map_err(|e| format!("ptsname: {e}"))?;
    let pts = pts.to_string_lossy().into_owned();
    let mut out = Vec::new();
    let r = read_until(s.fd(), &mut out, T, |_| false);
    let pid = s.pid as i64;
    s.close(T)?;
    r?;
    let out = String::from_utf8_lossy(&out);
    let tty = out.lines().next().unwrap_or("").trim();
    let f: Vec<i64> = out
        .rsplit_once(')')
        .map(|(_, r)| {
            r.split_whitespace()
                .skip(1)
                .filter_map(|x| x.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    // after ')': state ppid pgrp session tty_nr tpgid -> f = [ppid, pgrp, session, tty_nr, tpgid]
    let (sid, tty_nr, tpgid) = (
        f.get(2).copied().unwrap_or(-9),
        f.get(3).copied().unwrap_or(0),
        f.get(4).copied().unwrap_or(-9),
    );
    let d = format!("tty={tty} expect={pts} pid={pid} sid={sid} tty_nr={tty_nr} tpgid={tpgid}");
    if tty == pts && sid == pid && tty_nr != 0 && tpgid > 0 {
        Ok(d)
    } else {
        Err(d)
    }
}

fn c4(b: &'static str, force: bool) -> Result<String, String> {
    let (out, _) = run_to_eof(
        b,
        "grep",
        &["-E", "^Sig(Blk|Ign)", "/proc/self/status"],
        24,
        80,
        force,
    )?;
    let field = |k: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(k))
            .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
    };
    let (blk, ign) = (field("SigBlk:"), field("SigIgn:"));
    let must_dfl = [
        ("PIPE", libc::SIGPIPE),
        ("INT", libc::SIGINT),
        ("QUIT", libc::SIGQUIT),
        ("TSTP", libc::SIGTSTP),
        ("CHLD", libc::SIGCHLD),
    ];
    let bad: Vec<&str> = must_dfl
        .iter()
        .filter(|(_, s)| ign.unwrap_or(u64::MAX) & (1 << (s - 1)) != 0)
        .map(|(n, _)| *n)
        .collect();
    let d = format!("SigBlk={blk:016x?} SigIgn={ign:016x?} ignored={bad:?}");
    if blk == Some(0) && bad.is_empty() {
        Ok(d)
    } else {
        Err(d)
    }
}

fn c5(b: &'static str, force: bool) -> Result<String, String> {
    use rustix::fs::{CWD, Mode, OFlags, openat};
    let canary = openat(CWD, "/dev/null", OFlags::RDONLY, Mode::empty())
        .map_err(|e| format!("canary: {e}"))?;
    let cloexec = rustix::io::fcntl_getfd(&canary).map_err(|e| e.to_string())?;
    if !cloexec.is_empty() {
        return Err("harness: canary unexpectedly CLOEXEC".into());
    }
    let (out, _) = run_to_eof(b, "ls", &["-l", "/proc/self/fd"], 24, 80, force)?;
    let leaked: Vec<String> = out
        .lines()
        .filter_map(|l| l.split_once(" -> "))
        .filter_map(|(l, t)| Some((l.split_whitespace().last()?.parse::<i32>().ok()?, t.trim())))
        .filter(|(fd, t)| *fd >= 3 && !t.starts_with("/proc/"))
        .map(|(fd, t)| format!("{fd}->{t}"))
        .collect();
    let d = format!(
        "canary_fd={} leaked={leaked:?}",
        std::os::fd::AsRawFd::as_raw_fd(&canary)
    );
    if leaked.is_empty() { Ok(d) } else { Err(d) }
}

fn c6(b: &'static str, force: bool) -> Result<String, String> {
    let (fds, z) = (count_fds(), zombies());
    let t0 = Instant::now();
    let r = spawn(b, "/nonexistent/t03-prog", &[], winsize(24, 80), force);
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    let err = match r {
        Ok(s) => {
            let _ = s.close(T);
            return Err("spawn returned Ok".into());
        }
        Err(e) => e.to_string(),
    };
    let (dfd, dz) = (count_fds() as i64 - fds as i64, zombies() as i64 - z as i64);
    let d = format!("err=\"{err}\" in {ms:.1}ms fd_delta={dfd} zombie_delta={dz}");
    if dfd == 0 && dz == 0 { Ok(d) } else { Err(d) }
}

fn c8(b: &'static str, force: bool) -> Result<String, String> {
    let (out, _) = run_to_eof(b, "stty", &["size"], 37, 101, force)?;
    let got = out.trim();
    if got == "37 101" {
        Ok(format!("stty size={got}"))
    } else {
        Err(format!("stty size={got:?} expected \"37 101\""))
    }
}

fn c9(b: &'static str, force: bool) -> Result<String, String> {
    let script =
        "trap 'echo WINCH; stty size; exit 0' WINCH; echo READY; while :; do sleep 0.05; done";
    let s = run(b, "/bin/sh", &["-c", script], force)?;
    let mut out = Vec::new();
    let r = read_until(s.fd(), &mut out, T, |o| contains(o, b"READY")).and_then(|_| {
        rustix::termios::tcsetwinsize(s.fd(), winsize(50, 120)).map_err(|e| e.to_string())?;
        read_until(s.fd(), &mut out, T, |_| false)
    });
    s.close(T)?;
    r?;
    let d = tail(&out).replace("\r\n", "|");
    if contains(&out, b"WINCH") && contains(&out, b"50 120") {
        Ok(d)
    } else {
        Err(d)
    }
}

fn c10(b: &'static str, force: bool) -> Result<String, String> {
    let s = run(b, "head", &["-c", "1048576", "/dev/zero"], force)?;
    let mut out = Vec::with_capacity(1 << 21);
    let r = read_until(s.fd(), &mut out, T, |_| false);
    let exit = s.close(T)?;
    let eof = r?;
    let d = format!("bytes={} eof_via_eio={eof} exit={:?}", out.len(), exit.code);
    if out.len() == 1 << 20 && eof && exit.code == Some(0) {
        Ok(d)
    } else {
        Err(d)
    }
}

fn c11(b: &'static str, force: bool) -> Result<String, String> {
    let z = zombies();
    let s = run(b, "/bin/sh", &["-c", "printf done"], force)?;
    let pid = s.pid;
    let mut out = Vec::new();
    let r = read_until(s.fd(), &mut out, T, |_| false);
    let exit = s.close(T)?;
    r?;
    let gone = !std::path::Path::new(&format!("/proc/{pid}")).exists();
    let dz = zombies() as i64 - z as i64;
    let d = format!("exit={:?} proc_gone={gone} zombie_delta={dz}", exit.code);
    if gone && dz == 0 { Ok(d) } else { Err(d) }
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn c12(b: &'static str, force: bool) -> Result<String, String> {
    let mut ok = true;
    let mut d = Vec::new();
    for (sh, args) in [
        ("bash", &["--norc", "--noprofile", "-i"][..]),
        ("zsh", &["-f", "-i"][..]),
        ("fish", &["--no-config", "-i"][..]),
    ] {
        let Some(path) = which(sh) else {
            d.push(format!("{sh}:skip(not installed)"));
            continue;
        };
        let s = run(b, path.to_str().unwrap_or(sh), args, force)?;
        let mut out = Vec::new();
        let fd = s.fd();
        // fish >= 4 blocks startup on DA1 (ESC[0c) and CPR (ESC[6n) queries; answer them like a
        // VT220 at 1;1. Then type the marker command once the shell has asked its last question.
        let mut scanned = 0;
        let mut typed = false;
        let ready = read_until(fd, &mut out, T, |o| {
            let new = &o[scanned..];
            if contains(new, b"\x1b[6n") {
                let _ = write_all(fd, b"\x1b[1;1R");
            }
            if contains(new, b"\x1b[0c") || contains(new, b"\x1b[c") {
                let _ = write_all(fd, b"\x1b[?62;c");
            }
            if !typed && (sh != "fish" || contains(new, b"\x1b[?2004h")) {
                typed = write_all(fd, b"echo RE''ADY\r").is_ok();
            }
            scanned = o.len().saturating_sub(2); // overlap < shortest query (3 bytes): no double reply
            contains(o, b"READY")
        })
        .is_ok();
        match s.close(T) {
            Ok(e) => d.push(format!(
                "{sh}:exit_after={:.2}ms sig={:?} code={:?}{}",
                e.after.as_secs_f64() * 1e3,
                e.signal,
                e.code,
                if ready { "" } else { " (no READY)" }
            )),
            Err(e) => {
                ok = false;
                d.push(format!("{sh}:{e}"));
            }
        }
    }
    if b == "alacritty" {
        d.push("(alacritty: Pty drop = SIGHUP+close+wait, not master close alone)".into());
    }
    let d = d.join(" ");
    if ok { Ok(d) } else { Err(d) }
}

fn c13(b: &'static str, force: bool) -> Result<String, String> {
    let s = run(b, "/bin/sh", &["-i"], force)?;
    let r = c13_inner(&s);
    s.close(T)?;
    r
}

fn c13_inner(s: &Spawned) -> Result<String, String> {
    let mut out = Vec::new();
    write_all(s.fd(), b"echo RE''ADY\r").map_err(|e| e.to_string())?;
    read_until(s.fd(), &mut out, T, |o| contains(o, b"READY"))?;
    let pgrp = || rustix::termios::tcgetpgrp(s.fd()).map(|p| p.as_raw_nonzero().get());
    let idle = pgrp().map_err(|e| format!("tcgetpgrp idle: {e}"))?;
    // The inner sh is the job's process-group leader; it prints its pid then execs sleep in place,
    // so the printed pid is sleep's pid and the job's pgid.
    write_all(s.fd(), b"sh -c 'echo PID:$$; exec sleep 1000'\r").map_err(|e| e.to_string())?;
    let parse = |o: &[u8]| -> Option<i32> {
        let o = String::from_utf8_lossy(o);
        o.match_indices("PID:").find_map(|(i, _)| {
            let n: String = o[i + 4..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if o[i + 4 + n.len()..].starts_with(['\r', '\n']) {
                n.parse().ok()
            } else {
                None
            }
        })
    };
    read_until(s.fd(), &mut out, T, |o| parse(o).is_some())?;
    let sleep_pid = parse(&out).ok_or("no PID")?;
    // Bounded wait for exec to finish (comm flips from sh to sleep).
    let deadline = Instant::now() + T;
    let comm = || std::fs::read_to_string(format!("/proc/{sleep_pid}/comm")).unwrap_or_default();
    while comm().trim() != "sleep" && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    let fg = pgrp().map_err(|e| format!("tcgetpgrp fg: {e}"))?;
    let d = format!(
        "shell={} idle_fg={idle} sleep={sleep_pid} comm={} fg={fg}",
        s.pid,
        comm().trim()
    );
    if idle == s.pid as i32 && fg == sleep_pid && comm().trim() == "sleep" {
        Ok(d)
    } else {
        Err(d)
    }
}

fn c14(b: &'static str, force: bool) -> Result<String, String> {
    let cycle = || -> Result<Duration, String> {
        let s = run(b, "/bin/sh", &["-c", "printf R; exec sleep 1000"], force)?;
        let mut out = Vec::new();
        let r = read_until(s.fd(), &mut out, T, |o| o.contains(&b'R'));
        let e = s.close(T)?;
        r?;
        Ok(e.after)
    };
    cycle()?; // warm-up: first spawn may lazily open process-wide fds
    let (fds, z, th) = (count_fds(), zombies(), count("/proc/self/task"));
    let mut total = Duration::ZERO;
    for i in 0..100 {
        total += cycle().map_err(|e| format!("cycle {i}: {e}"))?;
    }
    let delta = |a: usize, b: usize| a as i64 - b as i64;
    let (dfd, dz, dth) = (
        delta(count_fds(), fds),
        delta(zombies(), z),
        delta(count("/proc/self/task"), th),
    );
    let d = format!(
        "fd_delta={dfd} zombie_delta={dz} thread_delta={dth} mean_hangup_exit={:.2}ms",
        total.as_secs_f64() * 10.0
    );
    if dfd == 0 && dz == 0 && dth == 0 {
        Ok(d)
    } else {
        Err(d)
    }
}

fn count(dir: &str) -> usize {
    std::fs::read_dir(dir).map_or(0, |d| d.count())
}

fn count_fds() -> usize {
    count("/proc/self/fd")
}

/// Zombie children of this process (state Z, ppid == us).
fn zombies() -> usize {
    let me = std::process::id().to_string();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return 0;
    };
    dir.filter_map(|e| std::fs::read_to_string(e.ok()?.path().join("stat")).ok())
        .filter(|st| {
            let mut f = st.rsplit_once(')').map_or("", |x| x.1).split_whitespace();
            f.next() == Some("Z") && f.next() == Some(me.as_str())
        })
        .count()
}
