use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A unique temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nt-pty-shell-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str, mode: u32) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn accepts_executable_regular_file() {
    let t = TempDir::new();
    assert!(is_valid_shell(&t.file("sh", 0o755)));
}

#[test]
fn rejects_relative_path() {
    assert!(!is_valid_shell(Path::new("sh")));
    assert!(!is_valid_shell(Path::new("bin/sh")));
    assert!(!is_valid_shell(Path::new("./sh")));
}

#[test]
fn rejects_empty_path() {
    assert!(!is_valid_shell(Path::new("")));
}

#[test]
fn rejects_directory() {
    let t = TempDir::new();
    assert!(!is_valid_shell(&t.0));
    assert!(!is_valid_shell(Path::new("/")));
}

#[test]
fn rejects_non_executable_file() {
    let t = TempDir::new();
    assert!(!is_valid_shell(&t.file("plain", 0o644)));
}

#[test]
fn rejects_missing_file() {
    let t = TempDir::new();
    assert!(!is_valid_shell(&t.0.join("absent")));
}

#[test]
fn first_valid_shell_picks_first_valid_candidate() {
    let t = TempDir::new();
    let good = t.file("good", 0o755);
    let later = t.file("later", 0o755);
    let got = first_valid_shell([
        PathBuf::from("relative-sh"),
        t.0.clone(),
        t.file("plain", 0o644),
        good.clone(),
        later,
    ]);
    assert_eq!(got, good);
}

#[test]
fn first_valid_shell_falls_through_to_bin_sh() {
    let t = TempDir::new();
    let got = first_valid_shell([
        PathBuf::from("relative-sh"),
        t.0.clone(),
        t.file("plain", 0o644),
    ]);
    assert_eq!(got, PathBuf::from("/bin/sh"));
}

#[test]
fn first_valid_shell_with_no_candidates_is_bin_sh() {
    assert_eq!(first_valid_shell(Vec::new()), PathBuf::from("/bin/sh"));
}

#[cfg(target_os = "linux")]
#[test]
fn login_arg0_is_none_on_linux() {
    assert_eq!(login_arg0(Path::new("/bin/zsh")), None);
    assert_eq!(login_arg0(Path::new("/bin/sh")), None);
}

#[cfg(target_os = "macos")]
#[test]
fn login_arg0_is_dash_basename_on_macos() {
    assert_eq!(
        login_arg0(Path::new("/bin/zsh")),
        Some(OsString::from("-zsh"))
    );
    assert_eq!(
        login_arg0(Path::new("/opt/homebrew/bin/fish")),
        Some(OsString::from("-fish"))
    );
}
