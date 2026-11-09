//! Tiny helpers used by the bench. Nothing here runs inside a timed region.

#![allow(dead_code)]

use std::process::Command;

fn fc_match(format: &str, pattern: &str) -> Option<String> {
    let out = Command::new("fc-match").args(["-f", format]).arg(pattern).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Fontconfig's first match for `pattern`, as a file path.
pub fn fc_match_file(pattern: &str) -> Option<String> {
    fc_match("%{file}", pattern)
}

/// Fontconfig's first match for `pattern`, as a family name.
pub fn fc_match_family(pattern: &str) -> Option<String> {
    fc_match("%{family[0]}", pattern)
}

/// Approximate the family fontconfig picks for a single character.
pub fn fc_match_family_for_char(ch: char) -> Option<String> {
    fc_match_family(&format!("JetBrains Mono:charset={:x}", ch as u32))
}

/// Peak resident set size in bytes from `/proc/self/status` (`VmHWM`).
pub fn vmhwm_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// Path behind a fontdb source, when it has one.
#[cfg(feature = "swash")]
pub fn source_path(source: &fontdb::Source) -> Option<String> {
    match source {
        fontdb::Source::File(path) => Some(path.display().to_string()),
        fontdb::Source::SharedFile(path, _) => Some(path.display().to_string()),
        fontdb::Source::Binary(_) => None,
    }
}

/// Path behind a cosmic-text (fontdb) source, when it has one.
#[cfg(feature = "cosmic")]
pub fn cosine_source_path(source: &cosmic_text::fontdb::Source) -> Option<String> {
    match source {
        cosmic_text::fontdb::Source::File(path) => Some(path.display().to_string()),
        cosmic_text::fontdb::Source::SharedFile(path, _) => Some(path.display().to_string()),
        cosmic_text::fontdb::Source::Binary(_) => None,
    }
}
