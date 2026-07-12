//! Keyboard -> VT byte-sequence encoding.
//!
//! Ported from EasyTer's keyPressEvent, whose behavior is pinned by its
//! dev/test_input_ux.py suite; the unit tests below mirror those expectations
//! so both terminals answer the keyboard identically.

use winit::keyboard::{Key, NamedKey};

/// xterm modifier parameter: 1 + Shift(1) + Alt(2) + Ctrl(4).
fn xterm_mod(shift: bool, alt: bool, ctrl: bool) -> u8 {
    1 + (shift as u8) + ((alt as u8) << 1) + ((ctrl as u8) << 2)
}

fn csi_arrow(ch: char, m: u8) -> Vec<u8> {
    if m == 1 {
        format!("\x1b[{ch}").into_bytes()
    } else {
        format!("\x1b[1;{m}{ch}").into_bytes()
    }
}

fn csi_tilde(n: u8, m: u8) -> Vec<u8> {
    if m == 1 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{m}~").into_bytes()
    }
}

/// Encode one key press as the bytes the child process should receive.
/// `text` is winit's composed text for the event (IME/layout aware), used
/// for plain character input — including Arabic.
pub fn encode(
    key: &Key,
    text: Option<&str>,
    shift: bool,
    alt: bool,
    ctrl: bool,
) -> Option<Vec<u8>> {
    let m = xterm_mod(shift, alt, ctrl);
    let seq = match key {
        // Shift+Enter = meta-Enter: inserts a newline in Ink TUIs (Claude Code)
        Key::Named(NamedKey::Enter) => {
            if shift {
                b"\x1b\r".to_vec()
            } else {
                b"\r".to_vec()
            }
        }
        // Ctrl+Backspace rubs out the previous word (readline/PSReadLine \x08)
        Key::Named(NamedKey::Backspace) => {
            if ctrl && !shift {
                b"\x08".to_vec()
            } else {
                b"\x7f".to_vec()
            }
        }
        Key::Named(NamedKey::Tab) => b"\t".to_vec(),
        Key::Named(NamedKey::Escape) => b"\x1b".to_vec(),
        Key::Named(NamedKey::ArrowUp) => csi_arrow('A', m),
        Key::Named(NamedKey::ArrowDown) => csi_arrow('B', m),
        Key::Named(NamedKey::ArrowRight) => csi_arrow('C', m),
        Key::Named(NamedKey::ArrowLeft) => csi_arrow('D', m),
        Key::Named(NamedKey::Home) => csi_arrow('H', m),
        Key::Named(NamedKey::End) => csi_arrow('F', m),
        Key::Named(NamedKey::PageUp) => csi_tilde(5, m),
        Key::Named(NamedKey::PageDown) => csi_tilde(6, m),
        Key::Named(NamedKey::Delete) => csi_tilde(3, m),
        // Ctrl+letter -> C0 control byte (Ctrl+C = \x03 ...). On Windows,
        // winit may hand us the control char itself instead of the letter —
        // pass it straight through.
        Key::Character(s) if ctrl => {
            let c = s.chars().next()?;
            if ('\u{1}'..='\u{1a}').contains(&c) {
                vec![c as u8]
            } else {
                let c = c.to_ascii_lowercase();
                if c.is_ascii_lowercase() {
                    vec![c as u8 - b'a' + 1]
                } else {
                    return None;
                }
            }
        }
        _ => text?.as_bytes().to_vec(),
    };
    if seq.is_empty() {
        None
    } else {
        Some(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(key: Key, shift: bool, alt: bool, ctrl: bool) -> String {
        String::from_utf8(encode(&key, None, shift, alt, ctrl).unwrap_or_default()).unwrap()
    }

    #[test]
    fn arrows_match_easyter_spec() {
        assert_eq!(enc(Key::Named(NamedKey::ArrowRight), false, false, false), "\x1b[C");
        assert_eq!(enc(Key::Named(NamedKey::ArrowRight), false, false, true), "\x1b[1;5C");
        assert_eq!(enc(Key::Named(NamedKey::ArrowLeft), false, false, true), "\x1b[1;5D");
        assert_eq!(enc(Key::Named(NamedKey::ArrowUp), true, false, false), "\x1b[1;2A");
        assert_eq!(enc(Key::Named(NamedKey::Home), false, false, true), "\x1b[1;5H");
        assert_eq!(enc(Key::Named(NamedKey::End), false, false, true), "\x1b[1;5F");
    }

    #[test]
    fn editing_keys() {
        assert_eq!(enc(Key::Named(NamedKey::Delete), false, false, false), "\x1b[3~");
        assert_eq!(enc(Key::Named(NamedKey::Delete), false, false, true), "\x1b[3;5~");
        assert_eq!(enc(Key::Named(NamedKey::Backspace), false, false, false), "\x7f");
        assert_eq!(enc(Key::Named(NamedKey::Backspace), false, false, true), "\x08");
        assert_eq!(enc(Key::Named(NamedKey::Enter), false, false, false), "\r");
        assert_eq!(enc(Key::Named(NamedKey::Enter), true, false, false), "\x1b\r");
    }

    #[test]
    fn ctrl_letters() {
        let c = Key::Character("c".into());
        assert_eq!(encode(&c, Some("c"), false, false, true).unwrap(), vec![0x03]);
        let t = Key::Character("t".into());
        assert_eq!(encode(&t, Some("t"), false, false, true).unwrap(), vec![0x14]);
        // Windows hands over the pre-composed control char with Ctrl held
        let raw = Key::Character("\u{6}".into());
        assert_eq!(encode(&raw, None, false, false, true).unwrap(), vec![0x06]);
    }

    #[test]
    fn arabic_text_passes_through() {
        let k = Key::Character("م".into());
        assert_eq!(
            encode(&k, Some("م"), false, false, false).unwrap(),
            "م".as_bytes().to_vec()
        );
    }
}
