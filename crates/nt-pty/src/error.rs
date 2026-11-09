use std::path::PathBuf;
use std::{fmt, io};

/// Why `Pty::spawn` failed; the variants need distinct user-facing messages.
#[derive(Debug)]
pub enum SpawnError {
    /// `cols` or `rows` was zero.
    InvalidSize,
    /// No PTY could be opened or set up (e.g. out of PTYs).
    OpenPty(io::Error),
    /// The program could not be executed (not found, not executable, ...).
    Exec {
        /// The program that failed.
        program: PathBuf,
        /// The exec error.
        source: io::Error,
    },
    /// Any other I/O failure while spawning.
    Io(io::Error),
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSize => f.write_str("terminal size must be at least 1x1"),
            Self::OpenPty(e) => write!(f, "cannot open a pseudo-terminal: {e}"),
            Self::Exec { program, source } => {
                write!(f, "cannot run {}: {source}", program.display())
            }
            Self::Io(e) => write!(f, "cannot spawn: {e}"),
        }
    }
}

impl std::error::Error for SpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidSize => None,
            Self::OpenPty(e) | Self::Io(e) | Self::Exec { source: e, .. } => Some(e),
        }
    }
}
