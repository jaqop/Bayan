//! Keyboard -> VT byte-sequence encoding.
//!
//! Ported from EasyTer's keyPressEvent, whose behavior is pinned by its
//! dev/test_input_ux.py suite; the unit tests below mirror those expectations
//! so both terminals answer the keyboard identically.
//!
//! When the child has enabled the kitty keyboard protocol, [`encode_input`]
//! emits CSI u (see `kitty`) and the legacy path below is what runs with the
//! protocol off — byte-for-byte, including Ctrl+letter by physical key.

use alacritty_terminal::term::TermMode;
use winit::keyboard::{Key, KeyCode, KeyLocation, NamedKey};

use crate::kitty;

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
        // Ctrl+letter -> control byte. Ctrl+Alt is AltGr on Windows and is
        // excluded: its composed text (@ € { } on non-US layouts) is the input.
        Key::Character(s) if ctrl && !alt => {
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
        // Alt+<char> is the meta key: ESC then the char, the way every
        // terminal emulator does it (readline's Alt+B/F word motion, and
        // emacs-style bindings in TUIs). Not when Ctrl is also down — that is
        // AltGr on many layouts, and the composed text is the real input.
        Key::Character(_) if alt && !ctrl => {
            let t = text?;
            let mut v = Vec::with_capacity(t.len() + 1);
            v.push(0x1b);
            v.extend_from_slice(t.as_bytes());
            v
        }
        _ => text?.as_bytes().to_vec(),
    };
    if seq.is_empty() {
        None
    } else {
        Some(seq)
    }
}

/// One key event, already resolved the way the window handed it over.
pub struct EncodeInput<'a> {
    pub key: &'a Key,
    /// `key_without_modifiers`: the layout's unshifted character. On an
    /// Arabic layout this is ش, not the Latin letter of the physical key.
    pub unmod: &'a Key,
    pub physical: Option<KeyCode>,
    pub location: KeyLocation,
    pub text: Option<&'a str>,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub super_key: bool,
    pub repeat: bool,
    pub released: bool,
    pub mode: TermMode,
    /// A–Z from the physical key. Legacy Ctrl+C uses this so an Arabic
    /// layout still sends `\x03`.
    pub physical_letter: Option<char>,
    /// `keyboard.kitty_base_layout_fallback`. When disambiguate is on and
    /// alternate keys are not, a non-ASCII chord is reported as its PC-101
    /// key so Ctrl+C still interrupts.
    pub base_layout_fallback: bool,
}

/// Protocol bits stored in [`TermMode`], packed back into the kitty flag byte.
pub fn protocol_bits(mode: TermMode) -> u8 {
    let mut f = 0u8;
    if mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        f |= kitty::DISAMBIGUATE;
    }
    if mode.contains(TermMode::REPORT_EVENT_TYPES) {
        f |= kitty::EVENTS;
    }
    if mode.contains(TermMode::REPORT_ALTERNATE_KEYS) {
        f |= kitty::ALTERNATE;
    }
    if mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
        f |= kitty::ALL_KEYS;
    }
    if mode.contains(TermMode::REPORT_ASSOCIATED_TEXT) {
        f |= kitty::ASSOCIATED_TEXT;
    }
    f
}

/// Config off wins over kitty bits a child managed to set. The encoder
/// reads `TermMode`; masking here is what keeps a disabled protocol from
/// emitting CSI u after a hostile push.
pub fn mode_for_keys(mut mode: TermMode, kitty_protocol: bool) -> TermMode {
    if !kitty_protocol {
        mode.remove(TermMode::KITTY_KEYBOARD_PROTOCOL);
    }
    mode
}

/// Encode one event. Kitty bytes win when the child asked for them; every
/// other press is the legacy sequence, and a release with nothing to say
/// produces nothing.
pub fn encode_input(input: &EncodeInput<'_>) -> Option<Vec<u8>> {
    // AltGr before the kitty flags, so it still types with the protocol off.
    // The predicate is the key and its PC-101 shift, so Ctrl+Alt+1 stays a
    // chord on both paths.
    if let Some(text) = altgr_text(input) {
        if input.released {
            return None;
        }
        return Some(text.as_bytes().to_vec());
    }
    let flags = kitty::Flags(protocol_bits(input.mode));
    if flags.affects_encoding() {
        if let Some(bytes) = encode_kitty(input, flags) {
            return Some(bytes);
        }
    }
    if input.released {
        return None;
    }
    if input.ctrl && !input.alt {
        if let Some(letter) = input.physical_letter {
            return Some(vec![letter as u8 - b'a' + 1]);
        }
    }
    // Actual AltGr text returned above. Ctrl+Alt without composed text is
    // a chord, including Ctrl+Alt+digits that have no legacy text encoding.
    if input.ctrl && input.alt && matches!(input.key, Key::Character(_)) {
        return encode(input.key, input.text, input.shift, false, true);
    }
    encode(input.key, input.text, input.shift, input.alt, input.ctrl)
}

/// AltGr text. Windows delivers AltGr as Ctrl+Alt, and winit's `text`
/// ignores Ctrl, so Ctrl+Alt+1 arrives with text `"1"`. That is a chord.
///
/// Shift is not an AltGr difference either. Ctrl+Alt+Shift+A arrives as
/// text `"A"` against `key_without_modifiers` `"a"`, and Ctrl+Alt+Shift+1
/// as `"!"` against `"1"`. Compare case-insensitively, and against the
/// PC-101 shifted character of that key. AltGr+E (`€` against `e` / `E`)
/// still differs, so it stays text — protocol on or off.
fn altgr_text<'a>(input: &EncodeInput<'a>) -> Option<&'a str> {
    if !(input.ctrl && input.alt) {
        return None;
    }
    let text = input.text?;
    if text.is_empty() || text.chars().any(is_control) {
        return None;
    }
    if same_key_or_pc101_shift(input, text) {
        return None;
    }
    Some(text)
}

/// `text` is this key, or only Shift applied to it.
fn same_key_or_pc101_shift(input: &EncodeInput<'_>, text: &str) -> bool {
    let unmod = match input.unmod {
        Key::Character(s) => Some(s.as_str()),
        Key::Named(NamedKey::Space) => Some(" "),
        _ => None,
    };
    if let Some(unmod) = unmod {
        if text.eq_ignore_ascii_case(unmod) {
            return true;
        }
    }
    // The physical key when we have one. Otherwise the unmodified character,
    // so Shift+1 is `!` even if the event carried no keycode.
    let base = input
        .physical
        .and_then(pc101_unshifted)
        .or_else(|| unmod.and_then(one_char).filter(|c| c.is_ascii()));
    match base.and_then(pc101_shifted) {
        Some(shifted) => one_char(text) == Some(shifted),
        None => false,
    }
}

fn encode_kitty(input: &EncodeInput<'_>, flags: kitty::Flags) -> Option<Vec<u8>> {
    let dis = flags.contains(kitty::DISAMBIGUATE) || flags.contains(kitty::ALL_KEYS);
    let mut code = kitty_code(input, dis, flags.contains(kitty::ALL_KEYS))?;
    // Spec would send the Arabic letter. With alternates off, an app that
    // only knows the PC-101 code (Claude Code, Ctrl+C) never sees an
    // interrupt. Fallback is on unless the user turns it off, and it steps
    // aside when the program asked for alternate keys.
    if input.base_layout_fallback
        && !flags.contains(kitty::ALTERNATE)
        && (input.ctrl || input.alt || input.super_key)
    {
        if let kitty::Code::Char(c) = code {
            if !c.is_ascii() {
                if let Some(base) = input.physical.and_then(pc101_unshifted) {
                    code = kitty::Code::Char(base);
                }
            }
        }
    }
    let base = input.physical.and_then(pc101_unshifted);
    let unshifted = match code {
        kitty::Code::Char(c) => Some(c),
        kitty::Code::Space => Some(' '),
        _ => None,
    };
    let shifted = shifted_char(unshifted, input.text, input.shift, base);
    let action = if input.released {
        kitty::Action::Release
    } else if input.repeat {
        kitty::Action::Repeat
    } else {
        kitty::Action::Press
    };
    kitty::encode(&kitty::Input {
        code,
        shifted,
        base_layout: base,
        text: input.text,
        shift: input.shift,
        alt: input.alt,
        ctrl: input.ctrl,
        super_key: input.super_key,
        action,
        flags,
    })
}

fn kitty_code(input: &EncodeInput<'_>, dis: bool, all: bool) -> Option<kitty::Code> {
    if input.location == KeyLocation::Numpad {
        if let Some(n) = keypad_number(input.key).or_else(|| keypad_number(input.unmod)) {
            let textual =
                matches!(input.unmod, Key::Character(_)) || matches!(input.key, Key::Character(_));
            // Digits on the keypad still produce text under disambiguate.
            // Operators do not, and "report all" numbers the digits too.
            if all || (dis && !textual) {
                return Some(kitty::Code::Keypad(n));
            }
        }
    }
    named_code(input.key, input.location)
        .or_else(|| named_code(input.unmod, input.location))
        .or_else(|| {
            let ch = text_char(input.unmod)
                .or_else(|| text_char(input.key))
                .or(input.physical_letter);
            ch.map(kitty::Code::Char)
        })
}

fn named_code(key: &Key, location: KeyLocation) -> Option<kitty::Code> {
    let Key::Named(named) = key else {
        return None;
    };
    Some(match named {
        NamedKey::Escape => kitty::Code::Escape,
        NamedKey::Enter => kitty::Code::Enter,
        NamedKey::Tab => kitty::Code::Tab,
        NamedKey::Backspace => kitty::Code::Backspace,
        NamedKey::Space => kitty::Code::Space,
        NamedKey::ArrowUp => kitty::Code::Up,
        NamedKey::ArrowDown => kitty::Code::Down,
        NamedKey::ArrowLeft => kitty::Code::Left,
        NamedKey::ArrowRight => kitty::Code::Right,
        NamedKey::Home => kitty::Code::Home,
        NamedKey::End => kitty::Code::End,
        NamedKey::PageUp => kitty::Code::PageUp,
        NamedKey::PageDown => kitty::Code::PageDown,
        NamedKey::Insert => kitty::Code::Insert,
        NamedKey::Delete => kitty::Code::Delete,
        NamedKey::F1 => kitty::Code::F(1),
        NamedKey::F2 => kitty::Code::F(2),
        NamedKey::F3 => kitty::Code::F(3),
        NamedKey::F4 => kitty::Code::F(4),
        NamedKey::F5 => kitty::Code::F(5),
        NamedKey::F6 => kitty::Code::F(6),
        NamedKey::F7 => kitty::Code::F(7),
        NamedKey::F8 => kitty::Code::F(8),
        NamedKey::F9 => kitty::Code::F(9),
        NamedKey::F10 => kitty::Code::F(10),
        NamedKey::F11 => kitty::Code::F(11),
        NamedKey::F12 => kitty::Code::F(12),
        NamedKey::F13 => kitty::Code::F(13),
        NamedKey::F14 => kitty::Code::F(14),
        NamedKey::F15 => kitty::Code::F(15),
        NamedKey::F16 => kitty::Code::F(16),
        NamedKey::F17 => kitty::Code::F(17),
        NamedKey::F18 => kitty::Code::F(18),
        NamedKey::F19 => kitty::Code::F(19),
        NamedKey::F20 => kitty::Code::F(20),
        NamedKey::F21 => kitty::Code::F(21),
        NamedKey::F22 => kitty::Code::F(22),
        NamedKey::F23 => kitty::Code::F(23),
        NamedKey::F24 => kitty::Code::F(24),
        NamedKey::F25 => kitty::Code::F(25),
        NamedKey::F26 => kitty::Code::F(26),
        NamedKey::F27 => kitty::Code::F(27),
        NamedKey::F28 => kitty::Code::F(28),
        NamedKey::F29 => kitty::Code::F(29),
        NamedKey::F30 => kitty::Code::F(30),
        NamedKey::F31 => kitty::Code::F(31),
        NamedKey::F32 => kitty::Code::F(32),
        NamedKey::F33 => kitty::Code::F(33),
        NamedKey::F34 => kitty::Code::F(34),
        NamedKey::F35 => kitty::Code::F(35),
        NamedKey::Shift => kitty::Code::Modifier {
            code: side_code(57441, 57447, location),
            kind: kitty::ModKind::Shift,
        },
        NamedKey::Control => kitty::Code::Modifier {
            code: side_code(57442, 57448, location),
            kind: kitty::ModKind::Ctrl,
        },
        NamedKey::Alt => kitty::Code::Modifier {
            code: side_code(57443, 57449, location),
            kind: kitty::ModKind::Alt,
        },
        NamedKey::Super => kitty::Code::Modifier {
            code: side_code(57444, 57450, location),
            kind: kitty::ModKind::Super,
        },
        _ => return None,
    })
}

fn side_code(left: u32, right: u32, location: KeyLocation) -> u32 {
    if location == KeyLocation::Right {
        right
    } else {
        left
    }
}

fn keypad_number(key: &Key) -> Option<u32> {
    Some(match key {
        Key::Character(s) => match s.as_str() {
            "0" => 57399,
            "1" => 57400,
            "2" => 57401,
            "3" => 57402,
            "4" => 57403,
            "5" => 57404,
            "6" => 57405,
            "7" => 57406,
            "8" => 57407,
            "9" => 57408,
            "." => 57409,
            "/" => 57410,
            "*" => 57411,
            "-" => 57412,
            "+" => 57413,
            "=" => 57415,
            _ => return None,
        },
        Key::Named(NamedKey::Enter) => 57414,
        Key::Named(NamedKey::ArrowLeft) => 57417,
        Key::Named(NamedKey::ArrowRight) => 57418,
        Key::Named(NamedKey::ArrowUp) => 57419,
        Key::Named(NamedKey::ArrowDown) => 57420,
        Key::Named(NamedKey::PageUp) => 57421,
        Key::Named(NamedKey::PageDown) => 57422,
        Key::Named(NamedKey::Home) => 57423,
        Key::Named(NamedKey::End) => 57424,
        Key::Named(NamedKey::Insert) => 57425,
        Key::Named(NamedKey::Delete) => 57426,
        _ => return None,
    })
}

/// A single non-control character, if that is the whole key name.
fn text_char(key: &Key) -> Option<char> {
    let raw = match key {
        Key::Character(s) => s.as_str(),
        Key::Named(NamedKey::Space) => " ",
        _ => return None,
    };
    let mut chars = raw.chars();
    let c = chars.next()?;
    if chars.next().is_some() || is_control(c) {
        return None;
    }
    Some(c)
}

fn is_control(c: char) -> bool {
    c < '\u{20}' || ('\u{7f}'..='\u{9f}').contains(&c)
}

/// Shifted alternate. The active layout wins (`text`); ASCII falls back to
/// the PC-101 shift pair. Arabic letters are not "uppercased" — if the
/// layout did not hand us the shifted glyph, the field stays empty.
fn shifted_char(
    unshifted: Option<char>,
    text: Option<&str>,
    shift: bool,
    base: Option<char>,
) -> Option<char> {
    if !shift {
        return None;
    }
    if let Some(c) = text.and_then(one_char) {
        if !is_control(c) {
            return Some(c);
        }
    }
    let c = unshifted?;
    if c.is_ascii() || base == Some(c) {
        pc101_shifted(c)
    } else {
        None
    }
}

fn one_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

/// Unshifted character of this physical key on a PC-101 keyboard.
fn pc101_unshifted(code: KeyCode) -> Option<char> {
    use KeyCode::*;
    Some(match code {
        Backquote => '`',
        Digit1 => '1',
        Digit2 => '2',
        Digit3 => '3',
        Digit4 => '4',
        Digit5 => '5',
        Digit6 => '6',
        Digit7 => '7',
        Digit8 => '8',
        Digit9 => '9',
        Digit0 => '0',
        Minus => '-',
        Equal => '=',
        KeyQ => 'q',
        KeyW => 'w',
        KeyE => 'e',
        KeyR => 'r',
        KeyT => 't',
        KeyY => 'y',
        KeyU => 'u',
        KeyI => 'i',
        KeyO => 'o',
        KeyP => 'p',
        BracketLeft => '[',
        BracketRight => ']',
        Backslash | IntlBackslash => '\\',
        KeyA => 'a',
        KeyS => 's',
        KeyD => 'd',
        KeyF => 'f',
        KeyG => 'g',
        KeyH => 'h',
        KeyJ => 'j',
        KeyK => 'k',
        KeyL => 'l',
        Semicolon => ';',
        Quote => '\'',
        KeyZ => 'z',
        KeyX => 'x',
        KeyC => 'c',
        KeyV => 'v',
        KeyB => 'b',
        KeyN => 'n',
        KeyM => 'm',
        Comma => ',',
        Period => '.',
        Slash => '/',
        Space => ' ',
        _ => return None,
    })
}

fn pc101_shifted(c: char) -> Option<char> {
    Some(match c {
        '`' => '~',
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        c if c.is_ascii_lowercase() => c.to_ascii_uppercase(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(key: Key, shift: bool, alt: bool, ctrl: bool) -> String {
        String::from_utf8(encode(&key, None, shift, alt, ctrl).unwrap_or_default()).unwrap()
    }

    #[test]
    fn arrows_match_easyter_spec() {
        assert_eq!(
            enc(Key::Named(NamedKey::ArrowRight), false, false, false),
            "\x1b[C"
        );
        assert_eq!(
            enc(Key::Named(NamedKey::ArrowRight), false, false, true),
            "\x1b[1;5C"
        );
        assert_eq!(
            enc(Key::Named(NamedKey::ArrowLeft), false, false, true),
            "\x1b[1;5D"
        );
        assert_eq!(
            enc(Key::Named(NamedKey::ArrowUp), true, false, false),
            "\x1b[1;2A"
        );
        assert_eq!(
            enc(Key::Named(NamedKey::Home), false, false, true),
            "\x1b[1;5H"
        );
        assert_eq!(
            enc(Key::Named(NamedKey::End), false, false, true),
            "\x1b[1;5F"
        );
    }

    #[test]
    fn editing_keys() {
        assert_eq!(
            enc(Key::Named(NamedKey::Delete), false, false, false),
            "\x1b[3~"
        );
        assert_eq!(
            enc(Key::Named(NamedKey::Delete), false, false, true),
            "\x1b[3;5~"
        );
        assert_eq!(
            enc(Key::Named(NamedKey::Backspace), false, false, false),
            "\x7f"
        );
        assert_eq!(
            enc(Key::Named(NamedKey::Backspace), false, false, true),
            "\x08"
        );
        assert_eq!(enc(Key::Named(NamedKey::Enter), false, false, false), "\r");
        assert_eq!(
            enc(Key::Named(NamedKey::Enter), true, false, false),
            "\x1b\r"
        );
    }

    #[test]
    fn ctrl_letters() {
        let c = Key::Character("c".into());
        assert_eq!(
            encode(&c, Some("c"), false, false, true).unwrap(),
            vec![0x03]
        );
        let t = Key::Character("t".into());
        assert_eq!(
            encode(&t, Some("t"), false, false, true).unwrap(),
            vec![0x14]
        );
        // Windows hands over the pre-composed control char with Ctrl held
        let raw = Key::Character("\u{6}".into());
        assert_eq!(encode(&raw, None, false, false, true).unwrap(), vec![0x06]);
    }

    #[test]
    fn alt_letter_is_meta_prefixed_but_altgr_is_not() {
        let b = Key::Character("b".into());
        assert_eq!(
            encode(&b, Some("b"), false, true, false).unwrap(),
            b"\x1bb".to_vec()
        );
        // AltGr arrives as Ctrl+Alt: the composed character passes as text
        // (it used to fall into the Ctrl+letter branch and vanish)
        let at = Key::Character("@".into());
        assert_eq!(
            encode(&at, Some("@"), false, true, true).unwrap(),
            b"@".to_vec()
        );
        let euro = Key::Character("e".into());
        assert_eq!(
            encode(&euro, Some("€"), false, true, true).unwrap(),
            "€".as_bytes().to_vec()
        );
        // plain typing is untouched
        assert_eq!(
            encode(&b, Some("b"), false, false, false).unwrap(),
            b"b".to_vec()
        );
    }

    #[test]
    fn arabic_text_passes_through() {
        let k = Key::Character("م".into());
        assert_eq!(
            encode(&k, Some("م"), false, false, false).unwrap(),
            "م".as_bytes().to_vec()
        );
    }

    fn ev<'a>(key: &'a Key, unmod: &'a Key) -> EncodeInput<'a> {
        EncodeInput {
            key,
            unmod,
            physical: None,
            location: KeyLocation::Standard,
            text: None,
            shift: false,
            alt: false,
            ctrl: false,
            super_key: false,
            repeat: false,
            released: false,
            mode: TermMode::empty(),
            physical_letter: None,
            base_layout_fallback: true,
        }
    }

    fn bytes(input: &EncodeInput<'_>) -> String {
        String::from_utf8(encode_input(input).unwrap_or_default()).unwrap()
    }

    #[test]
    fn protocol_off_matches_legacy_including_arabic_ctrl() {
        let right = Key::Named(NamedKey::ArrowRight);
        let mut arrow = ev(&right, &right);
        arrow.ctrl = true;
        assert_eq!(bytes(&arrow), "\u{1b}[1;5C");

        let enter = Key::Named(NamedKey::Enter);
        let mut shift_enter = ev(&enter, &enter);
        shift_enter.shift = true;
        assert_eq!(bytes(&shift_enter), "\u{1b}\r");

        // Arabic layout: logical char is ؤ, the physical C key still interrupts.
        let waw = Key::Character("ؤ".into());
        let mut ctrl_c = ev(&waw, &waw);
        ctrl_c.ctrl = true;
        ctrl_c.physical = Some(KeyCode::KeyC);
        ctrl_c.physical_letter = Some('c');
        assert_eq!(encode_input(&ctrl_c).unwrap(), vec![0x03]);

        let meem = Key::Character("م".into());
        let mut text = ev(&meem, &meem);
        text.text = Some("م");
        assert_eq!(bytes(&text), "م");
    }

    #[test]
    fn disambiguate_replaces_ctrl_letter_and_shift_enter() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES;
        let enter = Key::Named(NamedKey::Enter);
        let mut shift_enter = ev(&enter, &enter);
        shift_enter.shift = true;
        shift_enter.mode = mode;
        assert_eq!(bytes(&shift_enter), "\u{1b}[13;2u");

        let esc = Key::Named(NamedKey::Escape);
        let mut escape = ev(&esc, &esc);
        escape.mode = mode;
        assert_eq!(bytes(&escape), "\u{1b}[27u");

        let c = Key::Character("c".into());
        let mut ctrl_c = ev(&c, &c);
        ctrl_c.ctrl = true;
        ctrl_c.mode = mode;
        ctrl_c.physical = Some(KeyCode::KeyC);
        ctrl_c.physical_letter = Some('c');
        assert_eq!(bytes(&ctrl_c), "\u{1b}[99;5u");

        // page up stays the legacy tilde form, modifiers included
        let pg = Key::Named(NamedKey::PageUp);
        let mut page = ev(&pg, &pg);
        page.ctrl = true;
        page.mode = mode;
        assert_eq!(bytes(&page), "\u{1b}[5;5~");
    }

    #[test]
    fn arabic_layout_reports_base_and_shifted_key() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES
            | TermMode::REPORT_ALTERNATE_KEYS
            | TermMode::REPORT_ALL_KEYS_AS_ESC
            | TermMode::REPORT_ASSOCIATED_TEXT;
        // ش is the physical A key. Shift produces kasra (U+0650), not "SH".
        let sheen = Key::Character("ش".into());
        let kasra = Key::Character("\u{0650}".into());
        let mut shifted = ev(&kasra, &sheen);
        shifted.shift = true;
        shifted.text = Some("\u{0650}");
        shifted.physical = Some(KeyCode::KeyA);
        shifted.mode = mode;
        assert_eq!(bytes(&shifted), "\u{1b}[1588:1616:97;2;1616u");

        // Ctrl+ؤ (physical C): primary is the Arabic letter, base layout is c.
        // Windows may hand the logical key over as a C0 byte; the unshifted
        // key is what the protocol wants.
        let waw = Key::Character("ؤ".into());
        let mangled = Key::Character("\u{3}".into());
        let mut ctrl = ev(&mangled, &waw);
        ctrl.ctrl = true;
        ctrl.physical = Some(KeyCode::KeyC);
        ctrl.physical_letter = Some('c');
        ctrl.mode = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALTERNATE_KEYS;
        assert_eq!(bytes(&ctrl), "\u{1b}[1572::99;5u");

        assert_eq!(pc101_unshifted(KeyCode::KeyA), Some('a'));
        assert_eq!(pc101_unshifted(KeyCode::KeyC), Some('c'));
        assert_eq!(pc101_unshifted(KeyCode::Digit1), Some('1'));
        assert_eq!(pc101_unshifted(KeyCode::Slash), Some('/'));
    }

    #[test]
    fn protocol_bits_round_trip_the_five_flags() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES
            | TermMode::REPORT_EVENT_TYPES
            | TermMode::REPORT_ALTERNATE_KEYS
            | TermMode::REPORT_ALL_KEYS_AS_ESC
            | TermMode::REPORT_ASSOCIATED_TEXT;
        assert_eq!(protocol_bits(mode), 0b1_1111);
        assert_eq!(protocol_bits(TermMode::empty()), 0);
        assert_eq!(
            protocol_bits(TermMode::SHOW_CURSOR | TermMode::ALT_SCREEN),
            0,
            "unrelated modes are not kitty flags"
        );
    }

    #[test]
    fn arabic_ctrl_c_falls_back_to_base_layout_without_alternates() {
        let waw = Key::Character("ؤ".into());
        let mut ctrl = ev(&waw, &waw);
        ctrl.ctrl = true;
        ctrl.physical = Some(KeyCode::KeyC);
        ctrl.physical_letter = Some('c');
        ctrl.mode = TermMode::DISAMBIGUATE_ESC_CODES;
        // default: the PC-101 key, so the program sees Ctrl+C
        assert!(ctrl.base_layout_fallback);
        assert_eq!(bytes(&ctrl), "\u{1b}[99;5u");
        // opted out of the deviation: the spec's Arabic codepoint
        ctrl.base_layout_fallback = false;
        assert_eq!(bytes(&ctrl), "\u{1b}[1572;5u");
        // alternates requested: primary stays ؤ, base layout rides along
        ctrl.base_layout_fallback = true;
        ctrl.mode |= TermMode::REPORT_ALTERNATE_KEYS;
        assert_eq!(bytes(&ctrl), "\u{1b}[1572::99;5u");
    }

    #[test]
    fn altgr_prints_text_and_super_is_a_chord() {
        // AltGr+E: the glyph is not the unshifted key.
        let euro = Key::Character("€".into());
        let e = Key::Character("e".into());
        let mut altgr = ev(&euro, &e);
        altgr.ctrl = true;
        altgr.alt = true;
        altgr.text = Some("€");
        altgr.physical = Some(KeyCode::KeyE);
        altgr.mode = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(encode_input(&altgr).unwrap(), "€".as_bytes());

        // a real Ctrl+Alt+C (no symbol) is still the chord, mods 1+alt+ctrl
        let c = Key::Character("c".into());
        let mut chord = ev(&c, &c);
        chord.ctrl = true;
        chord.alt = true;
        chord.text = Some("c");
        chord.physical = Some(KeyCode::KeyC);
        chord.physical_letter = Some('c');
        chord.mode = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(bytes(&chord), "\u{1b}[99;7u");

        let mut sup = ev(&c, &c);
        sup.super_key = true;
        sup.physical = Some(KeyCode::KeyC);
        sup.physical_letter = Some('c');
        sup.mode = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(bytes(&sup), "\u{1b}[99;9u");
    }

    #[test]
    fn ctrl_alt_matches_the_unmodified_key_is_not_altgr() {
        let one = Key::Character("1".into());
        let mut digit = ev(&one, &one);
        digit.ctrl = true;
        digit.alt = true;
        digit.text = Some("1");
        digit.physical = Some(KeyCode::Digit1);
        // Protocol off: the legacy encoder drops Ctrl+non-letter. It must
        // not start typing "1" just because winit's text ignored Ctrl.
        assert!(encode_input(&digit).is_none());
        digit.mode = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(bytes(&digit), "\u{1b}[49;7u");

        let waw = Key::Character("ؤ".into());
        let mut arabic = ev(&waw, &waw);
        arabic.ctrl = true;
        arabic.alt = true;
        arabic.text = Some("ؤ");
        arabic.physical = Some(KeyCode::KeyC);
        arabic.physical_letter = Some('c');
        assert!(encode_input(&arabic).is_none());
        arabic.mode = TermMode::DISAMBIGUATE_ESC_CODES;
        // fallback on, no alternate flag: the PC-101 chord, not the letter
        assert_eq!(bytes(&arabic), "\u{1b}[99;7u");

        // Protocol off still types a real AltGr glyph. The legacy encoder
        // drops Ctrl+non-letter, which used to swallow €.
        let euro = Key::Character("€".into());
        let e = Key::Character("e".into());
        let mut altgr = ev(&euro, &e);
        altgr.ctrl = true;
        altgr.alt = true;
        altgr.text = Some("€");
        altgr.physical = Some(KeyCode::KeyE);
        assert_eq!(encode_input(&altgr).unwrap(), "€".as_bytes());
    }

    #[test]
    fn shift_alone_is_not_an_altgr_difference() {
        // text "A" differs from key_without_modifiers "a" only by case.
        let upper = Key::Character("A".into());
        let lower = Key::Character("a".into());
        let mut letter = ev(&upper, &lower);
        letter.ctrl = true;
        letter.alt = true;
        letter.shift = true;
        letter.text = Some("A");
        letter.physical = Some(KeyCode::KeyA);
        letter.physical_letter = Some('a');
        assert_eq!(encode_input(&letter).unwrap(), vec![0x01]);
        letter.mode = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(bytes(&letter), "\u{1b}[97;8u");

        // text "!" differs from "1", and it is the PC-101 shift of that key.
        let bang = Key::Character("!".into());
        let one = Key::Character("1".into());
        let mut digit = ev(&bang, &one);
        digit.ctrl = true;
        digit.alt = true;
        digit.shift = true;
        digit.text = Some("!");
        digit.physical = Some(KeyCode::Digit1);
        assert!(encode_input(&digit).is_none());
        digit.mode = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(bytes(&digit), "\u{1b}[49;8u");

        // A glyph that is neither the key nor its shift is still text.
        let euro = Key::Character("€".into());
        let e = Key::Character("e".into());
        let mut altgr = ev(&euro, &e);
        altgr.ctrl = true;
        altgr.alt = true;
        altgr.text = Some("€");
        altgr.physical = Some(KeyCode::KeyE);
        altgr.mode = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(encode_input(&altgr).unwrap(), "€".as_bytes());
    }

    fn is_csi_u(bytes: &[u8]) -> bool {
        bytes.len() >= 3 && bytes[0] == 0x1b && bytes[1] == b'[' && bytes.ends_with(b"u")
    }

    #[test]
    fn protocol_off_never_emits_csi_u_even_when_mode_bits_are_set() {
        let armed = TermMode::KITTY_KEYBOARD_PROTOCOL;
        let masked = mode_for_keys(armed, false);
        assert!(masked.is_empty() || !masked.intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));

        let c = Key::Character("c".into());
        let one = Key::Character("1".into());
        let waw = Key::Character("ؤ".into());
        let euro = Key::Character("€".into());
        let e = Key::Character("e".into());
        let enter = Key::Named(NamedKey::Enter);
        let tab = Key::Named(NamedKey::Tab);
        let space = Key::Named(NamedKey::Space);
        let up = Key::Named(NamedKey::ArrowUp);
        let esc = Key::Named(NamedKey::Escape);
        let bs = Key::Named(NamedKey::Backspace);
        let f3 = Key::Named(NamedKey::F3);

        let mut cases: Vec<EncodeInput<'_>> = vec![
            ev(&c, &c),
            ev(&one, &one),
            ev(&waw, &waw),
            ev(&enter, &enter),
            ev(&tab, &tab),
            ev(&space, &space),
            ev(&up, &up),
            ev(&esc, &esc),
            ev(&bs, &bs),
            ev(&f3, &f3),
        ];
        cases[0].ctrl = true;
        cases[0].physical = Some(KeyCode::KeyC);
        cases[0].physical_letter = Some('c');
        cases[1].ctrl = true;
        cases[1].alt = true;
        cases[1].text = Some("1");
        cases[1].physical = Some(KeyCode::Digit1);
        cases[2].ctrl = true;
        cases[2].alt = true;
        cases[2].text = Some("ؤ");
        cases[2].physical = Some(KeyCode::KeyC);
        cases[2].physical_letter = Some('c');
        cases[4].ctrl = true;
        cases[5].ctrl = true;
        cases[6].shift = true;
        cases[7].shift = true;

        for input in &mut cases {
            input.mode = masked;
            if let Some(bytes) = encode_input(input) {
                assert!(
                    !is_csi_u(&bytes),
                    "off produced {}",
                    String::from_utf8_lossy(&bytes)
                );
            }
            input.released = true;
            if let Some(bytes) = encode_input(input) {
                assert!(!is_csi_u(&bytes));
            }
            input.released = false;
        }

        // The same Ctrl+C with the bits left in place is a kitty sequence,
        // so the mask is what stopped it.
        let mut ctrl_c = ev(&c, &c);
        ctrl_c.ctrl = true;
        ctrl_c.physical = Some(KeyCode::KeyC);
        ctrl_c.physical_letter = Some('c');
        ctrl_c.mode = mode_for_keys(armed, true);
        let on = encode_input(&ctrl_c).unwrap();
        assert!(is_csi_u(&on));

        // AltGr still produces the glyph once the protocol is actually on.
        let mut altgr = ev(&euro, &e);
        altgr.ctrl = true;
        altgr.alt = true;
        altgr.text = Some("€");
        altgr.mode = mode_for_keys(armed, true);
        assert_eq!(encode_input(&altgr).unwrap(), "€".as_bytes());
        altgr.mode = masked;
        altgr.physical = Some(KeyCode::KeyE);
        let off = encode_input(&altgr).unwrap();
        assert_eq!(off, "€".as_bytes());
        assert!(!is_csi_u(&off));
    }
}
