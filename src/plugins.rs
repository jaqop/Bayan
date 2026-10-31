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

/// Section heading above plugin rows in the command palette.
pub const PALETTE_SECTION: &str = "إضافات";

/// Badge on every plugin palette row. Built-in rows do not carry it, so a
/// plugin cannot wear the same face as «لصق».
pub const PALETTE_BADGE: &str = "«إضافة»";

/// The label drawn for a plugin row: the badge, then the plugin's own words.
pub fn palette_label(label: &str) -> String {
    format!("{PALETTE_BADGE} {label}")
}

/// A command-palette entry: a label, and text sent to the active shell.
///
/// `send` is capped at [`crate::term::PASTE_GUARD_MAX_CHARS`] and is refused
/// when it contains a line break or any other character the paste guard
/// would stop for. The palette still arms whatever was accepted through that
/// guard. The label is refused when it copies a built-in command.
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

/// Tests feed `load_from` broken plugins on purpose; their failures must not
/// land in the user's real log.
fn log_path() -> PathBuf {
    if cfg!(test) {
        std::env::temp_dir().join("bayan-test-plugins.log")
    } else {
        crate::config::bayan_dir().join("plugins.log")
    }
}

/// Append a line to the plugin log. Defensive to the point of silence: a
/// logging failure must never become the user's problem.
fn log(msg: &str) {
    use std::io::Write;
    let p = log_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
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
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "brightblack",
    "brightred",
    "brightgreen",
    "brightyellow",
    "brightblue",
    "brightmagenta",
    "brightcyan",
    "brightwhite",
];

/// Validate one theme object. EasyTer's `_validate_theme` requires string
/// bg/fg and an ansi dict; this additionally checks every colour parses,
/// because a half-valid palette would render as silent black.
fn parse_theme(v: &serde_json::Value) -> Result<PluginTheme, String> {
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .ok_or("theme needs a string 'name'")?;
    let bg = v
        .get("bg")
        .and_then(|x| x.as_str())
        .ok_or("theme needs a string 'bg'")?;
    let fg = v
        .get("fg")
        .and_then(|x| x.as_str())
        .ok_or("theme needs a string 'fg'")?;
    let bg = parse_hex(bg).ok_or_else(|| format!("bad bg colour {bg:?}"))?;
    let fg = parse_hex(fg).ok_or_else(|| format!("bad fg colour {fg:?}"))?;
    let ansi = v
        .get("ansi")
        .and_then(|x| x.as_object())
        .ok_or("theme needs an 'ansi' object")?;
    let mut palette = [(0u8, 0u8, 0u8); 16];
    for (i, key) in ANSI_KEYS.iter().enumerate() {
        // a missing slot inherits fg rather than rendering invisible black
        palette[i] = match ansi.get(*key).and_then(|x| x.as_str()) {
            Some(s) => parse_hex(s).ok_or_else(|| format!("bad {key} colour {s:?}"))?,
            None => fg,
        };
    }
    Ok(PluginTheme {
        name: name.to_string(),
        bg,
        fg,
        palette,
    })
}

fn parse_action(v: &serde_json::Value) -> Result<PaletteAction, String> {
    let label = v
        .get("label")
        .and_then(|x| x.as_str())
        .ok_or("action needs a string 'label'")?;
    let send = v
        .get("send")
        .and_then(|x| x.as_str())
        .ok_or("action needs a string 'send'")?;
    let label = label.trim();
    if label.is_empty() {
        return Err("action needs a non-empty 'label'".into());
    }
    // A newline or a bidi override in the label would paint outside the row
    // or reorder the badge. The paste guard's set is the one the rest of
    // the UI already treats as unsafe to draw raw.
    if crate::term::needs_paste_guard(label).is_some() {
        return Err("label contains a line break or a control character".into());
    }
    if label_collides_with_builtin(label) {
        return Err(format!(
            "label collides with a built-in command: {}",
            label_match_key(label)
        ));
    }
    validate_send(send)?;
    Ok(PaletteAction {
        label: label.to_string(),
        send: send.to_string(),
    })
}

/// Built-in palette commands (`لصق`, `تبويب جديد`, …). Compared after trim
/// and after dropping LRM / RLM / ALM, which are not controls: `لصق` plus
/// U+200F still reads as لصق.
fn label_collides_with_builtin(label: &str) -> bool {
    let key = label_match_key(label);
    crate::keybinds::Action::ALL
        .iter()
        .any(|action| label_match_key(action.label()) == key)
}

fn label_match_key(label: &str) -> String {
    label
        .trim()
        .chars()
        .filter(|c| !matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}'))
        .collect()
}

/// Refuse a payload the paste guard would have to stop: a line break (CR is
/// Enter on ConPTY), any other control except tab, a bidi / zero-width
/// spoof, or more than [`crate::term::PASTE_GUARD_MAX_CHARS`] scalars.
fn validate_send(send: &str) -> Result<(), String> {
    let n = send.chars().count();
    let cap = crate::term::PASTE_GUARD_MAX_CHARS;
    if n > cap {
        return Err(format!("send is {n} characters; the limit is {cap}"));
    }
    if crate::term::needs_paste_guard(send).is_some() {
        return Err("send contains a line break or a control character".into());
    }
    Ok(())
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
        .filter(|p| {
            p.extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("json"))
        })
        .collect();
    files.sort(); // deterministic order, so a duplicate name resolves the same way twice

    for path in files {
        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
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

    #[test]
    fn test_runs_never_write_the_users_plugin_log() {
        assert!(!log_path().starts_with(crate::config::bayan_dir()));
    }

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
        assert!(
            err.contains("bad bg"),
            "expected a colour error, got: {err}"
        );
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
        assert_eq!(
            reg.loaded,
            vec!["a_good", "c_good"],
            "a broken plugin is skipped, not fatal"
        );
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
        let src = "{\"actions\":[{\"label\":\"  بناء  \",\"send\":\"cargo build\"}]}";
        let st = parse_plugin(src).unwrap();
        assert_eq!(st.actions[0].label, "بناء");
        assert_eq!(st.actions[0].send, "cargo build");
        assert_eq!(palette_label(&st.actions[0].label), "«إضافة» بناء");
    }

    #[test]
    fn send_rejects_line_breaks_controls_and_overlong_text() {
        let cr = "{\"actions\":[{\"label\":\"Build\",\"send\":\"cargo build\\r\"}]}";
        assert!(
            parse_plugin(cr)
                .unwrap_err()
                .contains("line break or a control"),
            "a trailing CR would submit on ConPTY"
        );
        let lf = "{\"actions\":[{\"label\":\"Build\",\"send\":\"echo a\\necho b\"}]}";
        assert!(parse_plugin(lf).is_err());
        let esc = "{\"actions\":[{\"label\":\"Build\",\"send\":\"echo \\u001b[2J\"}]}";
        assert!(parse_plugin(esc).is_err(), "ESC must not reach the PTY");
        let line_sep = "{\"actions\":[{\"label\":\"Build\",\"send\":\"a\\u2028b\"}]}";
        assert!(parse_plugin(line_sep).is_err());
        let rlo = "{\"actions\":[{\"label\":\"Build\",\"send\":\"echo \\u202ehi\"}]}";
        assert!(parse_plugin(rlo).is_err());
        let del = "{\"actions\":[{\"label\":\"Build\",\"send\":\"del\\u007fx\"}]}";
        assert!(parse_plugin(del).is_err());

        let cap = crate::term::PASTE_GUARD_MAX_CHARS;
        let at_cap = "م".repeat(cap);
        let ok = format!("{{\"actions\":[{{\"label\":\"بناء\",\"send\":\"{at_cap}\"}}]}}");
        let st = parse_plugin(&ok).unwrap();
        assert_eq!(st.actions[0].send.chars().count(), cap);
        // Arabic is two UTF-8 bytes; the cap counts scalars, and one more is refused.
        let over = "م".repeat(cap + 1);
        let too_long = format!("{{\"actions\":[{{\"label\":\"بناء\",\"send\":\"{over}\"}}]}}");
        let err = parse_plugin(&too_long).unwrap_err();
        assert!(
            err.contains(&cap.to_string()),
            "expected the character cap in: {err}"
        );

        // Tab is the one control the paste guard lets through.
        let tab = "{\"actions\":[{\"label\":\"بناء\",\"send\":\"echo\\thi\"}]}";
        assert_eq!(parse_plugin(tab).unwrap().actions[0].send, "echo\thi");
    }

    #[test]
    fn a_label_that_copies_a_builtin_rejects_the_whole_plugin() {
        let src = concat!(
            "{\"themes\":[{\"name\":\"Good\",\"bg\":\"#101010\",\"fg\":\"#ffffff\",\"ansi\":{}}],",
            "\"actions\":[{\"label\":\"لصق\",\"send\":\"echo hi\"}]}"
        );
        let err = parse_plugin(src).unwrap_err();
        assert!(err.contains("collides"), "{err}");
        assert!(err.contains("لصق"), "{err}");

        let padded = "{\"actions\":[{\"label\":\"  لصق  \",\"send\":\"echo hi\"}]}";
        assert!(parse_plugin(padded).is_err(), "trim still collides");
        // RLM is not a control, and it does not change what the row reads as.
        let marked = "{\"actions\":[{\"label\":\"لصق\\u200f\",\"send\":\"echo hi\"}]}";
        assert!(
            parse_plugin(marked).unwrap_err().contains("collides"),
            "a directional mark must not hide a built-in label"
        );
        let distinct = "{\"actions\":[{\"label\":\"لصق الملف\",\"send\":\"echo hi\"}]}";
        assert!(parse_plugin(distinct).is_ok());
        let broken_row = "{\"actions\":[{\"label\":\"بناء\\nلصق\",\"send\":\"echo hi\"}]}";
        assert!(parse_plugin(broken_row).is_err());
    }
}
