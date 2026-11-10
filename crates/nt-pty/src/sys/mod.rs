//! Platform PTY code: open master/slave, the child's pre-exec setup, passwd lookup, timed wait.
//! The only module that uses `libc` (D-T03-2); everything else goes through rustix.

use std::ffi::{CStr, OsStr};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

use rustix::pty::{OpenptFlags, grantpt, openpt, unlockpt};
use rustix::termios::Winsize;

/// Opens a master (close-on-exec) and its slave (`O_NOCTTY | O_CLOEXEC`), sized `size`.
pub(crate) fn open_pty(size: Winsize) -> io::Result<(OwnedFd, OwnedFd)> {
    let master = open_master()?;
    grantpt(&master)?;
    unlockpt(&master)?;
    let slave = open_slave(&master)?;
    rustix::termios::tcsetwinsize(&master, size)?;
    Ok((master, slave))
}

#[cfg(target_os = "linux")]
fn open_master() -> io::Result<OwnedFd> {
    Ok(openpt(
        OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC,
    )?)
}

#[cfg(not(target_os = "linux"))]
fn open_master() -> io::Result<OwnedFd> {
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
    // shortcut: macOS posix_openpt takes no O_CLOEXEC, so a fork in another thread between these
    // two calls leaks the master into that child. Upgrade if T22 finds a spawn racing PTY opens.
    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
    Ok(master)
}

#[cfg(target_os = "linux")]
fn open_slave(master: &OwnedFd) -> io::Result<OwnedFd> {
    // TIOCGPTPEER: no path lookup, so no TOCTOU on /dev/pts.
    rustix::pty::ioctl_tiocgptpeer(
        master,
        OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC,
    )
    .or_else(|_| open_slave_by_path(master))
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

/// Installs the child setup (signal reset, new session, controlling tty, CLOEXEC on fds >= 3) as
/// `cmd`'s `pre_exec`. Expects stdin to be the slave.
pub(crate) fn set_child_setup(cmd: &mut Command) {
    // Everything the child needs is computed here, before fork; the closure captures Copy values.
    let mut empty = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigemptyset only writes the local `empty`, initialising it fully.
    unsafe { libc::sigemptyset(empty.as_mut_ptr()) };
    // SAFETY: initialised by sigemptyset just above.
    let empty = unsafe { empty.assume_init() };
    // SAFETY: sigaction is plain data; all-zero is a valid value (null handler, no flags), and
    // every field that matters is set below.
    let mut dfl: libc::sigaction = unsafe { std::mem::zeroed() };
    dfl.sa_sigaction = libc::SIG_DFL;
    dfl.sa_mask = empty;
    let fd_cap = rustix::process::getrlimit(rustix::process::Resource::Nofile)
        .current
        .unwrap_or(65_536)
        .min(65_536);
    let fd_cap = libc::c_int::try_from(fd_cap).unwrap_or(65_536);
    // SAFETY: the closure runs in the forked child of a multithreaded parent, so it may only make
    // async-signal-safe calls: it captures three Copy values, allocates nothing, takes no lock,
    // cannot panic, and calls only `child_setup`, whose calls are each justified there.
    unsafe {
        cmd.pre_exec(move || child_setup(&empty, &dfl, fd_cap));
    }
}

/// Runs between fork and exec (std has already dup2'd the slave onto 0/1/2).
fn child_setup(
    empty: &libc::sigset_t,
    dfl: &libc::sigaction,
    fd_cap: libc::c_int,
) -> io::Result<()> {
    // SAFETY: sigprocmask is on the signal-safety(7) list; `empty` is an initialised set. The
    // child is single-threaded after fork. std does not reset the mask itself.
    let rc = unsafe { libc::sigprocmask(libc::SIG_SETMASK, empty, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // A parent started via `nohup` or as a background job inherits SIG_IGN for some of these, and
    // exec keeps SIG_IGN; reset so the shell sees defaults. std also resets SIGPIPE; repeated here
    // so the child's dispositions do not depend on std internals.
    for sig in [
        libc::SIGPIPE,
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
        // SAFETY: sigaction is on the signal-safety(7) list; `dfl` is a fully initialised
        // SIG_DFL action with an empty mask, so no handler code is installed.
        let rc = unsafe { libc::sigaction(sig, dfl, std::ptr::null_mut()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // setsid and ioctl are async-signal-safe (raw syscalls on Linux, libc wrappers on macOS).
    rustix::process::setsid()?;
    rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
    // Mark (not close) every fd >= 3 close-on-exec: closing would break std's exec-error pipe.
    #[cfg(target_os = "linux")]
    {
        // SAFETY: close_range is a direct syscall wrapper (no allocation, no locks), so it is
        // async-signal-safe; it only sets FD_CLOEXEC on descriptors this child owns.
        let rc = unsafe { libc::close_range(3, u32::MAX, libc::CLOSE_RANGE_CLOEXEC.cast_signed()) };
        if rc == 0 {
            return Ok(());
        }
    }
    // Fallback (macOS, or Linux < 5.11).
    for fd in 3..fd_cap {
        // SAFETY: fcntl(F_SETFD) is on the signal-safety(7) list. libc (not rustix) because the
        // number may be closed: EBADF is expected, and rustix's BorrowedFd needs an open fd.
        // FD_CLOEXEC is the only fd flag, so overwriting is fine.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok(())
}

/// The current user's login shell from the passwd database (thread-safe `getpwuid_r`).
pub(crate) fn passwd_shell() -> Option<PathBuf> {
    let uid = rustix::process::getuid().as_raw();
    let mut buf = vec![0u8; 1024];
    loop {
        // SAFETY: passwd is plain data (integers and pointers); all-zero is valid, and it is only
        // read after getpwuid_r reports success.
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut res: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer refers to a live local; `buf.len()` is the buffer's real size.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &raw mut pw,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &raw mut res,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || res.is_null() || pw.pw_shell.is_null() {
            return None;
        }
        // SAFETY: on success pw_shell points to a NUL-terminated string inside `buf`, still alive.
        let shell = unsafe { CStr::from_ptr(pw.pw_shell) };
        return Some(PathBuf::from(OsStr::from_bytes(shell.to_bytes())));
    }
}

/// Waits up to `timeout` for `child` to exit; `Ok(None)` if it is still running.
pub(crate) fn wait_timeout(child: &mut Child, timeout: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    #[cfg(target_os = "linux")]
    {
        use rustix::process::{Pid, PidfdFlags, pidfd_open};
        // The child is not reaped yet, so its pid cannot be reused under us.
        if let Ok(pidfd) = pidfd_open(Pid::from_child(child), PidfdFlags::empty()) {
            wait_readable(&pidfd, deadline);
            return child.try_wait();
        }
    }
    // Fallback (macOS, or Linux < 5.3): poll.
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        std::thread::sleep(left.min(Duration::from_millis(5)));
    }
}

/// poll(fd, POLLIN) until `deadline`, retrying EINTR; true if readable (or hung up).
// shortcut: macOS poll() on ptys is unverified (older kernels returned POLLNVAL for
// character devices); VERIFY at T22, fall back to select() if it fails there.
pub(crate) fn wait_readable(fd: &OwnedFd, deadline: Instant) -> bool {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(ts) = Timespec::try_from(left) else {
            return false;
        };
        let mut fds = [PollFd::new(fd, PollFlags::IN)];
        match poll(&mut fds, Some(&ts)) {
            Ok(n) => return n > 0,
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return false,
        }
    }
}
