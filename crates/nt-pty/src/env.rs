//! Child environment: scrub the launching terminal's identity, add ours, apply the caller's.

use std::collections::BTreeMap;
use std::ffi::OsString;

/// Variables that describe the launching terminal and would lie inside ours (plus `ALACRITTY_*`).
const SCRUBBED: [&str; 15] = [
    "TERM",
    "TERMCAP",
    "COLUMNS",
    "LINES",
    "TERM_SESSION_ID",
    "ITERM_SESSION_ID",
    "KITTY_WINDOW_ID",
    "WEZTERM_PANE",
    "VTE_VERSION",
    "WINDOWID",
    "XDG_ACTIVATION_TOKEN",
    "DESKTOP_STARTUP_ID",
    "TMUX",
    "TMUX_PANE",
    "STY",
];

/// Returns the full child env: `inherited` minus parent-terminal variables, plus `COLORTERM`,
/// `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`, `TERM=xterm-256color` only if `extra` has no `TERM`,
/// then `extra` (wins over everything).
pub(crate) fn child_env(
    inherited: impl Iterator<Item = (OsString, OsString)>,
    extra: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    let scrubbed = |k: &OsString| {
        let k = k.as_encoded_bytes();
        k.starts_with(b"ALACRITTY_") || SCRUBBED.iter().any(|s| s.as_bytes() == k)
    };
    // A map keeps keys unique; later inserts win.
    let mut env: BTreeMap<OsString, OsString> = inherited.filter(|(k, _)| !scrubbed(k)).collect();
    let ours = [
        ("COLORTERM", "truecolor"),
        ("TERM_PROGRAM", "newterm"),
        ("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION")),
        ("TERM", "xterm-256color"),
    ];
    env.extend(ours.map(|(k, v)| (k.into(), v.into())));
    env.extend(extra.iter().cloned());
    env.into_iter().collect()
}

#[cfg(test)]
#[path = "env_tests.rs"]
mod tests;
