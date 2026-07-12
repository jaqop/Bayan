//! Optional user config at ~/.bayan/config.json — Bayan runs perfectly
//! without it (Ghostty's philosophy: sane defaults, config for the willing).
//!
//! ```json
//! { "font_family": "JetBrains Mono", "font_size": 17.0 }
//! ```

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UserConfig {
    /// Named built-in theme (set by the in-app settings). Explicit
    /// bg/fg/palette below still override it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Primary font family; must be installed. Falls back to the built-in
    /// Nerd-Font-first candidate list when absent or not found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Base font size in points (default 15).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    /// Background / foreground as "#rrggbb" (EasyTer's heritage defaults).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    /// The 16 ANSI colors as "#rrggbb", normal 0-7 then bright 8-15.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub palette: Option<Vec<String>>,
    /// Programming ligatures (-> => != >= ...). Default on; needs a
    /// ligature-capable font (Cascadia Code, JetBrains Mono, Fira Code).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ligatures: Option<bool>,
    /// Cursor shape: "block" (default), "bar", "underline".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_style: Option<String>,
    /// Cursor blink (default on). Blinks for 15s after the last keystroke,
    /// then parks solid — so an idle window stops re-rendering (M14).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_blink: Option<bool>,
    /// Scrollback lines per pane (default 10000). New tabs only — the
    /// universal terminal convention; live sessions keep their buffer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrollback: Option<u32>,
    /// Auto-copy a mouse selection to the clipboard on release (default on,
    /// EasyTer's convention). Explicit Ctrl+C copy always works.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copy_on_select: Option<bool>,
    /// BEL behavior: "attention" (amber tab dot, default), "sound"
    /// (dot + system beep), "silent" (nothing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bell: Option<String>,
    /// Window opacity 0.5–1.0 (Windows layered-window alpha: the whole
    /// window, text included). Default 1.0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Default shell program for NEW tabs: "powershell.exe" (default),
    /// "pwsh.exe", "cmd.exe". Live sessions keep their shell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Hide the tab bar while only one tab is open (default off).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hide_single_tab: Option<bool>,
    /// Ask before closing a pane/window whose command is still running
    /// (default on).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_close: Option<bool>,
    /// Padding in px between the window edges and the terminal content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding: Option<u32>,
}

/// Cursor shape (the trio every terminal offers: block / bar / underline).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorStyle {
    Block,
    Bar,
    Underline,
}

impl CursorStyle {
    pub const ALL: [CursorStyle; 3] =
        [CursorStyle::Block, CursorStyle::Bar, CursorStyle::Underline];

    pub fn parse(s: &str) -> Self {
        match s {
            "bar" => CursorStyle::Bar,
            "underline" => CursorStyle::Underline,
            _ => CursorStyle::Block,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CursorStyle::Block => "block",
            CursorStyle::Bar => "bar",
            CursorStyle::Underline => "underline",
        }
    }
}

/// What a BEL does (beyond a TUI's own visuals).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BellMode {
    /// amber attention dot on the tab (the cockpit signal) — default
    Attention,
    /// the dot plus a system beep
    Sound,
    /// nothing at all
    Silent,
}

impl BellMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "sound" => BellMode::Sound,
            "silent" => BellMode::Silent,
            _ => BellMode::Attention,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BellMode::Attention => "attention",
            BellMode::Sound => "sound",
            BellMode::Silent => "silent",
        }
    }
}

pub const DEFAULT_SCROLLBACK: usize = 10_000;
/// The −/+ steppers in the settings panel walk these.
pub const SCROLLBACK_STEPS: [usize; 6] = [1_000, 5_000, 10_000, 20_000, 50_000, 100_000];
pub const OPACITY_STEPS: [u32; 6] = [50, 60, 70, 80, 90, 100]; // percent
pub const PADDING_STEPS: [u32; 6] = [0, 4, 8, 12, 16, 24]; // px

impl UserConfig {
    pub fn cursor(&self) -> CursorStyle {
        self.cursor_style.as_deref().map(CursorStyle::parse).unwrap_or(CursorStyle::Block)
    }

    pub fn bell_mode(&self) -> BellMode {
        self.bell.as_deref().map(BellMode::parse).unwrap_or(BellMode::Attention)
    }

    pub fn scrollback_lines(&self) -> usize {
        (self.scrollback.map(|n| n as usize).unwrap_or(DEFAULT_SCROLLBACK))
            .clamp(100, 100_000)
    }

    pub fn copy_on_select_on(&self) -> bool {
        self.copy_on_select.unwrap_or(true)
    }

    pub fn cursor_blink_on(&self) -> bool {
        self.cursor_blink.unwrap_or(true)
    }

    /// 0.5..=1.0 — anything outside is a typo, not a wish for invisibility.
    pub fn opacity_level(&self) -> f32 {
        self.opacity.unwrap_or(1.0).clamp(0.5, 1.0)
    }

    pub fn shell_program(&self) -> String {
        self.shell.clone().unwrap_or_else(|| "powershell.exe".to_string())
    }

    pub fn hide_single_tab_on(&self) -> bool {
        self.hide_single_tab.unwrap_or(false)
    }

    pub fn confirm_close_on(&self) -> bool {
        self.confirm_close.unwrap_or(true)
    }

    pub fn padding_px(&self) -> i32 {
        self.padding.unwrap_or(0).min(32) as i32
    }
}

/// "#rrggbb" (or "rrggbb") -> rgb. None on anything malformed.
pub fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((
        u8::from_str_radix(&h[0..2], 16).ok()?,
        u8::from_str_radix(&h[2..4], 16).ok()?,
        u8::from_str_radix(&h[4..6], 16).ok()?,
    ))
}

fn config_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("USERPROFILE")?;
    Some(std::path::Path::new(&home).join(".bayan").join("config.json"))
}

pub fn load() -> UserConfig {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        // Windows editors love a UTF-8 BOM; serde_json rejects it
        .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default()
}

/// Persist the config (the in-app settings panel writes it on close).
pub fn save(cfg: &UserConfig) {
    let Some(path) = config_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_fully_partially_or_not_at_all() {
        let full: UserConfig =
            serde_json::from_str(r#"{"font_family":"JetBrains Mono","font_size":17.5}"#).unwrap();
        assert_eq!(full.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(full.font_size, Some(17.5));
        // partial: missing keys default
        let part: UserConfig = serde_json::from_str(r#"{"font_size":12.0}"#).unwrap();
        assert!(part.font_family.is_none());
        // unknown keys are tolerated
        let extra: UserConfig = serde_json::from_str(r#"{"theme":"x"}"#).unwrap();
        assert!(extra.font_size.is_none());
        // garbage must not panic the loader path
        assert!(serde_json::from_str::<UserConfig>("{oops").is_err());
        // a UTF-8 BOM (Windows editors) must not kill the whole config
        let bom = "\u{feff}{\"font_size\":12.0}";
        let cfg: UserConfig =
            serde_json::from_str(bom.trim_start_matches('\u{feff}')).unwrap();
        assert_eq!(cfg.font_size, Some(12.0));
    }

    #[test]
    fn behavior_settings_resolve_with_defaults() {
        // absent keys resolve to the documented defaults
        let d = UserConfig::default();
        assert_eq!(d.cursor(), CursorStyle::Block);
        assert!(d.cursor_blink_on());
        assert_eq!(d.bell_mode(), BellMode::Attention);
        assert_eq!(d.scrollback_lines(), DEFAULT_SCROLLBACK);
        assert!(d.copy_on_select_on());
        // explicit values parse; unknown strings fall back, never panic
        let c: UserConfig = serde_json::from_str(
            r#"{"cursor_style":"bar","scrollback":50000,"copy_on_select":false,"bell":"silent"}"#,
        )
        .unwrap();
        assert_eq!(c.cursor(), CursorStyle::Bar);
        assert_eq!(c.scrollback_lines(), 50_000);
        assert!(!c.copy_on_select_on());
        assert_eq!(c.bell_mode(), BellMode::Silent);
        assert_eq!(CursorStyle::parse("banana"), CursorStyle::Block);
        assert_eq!(BellMode::parse("banana"), BellMode::Attention);
        // scrollback is clamped to sane bounds
        let tiny: UserConfig = serde_json::from_str(r#"{"scrollback":1}"#).unwrap();
        assert_eq!(tiny.scrollback_lines(), 100);
        // round-trip through as_str/parse
        for s in CursorStyle::ALL {
            assert_eq!(CursorStyle::parse(s.as_str()), s);
        }
    }

    #[test]
    fn batch_two_settings_resolve_with_defaults() {
        let d = UserConfig::default();
        assert_eq!(d.opacity_level(), 1.0);
        assert_eq!(d.shell_program(), "powershell.exe");
        assert!(!d.hide_single_tab_on());
        assert!(d.confirm_close_on());
        assert_eq!(d.padding_px(), 0);
        let c: UserConfig = serde_json::from_str(
            r#"{"opacity":0.8,"shell":"pwsh.exe","hide_single_tab":true,
                "confirm_close":false,"padding":12}"#,
        )
        .unwrap();
        assert_eq!(c.opacity_level(), 0.8);
        assert_eq!(c.shell_program(), "pwsh.exe");
        assert!(c.hide_single_tab_on());
        assert!(!c.confirm_close_on());
        assert_eq!(c.padding_px(), 12);
        // out-of-range values clamp instead of vanishing the window
        let wild: UserConfig =
            serde_json::from_str(r#"{"opacity":0.05,"padding":500}"#).unwrap();
        assert_eq!(wild.opacity_level(), 0.5);
        assert_eq!(wild.padding_px(), 32);
    }

    #[test]
    fn hex_colors_parse_strictly() {
        assert_eq!(parse_hex("#0d1117"), Some((0x0d, 0x11, 0x17)));
        assert_eq!(parse_hex("FFffFF"), Some((255, 255, 255)));
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("#zzzzzz"), None);
        assert_eq!(parse_hex(""), None);
    }
}
