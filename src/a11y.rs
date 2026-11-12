//! Windows High Contrast and the Accessibility text-size setting.
//!
//! The decisions live here as plain data so they can be tested without
//! Win32. `render` paints whatever [`map_chrome`] and [`map_terminal`]
//! return, and it sizes fonts with [`compose_ui_font_px`] and
//! [`compose_terminal_font_px`]. The platform read
//! ([`read_snapshot`]) is the only place that calls the system.
//!
//! ## Text size and an explicit `font_size`
//!
//! Windows Accessibility text size is a percent from 100 to 225. Bayan
//! stores it as a multiplier (`1.0`..=`2.25`). When
//! `follow_system_text_size` is on (the default), that multiplier applies
//! to UI chrome and to the terminal font, **including an explicit
//! `font_size`**. The configured point size is the size at 100%. DPI is a
//! separate multiplier, applied after the text scale:
//!
//! ```text
//! pixels = points × text_scale × dpi
//! ```
//!
//! Set `follow_system_text_size` to false to keep the point size absolute.
//! DPI still applies. Ctrl+wheel changes the 100% point size; the text
//! scale multiplies that too.
//!
//! This is not `PaletteLayout::text_scale` in `render`. That field
//! compresses the command palette when the window is short.

/// `#rrggbb` channels, the same tuple the renderer already paints with.
pub type Rgb = (u8, u8, u8);

/// `WM_SETTINGCHANGE` / `WM_WININICHANGE`.
pub const WM_SETTINGCHANGE: u32 = 0x001A;
/// `WM_SYSCOLORCHANGE`.
pub const WM_SYSCOLORCHANGE: u32 = 0x0015;

/// `HCF_HIGHCONTRASTON`.
pub const HCF_HIGHCONTRASTON: u32 = 0x0000_0001;

/// `GetSysColor` indexes Bayan reads. Stable since the Win32 headers.
pub const COLOR_WINDOW: i32 = 5;
pub const COLOR_WINDOWTEXT: i32 = 8;
pub const COLOR_HIGHLIGHT: i32 = 13;
pub const COLOR_HIGHLIGHTTEXT: i32 = 14;
pub const COLOR_BTNFACE: i32 = 15;
pub const COLOR_GRAYTEXT: i32 = 17;
pub const COLOR_HOTLIGHT: i32 = 26;

/// UI chrome face size at 96 DPI and 100% text size, in pixels.
pub const UI_FONT_PX: f32 = 16.0;
/// Matching line height. The terminal font does not use this.
pub const UI_LINE_PX: f32 = 22.4;

/// Accessibility slider, inclusive, in percent.
pub const TEXT_SCALE_MIN_PERCENT: i32 = 100;
pub const TEXT_SCALE_MAX_PERCENT: i32 = 225;

/// Colors from `GetSysColor` while High Contrast is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemColors {
    /// `COLOR_WINDOW` — title bar, tab bar, terminal background.
    pub window: Rgb,
    /// `COLOR_WINDOWTEXT` — chrome text, terminal foreground, cursor.
    pub window_text: Rgb,
    /// `COLOR_HIGHLIGHT` — selection, focus rings, the selected palette row.
    pub highlight: Rgb,
    /// `COLOR_HIGHLIGHTTEXT` — text drawn on a highlight.
    pub highlight_text: Rgb,
    /// `COLOR_HOTLIGHT` — links and other hot accents.
    pub hotlight: Rgb,
    /// `COLOR_GRAYTEXT` — disabled text and the scrollbar.
    pub gray_text: Rgb,
    /// `COLOR_BTNFACE` — palette, cards, buttons.
    pub btn_face: Rgb,
}

/// One chrome role. High Contrast replaces every field; off, the theme
/// that was passed in comes back unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChromeColors {
    pub tab_bar: Rgb,
    pub title_bar: Rgb,
    pub title_text: Rgb,
    pub palette: Rgb,
    pub palette_text: Rgb,
    pub palette_sel: Rgb,
    pub palette_sel_text: Rgb,
    pub card: Rgb,
    pub card_text: Rgb,
    pub scrollbar: Rgb,
    pub focus: Rgb,
    pub selection: Rgb,
    pub selection_text: Rgb,
    pub cursor: Rgb,
    pub muted: Rgb,
    pub accent: Rgb,
}

/// Foreground, background, selection and cursor inside the terminal grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalGridColors {
    pub bg: Rgb,
    pub fg: Rgb,
    pub selection: Rgb,
    pub selection_text: Rgb,
    pub cursor: Rgb,
}

/// What the window last read. `text_scale` is `1.0`..=`2.25`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct A11ySnapshot {
    pub high_contrast: bool,
    pub colors: SystemColors,
    pub text_scale: f32,
}

impl A11ySnapshot {
    /// High Contrast off, text size 100%. Renderer tests and the first
    /// paint before a read both start here, so a machine's settings never
    /// leak into a unit test that did not ask for them.
    pub fn neutral() -> Self {
        Self {
            high_contrast: false,
            colors: SystemColors {
                window: (0, 0, 0),
                window_text: (255, 255, 255),
                highlight: (0, 120, 215),
                highlight_text: (255, 255, 255),
                hotlight: (0, 102, 204),
                gray_text: (109, 109, 109),
                btn_face: (240, 240, 240),
            },
            text_scale: 1.0,
        }
    }
}

/// The message hook's predicate: these two messages mean the contrast
/// mode, the system colors, or the text-size slider may have moved.
pub fn message_triggers_reread(message: u32) -> bool {
    message == WM_SETTINGCHANGE || message == WM_SYSCOLORCHANGE
}

/// `HCF_HIGHCONTRASTON`. Other flags (`HCF_AVAILABLE`, hot-key bits) do
/// not turn the mode on.
pub fn high_contrast_flag(flags: u32) -> bool {
    flags & HCF_HIGHCONTRASTON != 0
}

/// COLORREF is `0x00bbggrr`.
pub fn colorref_to_rgb(colorref: u32) -> Rgb {
    (
        (colorref & 0xff) as u8,
        ((colorref >> 8) & 0xff) as u8,
        ((colorref >> 16) & 0xff) as u8,
    )
}

/// Clamp a raw percent into the Accessibility slider's 100–225 range.
pub fn clamp_text_scale_percent(raw: i32) -> i32 {
    raw.clamp(TEXT_SCALE_MIN_PERCENT, TEXT_SCALE_MAX_PERCENT)
}

/// Percent to the multiplier the painters use. Out-of-range percents clamp.
pub fn text_scale_factor(percent: i32) -> f32 {
    clamp_text_scale_percent(percent) as f32 / 100.0
}

/// `HKCU\Software\Microsoft\Accessibility\TextScaleFactor`. A missing value
/// is 100%: Windows writes the DWORD only after the slider moves.
pub fn text_scale_from_registry(value: Option<u32>) -> f32 {
    match value {
        Some(v) => text_scale_factor(dword_percent(v)),
        None => 1.0,
    }
}

fn dword_percent(v: u32) -> i32 {
    if v > i32::MAX as u32 {
        i32::MAX
    } else {
        v as i32
    }
}

/// UI face size in pixels. `base_px` is the 96 DPI, 100% size
/// ([`UI_FONT_PX`]). Text scale and DPI each multiply once.
pub fn compose_ui_font_px(base_px: f32, text_scale: f32, dpi: f32) -> f32 {
    base_px * text_scale * dpi
}

/// UI line height. Same composition as the face size, so a 150% slider
/// and a 150% DPI monitor do not collapse into one 1.5.
pub fn compose_ui_line_px(base_px: f32, text_scale: f32, dpi: f32) -> f32 {
    base_px * text_scale * dpi
}

/// Terminal face size in pixels.
///
/// `points` is the configured size (or the default) plus wheel zoom,
/// already clamped to the point-size range. It is the size at 100% text
/// scale. When `follow` is true the system text scale multiplies it;
/// DPI always multiplies. The two factors are never added or folded
/// together.
pub fn compose_terminal_font_px(points: f32, text_scale: f32, dpi: f32, follow: bool) -> f32 {
    let scale = if follow { text_scale } else { 1.0 };
    points * scale * dpi
}

/// Chrome colors for this frame. High Contrast off returns `theme`.
pub fn map_chrome(high_contrast: bool, system: SystemColors, theme: ChromeColors) -> ChromeColors {
    if !high_contrast {
        return theme;
    }
    ChromeColors {
        tab_bar: system.window,
        title_bar: system.window,
        title_text: system.window_text,
        palette: system.btn_face,
        palette_text: system.window_text,
        palette_sel: system.highlight,
        palette_sel_text: system.highlight_text,
        card: system.btn_face,
        card_text: system.window_text,
        scrollbar: system.gray_text,
        focus: system.highlight,
        selection: system.highlight,
        selection_text: system.highlight_text,
        cursor: system.window_text,
        muted: system.gray_text,
        accent: system.hotlight,
    }
}

/// Grid fg/bg/selection/cursor. The 16 ANSI slots are not touched: a
/// program's red stays red. `follow_in_grid` is the config option
/// `terminal_follow_high_contrast` (default true). High Contrast off, or
/// the option off, keeps `theme` inside the grid. Chrome still follows
/// High Contrast either way — that choice is [`map_chrome`].
pub fn map_terminal(
    high_contrast: bool,
    follow_in_grid: bool,
    system: SystemColors,
    theme: TerminalGridColors,
) -> TerminalGridColors {
    if high_contrast && follow_in_grid {
        TerminalGridColors {
            bg: system.window,
            fg: system.window_text,
            selection: system.highlight,
            selection_text: system.highlight_text,
            cursor: system.window_text,
        }
    } else {
        theme
    }
}

/// The hook sets the dirty bit; `about_to_wait` polls it and re-reads.
/// Held as values so a test can run the same steps the window runs.
/// The window itself uses an atomic, because the message hook cannot
/// borrow this value.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
pub struct A11yWatch {
    pub snapshot: A11ySnapshot,
    dirty: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl A11yWatch {
    pub fn new(snapshot: A11ySnapshot) -> Self {
        Self {
            snapshot,
            dirty: false,
        }
    }

    pub fn on_message(&mut self, message: u32) {
        if message_triggers_reread(message) {
            self.dirty = true;
        }
    }

    /// Re-read when a settings message arrived. Returns whether the
    /// snapshot changed. Any other message leaves `read` uncalled.
    pub fn poll<F>(&mut self, read: F) -> bool
    where
        F: FnOnce() -> A11ySnapshot,
    {
        if !self.dirty {
            return false;
        }
        self.dirty = false;
        let next = read();
        let changed = next != self.snapshot;
        self.snapshot = next;
        changed
    }
}

/// Current High Contrast flag, system colors, and text-scale multiplier.
/// Off Windows this is [`A11ySnapshot::neutral`]: there is no system
/// setting to follow.
pub fn read_snapshot() -> A11ySnapshot {
    #[cfg(windows)]
    {
        read_snapshot_win32()
    }
    #[cfg(not(windows))]
    {
        A11ySnapshot::neutral()
    }
}

#[cfg(windows)]
fn read_snapshot_win32() -> A11ySnapshot {
    A11ySnapshot {
        high_contrast: read_high_contrast(),
        colors: read_system_colors(),
        text_scale: text_scale_from_registry(read_text_scale_dword()),
    }
}

#[cfg(windows)]
fn read_high_contrast() -> bool {
    use windows_sys::Win32::UI::Accessibility::HIGHCONTRASTW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETHIGHCONTRAST};
    // The scheme name is not used. The buffer is still real so the call
    // never writes through a null pointer if this Windows version fills it.
    let mut scheme = [0u16; 260];
    let mut info = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        dwFlags: 0,
        lpszDefaultScheme: scheme.as_mut_ptr(),
    };
    // SAFETY: `info` is a live HIGHCONTRASTW whose cbSize is the struct
    // size, and `lpszDefaultScheme` points at `scheme` for this call only.
    // SystemParametersInfoW writes the flag and the name; it does not keep
    // either pointer.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            info.cbSize,
            &mut info as *mut HIGHCONTRASTW as *mut _,
            0,
        )
    };
    ok != 0 && high_contrast_flag(info.dwFlags)
}

#[cfg(windows)]
fn read_system_colors() -> SystemColors {
    use windows_sys::Win32::Graphics::Gdi::GetSysColor;
    // SAFETY: GetSysColor takes a color index and returns a COLORREF. No
    // pointers, and an unknown index still returns a defined fallback color.
    let rgb = |index: i32| colorref_to_rgb(unsafe { GetSysColor(index) });
    SystemColors {
        window: rgb(COLOR_WINDOW),
        window_text: rgb(COLOR_WINDOWTEXT),
        highlight: rgb(COLOR_HIGHLIGHT),
        highlight_text: rgb(COLOR_HIGHLIGHTTEXT),
        hotlight: rgb(COLOR_HOTLIGHT),
        gray_text: rgb(COLOR_GRAYTEXT),
        btn_face: rgb(COLOR_BTNFACE),
    }
}

/// The DWORD `Windows.UI.ViewManagement.UISettings.TextScaleFactor` persists
/// at `HKCU\Software\Microsoft\Accessibility\TextScaleFactor`. Bayan does not
/// link a WinRT crate (toasts already stay on `Shell_NotifyIcon` for that
/// reason). The slider writes this value and broadcasts `WM_SETTINGCHANGE`,
/// which is the Win32 signal for `TextScaleFactorChanged`.
#[cfg(windows)]
fn read_text_scale_dword() -> Option<u32> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
    };
    let subkey = wide(r"Software\Microsoft\Accessibility");
    let name = wide("TextScaleFactor");
    let mut key = std::ptr::null_mut();
    // SAFETY: `subkey` is a null-terminated UTF-16 path. On success `key`
    // is an open HKEY closed before this function returns. On failure it
    // is left null and is not closed.
    let opened =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut key) };
    if opened != 0 || key.is_null() {
        return None;
    }
    let mut kind = 0u32;
    let mut data = 0u32;
    let mut bytes = std::mem::size_of::<u32>() as u32;
    // SAFETY: `key` is the open handle from above, `name` is null-terminated,
    // and `data`/`bytes` describe a live u32. The call does not retain them.
    let queried = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            &mut data as *mut u32 as *mut u8,
            &mut bytes,
        )
    };
    // SAFETY: `key` is the same open handle. Closing it once is required.
    unsafe {
        RegCloseKey(key);
    }
    if queried != 0 || kind != REG_DWORD || bytes != 4 {
        None
    } else {
        Some(data)
    }
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system() -> SystemColors {
        SystemColors {
            window: (0x00, 0x00, 0x00),
            window_text: (0xff, 0xff, 0x00),
            highlight: (0x00, 0x00, 0xff),
            highlight_text: (0xff, 0xff, 0xff),
            hotlight: (0xff, 0x00, 0xff),
            gray_text: (0x80, 0x80, 0x80),
            btn_face: (0xff, 0xff, 0xff),
        }
    }

    fn theme_chrome() -> ChromeColors {
        ChromeColors {
            tab_bar: (0x0a, 0x1e, 0x1e),
            title_bar: (0x0a, 0x1e, 0x1e),
            title_text: (0xb6, 0xc2, 0xcf),
            palette: (0x16, 0x1b, 0x22),
            palette_text: (0xe6, 0xed, 0xf3),
            palette_sel: (0x21, 0x36, 0x50),
            palette_sel_text: (0xf0, 0xf6, 0xfc),
            card: (0x14, 0x19, 0x20),
            card_text: (0xc2, 0xb7, 0x96),
            scrollbar: (0xc2, 0xb7, 0x96),
            focus: (0x2e, 0xa0, 0x43),
            selection: (80, 140, 255),
            selection_text: (0xc2, 0xb7, 0x96),
            cursor: (0xc2, 0xb7, 0x96),
            muted: (0x9a, 0xa4, 0xb2),
            accent: (0x58, 0xd1, 0xc0),
        }
    }

    fn theme_grid() -> TerminalGridColors {
        TerminalGridColors {
            bg: (0x0d, 0x11, 0x17),
            fg: (0xe6, 0xed, 0xf3),
            selection: (80, 140, 255),
            selection_text: (0xe6, 0xed, 0xf3),
            cursor: (0xe6, 0xed, 0xf3),
        }
    }

    #[test]
    fn high_contrast_on_maps_chrome_to_system_colors() {
        let sys = system();
        let chrome = map_chrome(true, sys, theme_chrome());
        assert_eq!(chrome.tab_bar, sys.window);
        assert_eq!(chrome.title_bar, sys.window);
        assert_eq!(chrome.title_text, sys.window_text);
        assert_eq!(chrome.palette, sys.btn_face);
        assert_eq!(chrome.palette_text, sys.window_text);
        assert_eq!(chrome.palette_sel, sys.highlight);
        assert_eq!(chrome.palette_sel_text, sys.highlight_text);
        assert_eq!(chrome.card, sys.btn_face);
        assert_eq!(chrome.card_text, sys.window_text);
        assert_eq!(chrome.scrollbar, sys.gray_text);
        assert_eq!(chrome.focus, sys.highlight);
        assert_eq!(chrome.selection, sys.highlight);
        assert_eq!(chrome.selection_text, sys.highlight_text);
        assert_eq!(chrome.cursor, sys.window_text);
        assert_eq!(chrome.muted, sys.gray_text);
        assert_eq!(chrome.accent, sys.hotlight);
        assert_ne!(chrome, theme_chrome());
    }

    #[test]
    fn high_contrast_off_keeps_the_theme() {
        let chrome = map_chrome(false, system(), theme_chrome());
        assert_eq!(chrome, theme_chrome());
        let grid = map_terminal(false, true, system(), theme_grid());
        assert_eq!(grid, theme_grid());
    }

    #[test]
    fn terminal_grid_follows_high_contrast_unless_the_option_keeps_app_colors() {
        let sys = system();
        let followed = map_terminal(true, true, sys, theme_grid());
        assert_eq!(followed.bg, sys.window);
        assert_eq!(followed.fg, sys.window_text);
        assert_eq!(followed.selection, sys.highlight);
        assert_eq!(followed.selection_text, sys.highlight_text);
        assert_eq!(followed.cursor, sys.window_text);
        // The option keeps the theme inside the grid. Chrome above still maps.
        let kept = map_terminal(true, false, sys, theme_grid());
        assert_eq!(kept, theme_grid());
        assert_eq!(map_chrome(true, sys, theme_chrome()).tab_bar, sys.window);
    }

    #[test]
    fn text_scale_150_enlarges_the_ui_font_and_composes_with_dpi() {
        let ui = compose_ui_font_px(UI_FONT_PX, 1.5, 1.5);
        assert_eq!(ui, 36.0);
        assert_eq!(ui, UI_FONT_PX * 1.5 * 1.5);
        // Each factor contributes. 150% text on a 100% monitor is not the
        // same pixels as 100% text on a 150% monitor's... they match only
        // because both factors are 1.5. Folding them (using 1.5 once) is
        // the bug this guards: 16 × 1.5 × 1.5 is 36, not 24.
        assert_eq!(compose_ui_font_px(UI_FONT_PX, 1.0, 1.5), 24.0);
        assert_eq!(compose_ui_font_px(UI_FONT_PX, 1.5, 1.0), 24.0);
        assert_ne!(ui, compose_ui_font_px(UI_FONT_PX, 1.0, 1.5));
        let line = compose_ui_line_px(UI_LINE_PX, 1.5, 1.5);
        assert_eq!(line, UI_LINE_PX * 1.5 * 1.5);
        assert!(line > compose_ui_line_px(UI_LINE_PX, 1.0, 1.5));
        // Default terminal face (18pt) grows by the same two factors.
        let term = compose_terminal_font_px(18.0, 1.5, 1.5, true);
        assert_eq!(term, 40.5);
        assert_eq!(term, 18.0 * 1.5 * 1.5);
    }

    #[test]
    fn explicit_font_size_is_still_multiplied_by_text_scale() {
        // Choice: an explicit font_size is the 100% size. Following the
        // system multiplies it. Turning follow off keeps the points and
        // still applies DPI.
        assert_eq!(compose_terminal_font_px(16.0, 1.5, 1.5, true), 36.0);
        assert_eq!(compose_terminal_font_px(16.0, 1.5, 1.5, false), 24.0);
        assert_eq!(compose_terminal_font_px(16.0, 1.5, 1.0, false), 16.0);
    }

    #[test]
    fn out_of_range_text_scale_clamps_to_the_accessibility_slider() {
        assert_eq!(clamp_text_scale_percent(-10), 100);
        assert_eq!(clamp_text_scale_percent(0), 100);
        assert_eq!(clamp_text_scale_percent(99), 100);
        assert_eq!(clamp_text_scale_percent(100), 100);
        assert_eq!(clamp_text_scale_percent(150), 150);
        assert_eq!(clamp_text_scale_percent(225), 225);
        assert_eq!(clamp_text_scale_percent(226), 225);
        assert_eq!(clamp_text_scale_percent(10_000), 225);
        assert_eq!(text_scale_factor(50), 1.0);
        assert_eq!(text_scale_factor(150), 1.5);
        assert_eq!(text_scale_factor(400), 2.25);
        assert_eq!(text_scale_from_registry(None), 1.0);
        assert_eq!(text_scale_from_registry(Some(150)), 1.5);
        assert_eq!(text_scale_from_registry(Some(1)), 1.0);
        assert_eq!(text_scale_from_registry(Some(u32::MAX)), 2.25);
    }

    #[test]
    fn settings_change_triggers_a_reread() {
        let mut watch = A11yWatch::new(A11ySnapshot::neutral());
        let mut reads = 0;
        assert!(!watch.poll(|| {
            reads += 1;
            A11ySnapshot::neutral()
        }));
        assert_eq!(reads, 0, "an idle poll must not re-read");

        watch.on_message(0x000F); // WM_PAINT
        assert!(!watch.poll(|| {
            reads += 1;
            A11ySnapshot::neutral()
        }));
        assert_eq!(reads, 0, "paint is not a settings change");

        watch.on_message(WM_SETTINGCHANGE);
        let enlarged = A11ySnapshot {
            text_scale: 1.5,
            ..A11ySnapshot::neutral()
        };
        assert!(watch.poll(|| {
            reads += 1;
            enlarged
        }));
        assert_eq!(reads, 1);
        assert_eq!(watch.snapshot.text_scale, 1.5);

        // A second settings message re-reads even when the value is unchanged.
        watch.on_message(WM_SYSCOLORCHANGE);
        assert!(!watch.poll(|| {
            reads += 1;
            enlarged
        }));
        assert_eq!(reads, 2, "syscolor change must re-read");
        assert_eq!(watch.snapshot, enlarged);
    }

    #[test]
    fn colorref_is_decoded_as_blue_green_red() {
        assert_eq!(colorref_to_rgb(0x00ff_8000), (0x00, 0x80, 0xff));
        assert_eq!(colorref_to_rgb(0x0000_00ff), (0xff, 0x00, 0x00));
    }

    #[test]
    fn high_contrast_flag_is_only_hcf_highcontraston() {
        assert!(high_contrast_flag(HCF_HIGHCONTRASTON));
        assert!(high_contrast_flag(HCF_HIGHCONTRASTON | 0x2));
        assert!(!high_contrast_flag(0));
        assert!(!high_contrast_flag(0x2));
    }

    #[cfg(windows)]
    #[test]
    fn win32_constants_match_the_sdk() {
        use windows_sys::Win32::Graphics::Gdi::{
            COLOR_BTNFACE, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_HOTLIGHT,
            COLOR_WINDOW, COLOR_WINDOWTEXT,
        };
        use windows_sys::Win32::UI::Accessibility::HCF_HIGHCONTRASTON;
        use windows_sys::Win32::UI::WindowsAndMessaging::{WM_SETTINGCHANGE, WM_SYSCOLORCHANGE};
        assert_eq!(super::WM_SETTINGCHANGE, WM_SETTINGCHANGE);
        assert_eq!(super::WM_SYSCOLORCHANGE, WM_SYSCOLORCHANGE);
        assert_eq!(super::HCF_HIGHCONTRASTON, HCF_HIGHCONTRASTON);
        assert_eq!(super::COLOR_WINDOW, COLOR_WINDOW);
        assert_eq!(super::COLOR_WINDOWTEXT, COLOR_WINDOWTEXT);
        assert_eq!(super::COLOR_HIGHLIGHT, COLOR_HIGHLIGHT);
        assert_eq!(super::COLOR_HIGHLIGHTTEXT, COLOR_HIGHLIGHTTEXT);
        assert_eq!(super::COLOR_BTNFACE, COLOR_BTNFACE);
        assert_eq!(super::COLOR_GRAYTEXT, COLOR_GRAYTEXT);
        assert_eq!(super::COLOR_HOTLIGHT, COLOR_HOTLIGHT);
    }
}
