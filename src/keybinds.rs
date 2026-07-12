//! Configurable shortcuts: the action registry, the chord type, and the
//! effective map (defaults overridden by ~/.bayan/config.json).
//!
//! Chords resolve by PHYSICAL key (Bayan's Arabic-layout rule: Ctrl+T must
//! fire even when the T key's logical char is "ف"). Fixed, non-configurable
//! bindings stay out of this registry: Ctrl+Tab (cycle), Ctrl+wheel/Ctrl+0
//! (zoom), Alt+arrows (pane focus), Shift+PageUp/Down (scrollback), the
//! global quake hotkey, and plain Ctrl+C (copy-or-interrupt).

use winit::keyboard::{KeyCode, PhysicalKey};

/// Every rebindable app action.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    NewTab,
    ClosePane,
    Search,
    Settings,
    Copy,
    Paste,
    SplitH,
    SplitV,
    Cockpit,
    PromptPrev,
    PromptNext,
    ClaudeToggle,
}

impl Action {
    pub const ALL: [Action; 12] = [
        Action::NewTab,
        Action::ClosePane,
        Action::Search,
        Action::Settings,
        Action::Copy,
        Action::Paste,
        Action::SplitH,
        Action::SplitV,
        Action::Cockpit,
        Action::PromptPrev,
        Action::PromptNext,
        Action::ClaudeToggle,
    ];

    /// The config.json key.
    pub fn id(self) -> &'static str {
        match self {
            Action::NewTab => "new-tab",
            Action::ClosePane => "close-pane",
            Action::Search => "search",
            Action::Settings => "settings",
            Action::Copy => "copy",
            Action::Paste => "paste",
            Action::SplitH => "split-h",
            Action::SplitV => "split-v",
            Action::Cockpit => "cockpit",
            Action::PromptPrev => "prompt-prev",
            Action::PromptNext => "prompt-next",
            Action::ClaudeToggle => "claude-toggle",
        }
    }

    /// The editor row label.
    pub fn label(self) -> &'static str {
        match self {
            Action::NewTab => "تبويب جديد",
            Action::ClosePane => "إغلاق اللوحة",
            Action::Search => "البحث",
            Action::Settings => "الإعدادات",
            Action::Copy => "نسخ التحديد",
            Action::Paste => "لصق",
            Action::SplitH => "تقسيم جانبي",
            Action::SplitV => "تقسيم عمودي",
            Action::Cockpit => "مقصورة الوكلاء",
            Action::PromptPrev => "الأمر السابق",
            Action::PromptNext => "الأمر التالي",
            Action::ClaudeToggle => "وضع كلود",
        }
    }

    pub fn default_chord(self) -> Chord {
        use ChordKey::*;
        let c = |ctrl, shift, key| Chord { ctrl, shift, alt: false, key };
        match self {
            Action::NewTab => c(true, false, Letter('t')),
            Action::ClosePane => c(true, true, Letter('w')),
            Action::Search => c(true, false, Letter('f')),
            Action::Settings => c(true, false, Comma),
            Action::Copy => c(true, true, Letter('c')),
            Action::Paste => c(true, true, Letter('v')),
            Action::SplitH => c(true, true, Letter('e')),
            Action::SplitV => c(true, true, Letter('o')),
            Action::Cockpit => c(true, true, Letter('d')),
            Action::PromptPrev => c(true, true, Up),
            Action::PromptNext => c(true, true, Down),
            Action::ClaudeToggle => c(false, false, F(2)),
        }
    }
}

/// The non-modifier part of a chord, by physical key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChordKey {
    Letter(char), // 'a'..='z'
    Digit(u8),    // 0..=9
    F(u8),        // 1..=12
    Comma,
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Chord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: ChordKey,
}

impl Chord {
    /// The TUI pass-through rule generalizes from Ctrl+T/V/F: any PLAIN
    /// ctrl+letter belongs to a full-screen TUI when one is up.
    pub fn is_plain_ctrl_letter(&self) -> bool {
        self.ctrl && !self.shift && !self.alt && matches!(self.key, ChordKey::Letter(_))
    }

    /// A bindable chord needs a modifier or an F-key — anything else would
    /// swallow typing.
    pub fn is_bindable(&self) -> bool {
        self.ctrl || self.alt || matches!(self.key, ChordKey::F(_))
    }

    /// Config form: "ctrl+shift+t", "f2", "ctrl+,".
    pub fn to_config(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push("ctrl".into());
        }
        if self.shift {
            parts.push("shift".into());
        }
        if self.alt {
            parts.push("alt".into());
        }
        parts.push(match self.key {
            ChordKey::Letter(l) => l.to_string(),
            ChordKey::Digit(d) => d.to_string(),
            ChordKey::F(n) => format!("f{n}"),
            ChordKey::Comma => ",".into(),
            ChordKey::Up => "up".into(),
            ChordKey::Down => "down".into(),
            ChordKey::Left => "left".into(),
            ChordKey::Right => "right".into(),
        });
        parts.join("+")
    }

    pub fn parse(s: &str) -> Option<Chord> {
        let (mut ctrl, mut shift, mut alt, mut key) = (false, false, false, None);
        for part in s.split('+') {
            match part.trim().to_ascii_lowercase().as_str() {
                "ctrl" => ctrl = true,
                "shift" => shift = true,
                "alt" => alt = true,
                "," => key = Some(ChordKey::Comma),
                "up" => key = Some(ChordKey::Up),
                "down" => key = Some(ChordKey::Down),
                "left" => key = Some(ChordKey::Left),
                "right" => key = Some(ChordKey::Right),
                p => {
                    let mut chars = p.chars();
                    match (chars.next(), chars.as_str()) {
                        (Some(l @ 'a'..='z'), "") => key = Some(ChordKey::Letter(l)),
                        (Some(d @ '0'..='9'), "") => {
                            key = Some(ChordKey::Digit(d as u8 - b'0'))
                        }
                        (Some('f'), n) => {
                            let n: u8 = n.parse().ok()?;
                            if !(1..=12).contains(&n) {
                                return None;
                            }
                            key = Some(ChordKey::F(n));
                        }
                        _ => return None,
                    }
                }
            }
        }
        key.map(|key| Chord { ctrl, shift, alt, key })
    }

    /// Display form for the editor's keycaps: "Ctrl+Shift+T".
    pub fn display(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.alt {
            parts.push("Alt");
        }
        let key;
        parts.push(match self.key {
            ChordKey::Letter(l) => {
                key = l.to_ascii_uppercase().to_string();
                &key
            }
            ChordKey::Digit(d) => {
                key = d.to_string();
                &key
            }
            ChordKey::F(n) => {
                key = format!("F{n}");
                &key
            }
            ChordKey::Comma => ",",
            ChordKey::Up => "↑",
            ChordKey::Down => "↓",
            ChordKey::Left => "←",
            ChordKey::Right => "→",
        });
        parts.join("+")
    }
}

/// A key event's chord, by physical key (layout-proof). None for keys that
/// can't anchor a chord (Enter, Esc, bare modifiers, …).
pub fn chord_from(physical: PhysicalKey, ctrl: bool, shift: bool, alt: bool) -> Option<Chord> {
    use KeyCode::*;
    let PhysicalKey::Code(code) = physical else { return None };
    let key = match code {
        KeyA => ChordKey::Letter('a'),
        KeyB => ChordKey::Letter('b'),
        KeyC => ChordKey::Letter('c'),
        KeyD => ChordKey::Letter('d'),
        KeyE => ChordKey::Letter('e'),
        KeyF => ChordKey::Letter('f'),
        KeyG => ChordKey::Letter('g'),
        KeyH => ChordKey::Letter('h'),
        KeyI => ChordKey::Letter('i'),
        KeyJ => ChordKey::Letter('j'),
        KeyK => ChordKey::Letter('k'),
        KeyL => ChordKey::Letter('l'),
        KeyM => ChordKey::Letter('m'),
        KeyN => ChordKey::Letter('n'),
        KeyO => ChordKey::Letter('o'),
        KeyP => ChordKey::Letter('p'),
        KeyQ => ChordKey::Letter('q'),
        KeyR => ChordKey::Letter('r'),
        KeyS => ChordKey::Letter('s'),
        KeyT => ChordKey::Letter('t'),
        KeyU => ChordKey::Letter('u'),
        KeyV => ChordKey::Letter('v'),
        KeyW => ChordKey::Letter('w'),
        KeyX => ChordKey::Letter('x'),
        KeyY => ChordKey::Letter('y'),
        KeyZ => ChordKey::Letter('z'),
        Digit0 => ChordKey::Digit(0),
        Digit1 => ChordKey::Digit(1),
        Digit2 => ChordKey::Digit(2),
        Digit3 => ChordKey::Digit(3),
        Digit4 => ChordKey::Digit(4),
        Digit5 => ChordKey::Digit(5),
        Digit6 => ChordKey::Digit(6),
        Digit7 => ChordKey::Digit(7),
        Digit8 => ChordKey::Digit(8),
        Digit9 => ChordKey::Digit(9),
        F1 => ChordKey::F(1),
        F2 => ChordKey::F(2),
        F3 => ChordKey::F(3),
        F4 => ChordKey::F(4),
        F5 => ChordKey::F(5),
        F6 => ChordKey::F(6),
        F7 => ChordKey::F(7),
        F8 => ChordKey::F(8),
        F9 => ChordKey::F(9),
        F10 => ChordKey::F(10),
        F11 => ChordKey::F(11),
        F12 => ChordKey::F(12),
        Comma => ChordKey::Comma,
        ArrowUp => ChordKey::Up,
        ArrowDown => ChordKey::Down,
        ArrowLeft => ChordKey::Left,
        ArrowRight => ChordKey::Right,
        _ => return None,
    };
    Some(Chord { ctrl, shift, alt, key })
}

/// Every action's effective chord: the default unless config overrides it.
/// Unparseable overrides fall back to the default (a typo must not
/// brick a shortcut).
pub fn effective_map(cfg: &crate::config::UserConfig) -> Vec<(Action, Chord)> {
    Action::ALL
        .iter()
        .map(|&a| {
            let chord = cfg
                .keybinds
                .as_ref()
                .and_then(|m| m.get(a.id()))
                .and_then(|s| Chord::parse(s))
                .unwrap_or_else(|| a.default_chord());
            (a, chord)
        })
        .collect()
}

/// The action a chord fires, if any.
pub fn lookup(map: &[(Action, Chord)], chord: Chord) -> Option<Action> {
    map.iter().find(|(_, c)| *c == chord).map(|(a, _)| *a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_round_trip_config_form() {
        for a in Action::ALL {
            let d = a.default_chord();
            assert_eq!(Chord::parse(&d.to_config()), Some(d), "{}", a.id());
        }
        // punctuation and F-keys included
        assert_eq!(
            Chord::parse("ctrl+,").unwrap().key,
            ChordKey::Comma
        );
        assert_eq!(Chord::parse("f2").unwrap().key, ChordKey::F(2));
        // garbage is None, not a panic
        assert_eq!(Chord::parse(""), None);
        assert_eq!(Chord::parse("ctrl+banana"), None);
        assert_eq!(Chord::parse("f99"), None);
        assert_eq!(Chord::parse("ctrl+shift"), None); // modifiers alone
    }

    #[test]
    fn defaults_have_no_conflicts_and_are_bindable() {
        let map = effective_map(&crate::config::UserConfig::default());
        for (i, (a, c)) in map.iter().enumerate() {
            assert!(c.is_bindable(), "{} unbindable", a.id());
            for (b, c2) in &map[i + 1..] {
                assert_ne!(c, c2, "{} and {} share a chord", a.id(), b.id());
            }
        }
    }

    #[test]
    fn config_overrides_rebind_and_typos_fall_back() {
        let mut cfg = crate::config::UserConfig::default();
        let mut kb = std::collections::HashMap::new();
        kb.insert("new-tab".to_string(), "ctrl+shift+n".to_string());
        kb.insert("search".to_string(), "not a chord".to_string());
        cfg.keybinds = Some(kb);
        let map = effective_map(&cfg);
        let get = |a: Action| map.iter().find(|(x, _)| *x == a).unwrap().1;
        assert_eq!(get(Action::NewTab), Chord::parse("ctrl+shift+n").unwrap());
        assert_eq!(get(Action::Search), Action::Search.default_chord());
        // lookup fires the override, not the old default
        assert_eq!(
            lookup(&map, Chord::parse("ctrl+shift+n").unwrap()),
            Some(Action::NewTab)
        );
        assert_eq!(lookup(&map, Chord::parse("ctrl+t").unwrap()), None);
    }

    #[test]
    fn tui_passthrough_rule_matches_plain_ctrl_letters_only() {
        assert!(Chord::parse("ctrl+t").unwrap().is_plain_ctrl_letter());
        assert!(!Chord::parse("ctrl+shift+t").unwrap().is_plain_ctrl_letter());
        assert!(!Chord::parse("ctrl+,").unwrap().is_plain_ctrl_letter());
        assert!(!Chord::parse("f2").unwrap().is_plain_ctrl_letter());
    }
}
