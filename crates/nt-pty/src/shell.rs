//! Default shell detection and login-shell `argv[0]`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// True if `path` is absolute and names an executable regular file.
pub(crate) fn is_valid_shell(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_absolute()
        && path
            .metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The first valid candidate, else `/bin/sh`.
pub(crate) fn first_valid_shell(candidates: impl IntoIterator<Item = PathBuf>) -> PathBuf {
    candidates
        .into_iter()
        .find(|p| is_valid_shell(p))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// `Some("-<basename>")` on macOS (login shell, D-T05-1), `None` on Linux.
pub(crate) fn login_arg0(program: &Path) -> Option<OsString> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let mut arg0 = OsString::from("-");
    arg0.push(program.file_name()?);
    Some(arg0)
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
