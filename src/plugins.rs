//! Plugin host — ported from EasyTer's `pluginhost.py`, principles first.
//!
//! EasyTer loads Python modules; Rust is compiled, so a faithful port of the
//! MECHANISM would need an embedded script runtime or unsafe dynamic
//! libraries. Both fight this project's rule that dependencies stay few and
//! deliberate, and a `.dll` plugin can crash the host outright, which breaks
//! the very first principle below. So the HOST is ported and the first
//! backend is declarative: JSON files in `~/.bayan/plugins/`.
//!
//! The four principles that make EasyTer's host worth copying:
//!
//! 1. **Total isolation.** A bad plugin never takes the app down. Unreadable
//!    file, malformed JSON, invalid theme — caught, logged, skipped.
//! 2. **Staging with clean rollback.** A plugin's registrations are buffered
//!    and committed only if the WHOLE plugin validates. Half a plugin is
//!    never installed.
//! 3. **Read at call time.** Consumers query the registry when they need it,
//!    so load order does not matter and disabling can be live.
//! 4. **Failures are logged, not printed.** `~/.bayan/plugins.log`, appended,
//!    and the logger itself never fails loudly.

use std::path::{Path, PathBuf};

/// A theme contributed by a plugin. Owned, unlike the built-in `&'static`
/// table, because these are read from disk at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTheme {
    pub name: String,
    pub bg: (u8, u8, u8),
    pub fg: (u8, u8, u8),
    pub palette: [(u8, u8, u8); 16],
}

/// A command-palette entry: a label, and text sent to the active shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteAction {
    pub label: String,
    pub send: String,
}

/// Everything plugins have contributed. Consumers read this at call time.
#[derive(Debug, Default)]
pub struct Registry {
    pub themes: Vec<PluginTheme>,
    pub actions: Vec<PaletteAction>,
    /// plugin names that loaded cleanly
    pub loaded: Vec<String>,
    /// (name, why) for the ones that did not — surfaced in settings, not fatal
    pub failed: Vec<(String, String)>,
}

/// Per-plugin staging area. Nothing reaches the `Registry` until the whole
/// plugin parses, so a plugin with three good themes and one broken one
/// installs none of them — EasyTer's clean-rollback rule.
#[derive(Debug, Default)]
struct Staged {
    themes: Vec<PluginTheme>,
    actions: Vec<PaletteAction>,
}

fn log_path() -> PathBuf {
    crate::config::bayan_dir().join("plugins.log")
}

/// Append a line to the plugin log. Defensive to the point of silence: a
/// logging failure must never become the user's problem.
fn log(msg: &str) {
    use std::io::Write;
    let p = log_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{}", msg.trim_end());
    }
}

/// `#rrggbb` (or `rrggbb`) to a triple. None on anything else — the caller
/// turns that into a rejected plugin rather than a panic.
fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.strip_prefix('#').unwrap_or(s);
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// The 16 ANSI slots, in the order the built-in table uses.
const ANSI_KEYS: [&str; 16] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    "brightblack", "brightred", "brightgreen", "brightyellow",
    "brightblue", "brightmagenta", "brightcyan", "brightwhite",
];

/// Validate one theme object. EasyTer's `_validate_theme` requires string
/// bg/fg and an ansi dict; this additionally checks every colour parses,
/// because a half-valid palette would render as silent black.
fn parse_theme(v: &serde_json::Value) -> Result<PluginTheme, String> {
    let name = v.get("name").and_then(|x| x.as_str()).ok_or("theme needs a string 'name'")?;
    let bg = v.get("bg").and_then(|x| x.as_str()).ok_or("theme needs a string 'bg'")?;
    let fg = v.get("fg").and_then(|x| x.as_str()).ok_or("theme needs a string 'fg'")?;
    let bg = parse_hex(bg).ok_or_else(|| format!("bad bg colour {bg:?}"))?;
    let fg = parse_hex(fg).ok_or_else(|| format!("bad fg colour {fg:?}"))?;
    let ansi = v.get("ansi").and_then(|x| x.as_object()).ok_or("theme needs an 'ansi' object")?;
    let mut palette = [(0u8, 0u8, 0u8); 16];
    for (i, key) in ANSI_KEYS.iter().enumerate() {
        // a missing slot inherits fg rather than rendering invisible black
        palette[i] = match ansi.get(*key).and_then(|x| x.as_str()) {
            Some(s) => parse_hex(s).ok_or_else(|| format!("bad {key} colour {s:?}"))?,
            None => fg,
        };
    }
    Ok(PluginTheme { name: name.to_string(), bg, fg, palette })
}

fn parse_action(v: &serde_json::Value) -> Result<PaletteAction, String> {
    let label = v.get("label").and_then(|x| x.as_str()).ok_or("action needs a string 'label'")?;
    let send = v.get("send").and_then(|x| x.as_str()).ok_or("action needs a string 'send'")?;
    Ok(PaletteAction { label: label.to_string(), send: send.to_string() })
}

/// Parse one plugin file into a staging area. Any error rejects the WHOLE
/// file — principle 2.
fn parse_plugin(text: &str) -> Result<Staged, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let mut st = Staged::default();
    if let Some(arr) = v.get("themes") {
        let arr = arr.as_array().ok_or("'themes' must be an array")?;
        for t in arr {
            st.themes.push(parse_theme(t)?);
        }
    }
    if let Some(arr) = v.get("actions") {
        let arr = arr.as_array().ok_or("'actions' must be an array")?;
        for a in arr {
            st.actions.push(parse_action(a)?);
        }
    }
    if st.themes.is_empty() && st.actions.is_empty() {
        return Err("plugin contributes nothing".into());
    }
    Ok(st)
}

/// Load every `*.json` in `dir`, skipping `disabled` names and anything
/// starting with `_` (EasyTer's convention for "not a plugin").
///
/// Never fails: a missing directory yields an empty registry.
pub fn load_from(dir: &Path, disabled: &[String]) -> Registry {
    let mut reg = Registry::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return reg; // no plugins directory is the normal case, not an error
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("json")))
        .collect();
    files.sort(); // deterministic order, so a duplicate name resolves the same way twice

    for path in files {
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        if name.starts_with('_') || disabled.iter().any(|d| d == &name) {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                let why = format!("unreadable: {e}");
                log(&format!("{name}: {why}"));
                reg.failed.push((name, why));
                continue;
            }
        };
        match parse_plugin(&text) {
            Ok(st) => {
                // commit: only now does anything reach the shared registry
                reg.themes.extend(st.themes);
                reg.actions.extend(st.actions);
                reg.loaded.push(name);
            }
            Err(why) => {
                log(&format!("{name}: {why}"));
                reg.failed.push((name, why));
            }
        }
    }
    reg
}

/// The live registry. Read at call time (principle 3) rather than captured.
static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();

/// Load plugins once, from `~/.bayan/plugins/`.
pub fn init(disabled: &[String]) -> &'static Registry {
    REGISTRY.get_or_init(|| load_from(&crate::config::bayan_dir().join("plugins"), disabled))
}

/// Plugins loaded so far, or an empty registry if `init` has not run.
pub fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Registry::default)
}

/// A plugin theme by name — consulted after the built-in table.
pub fn theme_by_name(name: &str) -> Option<&'static PluginTheme> {
    registry().themes.iter().find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Built with explicit escapes rather than raw strings: a hex colour's
    /// `#` collides with the `r#"..."#` delimiter and the noise hides bugs.
    fn theme_json(name: &str, bg: &str) -> String {
        format!(
            "{{\"themes\":[{{\"name\":\"{name}\",\"bg\":\"{bg}\",\"fg\":\"#ffffff\",             \"ansi\":{{\"red\":\"#ff0000\",\"green\":\"#00ff00\"}}}}]}}"
        )
    }

    #[test]
    fn a_valid_plugin_contributes_its_theme() {
        let st = parse_plugin(&theme_json("Mine", "#101010")).unwrap();
        assert_eq!(st.themes.len(), 1);
        let t = &st.themes[0];
        assert_eq!(t.name, "Mine");
        assert_eq!(t.bg, (0x10, 0x10, 0x10));
        assert_eq!(t.palette[1], (0xff, 0, 0), "red slot");
        assert_eq!(t.palette[2], (0, 0xff, 0), "green slot");
        // an unspecified slot inherits fg rather than rendering invisible black
        assert_eq!(t.palette[4], t.fg, "missing blue falls back to fg");
    }

    #[test]
    fn one_bad_entry_rejects_the_whole_plugin() {
        // EasyTer's clean-rollback rule: staging is discarded wholesale, so a
        // plugin can never be half-installed
        let merged = concat!(
            "{\"themes\":[",
            "{\"name\":\"Good\",\"bg\":\"#101010\",\"fg\":\"#ffffff\",\"ansi\":{}},",
            "{\"name\":\"Bad\",\"bg\":\"not-a-colour\",\"fg\":\"#ffffff\",\"ansi\":{}}",
            "]}"
        );
        let err = parse_plugin(merged).unwrap_err();
        assert!(err.contains("bad bg"), "expected a colour error, got: {err}");
    }

    #[test]
    fn malformed_input_is_rejected_without_panicking() {
        let three_digit = theme_json("x", "#fff");
        let cases: Vec<String> = vec![
            String::new(),
            "{".to_string(),
            "[]".to_string(),
            "{\"themes\":\"not an array\"}".to_string(),
            "{\"themes\":[{\"name\":\"x\"}]}".to_string(),
            three_digit,
            "{\"actions\":[{\"label\":\"no send\"}]}".to_string(),
            "{}".to_string(),
        ];
        for src in cases {
            assert!(parse_plugin(&src).is_err(), "should reject: {src:?}");
        }
    }

    #[test]
    fn hex_parsing_is_strict() {
        assert_eq!(parse_hex("#062626"), Some((6, 38, 38)));
        assert_eq!(parse_hex("062626"), Some((6, 38, 38)));
        assert_eq!(parse_hex("#FFF"), None, "3-digit shorthand is not accepted");
        assert_eq!(parse_hex("#gggggg"), None);
        assert_eq!(parse_hex(""), None);
        assert_eq!(parse_hex("#0626266"), None);
    }

    #[test]
    fn a_broken_plugin_does_not_stop_the_others() {
        let dir = std::env::temp_dir().join("bayan_plugin_test_isolation");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a_good.json"), theme_json("Alpha", "#111111")).unwrap();
        std::fs::write(dir.join("b_broken.json"), "{ not json").unwrap();
        std::fs::write(dir.join("c_good.json"), theme_json("Gamma", "#222222")).unwrap();
        std::fs::write(dir.join("_skipped.json"), theme_json("Nope", "#333333")).unwrap();

        let reg = load_from(&dir, &[]);
        assert_eq!(reg.loaded, vec!["a_good", "c_good"], "a broken plugin is skipped, not fatal");
        assert_eq!(reg.failed.len(), 1);
        assert_eq!(reg.failed[0].0, "b_broken");
        assert_eq!(reg.themes.len(), 2, "the underscore file is ignored");
        assert!(reg.themes.iter().all(|t| t.name != "Nope"));

        // disabling is by name and skips silently — it is not a failure
        let reg = load_from(&dir, &["a_good".to_string()]);
        assert_eq!(reg.loaded, vec!["c_good"]);
        assert!(reg.failed.iter().all(|(n, _)| n != "a_good"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let reg = load_from(Path::new("Z:/definitely/not/here"), &[]);
        assert!(reg.loaded.is_empty() && reg.failed.is_empty() && reg.themes.is_empty());
    }

    #[test]
    fn actions_carry_label_and_payload() {
        let src = "{\"actions\":[{\"label\":\"Build\",\"send\":\"cargo build\\r\"}]}";
        let st = parse_plugin(src).unwrap();
        assert_eq!(st.actions[0].label, "Build");
        assert_eq!(st.actions[0].send, "cargo build\r");
    }
}
