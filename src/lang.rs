//! The interface language: Arabic (the default, right to left) or English
//! (left to right). It covers Bayan's own chrome only — panels, menus,
//! cards, toasts' fixed words. What programs print in a pane is never
//! touched.
//!
//! Strings stay where they are used, as `tr("عربي", "English")` pairs, so
//! a label and its translation cannot drift apart in separate tables.
//! Layout code asks `rtl()` which edge text starts from.
//!
//! The current language is process-wide in the app. Under `cfg(test)` it
//! is per thread, because the test harness runs tests in parallel threads:
//! a test that switches to English cannot flip another test's Arabic.

/// An interface language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Ar,
    En,
}

impl Lang {
    /// The config.json value: `"en"` is English, anything else (or
    /// nothing) is Arabic — a typo must not strand someone in a language
    /// they cannot read back out of.
    pub fn from_config(value: Option<&str>) -> Lang {
        match value.map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if v == "en" || v == "english" => Lang::En,
            _ => Lang::Ar,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Lang::Ar => "ar",
            Lang::En => "en",
        }
    }

    pub fn other(self) -> Lang {
        match self {
            Lang::Ar => Lang::En,
            Lang::En => Lang::Ar,
        }
    }

    /// The language's own name, written in that language: the switch
    /// shows the OTHER language's name, which its reader can always find.
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::Ar => "العربية",
            Lang::En => "English",
        }
    }
}

#[cfg(not(test))]
mod current {
    use super::Lang;
    use std::sync::atomic::{AtomicBool, Ordering};

    static EN: AtomicBool = AtomicBool::new(false);

    pub fn set(lang: Lang) {
        EN.store(lang == Lang::En, Ordering::Relaxed);
    }

    pub fn get() -> Lang {
        if EN.load(Ordering::Relaxed) {
            Lang::En
        } else {
            Lang::Ar
        }
    }
}

#[cfg(test)]
mod current {
    use super::Lang;
    use std::cell::Cell;

    thread_local! {
        static LANG: Cell<Lang> = const { Cell::new(Lang::Ar) };
    }

    pub fn set(lang: Lang) {
        LANG.with(|l| l.set(lang));
    }

    pub fn get() -> Lang {
        LANG.with(|l| l.get())
    }
}

pub use current::{get, set};

/// Text starts at the right edge (Arabic) rather than the left.
pub fn rtl() -> bool {
    get() == Lang::Ar
}

/// The string for the current language.
pub fn tr(ar: &'static str, en: &'static str) -> &'static str {
    pick(get(), ar, en)
}

/// The string for `lang`, whatever is on screen.
pub fn pick(lang: Lang, ar: &'static str, en: &'static str) -> &'static str {
    match lang {
        Lang::Ar => ar,
        Lang::En => en,
    }
}

/// Where a run of width `w` starts inside `(x, width)` with `pad` at the
/// reading edge: flush right in Arabic, flush left in English.
pub fn lead_x(x: i32, width: i32, pad: i32, w: i32) -> i32 {
    if rtl() {
        x + width - pad - w
    } else {
        x + pad
    }
}

/// The opposite edge from `lead_x`: where a row's secondary item (a
/// shortcut hint, keycaps, a control) sits.
pub fn trail_x(x: i32, width: i32, pad: i32, w: i32) -> i32 {
    if rtl() {
        x + pad
    } else {
        x + width - pad - w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_values_and_the_safe_default() {
        assert_eq!(Lang::from_config(Some("en")), Lang::En);
        assert_eq!(Lang::from_config(Some(" English ")), Lang::En);
        assert_eq!(Lang::from_config(Some("ar")), Lang::Ar);
        assert_eq!(Lang::from_config(Some("fr")), Lang::Ar);
        assert_eq!(Lang::from_config(None), Lang::Ar);
        assert_eq!(Lang::En.other(), Lang::Ar);
        assert_eq!(Lang::from_config(Some(Lang::En.id())), Lang::En);
    }

    #[test]
    fn tr_and_edges_follow_the_language_of_this_thread() {
        set(Lang::Ar);
        assert_eq!(tr("الإعدادات", "Settings"), "الإعدادات");
        assert_eq!(lead_x(100, 400, 24, 50), 100 + 400 - 24 - 50);
        assert_eq!(trail_x(100, 400, 24, 50), 124);
        set(Lang::En);
        assert_eq!(tr("الإعدادات", "Settings"), "Settings");
        assert_eq!(lead_x(100, 400, 24, 50), 124);
        assert_eq!(trail_x(100, 400, 24, 50), 100 + 400 - 24 - 50);
        assert!(!rtl());
        set(Lang::Ar);
    }
}
