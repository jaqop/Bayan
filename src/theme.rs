//! Windows light/dark following.
//!
//! `"theme"` is either a named scheme or one of `system`, `dark`, `light`.
//! `system` (also the absent key) tracks the app theme: winit's
//! `ThemeChanged`, and `WM_SETTINGCHANGE` whose lParam is `ImmersiveColorSet`,
//! both resolved through the `AppsUseLightTheme` registry value. `dark` and
//! `light` lock this file's palette pair. A named scheme, or any hand-set
//! `bg` / `fg` / `palette`, is left alone when Windows changes.
//!
//! OSC 11 is not answered here.

use crate::config::UserConfig;

/// Which side of the pair Bayan shows when the user has not named a scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeChoice {
    /// Follow `AppsUseLightTheme`.
    System,
    Dark,
    Light,
}

/// The three settings segments, left to right.
pub const APPEARANCE_SEGMENTS: [ThemeChoice; 3] =
    [ThemeChoice::System, ThemeChoice::Dark, ThemeChoice::Light];

impl ThemeChoice {
    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => crate::lang::tr("النظام", "System"),
            ThemeChoice::Dark => crate::lang::tr("داكن", "Dark"),
            ThemeChoice::Light => crate::lang::tr("فاتح", "Light"),
        }
    }
}

/// `theme` as a mode word. A named scheme is not one of these.
pub fn is_mode_word(theme: Option<&str>) -> bool {
    matches!(theme, Some("system" | "dark" | "light"))
}

/// Absent and `"system"` follow Windows. `"dark"` and `"light"` lock.
/// Anything else is a scheme name; the caller decides whether it locked.
pub fn choice_of(theme: Option<&str>) -> ThemeChoice {
    match theme {
        Some("dark") => ThemeChoice::Dark,
        Some("light") => ThemeChoice::Light,
        _ => ThemeChoice::System,
    }
}

/// Colours on screen, plus the caption flag `DWMWA_USE_IMMERSIVE_DARK_MODE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colors {
    pub bg: (u8, u8, u8),
    pub fg: (u8, u8, u8),
    pub palette: [(u8, u8, u8); 16],
    /// `true` writes `1` into `DWMWA_USE_IMMERSIVE_DARK_MODE`.
    pub title_bar_dark: bool,
}

impl Colors {
    pub fn from_parts(bg: (u8, u8, u8), fg: (u8, u8, u8), palette: [(u8, u8, u8); 16]) -> Self {
        Self {
            bg,
            fg,
            palette,
            title_bar_dark: title_bar_dark_for(bg),
        }
    }
}

/// Rec. 709 luminance, integer weights. A light page gets a light caption.
pub fn title_bar_dark_for((r, g, b): (u8, u8, u8)) -> bool {
    let y = 54 * u32::from(r) + 183 * u32::from(g) + 18 * u32::from(b);
    y < 128 * 256
}

/// Dark pair: the same colours as the built-in «بيان (الأزرق)».
pub const DARK: Colors = Colors {
    bg: (0x0d, 0x11, 0x17),
    fg: (0xe6, 0xed, 0xf3),
    palette: [
        (0x0d, 0x11, 0x17),
        (0xff, 0x6b, 0x6b),
        (0x7e, 0xe7, 0x87),
        (0xe3, 0xb3, 0x41),
        (0x6c, 0xa0, 0xf6),
        (0xd2, 0xa8, 0xff),
        (0x56, 0xd4, 0xdd),
        (0xe6, 0xed, 0xf3),
        (0x6e, 0x76, 0x81),
        (0xff, 0x8a, 0x8a),
        (0xa2, 0xf5, 0xb0),
        (0xf2, 0xcc, 0x60),
        (0x8d, 0xb4, 0xf8),
        (0xe0, 0xc1, 0xff),
        (0x7e, 0xe0, 0xe6),
        (0xff, 0xff, 0xff),
    ],
    title_bar_dark: true,
};

/// Light pair: paper background, dark ink, ANSI colours that stay readable on it.
pub const LIGHT: Colors = Colors {
    bg: (0xf6, 0xf8, 0xfa),
    fg: (0x1f, 0x23, 0x28),
    palette: [
        (0x1f, 0x23, 0x28),
        (0xcf, 0x22, 0x2e),
        (0x1a, 0x7f, 0x37),
        (0x9a, 0x67, 0x00),
        (0x09, 0x69, 0xda),
        (0x82, 0x50, 0xdf),
        (0x1b, 0x7c, 0x83),
        (0x57, 0x60, 0x6a),
        (0x6e, 0x77, 0x81),
        (0xa4, 0x0e, 0x26),
        (0x2d, 0xa4, 0x4e),
        (0xbf, 0x87, 0x00),
        (0x21, 0x86, 0xeb),
        (0xa4, 0x75, 0xf9),
        (0x31, 0x92, 0xaa),
        (0xff, 0xff, 0xff),
    ],
    title_bar_dark: false,
};

/// The pair for `choice`. `apps_light` is the `AppsUseLightTheme` sample:
/// `Some(true)` is light, `Some(false)` is dark, `None` (unreadable) is dark.
pub fn pair(choice: ThemeChoice, apps_light: Option<bool>) -> Colors {
    let light = match choice {
        ThemeChoice::Dark => false,
        ThemeChoice::Light => true,
        ThemeChoice::System => apps_light == Some(true),
    };
    if light {
        LIGHT
    } else {
        DARK
    }
}

/// `AppsUseLightTheme` is light only when the DWORD is exactly 1.
pub fn apps_use_light_theme(value: Option<u32>) -> bool {
    value == Some(1)
}

/// What a system-theme signal resolved to, and whether the next one may replace it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeSurface {
    pub colors: Colors,
    pub follows_system: bool,
}

/// A live theme signal. Both carry the new appearance; the registry DWORD
/// is the source for `ImmersiveColorSet`, and winit's theme is the fallback
/// when that value cannot be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemThemeEvent {
    /// `WindowEvent::ThemeChanged`. `light` is the appearance to apply.
    Winit { light: bool },
    /// `WM_SETTINGCHANGE` whose lParam is `ImmersiveColorSet`.
    ImmersiveColorSet { apps_use_light_theme: Option<u32> },
}

impl SystemThemeEvent {
    pub fn is_light(self) -> bool {
        match self {
            SystemThemeEvent::Winit { light } => light,
            SystemThemeEvent::ImmersiveColorSet {
                apps_use_light_theme: value,
            } => apps_use_light_theme(value),
        }
    }
}

impl ThemeSurface {
    /// Apply one system-theme signal. A surface that does not follow the
    /// system (a named scheme, hand-set colours, or a locked `dark`/`light`)
    /// comes back unchanged, caption flag included.
    pub fn apply_event(self, event: SystemThemeEvent) -> Self {
        if !self.follows_system {
            return self;
        }
        Self {
            colors: pair(ThemeChoice::System, Some(event.is_light())),
            follows_system: true,
        }
    }
}

/// Read `HKCU\...\Personalize\AppsUseLightTheme`. `None` when the value is
/// missing or this is not Windows.
pub fn read_apps_use_light_theme() -> Option<u32> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::{
            RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
        };
        let subkey: Vec<u16> =
            "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0"
                .encode_utf16()
                .collect();
        let name: Vec<u16> = "AppsUseLightTheme\0".encode_utf16().collect();
        let mut data: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: HKEY_CURRENT_USER is a predefined key. `subkey` and `name`
        // are NUL-terminated and outlive the call. `data` is a local DWORD
        // and `size` is its byte length; RegGetValueW writes at most that
        // many bytes and does not keep the pointers.
        let err = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&raw mut data).cast(),
                &raw mut size,
            )
        };
        if err == 0 {
            Some(data)
        } else {
            None
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// `WM_SETTINGCHANGE` carries this lParam when the app theme changed.
pub fn is_immersive_color_set(name: &str) -> bool {
    name == "ImmersiveColorSet"
}

/// True when `lparam` is the wide string `ImmersiveColorSet`.
/// `0` is the documented null lParam and is not that name.
pub fn setting_change_is_immersive(lparam: isize) -> bool {
    if lparam == 0 {
        return false;
    }
    // SAFETY: the caller passes `WM_SETTINGCHANGE`'s lParam during the
    // message hook. The system documents that pointer as either null or a
    // NUL-terminated wide string valid for the call. The read stops at the
    // first NUL or at 64 units, so a missing NUL cannot walk off the buffer
    // we are willing to inspect. The string is not retained.
    let name = unsafe {
        let p = lparam as *const u16;
        let mut n = 0;
        while n < 64 && *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    };
    is_immersive_color_set(&name)
}

/// The mode a fresh or mode-word config is in. Named schemes are `None`
/// here: the gallery tile is the selection, not one of these segments.
pub fn appearance_highlight(cfg: &UserConfig) -> Option<ThemeChoice> {
    match cfg.theme.as_deref() {
        Some("dark") => Some(ThemeChoice::Dark),
        Some("light") => Some(ThemeChoice::Light),
        Some("system") => Some(ThemeChoice::System),
        None if cfg.bg.is_none() && cfg.fg.is_none() && cfg.palette.is_none() => {
            Some(ThemeChoice::System)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface(colors: Colors, follows_system: bool) -> ThemeSurface {
        ThemeSurface {
            colors,
            follows_system,
        }
    }

    #[test]
    fn system_dark_and_light_follow_the_registry_value() {
        // Absent system sample, and a dark DWORD, both stay on the dark pair.
        assert_eq!(pair(ThemeChoice::System, None), DARK);
        assert_eq!(DARK.title_bar_dark, title_bar_dark_for(DARK.bg));
        assert_eq!(pair(ThemeChoice::System, Some(false)), DARK);
        assert!(!apps_use_light_theme(Some(0)));
        assert!(!apps_use_light_theme(None));
        assert!(!apps_use_light_theme(Some(2)));

        // Exactly 1 is light, and the light pair drops the dark caption.
        assert!(apps_use_light_theme(Some(1)));
        let light = pair(ThemeChoice::System, Some(true));
        assert_eq!(light, LIGHT);
        assert_eq!(light.palette, LIGHT.palette);
        assert!(!light.title_bar_dark);
        assert!(!title_bar_dark_for(LIGHT.bg));
        assert!(title_bar_dark_for(DARK.bg));

        // An explicit dark or light choice ignores the registry.
        assert_eq!(pair(ThemeChoice::Dark, Some(true)), DARK);
        assert!(pair(ThemeChoice::Dark, Some(true)).title_bar_dark);
        assert_eq!(pair(ThemeChoice::Light, Some(false)), LIGHT);
        assert!(!pair(ThemeChoice::Light, Some(false)).title_bar_dark);

        // The dark pair is the built-in default scheme, so an unreadable
        // registry value keeps today's colours.
        let bayan = crate::render::theme_by_name(crate::render::DEFAULT_THEME).unwrap();
        assert_eq!(DARK.bg, bayan.bg);
        assert_eq!(DARK.fg, bayan.fg);
        assert_eq!(DARK.palette, bayan.palette);

        assert_eq!(choice_of(None), ThemeChoice::System);
        assert_eq!(choice_of(Some("system")), ThemeChoice::System);
        assert_eq!(choice_of(Some("dark")), ThemeChoice::Dark);
        assert_eq!(choice_of(Some("light")), ThemeChoice::Light);
        assert!(is_mode_word(Some("system")));
        assert!(!is_mode_word(Some("Dracula")));
    }

    #[test]
    fn theme_changed_event_updates_palette_and_title_bar() {
        let dark = surface(DARK, true);

        let from_winit = dark.apply_event(SystemThemeEvent::Winit { light: true });
        assert!(from_winit.follows_system);
        assert_eq!(from_winit.colors.palette, LIGHT.palette);
        assert_eq!(from_winit.colors.bg, LIGHT.bg);
        assert_eq!(from_winit.colors.fg, LIGHT.fg);
        assert!(!from_winit.colors.title_bar_dark);

        let from_registry = from_winit.apply_event(SystemThemeEvent::ImmersiveColorSet {
            apps_use_light_theme: Some(0),
        });
        assert_eq!(from_registry.colors.palette, DARK.palette);
        assert!(from_registry.colors.title_bar_dark);

        let light_again = from_registry.apply_event(SystemThemeEvent::ImmersiveColorSet {
            apps_use_light_theme: Some(1),
        });
        assert_eq!(light_again.colors, LIGHT);
        assert!(!light_again.colors.title_bar_dark);

        // A missing DWORD is not light: the caption goes back to dark.
        let unread = light_again.apply_event(SystemThemeEvent::ImmersiveColorSet {
            apps_use_light_theme: None,
        });
        assert_eq!(unread.colors, DARK);
        assert!(unread.colors.title_bar_dark);

        // An explicit scheme is not replaced, and its caption flag stays.
        let explicit = Colors::from_parts((0x28, 0x2a, 0x36), (0xf8, 0xf8, 0xf2), DARK.palette);
        assert!(explicit.title_bar_dark);
        let locked = surface(explicit, false);
        assert_eq!(
            locked.apply_event(SystemThemeEvent::Winit { light: true }),
            locked
        );
        assert_eq!(
            locked.apply_event(SystemThemeEvent::ImmersiveColorSet {
                apps_use_light_theme: Some(1),
            }),
            locked
        );

        // A locked light choice does not flip when the system goes dark.
        let forced = surface(LIGHT, false);
        assert_eq!(
            forced.apply_event(SystemThemeEvent::Winit { light: false }),
            forced
        );
        assert!(!forced.colors.title_bar_dark);
    }

    #[test]
    fn immersive_color_set_is_the_only_setting_name() {
        assert!(is_immersive_color_set("ImmersiveColorSet"));
        assert!(!is_immersive_color_set("ImmersiveColor"));
        assert!(!is_immersive_color_set("Policy"));
        assert!(!is_immersive_color_set(""));
        assert!(!setting_change_is_immersive(0));

        let mut wide: Vec<u16> = "ImmersiveColorSet".encode_utf16().collect();
        wide.push(0);
        assert!(setting_change_is_immersive(wide.as_ptr() as isize));
        let mut other: Vec<u16> = "intl".encode_utf16().collect();
        other.push(0);
        assert!(!setting_change_is_immersive(other.as_ptr() as isize));
    }

    #[test]
    fn named_theme_and_hand_set_colours_do_not_follow() {
        let open = UserConfig::default();
        assert!(crate::render::follows_system_theme(&open));
        assert_eq!(appearance_highlight(&open), Some(ThemeChoice::System));
        let mut light = open.clone();
        light.system_apps_light = Some(true);
        assert_eq!(crate::render::resolved_colors(&light), LIGHT);

        let mut dark = UserConfig {
            theme: Some("dark".into()),
            system_apps_light: Some(true),
            ..Default::default()
        };
        assert!(!crate::render::follows_system_theme(&dark));
        assert_eq!(crate::render::resolved_colors(&dark), DARK);
        assert_eq!(appearance_highlight(&dark), Some(ThemeChoice::Dark));

        dark.theme = Some("light".into());
        dark.system_apps_light = Some(false);
        assert_eq!(crate::render::resolved_colors(&dark), LIGHT);
        assert_eq!(appearance_highlight(&dark), Some(ThemeChoice::Light));

        let mut named = UserConfig {
            theme: Some("Dracula".into()),
            system_apps_light: Some(true),
            ..Default::default()
        };
        assert!(!crate::render::follows_system_theme(&named));
        let dracula = crate::render::theme_by_name("Dracula").unwrap();
        let got = crate::render::resolved_colors(&named);
        assert_eq!(got.bg, dracula.bg);
        assert_eq!(got.fg, dracula.fg);
        assert_eq!(got.palette, dracula.palette);
        assert!(got.title_bar_dark);
        assert_eq!(appearance_highlight(&named), None);

        // A hand-set background is explicit even with no scheme name.
        named.theme = None;
        named.bg = Some("#112233".into());
        assert!(!crate::render::follows_system_theme(&named));
        let painted = crate::render::resolved_colors(&named);
        assert_eq!(painted.bg, (0x11, 0x22, 0x33));
        assert!(painted.title_bar_dark);

        // A theme name Bayan does not know stays on the default scheme.
        let unknown = UserConfig {
            theme: Some("no-such-theme".into()),
            system_apps_light: Some(true),
            ..Default::default()
        };
        assert!(!crate::render::follows_system_theme(&unknown));
        assert_eq!(crate::render::resolved_colors(&unknown), DARK);
    }
}
