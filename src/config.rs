//! Optional user config at ~/.bayan/config.json — Bayan runs perfectly
//! without it (Ghostty's philosophy: sane defaults, config for the willing).
//!
//! ```json
//! { "font_family": "JetBrains Mono", "font_size": 17.0 }
//! ```

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UserConfig {
    /// Interface language: `"en"` for English (left to right); anything
    /// else, or nothing, is Arabic. The settings header switches it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Restore saved tabs and panes on launch. Default off: start one fresh tab.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore_tabs: Option<bool>,
    /// Named built-in theme, or a mode word: `"system"` follows the Windows
    /// app theme, `"dark"` and `"light"` lock that pair. Absent means
    /// `"system"`. A named scheme is never replaced when Windows changes.
    /// Explicit bg/fg/palette below still override the colours and, like a
    /// named scheme, stop the live follow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Last `AppsUseLightTheme` sample (`Some(true)` is light). Not saved.
    /// `None` means the value could not be read, which resolves like dark.
    #[serde(skip)]
    pub system_apps_light: Option<bool>,
    /// Primary font family; must be installed. Falls back to the built-in
    /// Nerd-Font-first candidate list when absent or not found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// The font Arabic text in the terminal is drawn with (e.g. "Kawkab
    /// Mono", "Cairo"). Absent: the terminal font's own Arabic, with
    /// fallback when it has none. A name that is not installed, or cannot
    /// draw Arabic, counts as absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arabic_font: Option<String>,
    /// Base font size in points at 100% text size (default 18). When
    /// `follow_system_text_size` is on, Windows' Accessibility text size
    /// multiplies this value. It is not an absolute pixel size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    /// Multiply UI chrome and the terminal font by Windows text size
    /// (Accessibility, 100–225%). Default on. An explicit [`Self::font_size`]
    /// is the size at 100% and is still multiplied; set this false to keep
    /// that point size absolute. DPI is a separate multiplier either way:
    /// pixels = points × text scale × dpi.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow_system_text_size: Option<bool>,
    /// While High Contrast is on, map the terminal grid's background,
    /// foreground, selection and cursor to the system colors. Default on.
    /// Set false to keep the theme inside the grid. The tab bar, title bar,
    /// palette, cards, scrollbar and focus rings follow High Contrast either
    /// way. The 16 ANSI colors stay on the theme.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_follow_high_contrast: Option<bool>,
    /// Background / foreground as "#rrggbb" (EasyTer's heritage defaults).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    /// The 16 ANSI colors as "#rrggbb", normal 0-7 then bright 8-15.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub palette: Option<Vec<String>>,
    /// Exact tab name (or folder/SSH title when unnamed) to #rrggbb.
    /// Only chrome is colored; program text and ANSI colors stay intact.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub session_colors: std::collections::BTreeMap<String, String>,
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
    /// Clean copy. Absent means `claude_mode`: strip the TUI margin while
    /// Claude mode is on, and copy the grid bytes otherwise. See [`CopyConfig`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copy: Option<CopyConfig>,
    /// `"menu"` (default) or `"paste"`: what a right-click does at the prompt
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_click: Option<String>,
    /// OSC 52 clipboard writes from the child. Off unless this is exactly
    /// `"copy"`. This string never enables a read — `"read"`, `"paste"`,
    /// and `"on"` stay write-off and read-off. Writes also require the
    /// window to be focused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub osc52: Option<String>,
    /// OSC 52 clipboard reads (`OSC 52 ; c ; ?`). Off unless this is exactly
    /// `true`. Then each request needs explicit approval in the focused pane.
    /// No clipboard contents are read before approval. A missing key and
    /// `false` stay off; a background pane cannot request the read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub osc52_read: Option<bool>,
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
    /// When set, startup registers (`true`) or removes (`false`) the
    /// per-user Explorer «افتح بيان هنا» entries, and skips the write when
    /// they already match. Absent: leave the menu as it is. The same work as
    /// `bayan --register-context-menu`. HKCU only; there is no settings-panel
    /// row, the same way `osc52` is config-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explorer_context_menu: Option<bool>,
    /// When set, startup registers (`true`) or removes (`false`) the
    /// per-user `ITerminalHandoff` COM class. Absent: leave it alone.
    /// Default off. This does not select Bayan as the Windows default
    /// terminal; see `handoff`. HKCU only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_handoff: Option<bool>,
    /// OSC 133 shell integration for new panes. Absent or true injects
    /// prompt marks into PowerShell, Git Bash, WSL bash/zsh, and cmd.
    /// `false` launches every shell without them. An unknown program never
    /// gets the injection, even when this is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_integration: Option<bool>,
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
    /// Plugin file-stems to skip loading. This is the declarative JSON
    /// host. Executable Lua plugins are not controlled by this list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_plugins: Option<Vec<String>>,
    /// SHA-256 (hex, lowercase) of Lua files the confirm card has approved.
    /// `config.lua` and an executable plugin run only when the hash of their
    /// current bytes is here. A changed file is a different hash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lua_approvals: Vec<String>,
    /// Executable Lua plugins that may run, by file stem. Empty means none
    /// run, even if `lua_plugins_dir` contains scripts. `spawn` is a separate
    /// opt-in and defaults off. `config.lua` cannot grant these: the loader
    /// keeps the values from config.json.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lua_plugins: Vec<LuaPluginAllow>,
    /// Directory executable Lua plugins are read from. Absolute, or relative
    /// to the directory that holds config.json. Absent means no executable
    /// plugin is loaded — there is no default directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lua_plugins_dir: Option<String>,
    /// Native toast when a long command finishes while Bayan is unfocused
    /// (default on). Windows quiet hours are respected regardless.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notifications: Option<bool>,
    /// Toasts that a PROGRAM asks for through OSC 9 / 777 / 99 (default
    /// OFF). Any output can carry these — a remote host over ssh, a `cat`'d
    /// file, an install script — so they are opt-in. When on, the toast
    /// title is always "بيان · <tab>" and the program text is sanitized.
    /// Bayan's own toasts (a long command finished, the bell) follow
    /// `notifications` and do not need this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub osc_notify: Option<bool>,
    /// Editor for Ctrl+click on `path:line`. `{file}` `{line}` `{col}` are
    /// replaced after the template is split into arguments — never a shell
    /// command. Example: `code -g {file}:{line}:{col}`. Absent: `code -g`
    /// when `code.exe` is on PATH (or the `Code.exe` next to `code.cmd`),
    /// otherwise the file opens by shell association.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor: Option<String>,
    /// Shortcut overrides: action id -> chord ("new-tab": "ctrl+shift+n").
    /// Only NON-default bindings live here (the editor prunes defaults).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keybinds: Option<std::collections::HashMap<String, String>>,
    /// What Ctrl+Tab does: "recent" (default) jumps to the previously used
    /// tab and walks further down the history while Ctrl stays held, like a
    /// browser or IDE; "order" cycles left to right the classic way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_switch: Option<String>,
    /// BiDi escape-sequence policy. Absent means [`BidiConfig`]'s defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bidi: Option<BidiConfig>,
    /// Kitty keyboard protocol. Off unless the user opts in — see
    /// [`KeyboardConfig`].
    #[serde(default, skip_serializing_if = "KeyboardConfig::is_default")]
    pub keyboard: KeyboardConfig,
    /// Pixel-smooth scrollback. `{"scroll":{"smooth":false}}` is the old
    /// whole-line wheel. Default on, and omitted until someone sets it.
    #[serde(default, skip_serializing_if = "ScrollConfig::is_default")]
    pub scroll: ScrollConfig,
    /// Scrollbar on each pane. Absent means the right edge, a thin width,
    /// and hiding after the pointer has been idle.
    #[serde(
        default,
        skip_serializing_if = "crate::scrollbar::ScrollbarConfig::is_default"
    )]
    pub scrollbar: crate::scrollbar::ScrollbarConfig,
    /// Inline images. Absent means on, with the caps in [`ImageConfig`].
    #[serde(default, skip_serializing_if = "ImageConfig::is_default")]
    pub images: ImageConfig,
    /// Host aliases listed in the SSH palette, in addition to `~/.ssh/config`.
    /// There is no password field: the ssh client uses the agent and keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ssh_hosts: Vec<String>,
    /// Local session multiplexer. Off unless explicitly enabled, so a
    /// normal launch never starts `bayan --mux-server`.
    #[serde(default, skip_serializing_if = "MuxConfig::is_default")]
    pub mux: MuxConfig,
}

/// Selection copy. `clean` is `"off"`, `"on"`, or `"claude_mode"`.
///
/// ```json
/// { "copy": { "clean": "claude_mode" } }
/// ```
///
/// Absent, or `"copy": {}`, is `claude_mode`. A typo is too: it must not
/// start cleaning every shell copy. The copy-clean shortcut ignores this
/// and always cleans. Copy-mode `y` follows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CopyConfig {
    /// `"off"` copies the grid bytes. `"on"` always cleans. `"claude_mode"`
    /// cleans only while Claude mode is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clean: Option<String>,
}

/// How Bayan treats BDSM from the application, and how far it reorders.
///
/// `honor_bdsm` defaults to true: an explicit BDSM (`CSI 8 l`) disables
/// reordering for that pane, including Claude-mode auto-detection. Set it
/// false to keep Bayan's own detection when an application selects
/// explicit mode. `mode` is mintty's `Bidi=2/1/0`: `full` (default),
/// `alt_off` (no reordering on the alternate screen), `off` (no reordering
/// anywhere). BDSM still applies on top of a mode that would otherwise
/// reorder, when `honor_bdsm` is set.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct BidiConfig {
    /// The container `#[serde(default)]` fills a missing field from
    /// [`Default`], so an empty `{"bidi":{}}` keeps this true.
    pub honor_bdsm: bool,
    /// `"off"` | `"alt_off"` | `"full"`. A missing field, or a value that
    /// is not one of those (or the digits 0, 1, 2), stays `full`.
    #[serde(
        default,
        deserialize_with = "de_bidi_mode",
        skip_serializing_if = "bidi_mode_is_full"
    )]
    pub mode: crate::bidi_ctl::BidiReorder,
}

fn bidi_mode_is_full(mode: &crate::bidi_ctl::BidiReorder) -> bool {
    mode.is_full()
}

fn de_bidi_mode<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<crate::bidi_ctl::BidiReorder, D::Error> {
    struct Mode;
    impl<'de> serde::de::Visitor<'de> for Mode {
        type Value = crate::bidi_ctl::BidiReorder;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("bidi mode off, alt_off, or full")
        }

        fn visit_str<E: serde::de::Error>(
            self,
            value: &str,
        ) -> Result<crate::bidi_ctl::BidiReorder, E> {
            // A typo must not reject the rest of the file. Full is the
            // historical behavior, so an unknown word keeps it.
            Ok(crate::bidi_ctl::BidiReorder::parse(value)
                .unwrap_or(crate::bidi_ctl::BidiReorder::Full))
        }

        fn visit_u64<E: serde::de::Error>(
            self,
            value: u64,
        ) -> Result<crate::bidi_ctl::BidiReorder, E> {
            Ok(match value {
                0 => crate::bidi_ctl::BidiReorder::Off,
                1 => crate::bidi_ctl::BidiReorder::AltOff,
                _ => crate::bidi_ctl::BidiReorder::Full,
            })
        }

        fn visit_i64<E: serde::de::Error>(
            self,
            value: i64,
        ) -> Result<crate::bidi_ctl::BidiReorder, E> {
            self.visit_u64(value.max(0) as u64)
        }
    }
    deserializer.deserialize_any(Mode)
}

impl Default for BidiConfig {
    fn default() -> Self {
        Self {
            honor_bdsm: true,
            mode: crate::bidi_ctl::BidiReorder::Full,
        }
    }
}

/// Kitty keyboard protocol switches. Both live under `"keyboard"` in
/// config.json so the opt-in is obvious next to the fallback that keeps
/// Ctrl+C working on an Arabic layout.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct KeyboardConfig {
    /// Answer `CSI ? u` with `CSI ?0u` and ignore push/set until this is
    /// true. ConPTY (the system conhost portable-pty uses) has been reported
    /// to swallow the ESC of `CSI u`, which would deliver `[99;5u` as text
    /// and break Ctrl+C. Default off until that is verified on real Windows.
    pub kitty_protocol: bool,
    /// When disambiguate is on but "report alternates" is not, a non-ASCII
    /// chord is reported as its PC-101 base key (`ؤ` on the C key →
    /// `CSI 99;5u` instead of `CSI 1572;5u`). Deliberate departure from the
    /// kitty spec so Claude Code can still be interrupted. Default on.
    pub kitty_base_layout_fallback: bool,
}

impl KeyboardConfig {
    /// Untouched keyboard settings are not written back: saving a config
    /// the user never opened must not mint `"keyboard"` out of nothing.
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// One executable Lua plugin named in `lua_plugins`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LuaPluginAllow {
    /// File stem of `<name>.lua` inside [`UserConfig::lua_plugins_dir`].
    pub name: String,
    /// When true, that plugin's Lua state is given `bayan.spawn`. Off unless
    /// the key is exactly `true`. `config.lua` never receives `spawn`.
    #[serde(default)]
    pub spawn: bool,
}

impl Default for KeyboardConfig {
    fn default() -> Self {
        Self {
            kitty_protocol: false,
            kitty_base_layout_fallback: true,
        }
    }
}

/// Scrollback wheel. `smooth` draws a sub-line pixel offset instead of
/// waiting for a whole line. The default is on; false is the previous
/// whole-line wheel, byte for byte.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ScrollConfig {
    pub smooth: bool,
}

impl ScrollConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for ScrollConfig {
    fn default() -> Self {
        Self { smooth: true }
    }
}

/// Inline-image switch and caps. Missing keys use these defaults; values
/// above the hard ceilings in `images.rs` are clamped when a pane is spawned.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ImageConfig {
    /// `false` drops every image sequence without drawing it. The bytes are
    /// still consumed, so a sixel dump does not spill onto the grid.
    pub enabled: bool,
    /// Decoded bytes of one image. Default 20 MiB.
    pub max_bytes: u64,
    /// Maximum width in pixels.
    pub max_width: u32,
    /// Maximum height in pixels.
    pub max_height: u32,
    /// Decoded bytes one pane may hold.
    pub max_pane_bytes: u64,
    /// Decoded bytes every pane of this process may hold together.
    pub max_session_bytes: u64,
}

impl ImageConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn limits(&self) -> crate::images::ImageLimits {
        crate::images::ImageLimits::from_user(self)
    }
}

impl Default for ImageConfig {
    fn default() -> Self {
        let limits = crate::images::ImageLimits::default();
        Self {
            enabled: limits.enabled,
            max_bytes: limits.max_bytes as u64,
            max_width: limits.max_width,
            max_height: limits.max_height,
            max_pane_bytes: limits.max_pane_bytes as u64,
            max_session_bytes: limits.max_session_bytes as u64,
        }
    }
}

/// Opt-in multiplexer. `enabled` defaults to false: detach, reattach and
/// the per-user pipe server stay off until config.json says otherwise.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MuxConfig {
    pub enabled: bool,
}

impl MuxConfig {
    /// An untouched mux block is not written back. Saving a config the
    /// user never pointed at the multiplexer must not mint `"mux"`.
    pub fn is_default(&self) -> bool {
        !self.enabled
    }
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
    pub fn session_color(&self, title: &str) -> Option<(u8, u8, u8)> {
        self.session_colors.get(title).and_then(|s| parse_hex(s))
    }
    pub fn cursor(&self) -> CursorStyle {
        self.cursor_style
            .as_deref()
            .map(CursorStyle::parse)
            .unwrap_or(CursorStyle::Block)
    }

    pub fn bell_mode(&self) -> BellMode {
        self.bell
            .as_deref()
            .map(BellMode::parse)
            .unwrap_or(BellMode::Attention)
    }

    pub fn scrollback_lines(&self) -> usize {
        (self
            .scrollback
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_SCROLLBACK))
        .clamp(100, 100_000)
    }

    pub fn copy_on_select_on(&self) -> bool {
        self.copy_on_select.unwrap_or(true)
    }

    /// `copy.clean`. Missing, or anything other than `off` / `on`, is
    /// `claude_mode`.
    pub fn clean_copy(&self) -> crate::copyclean::CleanCopy {
        self.copy
            .as_ref()
            .and_then(|c| c.clean.as_deref())
            .map(crate::copyclean::CleanCopy::parse)
            .unwrap_or_default()
    }

    pub fn lang(&self) -> crate::lang::Lang {
        crate::lang::Lang::from_config(self.language.as_deref())
    }

    /// Right-click outside a mouse-tracking program opens the context menu.
    /// Only an explicit `"paste"` brings back M36's paste-on-right-click.
    pub fn right_click_menu_on(&self) -> bool {
        self.right_click.as_deref() != Some("paste")
    }

    /// True only for an explicit `"copy"`. `"paste"`, `"read"`, `"on"`, and
    /// a missing key all stay off — a typo must not open the clipboard.
    /// The read flag is separate and defaults off.
    pub fn osc52_copy_enabled(&self) -> bool {
        matches!(self.osc52.as_deref(), Some("copy"))
    }

    /// OSC 52 read. Exactly `true` in config; anything else, including a
    /// missing key, is off. Turning it on does not by itself write a reply.
    pub fn osc52_read_enabled(&self) -> bool {
        self.osc52_read.unwrap_or(false)
    }

    pub fn cursor_blink_on(&self) -> bool {
        self.cursor_blink.unwrap_or(true)
    }

    /// Default on. See the `follow_system_text_size` field.
    pub fn follow_system_text_size(&self) -> bool {
        self.follow_system_text_size.unwrap_or(true)
    }

    /// Default on. See the `terminal_follow_high_contrast` field.
    pub fn terminal_follows_high_contrast(&self) -> bool {
        self.terminal_follow_high_contrast.unwrap_or(true)
    }

    /// 0.5..=1.0 — anything outside is a typo, not a wish for invisibility.
    pub fn opacity_level(&self) -> f32 {
        self.opacity.unwrap_or(1.0).clamp(0.5, 1.0)
    }

    pub fn shell_program(&self) -> String {
        self.shell
            .clone()
            .unwrap_or_else(|| crate::term::DEFAULT_SHELL.to_string())
    }

    /// Prompt marks (OSC 133) for shells Bayan knows. Default on. An
    /// explicit `false` is the opt-out; anything absent stays on.
    pub fn shell_integration_on(&self) -> bool {
        self.shell_integration.unwrap_or(true)
    }

    pub fn hide_single_tab_on(&self) -> bool {
        self.hide_single_tab.unwrap_or(false)
    }

    pub fn confirm_close_on(&self) -> bool {
        self.confirm_close.unwrap_or(true)
    }

    pub fn disabled_plugins(&self) -> Vec<String> {
        self.disabled_plugins.clone().unwrap_or_default()
    }

    /// Content inset in logical pixels. Default 10. The settings stepper
    /// shows this, not the DPI-scaled size.
    pub fn padding_px(&self) -> i32 {
        self.padding.unwrap_or(10).min(32) as i32
    }

    /// [`padding_px`] in physical pixels, so a 150% display does not clip
    /// the scrollbar down to an unscaled gutter.
    pub fn padding_px_at(&self, scale: f32) -> i32 {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        ((self.padding_px() as f32) * scale).round().max(0.0) as i32
    }

    pub fn notifications_on(&self) -> bool {
        self.notifications.unwrap_or(true)
    }

    /// Program-requested (OSC 9 / 777 / 99) toasts: opt-in, and only ever
    /// while notifications as a whole are on.
    pub fn osc_notify_on(&self) -> bool {
        self.osc_notify.unwrap_or(false) && self.notifications_on()
    }

    /// Ctrl+Tab walks the most-recently-used history unless the config says
    /// "order" (anything else, including a typo, keeps the default).
    pub fn tab_switch_recent(&self) -> bool {
        !self
            .tab_switch
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("order"))
    }

    /// Explicit BDSM overrides Claude-mode auto-detection. Default true.
    pub fn honor_bdsm(&self) -> bool {
        self.bidi.as_ref().map(|b| b.honor_bdsm).unwrap_or(true)
    }

    /// Mintty-style reorder mode. Absent config is `full`, the behavior
    /// from before the setting existed. A pane may still override it.
    pub fn bidi_mode(&self) -> crate::bidi_ctl::BidiReorder {
        self.bidi
            .as_ref()
            .map(|b| b.mode)
            .unwrap_or(crate::bidi_ctl::BidiReorder::Full)
    }

    pub fn kitty_protocol_on(&self) -> bool {
        self.keyboard.kitty_protocol
    }

    pub fn kitty_base_layout_fallback_on(&self) -> bool {
        self.keyboard.kitty_base_layout_fallback
    }

    /// Sub-line scrollback. Absent, or `"scroll": {}`, stays on.
    pub fn smooth_scroll_on(&self) -> bool {
        self.scroll.smooth
    }

    /// The mux server is off unless config.json sets `mux.enabled` to true.
    /// Anything else, including a missing `mux` object, keeps today's
    /// local ConPTY tabs and does not start a server.
    pub fn mux_enabled(&self) -> bool {
        self.mux.enabled
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

/// `~/.bayan` — config, session, plugin log and the plugins directory all
/// live here. Falls back to the current directory if USERPROFILE is unset,
/// so nothing panics on an odd environment.
pub fn bayan_dir() -> std::path::PathBuf {
    match std::env::var_os("USERPROFILE") {
        Some(home) => std::path::Path::new(&home).join(".bayan"),
        None => std::path::PathBuf::from(".bayan"),
    }
}

pub fn config_path() -> std::path::PathBuf {
    bayan_dir().join("config.json")
}

pub fn load() -> std::io::Result<UserConfig> {
    load_from(&config_path())
}

fn load_from(path: &std::path::Path) -> std::io::Result<UserConfig> {
    match std::fs::read(path) {
        Ok(bytes) => parse_bytes(&bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(UserConfig::default()),
        Err(error) => Err(error),
    }
}

/// Parse config bytes. A UTF-8 BOM is ignored; invalid JSON or invalid
/// UTF-8 is an error and must not be turned into defaults by the caller
/// that is reloading a live session.
pub fn parse_bytes(bytes: &[u8]) -> std::io::Result<UserConfig> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    // Windows editors can add a UTF-8 BOM, which serde_json rejects.
    serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(Into::into)
}

/// The bytes the settings panel writes. It hands them to the file watcher
/// before the rename so that write is not loaded again.
pub fn encode(cfg: &UserConfig) -> std::io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(cfg).map_err(Into::into)
}

pub fn write_bytes_at(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    crate::persist::write(path, bytes)
}

pub fn write_bytes(bytes: &[u8]) -> std::io::Result<()> {
    write_bytes_at(&config_path(), bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_config_is_distinguished_from_missing_config() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(load_from(&manifest.join("nonexistent-bayan-config.json")).is_ok());
        assert_eq!(
            load_from(&manifest.join("Cargo.toml"))
                .err()
                .map(|e| e.kind()),
            Some(std::io::ErrorKind::InvalidData)
        );
    }

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
        let cfg: UserConfig = serde_json::from_str(bom.trim_start_matches('\u{feff}')).unwrap();
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
        assert!(d.shell_integration_on(), "absent means on");
        let off: UserConfig = serde_json::from_str(r#"{"shell_integration":false}"#).unwrap();
        assert!(!off.shell_integration_on());
        let on: UserConfig = serde_json::from_str(r#"{"shell_integration":true}"#).unwrap();
        assert!(on.shell_integration_on());
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
        assert!(d.notifications_on());
        assert!(!d.osc_notify_on(), "program toasts are opt-in (NEW-01)");
        let c: UserConfig = serde_json::from_str(r#"{"osc_notify":true}"#).unwrap();
        assert!(c.osc_notify_on());
        let c: UserConfig =
            serde_json::from_str(r#"{"osc_notify":true,"notifications":false}"#).unwrap();
        assert!(!c.osc_notify_on());
        assert_eq!(d.shell_program(), "powershell.exe");
        assert!(!d.hide_single_tab_on());
        assert!(d.confirm_close_on());
        assert_eq!(
            d.padding_px(),
            10,
            "a default gutter, not flush to the edge"
        );
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
        let wild: UserConfig = serde_json::from_str(r#"{"opacity":0.05,"padding":500}"#).unwrap();
        assert_eq!(wild.opacity_level(), 0.5);
        assert_eq!(wild.padding_px(), 32);
        assert_eq!(d.padding_px_at(1.0), 10);
        assert_eq!(d.padding_px_at(1.5), 15);
        assert_eq!(d.padding_px_at(2.0), 20);
        assert_eq!(d.padding_px_at(0.0), 10);
        assert!(d.padding_px_at(1.5) >= d.scrollbar.width_px_at(1.5));
    }

    #[test]
    fn tab_switch_defaults_to_recent_and_only_order_opts_out() {
        assert!(UserConfig::default().tab_switch_recent());
        let order: UserConfig = serde_json::from_str(r#"{"tab_switch":"order"}"#).unwrap();
        assert!(!order.tab_switch_recent());
        let typo: UserConfig = serde_json::from_str(r#"{"tab_switch":"ordr"}"#).unwrap();
        assert!(
            typo.tab_switch_recent(),
            "a typo must not silently change behaviour"
        );
    }

    #[test]
    fn saving_omits_unset_fields() {
        let json = serde_json::to_string(&UserConfig::default()).unwrap();
        assert_eq!(
            json, "{}",
            "an untouched config serializes to nothing: {json}"
        );
    }

    #[test]
    fn accessibility_flags_default_on_and_round_trip() {
        let d = UserConfig::default();
        assert!(d.follow_system_text_size());
        assert!(d.terminal_follows_high_contrast());
        let off: UserConfig = serde_json::from_str(
            r#"{"follow_system_text_size":false,"terminal_follow_high_contrast":false}"#,
        )
        .unwrap();
        assert!(!off.follow_system_text_size());
        assert!(!off.terminal_follows_high_contrast());
        let on: UserConfig = serde_json::from_str(
            r#"{"follow_system_text_size":true,"terminal_follow_high_contrast":true}"#,
        )
        .unwrap();
        assert!(on.follow_system_text_size());
        assert!(on.terminal_follows_high_contrast());
    }

    #[test]
    fn honor_bdsm_defaults_to_true() {
        let d = UserConfig::default();
        assert!(d.honor_bdsm());
        let off: UserConfig = serde_json::from_str(r#"{"bidi":{"honor_bdsm":false}}"#).unwrap();
        assert!(!off.honor_bdsm());
        // an empty object still gets the field default
        let empty: UserConfig = serde_json::from_str(r#"{"bidi":{}}"#).unwrap();
        assert!(empty.honor_bdsm());
        assert_eq!(empty.bidi_mode(), crate::bidi_ctl::BidiReorder::Full);
    }

    #[test]
    fn bidi_mode_is_full_unless_the_config_says_otherwise() {
        use crate::bidi_ctl::BidiReorder;
        assert_eq!(UserConfig::default().bidi_mode(), BidiReorder::Full);
        let absent: UserConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.bidi_mode(), BidiReorder::Full);
        let off: UserConfig = serde_json::from_str(r#"{"bidi":{"mode":"off"}}"#).unwrap();
        assert_eq!(off.bidi_mode(), BidiReorder::Off);
        assert!(off.honor_bdsm(), "mode does not touch honor_bdsm");
        let alt: UserConfig = serde_json::from_str(r#"{"bidi":{"mode":"alt_off"}}"#).unwrap();
        assert_eq!(alt.bidi_mode(), BidiReorder::AltOff);
        let digits: UserConfig = serde_json::from_str(r#"{"bidi":{"mode":1}}"#).unwrap();
        assert_eq!(digits.bidi_mode(), BidiReorder::AltOff);
        let zero: UserConfig = serde_json::from_str(r#"{"bidi":{"mode":"0"}}"#).unwrap();
        assert_eq!(zero.bidi_mode(), BidiReorder::Off);
        // a typo keeps full rather than refusing the file
        let typo: UserConfig = serde_json::from_str(r#"{"bidi":{"mode":"line"}}"#).unwrap();
        assert_eq!(typo.bidi_mode(), BidiReorder::Full);
        // the default mode is not written back next to an explicit flag
        let saved = serde_json::to_string(&BidiConfig {
            honor_bdsm: false,
            mode: BidiReorder::Full,
        })
        .unwrap();
        assert!(
            !saved.contains("mode"),
            "full is the default and must not appear: {saved}"
        );
        let kept = serde_json::to_string(&BidiConfig {
            honor_bdsm: true,
            mode: BidiReorder::Off,
        })
        .unwrap();
        assert!(kept.contains("\"mode\":\"off\""), "{kept}");
    }

    #[test]
    fn smooth_scroll_defaults_on_and_false_is_explicit() {
        assert!(UserConfig::default().smooth_scroll_on());
        let absent: UserConfig = serde_json::from_str("{}").unwrap();
        assert!(absent.smooth_scroll_on());
        let empty: UserConfig = serde_json::from_str(r#"{"scroll":{}}"#).unwrap();
        assert!(empty.smooth_scroll_on());
        let off: UserConfig = serde_json::from_str(r#"{"scroll":{"smooth":false}}"#).unwrap();
        assert!(!off.smooth_scroll_on());
        let json = serde_json::to_string(&UserConfig::default()).unwrap();
        assert!(
            !json.contains("scroll"),
            "the default must not appear in an untouched config: {json}"
        );
        let saved = serde_json::to_string(&off).unwrap();
        assert!(saved.contains("\"smooth\":false"), "{saved}");
    }

    #[test]
    fn image_caps_default_on_and_round_trip() {
        let d = UserConfig::default();
        assert!(d.images.enabled);
        assert_eq!(d.images.max_bytes, 20 * 1024 * 1024);
        let limits = d.images.limits();
        assert!(limits.enabled);
        assert_eq!(limits.max_width, 4096);
        assert_eq!(limits.max_pane_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.max_session_bytes, 128 * 1024 * 1024);
        // A missing key keeps the defaults, and the default object is omitted
        // from a saved config (see saving_omits_unset_fields).
        let absent: UserConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.images, ImageConfig::default());
        let off: UserConfig =
            serde_json::from_str(r#"{"images":{"enabled":false,"max_bytes":10}}"#).unwrap();
        assert!(!off.images.enabled);
        let clamped = off.images.limits();
        assert_eq!(clamped.max_bytes, 10);
        // Above the hard ceiling is clamped, not treated as unlimited.
        let wild: UserConfig = serde_json::from_str(
            r#"{"images":{"max_bytes":999999999999,"max_width":100000,"max_pane_bytes":1,"max_session_bytes":1}}"#,
        )
        .unwrap();
        let limits = wild.images.limits();
        assert!(limits.max_bytes <= crate::images::CEIL_IMAGE_BYTES);
        assert!(limits.max_width <= crate::images::CEIL_DIM);
        assert!(limits.max_pane_bytes <= limits.max_session_bytes);
    }

    #[test]
    fn kitty_keyboard_config_defaults_off_and_fallback_on() {
        let d = UserConfig::default();
        assert!(!d.kitty_protocol_on());
        assert!(d.kitty_base_layout_fallback_on());
        // a config that never mentions keyboard keeps those defaults
        let absent: UserConfig = serde_json::from_str("{}").unwrap();
        assert!(!absent.kitty_protocol_on());
        assert!(absent.kitty_base_layout_fallback_on());
        let c: UserConfig = serde_json::from_str(
            r#"{"keyboard":{"kitty_protocol":true,"kitty_base_layout_fallback":false}}"#,
        )
        .unwrap();
        assert!(c.kitty_protocol_on());
        assert!(!c.kitty_base_layout_fallback_on());
    }

    #[test]
    fn clean_copy_defaults_to_claude_mode_and_only_off_or_on_opt_out() {
        use crate::copyclean::CleanCopy;
        assert_eq!(UserConfig::default().clean_copy(), CleanCopy::ClaudeMode);
        let absent: UserConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.clean_copy(), CleanCopy::ClaudeMode);
        let empty: UserConfig = serde_json::from_str(r#"{"copy":{}}"#).unwrap();
        assert_eq!(empty.clean_copy(), CleanCopy::ClaudeMode);
        let off: UserConfig = serde_json::from_str(r#"{"copy":{"clean":"off"}}"#).unwrap();
        assert_eq!(off.clean_copy(), CleanCopy::Off);
        let on: UserConfig = serde_json::from_str(r#"{"copy":{"clean":"on"}}"#).unwrap();
        assert_eq!(on.clean_copy(), CleanCopy::On);
        let named: UserConfig =
            serde_json::from_str(r#"{"copy":{"clean":"claude_mode"}}"#).unwrap();
        assert_eq!(named.clean_copy(), CleanCopy::ClaudeMode);
        let typo: UserConfig = serde_json::from_str(r#"{"copy":{"clean":"banana"}}"#).unwrap();
        assert_eq!(
            typo.clean_copy(),
            CleanCopy::ClaudeMode,
            "a typo must not change when cleaning runs"
        );
        // an untouched config still writes nothing
        assert_eq!(serde_json::to_string(&UserConfig::default()).unwrap(), "{}");
        let saved = serde_json::to_string(&off).unwrap();
        assert!(saved.contains("\"clean\":\"off\""), "{saved}");
    }

    #[test]
    fn mux_defaults_off_and_is_omitted_from_a_default_save() {
        let d = UserConfig::default();
        assert!(!d.mux_enabled());
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(
            json, "{}",
            "mux and ssh_hosts stay out of a fresh file: {json}"
        );
        let on: UserConfig = serde_json::from_str(r#"{"mux":{"enabled":true}}"#).unwrap();
        assert!(on.mux_enabled());
        // an empty object still gets the field default: off
        let empty: UserConfig = serde_json::from_str(r#"{"mux":{}}"#).unwrap();
        assert!(!empty.mux_enabled());
        let hosts: UserConfig =
            serde_json::from_str(r#"{"ssh_hosts":["example.com"],"ssh_password":"nope"}"#).unwrap();
        assert_eq!(hosts.ssh_hosts, vec!["example.com".to_string()]);
        let saved = serde_json::to_string(&hosts).unwrap();
        assert!(!saved.contains("password"), "{saved}");
        assert!(!saved.contains("nope"), "{saved}");
    }

    #[test]
    fn hex_colors_parse_strictly() {
        assert_eq!(parse_hex("#0d1117"), Some((0x0d, 0x11, 0x17)));
        assert_eq!(parse_hex("FFffFF"), Some((255, 255, 255)));
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("#zzzzzz"), None);
        assert_eq!(parse_hex(""), None);
    }

    #[test]
    fn session_colors_match_explicit_names_and_reject_invalid_colors() {
        let cfg: UserConfig = serde_json::from_str(
            r##"{"session_colors":{"prod":"#ff3322","test":"#22aa44","bad":"red"}}"##,
        )
        .unwrap();
        assert_eq!(cfg.session_color("prod"), Some((255, 51, 34)));
        assert_eq!(cfg.session_color("test"), Some((34, 170, 68)));
        assert_eq!(cfg.session_color("bad"), None);
        assert_eq!(cfg.session_color("unknown"), None);
        assert_eq!(UserConfig::default().session_color("prod"), None);
    }

    /// BAYAN-001: OSC 52 writes default off, and no value enables a read.
    #[test]
    fn osc52_copy_is_off_unless_explicitly_enabled() {
        assert!(!UserConfig::default().osc52_copy_enabled());
        let missing: UserConfig = serde_json::from_str("{}").unwrap();
        assert!(!missing.osc52_copy_enabled());
        for off in ["off", "paste", "read", "copy-paste", "on", ""] {
            let cfg: UserConfig = serde_json::from_str(&format!(r#"{{"osc52":"{off}"}}"#)).unwrap();
            assert!(
                !cfg.osc52_copy_enabled(),
                "{off} must not enable a write or a read"
            );
            assert!(!cfg.osc52_read_enabled(), "{off} must not enable a read");
        }
        let on: UserConfig = serde_json::from_str(r#"{"osc52":"copy"}"#).unwrap();
        assert!(on.osc52_copy_enabled());
        assert!(!on.osc52_read_enabled(), "copy does not enable a read");
    }

    /// The read flag is its own key and defaults off. The `osc52` string
    /// cannot turn it on.
    #[test]
    fn osc52_read_is_off_unless_explicitly_enabled() {
        assert!(!UserConfig::default().osc52_read_enabled());
        let missing: UserConfig = serde_json::from_str("{}").unwrap();
        assert!(!missing.osc52_read_enabled());
        let off: UserConfig = serde_json::from_str(r#"{"osc52_read":false}"#).unwrap();
        assert!(!off.osc52_read_enabled());
        let on: UserConfig = serde_json::from_str(r#"{"osc52_read":true}"#).unwrap();
        assert!(on.osc52_read_enabled());
        assert!(
            !on.osc52_copy_enabled(),
            "the read flag does not enable a write"
        );
        for text in ["read", "paste", "copy", "on"] {
            let cfg: UserConfig =
                serde_json::from_str(&format!(r#"{{"osc52":"{text}"}}"#)).unwrap();
            assert!(!cfg.osc52_read_enabled(), "{text} must not enable a read");
        }
    }

    #[test]
    fn explorer_context_menu_is_opt_in_and_omitted_when_unset() {
        assert!(UserConfig::default().explorer_context_menu.is_none());
        let on: UserConfig = serde_json::from_str(r#"{"explorer_context_menu":true}"#).unwrap();
        assert_eq!(on.explorer_context_menu, Some(true));
        let off: UserConfig = serde_json::from_str(r#"{"explorer_context_menu":false}"#).unwrap();
        assert_eq!(off.explorer_context_menu, Some(false));
    }

    #[test]
    fn terminal_handoff_is_off_unless_the_config_says_so() {
        assert!(UserConfig::default().terminal_handoff.is_none());
        let absent: UserConfig = serde_json::from_str("{}").unwrap();
        assert!(absent.terminal_handoff.is_none());
        let on: UserConfig = serde_json::from_str(r#"{"terminal_handoff":true}"#).unwrap();
        assert_eq!(on.terminal_handoff, Some(true));
        let off: UserConfig = serde_json::from_str(r#"{"terminal_handoff":false}"#).unwrap();
        assert_eq!(off.terminal_handoff, Some(false));
        let saved = serde_json::to_string(&UserConfig::default()).unwrap();
        assert!(!saved.contains("terminal_handoff"));
    }
}
