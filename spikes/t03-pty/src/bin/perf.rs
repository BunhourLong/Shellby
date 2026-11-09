//! `perf <rustix|portable|alacritty|all>`: echo latency (direct, model I, model II), throughput,
//! spawn time (per backend), writer backpressure. Echo/throughput/backpressure use the rustix
//! backend: the I/O model is independent of who opened the fd.

use rustix::event::{EventfdFlags, PollFd, PollFlags, eventfd, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, Signal};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use t03_pty::{
    BACKENDS, Spawned, contains, read_until, spawn, spawn_rustix, wait_readable, winsize, write_all,
};

const T: Duration = Duration::from_secs(10);
const ECHO_N: usize = 10_000;
const RAW_CAT: &str = "stty raw -echo; printf R; exec cat";

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    println!(
        "{:<34} {:>10} {:>10} {:>10}",
        "metric", "p50_us", "p99_us", "max_us"
    );
    row("echo direct (write+read)", echo_direct());
    row("echo model I (sync_channel+writer)", echo_model1());
    row("echo model II (poll+eventfd thread)", echo_model2());
    for b in BACKENDS.iter().filter(|b| which == "all" || **b == which) {
        row(&format!("spawn->first byte {b} (n=100)"), spawn_time(b));
    }
    println!();
    throughput();
    println!();
    backpressure();
}

fn row(name: &str, r: Result<Vec<u64>, String>) {
    match r {
        Ok(mut v) => {
            v.sort_unstable();
            let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize] as f64 / 1e3;
            println!(
                "{name:<34} {:>10.1} {:>10.1} {:>10.1}",
                p(0.5),
                p(0.99),
                p(1.0)
            );
        }
        Err(e) => println!("{name:<34} ERROR {e}"),
    }
}

fn raw_cat() -> Result<Spawned, String> {
    let s = spawn_rustix("/bin/sh", &["-c", RAW_CAT], winsize(24, 80), false)
        .map_err(|e| e.to_string())?;
    read_until(s.fd(), &mut Vec::new(), T, |o| contains(o, b"R"))?;
    Ok(s)
}

/// Blocking 1-byte read (fd is blocking in the rustix backend).
fn read1(fd: BorrowedFd<'_>) -> Result<u8, String> {
    let mut b = [0u8; 1];
    loop {
        match rustix::io::read(fd, &mut b) {
            Ok(1) => return Ok(b[0]),
            Ok(_) | Err(Errno::IO) => return Err("EOF".into()),
            Err(Errno::INTR) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// 100 warm-up + ECHO_N timed 1-byte round trips; `rt(byte)` must return the echoed byte.
fn echo_loop(mut rt: impl FnMut(u8) -> Result<u8, String>) -> Result<Vec<u64>, String> {
    let mut v = Vec::with_capacity(ECHO_N);
    for i in 0..ECHO_N + 100 {
        let c = b'a' + (i % 26) as u8;
        let t0 = Instant::now();
        let got = rt(c)?;
        let dt = t0.elapsed().as_nanos() as u64;
        if got != c {
            return Err(format!("echo mismatch at {i}: sent {c} got {got}"));
        }
        if i >= 100 {
            v.push(dt);
        }
    }
    Ok(v)
}

fn echo_direct() -> Result<Vec<u64>, String> {
    let s = raw_cat()?;
    let fd = s.fd();
    let r = echo_loop(|c| {
        write_all(fd, &[c]).map_err(|e| e.to_string())?;
        read1(fd)
    });
    let _ = s.close(T);
    r
}

fn model1(fd: OwnedFd) -> (SyncSender<Vec<u8>>, JoinHandle<()>) {
    let (tx, rx) = sync_channel::<Vec<u8>>(1024);
    let th = thread::spawn(move || {
        for b in rx {
            if write_all(fd.as_fd(), &b).is_err() {
                break;
            }
        }
    });
    (tx, th)
}

fn echo_model1() -> Result<Vec<u64>, String> {
    let s = raw_cat()?;
    let (tx, th) = model1(s.fd().try_clone_to_owned().map_err(|e| e.to_string())?);
    let fd = s.fd();
    let r = echo_loop(|c| {
        tx.send(vec![c]).map_err(|e| e.to_string())?;
        read1(fd)
    });
    let _ = s.close(T); // EIO makes the writer's next write fail; dropping tx ends it otherwise
    drop(tx);
    let _ = th.join();
    r
}

/// Model II: one I/O thread, O_NONBLOCK master, poll {master IN [+OUT], eventfd}.
struct Io2 {
    q: Arc<Mutex<(Vec<u8>, bool)>>,
    efd: Arc<OwnedFd>,
    rx: Receiver<Vec<u8>>,
    th: JoinHandle<()>,
}

impl Io2 {
    fn start(master: OwnedFd) -> Result<Io2, String> {
        let fl = fcntl_getfl(&master).map_err(|e| e.to_string())?;
        fcntl_setfl(&master, fl | OFlags::NONBLOCK).map_err(|e| e.to_string())?;
        let efd = Arc::new(
            eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
                .map_err(|e| e.to_string())?,
        );
        let q = Arc::new(Mutex::new((Vec::new(), false)));
        let (tx, rx) = channel();
        let (q2, efd2) = (q.clone(), efd.clone());
        let th = thread::spawn(move || io2_loop(master, &efd2, &q2, tx));
        Ok(Io2 { q, efd, rx, th })
    }

    fn send(&self, b: &[u8]) {
        self.q.lock().unwrap().0.extend_from_slice(b);
        let _ = rustix::io::write(&*self.efd, &1u64.to_ne_bytes());
    }

    fn stop(self) {
        self.q.lock().unwrap().1 = true;
        let _ = rustix::io::write(&*self.efd, &1u64.to_ne_bytes());
        let _ = self.th.join();
    }
}

fn io2_loop(
    master: OwnedFd,
    efd: &OwnedFd,
    q: &Mutex<(Vec<u8>, bool)>,
    tx: std::sync::mpsc::Sender<Vec<u8>>,
) {
    let (mut pending, mut off) = (Vec::<u8>::new(), 0usize);
    let mut buf = vec![0u8; 65_536];
    loop {
        let want = if off < pending.len() {
            PollFlags::IN | PollFlags::OUT
        } else {
            PollFlags::IN
        };
        let mut fds = [PollFd::new(&master, want), PollFd::new(efd, PollFlags::IN)];
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(_) => return,
        }
        let (m, e) = (fds[0].revents(), fds[1].revents());
        if e.contains(PollFlags::IN) {
            let _ = rustix::io::read(efd, &mut [0u8; 8]);
            let mut g = q.lock().unwrap();
            if g.1 {
                return;
            }
            if off == pending.len() {
                pending.clear();
                off = 0;
            }
            pending.append(&mut g.0);
        }
        // Try the write right away on new bytes; POLLOUT only matters after EAGAIN.
        while off < pending.len() {
            match rustix::io::write(&master, &pending[off..]) {
                Ok(n) => off += n,
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => break,
                Err(_) => return,
            }
        }
        if m.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
            loop {
                match rustix::io::read(&master, &mut buf) {
                    Ok(0) | Err(Errno::IO) => return,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                    Err(Errno::INTR) => {}
                    Err(Errno::AGAIN) => break,
                    Err(_) => return,
                }
            }
        }
    }
}

fn echo_model2() -> Result<Vec<u64>, String> {
    let s = raw_cat()?;
    let io = Io2::start(s.fd().try_clone_to_owned().map_err(|e| e.to_string())?)?;
    let r = echo_loop(|c| {
        io.send(&[c]);
        let v = io.rx.recv_timeout(T).map_err(|e| e.to_string())?;
        if v.len() == 1 {
            Ok(v[0])
        } else {
            Err(format!("got {} bytes", v.len()))
        }
    });
    io.stop();
    let _ = s.close(T);
    r
}

fn spawn_time(b: &str) -> Result<Vec<u64>, String> {
    let mut v = Vec::with_capacity(100);
    for _ in 0..100 {
        let t0 = Instant::now();
        let s = spawn(b, "/bin/sh", &["-c", "printf x"], winsize(24, 80), false)
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        read_until(s.fd(), &mut out, T, |o| !o.is_empty())?;
        v.push(t0.elapsed().as_nanos() as u64);
        let _ = read_until(s.fd(), &mut out, T, |_| false);
        s.close(T)?;
    }
    Ok(v)
}

/// Deletes the file on drop, also on early return.
struct TmpFile(PathBuf);
impl Drop for TmpFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn tmp(name: &str) -> TmpFile {
    TmpFile(std::env::temp_dir().join(format!("t03-{}-{name}", std::process::id())))
}

/// Printable pseudo-random bytes: raw mode passes them untouched and they are never tty controls.
fn pattern(len: usize) -> Vec<u8> {
    let mut x = 0x2545_f491_4f6c_dd1du64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            b' ' + (x % 95) as u8
        })
        .collect()
}

fn throughput() {
    const SIZE: usize = 256 << 20;
    let f = tmp("tp.bin");
    let chunk = pattern(1 << 20);
    let w = std::fs::File::create(&f.0).and_then(|mut file| {
        use std::io::Write;
        (0..SIZE / chunk.len()).try_for_each(|_| file.write_all(&chunk))
    });
    if let Err(e) = w {
        println!("throughput ERROR {e}");
        return;
    }
    println!(
        "{:<22} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "throughput 256MiB", "MB/s", "reads", "=1", "<=512", "<=4K", "<=16K", "<=64K"
    );
    for bufsz in [65_536usize, 4096] {
        match throughput_run(&f.0, bufsz, SIZE) {
            Ok((mbs, n, h)) => println!(
                "{:<22} {mbs:>9.0} {n:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
                format!("buf {} KiB", bufsz / 1024),
                h[0],
                h[1],
                h[2],
                h[3],
                h[4]
            ),
            Err(e) => println!("buf {bufsz}: ERROR {e}"),
        }
    }
}

fn throughput_run(
    path: &std::path::Path,
    bufsz: usize,
    size: usize,
) -> Result<(f64, u64, [u64; 5]), String> {
    let p = path.to_str().ok_or("non-utf8 tmp path")?;
    let s = spawn_rustix(
        "/bin/sh",
        &["-c", "stty raw -echo; printf R; exec cat \"$0\"", p],
        winsize(24, 80),
        false,
    )
    .map_err(|e| e.to_string())?;
    let fd = s.fd();
    let mut buf = vec![0u8; bufsz];
    let (mut total, mut reads, mut h) = (0usize, 0u64, [0u64; 5]);
    let mut t0 = None;
    let res = loop {
        let n = match rustix::io::read(fd, &mut buf) {
            Ok(0) | Err(Errno::IO) => break Ok(()),
            Ok(n) => n,
            Err(Errno::INTR) => continue,
            Err(e) => break Err(e.to_string()),
        };
        let data = match t0 {
            Some(_) => n,
            None => {
                // The marker byte starts the clock; anything after it in this read is data.
                let Some(i) = buf[..n].iter().position(|&c| c == b'R') else {
                    continue;
                };
                t0 = Some(Instant::now());
                n - i - 1
            }
        };
        total += data;
        reads += 1;
        h[match n {
            1 => 0,
            2..=512 => 1,
            513..=4096 => 2,
            4097..=16_384 => 3,
            _ => 4,
        }] += 1;
    };
    let secs = t0.map_or(0.0, |t| t.elapsed().as_secs_f64());
    let _ = s.close(T);
    res?;
    if total != size {
        return Err(format!("read {total} bytes, expected {size}"));
    }
    Ok((size as f64 / secs / 1e6, reads, h))
}

fn backpressure() {
    println!(
        "{:<22} {:>12} {:>14} {:>10}",
        "backpressure 1MiB", "enqueue_us", "drain_ms", "in_order"
    );
    for model in ["model I", "model II"] {
        match backpressure_run(model) {
            Ok((enq, drain, ok)) => println!(
                "{model:<22} {:>12.1} {:>14.2} {:>10}",
                enq.as_secs_f64() * 1e6,
                drain.as_secs_f64() * 1e3,
                ok
            ),
            Err(e) => println!("{model:<22} ERROR {e}"),
        }
    }
}

/// Child stops itself (so it is provably not reading), we enqueue 1 MiB and time the enqueue,
/// then SIGCONT; `head` copies exactly 1 MiB to a file which we compare byte for byte.
fn backpressure_run(model: &str) -> Result<(Duration, Duration, bool), String> {
    let out = tmp(&format!("bp-{}.out", model.len()));
    let p = out.0.to_str().ok_or("non-utf8 tmp path")?;
    let script = "stty raw -echo; printf R; kill -STOP $$; exec head -c 1048576 > \"$0\"";
    let s = spawn_rustix("/bin/sh", &["-c", script, p], winsize(24, 80), false)
        .map_err(|e| e.to_string())?;
    let pid = Pid::from_raw(s.pid as i32).ok_or("pid")?;
    let pidfd = rustix::process::pidfd_open(pid, PidfdFlags::empty()).map_err(|e| e.to_string())?;
    let r = (|| {
        read_until(s.fd(), &mut Vec::new(), T, |o| contains(o, b"R"))?;
        let stat = || std::fs::read_to_string(format!("/proc/{}/stat", s.pid)).unwrap_or_default();
        let deadline = Instant::now() + T;
        while !stat()
            .rsplit_once(')')
            .is_some_and(|x| x.1.starts_with(" T"))
        {
            if Instant::now() > deadline {
                return Err("child never stopped".to_string());
            }
            thread::sleep(Duration::from_millis(1));
        }
        let data = pattern(1 << 20);
        let dup = s.fd().try_clone_to_owned().map_err(|e| e.to_string())?;
        let (enq, finish): (Duration, Box<dyn FnOnce()>) = if model == "model I" {
            let (tx, th) = model1(dup);
            let msg = data.clone();
            let t0 = Instant::now();
            tx.send(msg).map_err(|e| e.to_string())?;
            let enq = t0.elapsed();
            (
                enq,
                Box::new(move || {
                    drop(tx);
                    let _ = th.join();
                }),
            )
        } else {
            let io = Io2::start(dup)?;
            let t0 = Instant::now();
            io.send(&data);
            let enq = t0.elapsed();
            (enq, Box::new(move || io.stop()))
        };
        let t1 = Instant::now();
        rustix::process::pidfd_send_signal(&pidfd, Signal::CONT).map_err(|e| e.to_string())?;
        let exited = wait_readable(pidfd.as_fd(), T);
        let drain = t1.elapsed();
        if !exited {
            // Unblocks a writer stuck on a full tty queue (its write then fails with EIO).
            let _ = rustix::process::pidfd_send_signal(&pidfd, Signal::KILL);
        }
        finish();
        if !exited {
            return Err("head did not finish within 10 s".into());
        }
        let got = std::fs::read(&out.0).map_err(|e| e.to_string())?;
        Ok((enq, drain, got == data))
    })();
    let _ = s.close(T);
    r
}
