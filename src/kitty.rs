//! Kitty keyboard protocol encoder.
//!
//! The disambiguate level follows EasyTer's `kitty_encode_key`: Esc, any
//! modified Enter/Tab/Backspace/Space, and ctrl/alt chords become CSI u,
//! while plain text and unmodified Enter/Tab/Backspace/arrows stay on the
//! legacy path. The other progressive-enhancement flags extend that, per
//! <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>.
//!
//! Alternate keys are layout-sensitive. The primary code is the unshifted
//! character in the *active* layout, the shifted alternate is that layout's
//! shifted character, and the base-layout alternate is the PC-101 key in the
//! same physical position. On an Arabic layout those three differ: ش (the
//! key left of S) reports base layout `a`, not a case-fold of ش.

/// Disambiguate escape codes (`0b1`).
pub const DISAMBIGUATE: u8 = 0b0000_0001;
/// Report press / repeat / release (`0b10`).
pub const EVENTS: u8 = 0b0000_0010;
/// Report shifted and base-layout alternates (`0b100`).
pub const ALTERNATE: u8 = 0b0000_0100;
/// Report every key as an escape (`0b1000`). Implies disambiguation.
pub const ALL_KEYS: u8 = 0b0000_1000;
/// Embed the text the key produces (`0b10000`). Meaningful with [`ALL_KEYS`].
pub const ASSOCIATED_TEXT: u8 = 0b0001_0000;

/// Active progressive-enhancement bits (the low five).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flags(pub u8);

impl Flags {
    pub fn contains(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    /// Alternate keys and associated text only reshape a sequence that one
    /// of the other flags already decided to emit.
    pub fn affects_encoding(self) -> bool {
        self.0 & (DISAMBIGUATE | EVENTS | ALL_KEYS) != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Press,
    Repeat,
    Release,
}

/// Which modifier a modifier-key event itself is, so the bit tracks the
/// key rather than the (often stale) modifier snapshot around it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModKind {
    Shift,
    Alt,
    Ctrl,
    Super,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    Char(char),
    Escape,
    Enter,
    Tab,
    Backspace,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// `1..=35`.
    F(u8),
    /// Keypad number from the protocol's private-use table.
    Keypad(u32),
    Modifier {
        code: u32,
        kind: ModKind,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Input<'a> {
    pub code: Code,
    /// Shifted character in the active layout, when the terminal knows it.
    pub shifted: Option<char>,
    /// Unshifted PC-101 character for this physical key.
    pub base_layout: Option<char>,
    /// Text the key would insert. Control characters are dropped.
    pub text: Option<&'a str>,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub super_key: bool,
    pub action: Action,
    pub flags: Flags,
}

/// `Some` when the protocol wants bytes of its own. `None` means "use the
/// legacy encoder" — for a release, the caller sends nothing.
pub fn encode(input: &Input<'_>) -> Option<Vec<u8>> {
    if !input.flags.affects_encoding() {
        return None;
    }
    let all = input.flags.contains(ALL_KEYS);
    // "Report all keys" is defined to imply disambiguation.
    let dis = input.flags.contains(DISAMBIGUATE) || all;
    let events = input.flags.contains(EVENTS);
    let event = event_digit(input.action, events);

    // Releases exist only with event types. Enter/Tab/Backspace stay silent
    // on release unless every key is an escape, so a crashed TUI can still
    // be `reset` from the shell.
    if input.action == Action::Release {
        if !events {
            return None;
        }
        if matches!(input.code, Code::Enter | Code::Tab | Code::Backspace) && !all {
            return None;
        }
    }

    let mods = modifier_param(input);
    let text = associated_text(input, all);

    match input.code {
        Code::Escape => {
            let emit = dis || event != 0;
            emit.then(|| format_seq("27", "", false, mods, event, &text, 'u'))
        }
        Code::Enter => control_key(13, input, dis, all, event, mods, &text),
        Code::Tab => control_key(9, input, dis, all, event, mods, &text),
        Code::Backspace => control_key(127, input, dis, all, event, mods, &text),
        Code::Space => space_key(input, dis, all, event, mods, &text),
        Code::Char(c) => char_key(c, input, dis, all, event, mods, &text),
        Code::Up => arrow("A", dis, all, event, mods),
        Code::Down => arrow("B", dis, all, event, mods),
        Code::Right => arrow("C", dis, all, event, mods),
        Code::Left => arrow("D", dis, all, event, mods),
        Code::Home => arrow("H", dis, all, event, mods),
        Code::End => arrow("F", dis, all, event, mods),
        Code::PageUp => tilde_legacy("5", event, mods),
        Code::PageDown => tilde_legacy("6", event, mods),
        Code::Delete => tilde_legacy("3", event, mods),
        Code::Insert => {
            // No legacy encoding in Bayan, so any active flag reports it.
            functional_always("2", false, '~', event, mods)
        }
        Code::F(n) => f_key(n, event, mods),
        Code::Keypad(n) => functional_always(&n.to_string(), false, 'u', event, mods),
        Code::Modifier { code, .. } => {
            // Modifier keys themselves show up only in "report all".
            if !all {
                return None;
            }
            functional_always(&code.to_string(), false, 'u', event, mods)
        }
    }
}

fn event_digit(action: Action, events: bool) -> u8 {
    if !events {
        return 0;
    }
    match action {
        // Press is the default; the `:1` sub-field is omitted.
        Action::Press => 0,
        Action::Repeat => 2,
        Action::Release => 3,
    }
}

/// xterm/kitty modifier parameter: `1 + shift + 2*alt + 4*ctrl + 8*super`.
fn modifier_param(input: &Input<'_>) -> u8 {
    let mut shift = input.shift;
    let mut alt = input.alt;
    let mut ctrl = input.ctrl;
    let mut super_key = input.super_key;
    // Winit delivers the modifier snapshot from *before* this event. For the
    // modifier key itself, the protocol wants the bit to include the press
    // and exclude the release.
    if let Code::Modifier { kind, .. } = input.code {
        let down = input.action != Action::Release;
        match kind {
            ModKind::Shift => shift = down,
            ModKind::Alt => alt = down,
            ModKind::Ctrl => ctrl = down,
            ModKind::Super => super_key = down,
        }
    }
    1 + u8::from(shift) + (u8::from(alt) << 1) + (u8::from(ctrl) << 2) + (u8::from(super_key) << 3)
}

fn associated_text(input: &Input<'_>, all: bool) -> Vec<u32> {
    if !all || !input.flags.contains(ASSOCIATED_TEXT) || input.action == Action::Release {
        return Vec::new();
    }
    let Some(text) = input.text else {
        return Vec::new();
    };
    text.chars()
        .filter(|c| !is_control(*c))
        .map(u32::from)
        .collect()
}

fn is_control(c: char) -> bool {
    c < '\u{20}' || ('\u{7f}'..='\u{9f}').contains(&c)
}

/// Enter, Tab, Backspace: unmodified press stays legacy unless every key is
/// an escape. Any modifier (including shift alone) disambiguates — that is
/// EasyTer's order, and it is what makes shift+Enter distinct from Enter.
fn control_key(
    code: u32,
    input: &Input<'_>,
    dis: bool,
    all: bool,
    event: u8,
    mods: u8,
    text: &[u32],
) -> Option<Vec<u8>> {
    let modified = input.shift || input.alt || input.ctrl || input.super_key;
    // Unmodified Enter/Tab/Backspace stay legacy (a repeat is another
    // `\r`/`\t`/`DEL`). A modifier disambiguates; "report all" does too.
    let emit = all || (modified && (dis || event != 0));
    emit.then(|| format_seq(&code.to_string(), "", false, mods, event, text, 'u'))
}

fn space_key(
    input: &Input<'_>,
    dis: bool,
    all: bool,
    event: u8,
    mods: u8,
    text: &[u32],
) -> Option<Vec<u8>> {
    // Checked before the "shift+printable is just text" gate in EasyTer, so
    // shift+space is CSI 32;2 u, not a plain space.
    let modified = input.shift || input.alt || input.ctrl || input.super_key;
    let emit = all || (dis && modified) || (event != 0 && modified);
    emit.then(|| {
        let alt = alternates(input, ' ');
        format_seq("32", &alt, false, mods, event, text, 'u')
    })
}

fn char_key(
    c: char,
    input: &Input<'_>,
    dis: bool,
    all: bool,
    event: u8,
    mods: u8,
    text: &[u32],
) -> Option<Vec<u8>> {
    // ctrl/alt/super (and the combinations). Shift alone stays text.
    // Super is a modifier in the protocol, so disambiguate reports it.
    let modified = input.ctrl || input.alt || input.super_key;
    let emit = all || (dis && modified) || (event != 0 && modified);
    emit.then(|| {
        let alt = alternates(input, c);
        format_seq(
            &u32::from(c).to_string(),
            &alt,
            false,
            mods,
            event,
            text,
            'u',
        )
    })
}

/// Arrows, Home, End. Unmodified press keeps the legacy `CSI A` form.
/// Modified press matches EasyTer (`CSI 1;5C`) once disambiguate is on;
/// otherwise the legacy encoder already emits that same sequence.
fn arrow(letter: &str, dis: bool, all: bool, event: u8, mods: u8) -> Option<Vec<u8>> {
    let modified = mods != 1;
    if event == 0 && (!modified || !(dis || all)) {
        return None;
    }
    Some(format_seq(
        "1",
        "",
        true,
        mods,
        event,
        &[],
        letter.chars().next().unwrap(),
    ))
}

/// Page up/down and Delete. Bayan's legacy encoder already emits these,
/// modifiers included, so a plain press falls through. A repeat/release
/// needs the event sub-field, which legacy cannot carry.
fn tilde_legacy(number: &str, event: u8, mods: u8) -> Option<Vec<u8>> {
    if event == 0 {
        return None;
    }
    Some(format_seq(number, "", false, mods, event, &[], '~'))
}

fn functional_always(
    number: &str,
    omit_one: bool,
    term: char,
    event: u8,
    mods: u8,
) -> Option<Vec<u8>> {
    Some(format_seq(number, "", omit_one, mods, event, &[], term))
}

fn f_key(n: u8, event: u8, mods: u8) -> Option<Vec<u8>> {
    let (number, term, omit_one): (&str, char, bool) = match n {
        1 => ("1", 'P', true),
        2 => ("1", 'Q', true),
        // F3 as CSI R collides with the cursor-position report.
        3 => ("13", '~', false),
        4 => ("1", 'S', true),
        5 => ("15", '~', false),
        6 => ("17", '~', false),
        7 => ("18", '~', false),
        8 => ("19", '~', false),
        9 => ("20", '~', false),
        10 => ("21", '~', false),
        11 => ("23", '~', false),
        12 => ("24", '~', false),
        13..=35 => {
            return functional_always(
                &(57376 + u32::from(n - 13)).to_string(),
                false,
                'u',
                event,
                mods,
            )
        }
        _ => return None,
    };
    functional_always(number, omit_one, term, event, mods)
}

/// `:shifted:base`, `::base` when shift did not produce a distinct
/// character, or empty when neither alternate adds information.
fn alternates(input: &Input<'_>, primary: char) -> String {
    if !input.flags.contains(ALTERNATE) {
        return String::new();
    }
    let primary = u32::from(primary);
    let shifted = if input.shift {
        input
            .shifted
            .filter(|c| !is_control(*c) && u32::from(*c) != primary)
            .map(u32::from)
    } else {
        None
    };
    let base = input.base_layout.filter(|c| !is_control(*c)).map(u32::from);
    let base = base.filter(|b| *b != primary && Some(*b) != shifted);
    match (shifted, base) {
        (None, None) => String::new(),
        (Some(s), None) => format!(":{s}"),
        (None, Some(b)) => format!("::{b}"),
        (Some(s), Some(b)) => format!(":{s}:{b}"),
    }
}

fn format_seq(
    number: &str,
    alternates: &str,
    omit_default_one: bool,
    mods: u8,
    event: u8,
    text: &[u32],
    term: char,
) -> Vec<u8> {
    let bare = alternates.is_empty() && mods == 1 && event == 0 && text.is_empty();
    let mut out = String::from("\u{1b}[");
    if !(omit_default_one && number == "1" && bare) {
        out.push_str(number);
        out.push_str(alternates);
        if !bare {
            out.push(';');
            out.push_str(&mods.to_string());
            if event != 0 {
                out.push(':');
                out.push(char::from(b'0' + event));
            }
            if !text.is_empty() {
                out.push(';');
                for (i, cp) in text.iter().enumerate() {
                    if i > 0 {
                        out.push(':');
                    }
                    out.push_str(&cp.to_string());
                }
            }
        }
    }
    out.push(term);
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(bits: u8) -> Flags {
        Flags(bits)
    }

    fn press(code: Code, bits: u8) -> Input<'static> {
        Input {
            code,
            shifted: None,
            base_layout: None,
            text: None,
            shift: false,
            alt: false,
            ctrl: false,
            super_key: false,
            action: Action::Press,
            flags: flags(bits),
        }
    }

    fn seq(input: &Input<'_>) -> Option<String> {
        encode(input).map(|b| String::from_utf8(b).unwrap())
    }

    /// EasyTer `kitty_encode_key`, disambiguate flag only.
    #[test]
    fn disambiguate_matches_easyter() {
        let d = DISAMBIGUATE;
        assert_eq!(seq(&press(Code::Escape, d)).as_deref(), Some("\u{1b}[27u"));

        assert_eq!(seq(&press(Code::Up, d)), None);
        let mut right = press(Code::Right, d);
        right.ctrl = true;
        assert_eq!(seq(&right).as_deref(), Some("\u{1b}[1;5C"));
        let mut up = press(Code::Up, d);
        up.shift = true;
        assert_eq!(seq(&up).as_deref(), Some("\u{1b}[1;2A"));
        let mut home = press(Code::Home, d);
        home.ctrl = true;
        assert_eq!(seq(&home).as_deref(), Some("\u{1b}[1;5H"));

        assert_eq!(seq(&press(Code::Enter, d)), None);
        let mut enter = press(Code::Enter, d);
        enter.shift = true;
        assert_eq!(seq(&enter).as_deref(), Some("\u{1b}[13;2u"));
        enter.shift = false;
        enter.ctrl = true;
        assert_eq!(seq(&enter).as_deref(), Some("\u{1b}[13;5u"));

        assert_eq!(seq(&press(Code::Tab, d)), None);
        let mut tab = press(Code::Tab, d);
        tab.shift = true;
        assert_eq!(seq(&tab).as_deref(), Some("\u{1b}[9;2u"));

        assert_eq!(seq(&press(Code::Backspace, d)), None);
        let mut bs = press(Code::Backspace, d);
        bs.alt = true;
        assert_eq!(seq(&bs).as_deref(), Some("\u{1b}[127;3u"));

        let mut space = press(Code::Space, d);
        space.ctrl = true;
        assert_eq!(seq(&space).as_deref(), Some("\u{1b}[32;5u"));
        space.ctrl = false;
        space.shift = true;
        assert_eq!(seq(&space).as_deref(), Some("\u{1b}[32;2u"));

        // shift+letter is text; ctrl/alt letters are CSI u at the lowercase code
        let mut a = press(Code::Char('a'), d);
        a.shift = true;
        assert_eq!(seq(&a), None);
        let mut c = press(Code::Char('c'), d);
        c.ctrl = true;
        assert_eq!(seq(&c).as_deref(), Some("\u{1b}[99;5u"));
        c.shift = true;
        assert_eq!(seq(&c).as_deref(), Some("\u{1b}[99;6u"));
        let mut alt_a = press(Code::Char('a'), d);
        alt_a.alt = true;
        assert_eq!(seq(&alt_a).as_deref(), Some("\u{1b}[97;3u"));
        assert_eq!(seq(&press(Code::Char('a'), d)), None);
        let mut one = press(Code::Char('1'), d);
        one.ctrl = true;
        assert_eq!(seq(&one).as_deref(), Some("\u{1b}[49;5u"));
        let mut semi = press(Code::Char(';'), d);
        semi.alt = true;
        assert_eq!(seq(&semi).as_deref(), Some("\u{1b}[59;3u"));
    }

    #[test]
    fn no_encoding_flags_leave_everything_legacy() {
        assert_eq!(seq(&press(Code::Escape, 0)), None);
        assert_eq!(seq(&press(Code::Char('c'), ALTERNATE)), None);
        assert_eq!(seq(&press(Code::Char('a'), ASSOCIATED_TEXT)), None);
        let mut c = press(Code::Char('c'), ALTERNATE | ASSOCIATED_TEXT);
        c.ctrl = true;
        assert_eq!(seq(&c), None);
    }

    #[test]
    fn event_types_mark_repeat_and_release() {
        let bits = DISAMBIGUATE | EVENTS;
        let mut c = press(Code::Char('c'), bits);
        c.ctrl = true;
        assert_eq!(seq(&c).as_deref(), Some("\u{1b}[99;5u"));
        c.action = Action::Repeat;
        assert_eq!(seq(&c).as_deref(), Some("\u{1b}[99;5:2u"));
        c.action = Action::Release;
        assert_eq!(seq(&c).as_deref(), Some("\u{1b}[99;5:3u"));

        // events alone: the press stays the legacy C0 byte, the release does not
        let mut only = press(Code::Char('c'), EVENTS);
        only.ctrl = true;
        assert_eq!(seq(&only), None);
        only.action = Action::Release;
        assert_eq!(seq(&only).as_deref(), Some("\u{1b}[99;5:3u"));

        let mut up = press(Code::Up, EVENTS);
        assert_eq!(seq(&up), None);
        up.action = Action::Release;
        assert_eq!(seq(&up).as_deref(), Some("\u{1b}[1;1:3A"));
        up.ctrl = true;
        up.action = Action::Repeat;
        assert_eq!(seq(&up).as_deref(), Some("\u{1b}[1;5:2A"));
    }

    #[test]
    fn enter_tab_backspace_release_needs_report_all() {
        let mut enter = press(Code::Enter, DISAMBIGUATE | EVENTS);
        enter.action = Action::Release;
        assert_eq!(seq(&enter), None);
        enter.flags = flags(ALL_KEYS | EVENTS);
        assert_eq!(seq(&enter).as_deref(), Some("\u{1b}[13;1:3u"));

        let mut tab = press(Code::Tab, ALL_KEYS);
        assert_eq!(seq(&tab).as_deref(), Some("\u{1b}[9u"));
        tab.action = Action::Release;
        assert_eq!(seq(&tab), None, "release without event types is silent");
    }

    #[test]
    fn report_all_and_associated_text() {
        let mut a = press(Code::Char('a'), ALL_KEYS);
        assert_eq!(seq(&a).as_deref(), Some("\u{1b}[97u"));
        a.flags = flags(ALL_KEYS | ASSOCIATED_TEXT);
        a.text = Some("a");
        assert_eq!(seq(&a).as_deref(), Some("\u{1b}[97;1;97u"));
        a.shift = true;
        a.text = Some("A");
        a.shifted = Some('A');
        assert_eq!(seq(&a).as_deref(), Some("\u{1b}[97;2;65u"));

        // control text is not associated text
        let mut c = press(Code::Char('c'), ALL_KEYS | ASSOCIATED_TEXT);
        c.ctrl = true;
        c.text = Some("\u{3}");
        assert_eq!(seq(&c).as_deref(), Some("\u{1b}[99;5u"));

        // the text flag without "report all" does not invent sequences
        let mut plain = press(Code::Char('a'), ASSOCIATED_TEXT | DISAMBIGUATE);
        plain.text = Some("a");
        assert_eq!(seq(&plain), None);
    }

    #[test]
    fn alternate_keys_us_layout_and_arabic() {
        // US: shift+a under report-all. Primary stays lowercase; shifted is A.
        // Base layout 'a' equals the primary, so it is omitted.
        let mut a = press(Code::Char('a'), ALL_KEYS | ALTERNATE);
        a.shift = true;
        a.shifted = Some('A');
        a.base_layout = Some('a');
        assert_eq!(seq(&a).as_deref(), Some("\u{1b}[97:65;2u"));

        // ctrl+c on US, alternates on: base 'c' == primary, no sub-field
        let mut c = press(Code::Char('c'), DISAMBIGUATE | ALTERNATE);
        c.ctrl = true;
        c.base_layout = Some('c');
        assert_eq!(seq(&c).as_deref(), Some("\u{1b}[99;5u"));

        // Arabic ش sits on the physical A key. No shift: empty shifted
        // sub-field, base layout 'a' (97). Disambiguate alone does not
        // attach alternates — the flag is required.
        let mut sheen = press(Code::Char('ش'), DISAMBIGUATE);
        sheen.ctrl = true;
        sheen.base_layout = Some('a');
        assert_eq!(seq(&sheen).as_deref(), Some("\u{1b}[1588;5u"));
        sheen.flags = flags(DISAMBIGUATE | ALTERNATE);
        assert_eq!(seq(&sheen).as_deref(), Some("\u{1b}[1588::97;5u"));

        // Shifted ش on the Arabic 101 layout is a diacritic, not to_uppercase.
        // U+0650 ARABIC KASRA, plus the associated text when both flags ask.
        let mut shifted = press(Code::Char('ش'), ALL_KEYS | ALTERNATE | ASSOCIATED_TEXT);
        shifted.shift = true;
        shifted.shifted = Some('\u{0650}');
        shifted.base_layout = Some('a');
        shifted.text = Some("\u{0650}");
        assert_eq!(
            seq(&shifted).as_deref(),
            Some("\u{1b}[1588:1616:97;2;1616u")
        );

        // The C key is ؤ (U+0624). Base layout is 'c', not a Latin fold of ؤ.
        let mut waw = press(Code::Char('ؤ'), DISAMBIGUATE | ALTERNATE);
        waw.ctrl = true;
        waw.base_layout = Some('c');
        assert_eq!(seq(&waw).as_deref(), Some("\u{1b}[1572::99;5u"));
    }

    #[test]
    fn functional_and_modifier_keys() {
        assert_eq!(
            seq(&press(Code::F(5), DISAMBIGUATE)).as_deref(),
            Some("\u{1b}[15~")
        );
        let mut f5 = press(Code::F(5), DISAMBIGUATE);
        f5.ctrl = true;
        assert_eq!(seq(&f5).as_deref(), Some("\u{1b}[15;5~"));
        assert_eq!(
            seq(&press(Code::F(3), DISAMBIGUATE)).as_deref(),
            Some("\u{1b}[13~")
        );
        assert_eq!(
            seq(&press(Code::F(1), DISAMBIGUATE)).as_deref(),
            Some("\u{1b}[P")
        );
        let mut f1 = press(Code::F(1), DISAMBIGUATE);
        f1.shift = true;
        assert_eq!(seq(&f1).as_deref(), Some("\u{1b}[1;2P"));
        assert_eq!(
            seq(&press(Code::F(13), ALL_KEYS)).as_deref(),
            Some("\u{1b}[57376u")
        );
        assert_eq!(
            seq(&press(Code::Insert, EVENTS)).as_deref(),
            Some("\u{1b}[2~")
        );
        assert_eq!(
            seq(&press(Code::Keypad(57414), DISAMBIGUATE)).as_deref(),
            Some("\u{1b}[57414u")
        );

        let shift_key = Code::Modifier {
            code: 57441,
            kind: ModKind::Shift,
        };
        assert_eq!(seq(&press(shift_key, DISAMBIGUATE)), None);
        assert_eq!(
            seq(&press(shift_key, ALL_KEYS)).as_deref(),
            Some("\u{1b}[57441;2u")
        );
        let mut up = press(shift_key, ALL_KEYS | EVENTS);
        up.action = Action::Release;
        assert_eq!(seq(&up).as_deref(), Some("\u{1b}[57441;1:3u"));

        // delete press stays legacy; its release does not
        assert_eq!(seq(&press(Code::Delete, DISAMBIGUATE)), None);
        let mut del = press(Code::Delete, EVENTS);
        del.ctrl = true;
        del.action = Action::Release;
        assert_eq!(seq(&del).as_deref(), Some("\u{1b}[3;5:3~"));
    }

    #[test]
    fn super_letter_is_a_disambiguated_chord() {
        let mut c = press(Code::Char('c'), DISAMBIGUATE);
        c.super_key = true;
        // 1 + super(8) = 9. Disambiguate is enough; report-all is not required.
        assert_eq!(seq(&c).as_deref(), Some("\u{1b}[99;9u"));
        c.ctrl = true;
        assert_eq!(seq(&c).as_deref(), Some("\u{1b}[99;13u"));
    }
}
