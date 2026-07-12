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
        .and_then(|s| serde_json::from_str(&s).ok())
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
