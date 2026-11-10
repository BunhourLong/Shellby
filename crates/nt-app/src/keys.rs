//! Minimal key → bytes for T08: text, Enter, Backspace, Tab, Esc, arrows, Ctrl+letter.
// shortcut: replaced whole by nt-term::input (kitty protocol, modifiers, IME) in T09.

use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Appends the legacy encoding of `event` to `out`; leaves `out` untouched for keys this
/// minimal table does not cover.
pub(crate) fn encode_basic(
    event: &KeyEvent,
    mods: ModifiersState,
    app_cursor: bool,
    out: &mut Vec<u8>,
) {
    let arrow = |c: u8| {
        if app_cursor {
            [0x1b, b'O', c]
        } else {
            [0x1b, b'[', c]
        }
    };
    let ctrl_only = mods.control_key() && !mods.alt_key() && !mods.super_key();
    match &event.logical_key {
        Key::Named(NamedKey::Enter) => out.push(b'\r'),
        Key::Named(NamedKey::Backspace) => out.push(0x7f),
        Key::Named(NamedKey::Tab) => out.push(b'\t'),
        Key::Named(NamedKey::Escape) => out.push(0x1b),
        Key::Named(NamedKey::ArrowUp) => out.extend_from_slice(&arrow(b'A')),
        Key::Named(NamedKey::ArrowDown) => out.extend_from_slice(&arrow(b'B')),
        Key::Named(NamedKey::ArrowRight) => out.extend_from_slice(&arrow(b'C')),
        Key::Named(NamedKey::ArrowLeft) => out.extend_from_slice(&arrow(b'D')),
        Key::Named(NamedKey::Space) if ctrl_only => out.push(0),
        Key::Character(s) if ctrl_only => {
            if let [c] = s.as_bytes() {
                match c.to_ascii_lowercase() {
                    c @ b'a'..=b'z' => out.push(c - b'a' + 1),
                    b'[' => out.push(0x1b),
                    _ => {}
                }
            }
        }
        _ if !mods.control_key() && !mods.alt_key() && !mods.super_key() => {
            if let Some(text) = &event.text {
                out.extend_from_slice(text.as_bytes());
            }
        }
        _ => {}
    }
}
