//! T03 PTY spike: three spawn backends behind one `match` (no trait: throwaway code).
//! Shared helpers for the `checks` and `perf` bins live here too.

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, Signal, WaitId, WaitIdOptions};
use rustix::termios::Winsize;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const BACKENDS: [&str; 3] = ["rustix", "portable", "alacritty"];

pub enum Handle {
    Rustix {
        master: OwnedFd,
        child: Child,
    },
    Portable {
        master: Box<dyn portable_pty::MasterPty + Send>,
        child: Box<dyn portable_pty::Child + Send + Sync>,
    },
    /// alacritty's `Pty` owns master and child together; its `Drop` sends SIGHUP and blocks in `wait`.
    Alacritty(alacritty_terminal::tty::Pty),
}

pub struct Spawned {
    pub pid: u32,
    pub h: Handle,
}

pub struct Exit {
    /// Time from master close (or alacritty `Pty` drop) to the child's exit.
    pub after: Duration,
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

pub fn winsize(rows: u16, cols: u16) -> Winsize {
    Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

pub fn spawn(
    backend: &str,
    program: &str,
    args: &[&str],
    size: Winsize,
    force_fcntl_loop: bool,
) -> io::Result<Spawned> {
    match backend {
        "rustix" => spawn_rustix(program, args, size, force_fcntl_loop),
        "portable" => spawn_portable(program, args, size),
        "alacritty" => spawn_alacritty(program, args, size),
        _ => Err(io::Error::other(format!("unknown backend {backend}"))),
    }
}

#[cfg(target_os = "linux")]
fn open_slave(master: &OwnedFd) -> io::Result<OwnedFd> {
    use rustix::pty::{OpenptFlags, ioctl_tiocgptpeer};
    // TIOCGPTPEER: no path lookup, so no TOCTOU on /dev/pts.
    match ioctl_tiocgptpeer(
        master,
        OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC,
    ) {
        Ok(fd) => Ok(fd),
        Err(_) => open_slave_by_path(master),
    }
}

#[cfg(not(target_os = "linux"))]
fn open_slave(master: &OwnedFd) -> io::Result<OwnedFd> {
    open_slave_by_path(master)
}

fn open_slave_by_path(master: &OwnedFd) -> io::Result<OwnedFd> {
    use rustix::fs::{Mode, OFlags, open};
    let path = rustix::pty::ptsname(master, Vec::new())?;
    Ok(open(
        path.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

pub fn spawn_rustix(
    program: &str,
    args: &[&str],
    size: Winsize,
    force_fcntl_loop: bool,
) -> io::Result<Spawned> {
    use rustix::pty::{OpenptFlags, grantpt, openpt, unlockpt};
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
    grantpt(&master)?;
    unlockpt(&master)?;
    let slave = open_slave(&master)?;
    rustix::termios::tcsetwinsize(&master, size)?;

    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // Everything the child needs is computed here, before fork; the closure captures plain values.
    let use_close_range = cfg!(target_os = "linux") && !force_fcntl_loop;
    let fd_cap = rustix::process::getrlimit(rustix::process::Resource::Nofile)
        .current
        .unwrap_or(65_536)
        .min(65_536) as i32;
    // SAFETY: the closure runs in the forked child of a multithreaded parent, so it may only make
    // async-signal-safe calls: it captures two Copy values, allocates nothing, takes no lock, prints
    // nothing, and calls only `child_setup`, whose calls are each justified there.
    unsafe {
        cmd.pre_exec(move || child_setup(use_close_range, fd_cap));
    }
    let child = cmd.spawn();
    drop(cmd); // closes the parent's slave copies; otherwise EIO never arrives on the master
    let child = child?;
    Ok(Spawned {
        pid: child.id(),
        h: Handle::Rustix { master, child },
    })
}

/// Runs between fork and exec (std has already dup2'd the slave onto 0/1/2).
fn child_setup(use_close_range: bool, fd_cap: i32) -> io::Result<()> {
    // SAFETY: sigemptyset and sigprocmask are on the signal-safety(7) list; `set` is a local,
    // fully initialised by sigemptyset before use. The child is single-threaded after fork.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
    }
    // A parent started via `nohup` or as a background job of a non-interactive shell inherits
    // SIG_IGN for HUP/INT/QUIT, and exec keeps SIG_IGN; reset so the shell sees defaults (C4).
    // SIGPIPE is already reset by std. SIGCHLD SIG_IGN would make the shell's waits fail.
    for sig in [
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGTSTP,
        libc::SIGTTIN,
        libc::SIGTTOU,
        libc::SIGCHLD,
        libc::SIGTERM,
        libc::SIGALRM,
    ] {
        // SAFETY: signal() is on the signal-safety(7) list (glibc implements it as one sigaction
        // syscall); SIG_DFL installs no handler code.
        unsafe { libc::signal(sig, libc::SIG_DFL) };
    }
    // rustix on Linux issues raw syscalls: no allocation, no locks.
    rustix::process::setsid()?;
    rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
    // Mark (not close) every fd >= 3 close-on-exec: closing would break std's exec-error pipe.
    // Fallback (kernel < 5.11, or forced): the fcntl loop below.
    #[cfg(target_os = "linux")]
    if use_close_range {
        // SAFETY: close_range is a direct syscall wrapper (no allocation, no locks), so it is
        // async-signal-safe; it only flips FD_CLOEXEC on descriptors this child owns.
        let rc = unsafe { libc::close_range(3, u32::MAX, libc::CLOSE_RANGE_CLOEXEC as i32) };
        if rc == 0 {
            return Ok(());
        }
    }
    let _ = use_close_range;
    for fd in 3..fd_cap {
        // SAFETY: fcntl(F_SETFD) is on the signal-safety(7) list. libc (not rustix) because the
        // number may be closed: EBADF is the expected answer, and rustix's BorrowedFd::borrow_raw
        // contract requires an open fd. FD_CLOEXEC is the only fd flag, so overwriting is fine.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok(())
}

pub fn spawn_portable(program: &str, args: &[&str], size: Winsize) -> io::Result<Spawned> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: size.ws_row,
            cols: size.ws_col,
            pixel_width: size.ws_xpixel,
            pixel_height: size.ws_ypixel,
        })
        .map_err(display_err)?;
    let mut cb = CommandBuilder::new(program);
    cb.args(args);
    let child = pair.slave.spawn_command(cb).map_err(display_err)?;
    drop(pair.slave);
    Ok(Spawned {
        pid: child.process_id().unwrap_or(0),
        h: Handle::Portable {
            master: pair.master,
            child,
        },
    })
}

fn display_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{e:#}"))
}

pub fn spawn_alacritty(program: &str, args: &[&str], size: Winsize) -> io::Result<Spawned> {
    use alacritty_terminal::event::WindowSize;
    use alacritty_terminal::tty::{Options, Shell, new};
    let opts = Options {
        shell: Some(Shell::new(
            program.into(),
            args.iter().map(|s| s.to_string()).collect(),
        )),
        ..Options::default()
    };
    let pty = new(
        &opts,
        WindowSize {
            num_lines: size.ws_row,
            num_cols: size.ws_col,
            cell_width: 0,
            cell_height: 0,
        },
        0,
    )?;
    Ok(Spawned {
        pid: pty.child().id(),
        h: Handle::Alacritty(pty),
    })
}

impl Spawned {
    pub fn fd(&self) -> BorrowedFd<'_> {
        match &self.h {
            Handle::Rustix { master, .. } => master.as_fd(),
            Handle::Portable { master, .. } => {
                let raw = master.as_raw_fd().expect("unix master has an fd");
                // SAFETY: `raw` is owned by `master`, which lives as long as the returned borrow
                // (tied to &self); portable-pty never closes it before drop.
                unsafe { BorrowedFd::borrow_raw(raw) }
            }
            Handle::Alacritty(pty) => pty.file().as_fd(),
        }
    }

    /// Close the master (alacritty: drop the whole `Pty`), wait up to `timeout` for the child to
    /// exit, SIGKILL it on timeout, then reap. Err carries the timeout description.
    pub fn close(self, timeout: Duration) -> Result<Exit, String> {
        let pid = Pid::from_raw(self.pid as i32).ok_or("pid 0")?;
        // pidfd before close: pins the pid against reuse and lets us poll for exit.
        let pidfd = rustix::process::pidfd_open(pid, PidfdFlags::empty())
            .map_err(|e| format!("pidfd_open: {e}"))?;
        let t0 = Instant::now();
        let reap: Box<dyn FnOnce() + Send> = match self.h {
            Handle::Rustix { master, mut child } => {
                drop(master);
                Box::new(move || drop(child.wait()))
            }
            Handle::Portable { master, mut child } => {
                drop(master);
                Box::new(move || drop(child.wait()))
            }
            Handle::Alacritty(pty) => {
                let t = std::thread::spawn(move || drop(pty));
                Box::new(move || drop(t.join()))
            }
        };
        let exited = wait_readable(pidfd.as_fd(), timeout);
        let after = t0.elapsed();
        let (mut code, mut signal) = (None, None);
        if exited {
            let opts = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
            if let Ok(Some(st)) = rustix::process::waitid(WaitId::PidFd(pidfd.as_fd()), opts) {
                (code, signal) = (st.exit_status(), st.terminating_signal());
            }
        } else {
            let _ = rustix::process::pidfd_send_signal(&pidfd, Signal::KILL);
        }
        reap();
        if exited {
            Ok(Exit {
                after,
                code,
                signal,
            })
        } else {
            Err(format!(
                "child alive {timeout:?} after master close; SIGKILLed"
            ))
        }
    }
}

/// poll(fd, POLLIN) with a deadline, retrying EINTR (alacritty installs a global SIGCHLD handler).
pub fn wait_readable(fd: BorrowedFd<'_>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ts = Timespec {
            tv_sec: left.as_secs() as _,
            tv_nsec: left.subsec_nanos() as _,
        };
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        match poll(&mut fds, Some(&ts)) {
            Ok(0) => return false,
            Ok(_) => return true,
            Err(Errno::INTR) => continue,
            Err(_) => return false,
        }
    }
}

/// Append master output to `out` until `done(out)` (Ok(false)) or EOF (Ok(true)); EIO is EOF.
/// Works on blocking and O_NONBLOCK fds alike (poll before every read).
pub fn read_until(
    fd: BorrowedFd<'_>,
    out: &mut Vec<u8>,
    timeout: Duration,
    mut done: impl FnMut(&[u8]) -> bool,
) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 65_536];
    while !done(out) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || !wait_readable(fd, left) {
            return Err(format!("timeout after {timeout:?}; got {:?}", tail(out)));
        }
        match rustix::io::read(fd, &mut buf) {
            Ok(0) | Err(Errno::IO) => return Ok(true),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(Errno::INTR | Errno::AGAIN) => {}
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    Ok(false)
}

pub fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Last 120 bytes, lossily, for FAIL details.
pub fn tail(b: &[u8]) -> String {
    String::from_utf8_lossy(&b[b.len().saturating_sub(120)..]).into_owned()
}

pub fn write_all(fd: BorrowedFd<'_>, mut b: &[u8]) -> io::Result<()> {
    while !b.is_empty() {
        match rustix::io::write(fd, b) {
            Ok(n) => b = &b[n..],
            Err(Errno::INTR) => {}
            Err(Errno::AGAIN) => {
                let mut fds = [PollFd::new(&fd, PollFlags::OUT)];
                let _ = poll(&mut fds, None);
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
