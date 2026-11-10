//! Minimal xterm-style key → bytes for T08: text, Ctrl+letter, Alt as an ESC prefix, and
//! cursor/editing keys with xterm modifier parameters (`CSI 1;<m>D`, m = 1 + Shift + 2·Alt +
//! 4·Ctrl). Super is not encoded.
// shortcut: replaced whole by nt-term::input (kitty protocol, keypad, F-keys, IME) in T09.

use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Appends the legacy encoding of `key` (with winit's `text` for it) to `out`; leaves `out`
/// untouched for keys this table does not cover.
pub(crate) fn encode_basic(
    key: &Key,
    text: Option<&str>,
    mods: ModifiersState,
    app_cursor: bool,
    out: &mut Vec<u8>,
) {
    let (shift, alt, ctrl) = (mods.shift_key(), mods.alt_key(), mods.control_key());
    let m = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    // Cursor keys: SS3/CSI <c> unmodified, CSI 1;<m> <c> with modifiers.
    let letter = |c: u8, out: &mut Vec<u8>| match m {
        1 if app_cursor => out.extend_from_slice(&[0x1b, b'O', c]),
        1 => out.extend_from_slice(&[0x1b, b'[', c]),
        _ => out.extend_from_slice(&[0x1b, b'[', b'1', b';', b'0' + m, c]),
    };
    let tilde = |n: u8, out: &mut Vec<u8>| match m {
        1 => out.extend_from_slice(&[0x1b, b'[', n, b'~']),
        _ => out.extend_from_slice(&[0x1b, b'[', n, b';', b'0' + m, b'~']),
    };
    let esc = |out: &mut Vec<u8>| {
        if alt {
            out.push(0x1b);
        }
    };
    match key {
        Key::Named(NamedKey::ArrowUp) => letter(b'A', out),
        Key::Named(NamedKey::ArrowDown) => letter(b'B', out),
        Key::Named(NamedKey::ArrowRight) => letter(b'C', out),
        Key::Named(NamedKey::ArrowLeft) => letter(b'D', out),
        Key::Named(NamedKey::Home) => letter(b'H', out),
        Key::Named(NamedKey::End) => letter(b'F', out),
        Key::Named(NamedKey::Insert) => tilde(b'2', out),
        Key::Named(NamedKey::Delete) => tilde(b'3', out),
        Key::Named(NamedKey::PageUp) => tilde(b'5', out),
        Key::Named(NamedKey::PageDown) => tilde(b'6', out),
        Key::Named(NamedKey::Tab) if shift => out.extend_from_slice(b"\x1b[Z"),
        Key::Named(NamedKey::Tab) => {
            esc(out);
            out.push(b'\t');
        }
        Key::Named(NamedKey::Enter) => {
            esc(out);
            out.push(b'\r');
        }
        Key::Named(NamedKey::Backspace) => {
            esc(out);
            out.push(if ctrl { 0x08 } else { 0x7f });
        }
        Key::Named(NamedKey::Escape) => {
            esc(out);
            out.push(0x1b);
        }
        Key::Named(NamedKey::Space) if ctrl => {
            esc(out);
            out.push(0);
        }
        Key::Character(s) if ctrl => {
            if let [c] = s.as_bytes() {
                let byte = match c.to_ascii_lowercase() {
                    c @ b'a'..=b'z' => c - b'a' + 1,
                    b'[' => 0x1b,
                    _ => return,
                };
                esc(out);
                out.push(byte);
            }
        }
        // Alt+char: the logical key (xkb leaves Alt out of `text` on Linux, but not everywhere).
        Key::Character(s) if alt && !mods.super_key() => {
            esc(out);
            out.extend_from_slice(s.as_bytes());
        }
        _ if !mods.super_key() => {
            if let Some(text) = text {
                esc(out);
                out.extend_from_slice(text.as_bytes());
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(key: &Key, mods: ModifiersState) -> Vec<u8> {
        let text = match key {
            Key::Character(s) => Some(s.to_string()),
            _ => None,
        };
        let mut out = Vec::new();
        encode_basic(key, text.as_deref(), mods, false, &mut out);
        out
    }

    const SHIFT: ModifiersState = ModifiersState::SHIFT;
    const ALT: ModifiersState = ModifiersState::ALT;
    const CTRL: ModifiersState = ModifiersState::CONTROL;

    #[test]
    fn modified_arrows_use_xterm_parameters() {
        // zsh-shift-select binds Shift+arrows; ~/.zshrc-style word motion binds Alt+arrows.
        assert_eq!(enc(&Key::Named(NamedKey::ArrowLeft), SHIFT), b"\x1b[1;2D");
        assert_eq!(enc(&Key::Named(NamedKey::ArrowRight), ALT), b"\x1b[1;3C");
        assert_eq!(
            enc(&Key::Named(NamedKey::ArrowUp), SHIFT | ALT),
            b"\x1b[1;4A"
        );
        assert_eq!(enc(&Key::Named(NamedKey::ArrowDown), CTRL), b"\x1b[1;5B");
        assert_eq!(
            enc(&Key::Named(NamedKey::ArrowLeft), CTRL | SHIFT),
            b"\x1b[1;6D"
        );
    }

    #[test]
    fn plain_arrows_follow_cursor_key_mode() {
        assert_eq!(
            enc(&Key::Named(NamedKey::ArrowLeft), ModifiersState::empty()),
            b"\x1b[D"
        );
        let mut out = Vec::new();
        encode_basic(
            &Key::Named(NamedKey::ArrowLeft),
            None,
            ModifiersState::empty(),
            true,
            &mut out,
        );
        assert_eq!(out, b"\x1bOD");
    }

    #[test]
    fn home_end_and_tilde_keys() {
        assert_eq!(
            enc(&Key::Named(NamedKey::Home), ModifiersState::empty()),
            b"\x1b[H"
        );
        assert_eq!(enc(&Key::Named(NamedKey::End), SHIFT), b"\x1b[1;2F");
        assert_eq!(
            enc(&Key::Named(NamedKey::Delete), ModifiersState::empty()),
            b"\x1b[3~"
        );
        assert_eq!(enc(&Key::Named(NamedKey::PageUp), SHIFT), b"\x1b[5;2~");
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(enc(&Key::Character("b".into()), ALT), b"\x1bb");
        assert_eq!(enc(&Key::Named(NamedKey::Backspace), ALT), b"\x1b\x7f");
        assert_eq!(enc(&Key::Character("c".into()), CTRL | ALT), b"\x1b\x03");
    }

    #[test]
    fn unmodified_basics_unchanged() {
        let none = ModifiersState::empty();
        assert_eq!(enc(&Key::Character("a".into()), none), b"a");
        assert_eq!(enc(&Key::Character("c".into()), CTRL), b"\x03");
        assert_eq!(enc(&Key::Named(NamedKey::Enter), none), b"\r");
        assert_eq!(enc(&Key::Named(NamedKey::Tab), SHIFT), b"\x1b[Z");
        assert_eq!(enc(&Key::Named(NamedKey::Space), CTRL), b"\0");
    }
}
