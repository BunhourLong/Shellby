//! PTY spawn and I/O: a child process on a fresh pseudo-terminal, a blocking reader, and a
//! never-blocking writer. Knows nothing about terminals or rendering (00-architecture §3.4).

mod env;
mod error;
mod shell;
mod sys;
mod writer;

use std::ffi::OsString;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;

use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group};
use rustix::termios::Winsize;

pub use error::SpawnError;
pub use writer::PtyWriter;

/// Terminal size in cells and pixels; `cols` and `rows` must be non-zero.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PtySize {
    /// Columns (cells).
    pub cols: u16,
    /// Rows (cells).
    pub rows: u16,
    /// Width in pixels, 0 if unknown.
    pub px_w: u16,
    /// Height in pixels, 0 if unknown.
    pub px_h: u16,
}

/// What to run on the PTY.
#[derive(Clone, Debug)]
pub struct SpawnSpec {
    /// Program to exec; searched on `PATH` when not absolute.
    pub program: PathBuf,
    /// `argv[0]`; `None` uses `program` as given. `"-zsh"` style marks a login shell.
    pub arg0: Option<OsString>,
    /// Arguments after `argv[0]`.
    pub args: Vec<OsString>,
    /// Working directory; `None` inherits ours. An invalid dir falls back to `$HOME`, then `/`.
    pub cwd: Option<PathBuf>,
    /// Applied last, overriding inherited and default variables (the caller owns `TERM`).
    pub env: Vec<(OsString, OsString)>,
}

impl SpawnSpec {
    /// The user's shell (`$SHELL`, then the passwd entry, then `/bin/sh`), as a login shell on
    /// macOS (`arg0 = "-<basename>"`, D-T05-1) and a non-login shell on Linux, with no args, cwd
    /// or extra env.
    pub fn default_shell() -> Self {
        let candidates = [
            std::env::var_os("SHELL").map(PathBuf::from),
            sys::passwd_shell(),
        ];
        let program = shell::first_valid_shell(candidates.into_iter().flatten());
        Self {
            arg0: shell::login_arg0(&program),
            program,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
        }
    }
}

/// A child process running on a PTY. Dropping it hangs up and reaps the child.
#[derive(Debug)]
pub struct Pty {
    master: OwnedFd,
    child: Child,
    reaped: Option<ExitStatus>,
    writer: Option<writer::WriterHandle>,
}

impl Pty {
    /// Spawns `spec` on a new PTY of `size`, in its own session with the PTY as controlling tty.
    ///
    /// An invalid `cwd` falls back to `$HOME`, then `/`, and never fails the spawn.
    pub fn spawn(
        spec: &SpawnSpec,
        size: PtySize,
    ) -> Result<(Pty, PtyReader, PtyWriter), SpawnError> {
        let ws = winsize(size).ok_or(SpawnError::InvalidSize)?;
        let (master, slave) = sys::open_pty(ws).map_err(SpawnError::OpenPty)?;

        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .env_clear()
            .envs(env::child_env(std::env::vars_os(), &spec.env))
            .stdin(Stdio::from(slave.try_clone().map_err(SpawnError::Io)?))
            .stdout(Stdio::from(slave.try_clone().map_err(SpawnError::Io)?))
            .stderr(Stdio::from(slave));
        if let Some(arg0) = &spec.arg0 {
            cmd.arg0(arg0);
        }
        if let Some(dir) = &spec.cwd {
            cmd.current_dir(resolve_cwd(dir));
        }
        sys::set_child_setup(&mut cmd);
        let child = cmd.spawn();
        drop(cmd); // closes the parent's slave copies; otherwise EIO never arrives on the master
        let child = child.map_err(|source| SpawnError::Exec {
            program: spec.program.clone(),
            source,
        })?;

        // From here on, dropping `pty` hangs up and reaps the child.
        let mut pty = Pty {
            master,
            child,
            reaped: None,
            writer: None,
        };
        let reader = PtyReader {
            fd: pty.master.try_clone().map_err(SpawnError::Io)?,
        };
        let (writer, handle) = writer::start(pty.master.try_clone().map_err(SpawnError::Io)?)
            .map_err(SpawnError::Io)?;
        pty.writer = Some(handle);
        Ok((pty, reader, writer))
    }

    /// Sets the window size (the kernel sends SIGWINCH); zero cols or rows is `InvalidInput`.
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        let ws = winsize(size).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal size must be at least 1x1",
            )
        })?;
        Ok(rustix::termios::tcsetwinsize(&self.master, ws)?)
    }

    /// The child's pid (also its process-group and session id).
    pub fn child_pid(&self) -> u32 {
        self.child.id()
    }

    /// The PTY's foreground process-group id (a pgid, not necessarily the "interesting" pid);
    /// `None` on error.
    pub fn foreground_pid(&self) -> Option<u32> {
        let pgid = rustix::termios::tcgetpgrp(&self.master).ok()?;
        u32::try_from(pgid.as_raw_pid()).ok()
    }

    /// Reaps the child if it has exited; the status is cached, so repeated calls are fine.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.reaped.is_none() {
            self.reaped = self.child.try_wait()?;
        }
        Ok(self.reaped)
    }

    /// SIGHUP to the child's process group, then SIGKILL after a grace period, then reap.
    /// Idempotent; returns within ~150 ms.
    pub fn kill(&mut self) -> io::Result<()> {
        if self.reaped.is_some() {
            return Ok(());
        }
        // Unreaped, so the pid (== pgid, the child is a session leader) cannot be reused.
        let pgid = Pid::from_child(&self.child);
        signal_group(pgid, Signal::HUP)?;
        if let Some(status) = sys::wait_timeout(&mut self.child, Duration::from_millis(100))? {
            self.reaped = Some(status);
            return Ok(());
        }
        signal_group(pgid, Signal::KILL)?;
        self.reaped = Some(self.child.wait()?);
        Ok(())
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if self.reaped.is_none() {
            let _ = self.kill();
        }
        if let Some(writer) = self.writer.take() {
            writer.shutdown(Duration::from_millis(100));
        }
        // `master` closes after this body.
    }
}

/// `size` as a `Winsize`, or `None` if `cols` or `rows` is zero.
fn winsize(size: PtySize) -> Option<Winsize> {
    (size.cols != 0 && size.rows != 0).then_some(Winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.px_w,
        ws_ypixel: size.px_h,
    })
}

/// Sends `sig` to process group `pgid`; a group that is already gone is not an error.
fn signal_group(pgid: Pid, sig: Signal) -> io::Result<()> {
    match kill_process_group(pgid, sig) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// `dir` if it is an absolute existing directory, else `$HOME` (same check), else `/`.
fn resolve_cwd(dir: &Path) -> PathBuf {
    let valid = |p: &Path| p.is_absolute() && p.metadata().is_ok_and(|m| m.is_dir());
    if valid(dir) {
        return dir.to_path_buf();
    }
    let fallback = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| valid(h))
        .unwrap_or_else(|| PathBuf::from("/"));
    if std::env::var_os("NT_LOG").is_some() {
        #[allow(
            clippy::print_stderr,
            reason = "no logger until T06; opt-in via NT_LOG"
        )]
        {
            eprintln!(
                "nt-pty: cwd {} is not a directory, using {}",
                dir.display(),
                fallback.display()
            );
        }
    }
    fallback
}

/// Blocking reader of the child's output. `read` returns `Ok(0)` at EOF (child gone; EIO maps
/// here) and retries EINTR. May outlive `Pty`.
#[derive(Debug)]
pub struct PtyReader {
    fd: OwnedFd,
}

impl io::Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match rustix::io::read(&self.fd, &mut *buf) {
                Ok(n) => return Ok(n),
                Err(Errno::INTR) => {}
                Err(Errno::IO) => return Ok(0),
                Err(e) => return Err(e.into()),
            }
        }
    }
}
