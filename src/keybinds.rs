//! Configurable shortcuts: the action registry, the chord type, and the
//! effective map (defaults overridden by ~/.bayan/config.json).
//!
//! Chords resolve by PHYSICAL key (Bayan's Arabic-layout rule: Ctrl+T must
//! fire even when the T key's logical char is "ف"). Fixed, non-configurable
//! bindings stay out of this registry: Ctrl+Tab (cycle), Ctrl+wheel/Ctrl+0
//! (zoom), Alt+arrows (pane focus), Shift+PageUp/Down (scrollback), the
//! global quake hotkey, plain Ctrl+C (copy-or-interrupt), and the Ctrl+V
//! paste alias (an explicit rebind to that chord takes precedence).

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
    BidiToggle,
    ResetKeyboard,
    RenameTab,
    Broadcast,
    ZoomPane,
    CopyLastOutput,
    NewGroup,
    NextGroup,
    PrevGroup,
    CopyMode,
    CopyClean,
    Fullscreen,
    ClearScrollback,
    ResetTerminal,
    NewWindow,
    DuplicateTab,
    SaveOutput,
}

impl Action {
    pub const ALL: [Action; 29] = [
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
        Action::BidiToggle,
        Action::ResetKeyboard,
        Action::RenameTab,
        Action::Broadcast,
        Action::ZoomPane,
        Action::CopyLastOutput,
        Action::NewGroup,
        Action::NextGroup,
        Action::PrevGroup,
        Action::CopyMode,
        Action::CopyClean,
        Action::Fullscreen,
        Action::ClearScrollback,
        Action::ResetTerminal,
        Action::NewWindow,
        Action::DuplicateTab,
        Action::SaveOutput,
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
            Action::BidiToggle => "bidi-toggle",
            Action::ResetKeyboard => "reset-keyboard",
            Action::RenameTab => "rename-tab",
            Action::Broadcast => "broadcast",
            Action::ZoomPane => "zoom-pane",
            Action::CopyLastOutput => "copy-last-output",
            Action::NewGroup => "new-group",
            Action::NextGroup => "next-group",
            Action::PrevGroup => "prev-group",
            Action::CopyMode => "copy-mode",
            Action::CopyClean => "copy-clean",
            Action::Fullscreen => "fullscreen",
            Action::ClearScrollback => "clear-scrollback",
            Action::ResetTerminal => "reset-terminal",
            Action::NewWindow => "new-window",
            Action::DuplicateTab => "duplicate-tab",
            Action::SaveOutput => "save-output",
        }
    }

    /// The editor row label.
    /// The action's name in the interface language.
    pub fn label(self) -> &'static str {
        self.label_in(crate::lang::get())
    }

    /// The action's name in `lang` — what the plugin check compares
    /// against in BOTH languages.
    pub fn label_in(self, lang: crate::lang::Lang) -> &'static str {
        match self {
            Action::NewTab => crate::lang::pick(lang, "تبويب جديد", "New tab"),
            Action::ClosePane => crate::lang::pick(lang, "إغلاق اللوحة", "Close pane"),
            Action::Search => crate::lang::pick(lang, "البحث", "Search"),
            Action::Settings => crate::lang::pick(lang, "الإعدادات", "Settings"),
            Action::Copy => crate::lang::pick(lang, "نسخ التحديد", "Copy selection"),
            Action::Paste => crate::lang::pick(lang, "لصق", "Paste"),
            Action::SplitH => crate::lang::pick(lang, "تقسيم: جنبًا إلى جنب", "Split: side by side"),
            Action::SplitV => crate::lang::pick(lang, "لوحة أسفل", "Pane below"),
            Action::Cockpit => crate::lang::pick(lang, "مقصورة الوكلاء", "Agent cockpit"),
            Action::PromptPrev => crate::lang::pick(lang, "الأمر السابق", "Previous command"),
            Action::PromptNext => crate::lang::pick(lang, "الأمر التالي", "Next command"),
            Action::ClaudeToggle => crate::lang::pick(lang, "وضع كلود", "Claude mode"),
            Action::BidiToggle => crate::lang::pick(lang, "اتجاه النص", "BiDi mode"),
            Action::ResetKeyboard => {
                crate::lang::pick(lang, "إعادة ضبط لوحة المفاتيح", "Reset keyboard")
            }
            Action::RenameTab => crate::lang::pick(lang, "تسمية التبويب", "Rename tab"),
            Action::Broadcast => {
                crate::lang::pick(lang, "الكتابة في كل اللوحات", "Type to all panes")
            }
            Action::ZoomPane => crate::lang::pick(lang, "تكبير اللوحة", "Zoom pane"),
            Action::CopyLastOutput => {
                crate::lang::pick(lang, "نسخ ناتج آخر أمر", "Copy last output")
            }
            Action::NewGroup => crate::lang::pick(lang, "مجموعة جديدة", "New group"),
            Action::NextGroup => crate::lang::pick(lang, "المجموعة التالية", "Next group"),
            Action::PrevGroup => crate::lang::pick(lang, "المجموعة السابقة", "Previous group"),
            Action::CopyMode => crate::lang::pick(lang, "وضع النسخ", "Copy mode"),
            Action::CopyClean => crate::lang::pick(lang, "نسخ نظيف", "Copy clean"),
            Action::Fullscreen => crate::lang::pick(lang, "ملء الشاشة", "Full screen"),
            Action::ClearScrollback => crate::lang::pick(lang, "مسح السجل", "Clear scrollback"),
            Action::ResetTerminal => {
                crate::lang::pick(lang, "إعادة ضبط أوضاع الطرفية", "Reset terminal modes")
            }
            Action::NewWindow => crate::lang::pick(lang, "نافذة جديدة", "New window"),
            Action::DuplicateTab => crate::lang::pick(lang, "تكرار التبويب", "Duplicate tab"),
            Action::SaveOutput => {
                crate::lang::pick(lang, "حفظ المخرجات في ملف", "Save output to a file")
            }
        }
    }

    pub fn default_chord(self) -> Chord {
        use ChordKey::*;
        let c = |ctrl, shift, key| Chord {
            ctrl,
            shift,
            alt: false,
            key,
        };
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
            // Cycles this pane's BiDi mode (full → alt screen off → off).
            // Ctrl+Shift, not bare F3: htop and mc search and view on F3.
            // Physical F3, so an Arabic layout still reaches it, and it can
            // be rebound.
            Action::BidiToggle => c(true, true, F(3)),
            // Ctrl+Shift+K: the leaked-flags escape hatch. Physical K, so an
            // Arabic layout still reaches it.
            Action::ResetKeyboard => c(true, true, Letter('k')),
            // R for rename, B for broadcast, Z for zoom (WezTerm's
            // TogglePaneZoomState chord), L for the last command's lines
            Action::RenameTab => c(true, true, Letter('r')),
            Action::Broadcast => c(true, true, Letter('b')),
            Action::ZoomPane => c(true, true, Letter('z')),
            Action::CopyLastOutput => c(true, true, Letter('l')),
            // G for group. Groups step on PageDown/PageUp, the browsers'
            // tab-strip keys: Alt+arrows already move pane focus and
            // Ctrl+Shift+arrows are the shell's word selection. The group
            // chip in the tab bar is the click target for the same step.
            Action::NewGroup => c(true, true, Letter('g')),
            Action::NextGroup => c(true, true, PageDown),
            Action::PrevGroup => c(true, true, PageUp),
            // X: WezTerm's ActivateCopyMode chord
            Action::CopyMode => c(true, true, Letter('x')),
            // Y for yank, cleaned. X is copy mode.
            Action::CopyClean => c(true, true, Letter('y')),
            // Bare F11, the key every terminal and browser uses. A program
            // that wants F11 itself needs this rebound.
            Action::Fullscreen => c(false, false, F(11)),
            // H for history, J beside it for the mode reset, N for a
            // window (Windows Terminal's chord), T beside Ctrl+T for the
            // same tab again, S for save.
            Action::ClearScrollback => c(true, true, Letter('h')),
            Action::ResetTerminal => c(true, true, Letter('j')),
            Action::NewWindow => c(true, true, Letter('n')),
            Action::DuplicateTab => c(true, true, Letter('t')),
            Action::SaveOutput => c(true, true, Letter('s')),
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
    PageUp,
    PageDown,
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
    pub fn to_config(self) -> String {
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
            ChordKey::PageUp => "pageup".into(),
            ChordKey::PageDown => "pagedown".into(),
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
                "pageup" => key = Some(ChordKey::PageUp),
                "pagedown" => key = Some(ChordKey::PageDown),
                p => {
                    let mut chars = p.chars();
                    match (chars.next(), chars.as_str()) {
                        (Some(l @ 'a'..='z'), "") => key = Some(ChordKey::Letter(l)),
                        (Some(d @ '0'..='9'), "") => key = Some(ChordKey::Digit(d as u8 - b'0')),
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
        key.map(|key| Chord {
            ctrl,
            shift,
            alt,
            key,
        })
    }

    /// Display form for the editor's keycaps: "Ctrl+Shift+T".
    pub fn display(self) -> String {
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
            ChordKey::PageUp => "PgUp",
            ChordKey::PageDown => "PgDn",
        });
        parts.join("+")
    }
}

/// A key event's chord, by physical key (layout-proof). None for keys that
/// can't anchor a chord (Enter, Esc, bare modifiers, …).
pub fn chord_from(physical: PhysicalKey, ctrl: bool, shift: bool, alt: bool) -> Option<Chord> {
    use KeyCode::*;
    let PhysicalKey::Code(code) = physical else {
        return None;
    };
    let key = match code {
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
        PageUp => ChordKey::PageUp,
        PageDown => ChordKey::PageDown,
        _ => match crate::keys::pc101_unshifted(code)? {
            c @ 'a'..='z' => ChordKey::Letter(c),
            d @ '0'..='9' => ChordKey::Digit(d as u8 - b'0'),
            _ => return None,
        },
    };
    Some(Chord {
        ctrl,
        shift,
        alt,
        key,
    })
}

/// Every action's effective chord: the default unless config overrides it.
/// Unparseable overrides fall back to the default (a typo must not
/// brick a shortcut), and so do unbindable ones (no Ctrl, Alt or F-key).
pub fn effective_map(cfg: &crate::config::UserConfig) -> Vec<(Action, Chord)> {
    Action::ALL
        .iter()
        .map(|&a| {
            let chord = cfg
                .keybinds
                .as_ref()
                .and_then(|m| m.get(a.id()))
                .and_then(|s| Chord::parse(s))
                // BAYAN-011: the same rule the shortcut editor enforces — a
                // bare letter or Shift+key would swallow ordinary typing
                .filter(Chord::is_bindable)
                .unwrap_or_else(|| a.default_chord());
            (a, chord)
        })
        .collect()
}

/// The action a chord fires, if any.
pub fn lookup(map: &[(Action, Chord)], chord: Chord) -> Option<Action> {
    map.iter().find(|(_, c)| *c == chord).map(|(a, _)| *a)
}

/// Resolve an app action before sending an unhandled chord to the PTY.
pub fn resolve(
    map: &[(Action, Chord)],
    chord: Chord,
    alt_screen: bool,
    has_selection: bool,
) -> Option<Action> {
    let action = lookup(map, chord).or_else(|| {
        if !chord.is_plain_ctrl_letter() {
            return None;
        }
        match chord.key {
            ChordKey::Letter('v') => Some(Action::Paste),
            ChordKey::Letter('c') if has_selection => Some(Action::Copy),
            _ => None,
        }
    })?;
    // Clipboard access belongs to the terminal, including bracketed-paste
    // delivery to TUIs. Other plain Ctrl+letters retain their PTY meaning.
    let clipboard_action = action == Action::Paste || (action == Action::Copy && has_selection);
    if alt_screen && chord.is_plain_ctrl_letter() && !clipboard_action {
        None
    } else {
        Some(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_come_from_the_physical_key() {
        let key = |c| chord_from(PhysicalKey::Code(c), true, false, false).map(|ch| ch.key);
        assert_eq!(key(KeyCode::KeyT), Some(ChordKey::Letter('t')));
        assert_eq!(key(KeyCode::KeyZ), Some(ChordKey::Letter('z')));
        assert_eq!(key(KeyCode::Digit0), Some(ChordKey::Digit(0)));
        assert_eq!(key(KeyCode::Digit9), Some(ChordKey::Digit(9)));
        assert_eq!(key(KeyCode::F12), Some(ChordKey::F(12)));
        assert_eq!(key(KeyCode::Comma), Some(ChordKey::Comma));
        assert_eq!(key(KeyCode::Numpad1), None);
        assert_eq!(key(KeyCode::Semicolon), None);
    }

    #[test]
    fn both_paste_chords_work_in_shells_and_full_screen_apps() {
        let map = effective_map(&crate::config::UserConfig::default());
        for alt_screen in [false, true] {
            for shortcut in ["ctrl+v", "ctrl+shift+v"] {
                let chord = Chord::parse(shortcut).unwrap();
                assert_eq!(
                    resolve(&map, chord, alt_screen, false),
                    Some(Action::Paste),
                    "{shortcut}, alt_screen={alt_screen}"
                );
            }
        }
        assert_eq!(
            resolve(&map, Chord::parse("ctrl+f").unwrap(), true, false),
            None
        );
    }

    #[test]
    fn paste_alias_respects_explicit_rebindings_and_altgr() {
        let cfg = crate::config::UserConfig {
            keybinds: Some(std::collections::HashMap::from([
                ("search".into(), "ctrl+v".into()),
                ("paste".into(), "ctrl+p".into()),
            ])),
            ..Default::default()
        };
        let map = effective_map(&cfg);
        assert_eq!(
            resolve(&map, Chord::parse("ctrl+v").unwrap(), false, false),
            Some(Action::Search)
        );
        assert_eq!(
            resolve(&map, Chord::parse("ctrl+p").unwrap(), true, false),
            Some(Action::Paste)
        );
        assert_eq!(
            resolve(&map, Chord::parse("ctrl+alt+v").unwrap(), false, false),
            None
        );
    }

    #[test]
    fn ctrl_c_copies_selection_and_otherwise_reaches_the_child() {
        let map = effective_map(&crate::config::UserConfig::default());
        let copy = Chord::parse("ctrl+c").unwrap();
        for alt_screen in [false, true] {
            assert_eq!(resolve(&map, copy, alt_screen, true), Some(Action::Copy));
            assert_eq!(resolve(&map, copy, alt_screen, false), None);
            assert_eq!(
                resolve(
                    &map,
                    Chord::parse("ctrl+shift+c").unwrap(),
                    alt_screen,
                    true
                ),
                Some(Action::Copy)
            );
        }
        assert_eq!(
            resolve(&map, Chord::parse("ctrl+alt+c").unwrap(), false, true),
            None
        );
    }

    #[test]
    fn chords_round_trip_config_form() {
        for a in Action::ALL {
            let d = a.default_chord();
            assert_eq!(Chord::parse(&d.to_config()), Some(d), "{}", a.id());
        }
        // punctuation and F-keys included
        assert_eq!(Chord::parse("ctrl+,").unwrap().key, ChordKey::Comma);
        assert_eq!(Chord::parse("f2").unwrap().key, ChordKey::F(2));
        assert_eq!(
            Chord::parse("ctrl+shift+pagedown"),
            Some(Action::NextGroup.default_chord())
        );
        assert_eq!(
            chord_from(PhysicalKey::Code(KeyCode::PageUp), true, true, false),
            Some(Action::PrevGroup.default_chord())
        );
        assert_eq!(
            Action::NextGroup.default_chord().display(),
            "Ctrl+Shift+PgDn"
        );
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
    fn reset_keyboard_chord_can_be_rebound() {
        // Ctrl+Shift+K is taken from the program in the terminal. It is a
        // normal binding, so the user can move it and hand the chord back.
        let mut cfg = crate::config::UserConfig::default();
        let mut kb = std::collections::HashMap::new();
        kb.insert("reset-keyboard".to_string(), "ctrl+shift+f12".to_string());
        cfg.keybinds = Some(kb);
        let map = effective_map(&cfg);
        assert_eq!(lookup(&map, Chord::parse("ctrl+shift+k").unwrap()), None);
        assert_eq!(
            lookup(&map, Chord::parse("ctrl+shift+f12").unwrap()),
            Some(Action::ResetKeyboard)
        );
        let defaults = effective_map(&crate::config::UserConfig::default());
        assert_eq!(
            lookup(&defaults, Action::ResetKeyboard.default_chord()),
            Some(Action::ResetKeyboard)
        );
    }

    #[test]
    fn copy_clean_reaches_a_full_screen_tui_and_does_not_take_copy_mode() {
        let map = effective_map(&crate::config::UserConfig::default());
        let clean = Action::CopyClean.default_chord();
        assert_eq!(clean, Chord::parse("ctrl+shift+y").unwrap());
        for alt_screen in [false, true] {
            assert_eq!(
                resolve(&map, clean, alt_screen, true),
                Some(Action::CopyClean),
                "alt_screen={alt_screen}"
            );
            assert_eq!(
                resolve(&map, clean, alt_screen, false),
                Some(Action::CopyClean)
            );
        }
        assert_eq!(
            resolve(&map, Chord::parse("ctrl+shift+x").unwrap(), true, false),
            Some(Action::CopyMode)
        );
    }

    #[test]
    fn bidi_toggle_defaults_to_ctrl_shift_f3_and_leaves_f3_to_programs() {
        let map = effective_map(&crate::config::UserConfig::default());
        assert_eq!(
            lookup(&map, Chord::parse("ctrl+shift+f3").unwrap()),
            Some(Action::BidiToggle)
        );
        // htop and mc search on bare F3
        assert_eq!(lookup(&map, Chord::parse("f3").unwrap()), None);
        assert_ne!(
            Action::BidiToggle.default_chord(),
            Action::ClaudeToggle.default_chord()
        );
    }

    #[test]
    fn tui_passthrough_rule_matches_plain_ctrl_letters_only() {
        assert!(Chord::parse("ctrl+t").unwrap().is_plain_ctrl_letter());
        assert!(!Chord::parse("ctrl+shift+t").unwrap().is_plain_ctrl_letter());
        assert!(!Chord::parse("ctrl+,").unwrap().is_plain_ctrl_letter());
        assert!(!Chord::parse("f2").unwrap().is_plain_ctrl_letter());
    }

    #[test]
    fn config_cannot_bind_an_action_to_plain_typing() {
        // BAYAN-011: "a" or "shift+v" from config.json falls back to default
        let a = Action::ALL[0];
        for bad in ["a", "shift+v", "enter", "space"] {
            let mut map = std::collections::HashMap::new();
            map.insert(a.id().to_string(), bad.to_string());
            let cfg = crate::config::UserConfig {
                keybinds: Some(map),
                ..Default::default()
            };
            let (_, chord) = effective_map(&cfg)[0];
            assert_eq!(chord, a.default_chord(), "{bad:?} was bound");
        }
        // a real chord still overrides
        let mut map = std::collections::HashMap::new();
        map.insert(a.id().to_string(), "ctrl+alt+q".to_string());
        let cfg = crate::config::UserConfig {
            keybinds: Some(map),
            ..Default::default()
        };
        assert_eq!(
            effective_map(&cfg)[0].1,
            Chord::parse("ctrl+alt+q").unwrap()
        );
    }
}
