//! OSC strategy B: a gated scanner that reports OSC 7/9/99/777 exactly where
//! vte 0.15.0 (UTF-8, feature "std") would dispatch them, without a second full parse.
//!
//! Mirrored vte facts (spikes/vendor/vte-0.15.0/src/lib.rs):
//! - ESC enters Escape from every state: ground :597-616/:648-651, CSI/DCS/SOS/PM/APC via
//!   `anywhere` :437-450, DCS passthrough :325-329, OSC :419-423, Escape itself :387.
//! - Escape stays put on C0 except CAN/SUB, ESC, DEL, 0x80..=0xFF (:342, :387-388) and only
//!   ']' enters OscString (:372-376; the sole `State::OscString` transition).
//! - OscString: BEL/CAN/SUB/ESC all dispatch (:410-423); other C0 are dropped (:409);
//!   everything else, ';' and bytes >= 0x80 included, is payload (:424-433).
//!
//! So vte's other states never matter: between OSCs, only the next ESC does.

/// Largest OSC body kept. vte with "std" has no cap (lib.rs:63-64), so B equals A only below it.
/// Overflow policy: the whole OSC is dropped (never delivered truncated).
pub const OSC_CAP: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    /// Inside an OSC that may still be tracked; bytes go to `buf`.
    Osc,
    /// Inside an OSC that is untracked or over the cap; wait for its end.
    OscSkip,
}

pub struct OscScanner {
    state: State,
    buf: Vec<u8>,
}

impl Default for OscScanner {
    fn default() -> Self {
        Self::new()
    }
}

/// OSC numbers newTerm handles; `cmd` is the bytes before the first ';'.
pub fn tracked_cmd(cmd: &[u8]) -> bool {
    matches!(cmd, b"7" | b"9" | b"99" | b"777")
}

/// `false` once the first field is known not to be tracked (tracked commands are <= 3 bytes).
fn may_be_tracked(buf: &[u8]) -> bool {
    let head = &buf[..buf.len().min(4)];
    match head.iter().position(|&b| b == b';') {
        Some(p) => tracked_cmd(&head[..p]),
        None => head.len() < 4,
    }
}

impl OscScanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
        }
    }

    /// Feed PTY bytes. `on_osc` gets the body of each completed tracked OSC (between `ESC ]`
    /// and the terminator, vte-ignored C0 removed, ';' kept). The slice is reused afterwards.
    pub fn advance(&mut self, bytes: &[u8], on_osc: &mut impl FnMut(&[u8])) {
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                State::Ground => match bytes[i..].iter().position(|&b| b == 0x1B) {
                    Some(p) => {
                        i += p + 1;
                        self.state = State::Escape;
                    }
                    None => return,
                },
                State::Escape => {
                    match bytes[i] {
                        b']' => {
                            self.buf.clear();
                            self.state = State::Osc;
                        }
                        0x00..=0x17 | 0x19 | 0x1B..=0x1F | 0x7F..=0xFF => {}
                        _ => self.state = State::Ground,
                    }
                    i += 1;
                }
                State::Osc | State::OscSkip => {
                    let rest = &bytes[i..];
                    let n = rest.iter().position(|&b| b < 0x20).unwrap_or(rest.len());
                    if self.state == State::Osc {
                        if self.buf.len() + n > OSC_CAP {
                            self.state = State::OscSkip;
                        } else {
                            self.buf.extend_from_slice(&rest[..n]);
                            if !may_be_tracked(&self.buf) {
                                self.state = State::OscSkip;
                            }
                        }
                    }
                    i += n;
                    let Some(&b) = bytes.get(i) else { return };
                    i += 1;
                    if matches!(b, 0x07 | 0x18 | 0x1A | 0x1B) {
                        if self.state == State::Osc {
                            let cmd = self.buf.split(|&b| b == b';').next().unwrap_or(&[]);
                            if tracked_cmd(cmd) {
                                on_osc(&self.buf);
                            }
                        }
                        self.state = if b == 0x1B {
                            State::Escape
                        } else {
                            State::Ground
                        };
                    }
                }
            }
        }
    }
}
