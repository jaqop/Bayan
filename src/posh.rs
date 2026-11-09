//! oh-my-posh prompt themes: list, read the current one, apply, and render a
//! LIVE preview. Ported from EasyTer's `posh.py` + `prompt_picker.py`.
//!
//! Bayan has an advantage EasyTer did not. EasyTer runs on Qt, which cannot
//! display ANSI, so it converted `oh-my-posh print primary` output into HTML
//! to preview a theme. Bayan IS a terminal: the preview is ANSI bytes fed
//! through the same renderer that draws everything else, so what the picker
//! shows is exactly what the prompt will look like — not an approximation of
//! it in a different rendering engine.
//!
//! Applying only ever rewrites the ONE managed line in `$PROFILE`. Every other
//! line the user has written is left untouched.

use crate::process::run_with_timeout;

use std::path::PathBuf;

/// Themes worth showing first, in this order. The rest follow alphabetically.
/// EasyTer's `FEATURED`, kept because the ordering is a curation decision,
/// not an implementation detail.
const FEATURED: &[&str] = &[
    "kali",
    "jandedobbeleer",
    "paradox",
    "atomic",
    "powerlevel10k_rainbow",
    "clean-detailed",
    "montys",
    "agnoster",
    "robbyrussell",
    "pure",
    "night-owl",
];

/// Where the `.omp.json` files live. `POSH_THEMES_PATH` wins, as oh-my-posh
/// itself honours it — but only when it is a LOCAL directory (R-04): the
/// same `accept_local_cwd` + `usable_spawn_dir` gate as a shell's start
/// directory, so a UNC or mapped-network themes path is never listed (that
/// would open SMB and leak NTLM) and never has its files run. Otherwise
/// the default install location.
pub fn themes_dir() -> PathBuf {
    let env = std::env::var_os("POSH_THEMES_PATH").and_then(|p| p.into_string().ok());
    themes_dir_from(env.as_deref(), crate::term::usable_spawn_dir)
}

fn themes_dir_from(env: Option<&str>, usable: impl Fn(&str) -> bool) -> PathBuf {
    if let Some(dir) = env
        .and_then(crate::term::accept_local_cwd)
        .filter(|d| usable(d))
    {
        return PathBuf::from(dir);
    }
    match std::env::var_os("USERPROFILE") {
        Some(h) => PathBuf::from(h).join(".poshthemes"),
        None => PathBuf::from(".poshthemes"),
    }
}

/// R-05: the oh-my-posh binary by absolute path. A bare `oh-my-posh` lets
/// Windows look in Bayan's own folder first, so a planted `oh-my-posh.exe`
/// beside `bayan.exe` (Downloads, `target\release`) would run when the
/// picker opens. Its default install location wins, then PATH — skipping
/// relative entries, non-local directories and Bayan's own folder.
pub fn posh_exe() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").map(|l| {
        PathBuf::from(l)
            .join("Programs")
            .join("oh-my-posh")
            .join("bin")
    });
    let own = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()));
    let path: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    posh_exe_in(local, &path, own.as_deref(), |p| p.is_file())
}

fn posh_exe_in(
    local: Option<PathBuf>,
    path: &[PathBuf],
    own_dir: Option<&std::path::Path>,
    is_file: impl Fn(&std::path::Path) -> bool,
) -> Option<PathBuf> {
    let same = |a: &std::path::Path, b: &std::path::Path| {
        a.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches(['\\', '/']))
    };
    local
        .into_iter()
        .chain(path.iter().cloned())
        .filter(|d| d.is_absolute())
        .filter(|d| {
            d.to_str()
                .is_some_and(|s| crate::term::accept_local_cwd(s).is_some())
        })
        .filter(|d| !own_dir.is_some_and(|o| same(d, o)))
        .map(|d| d.join("oh-my-posh.exe"))
        .find(|p| is_file(p))
}

/// R-04: does a theme run commands when it renders? oh-my-posh's `command`
/// segment executes a shell command for its text, so previewing such a theme
/// would run it the moment the picker opened. Those themes are listed and
/// can still be chosen, but are not previewed. An unreadable or invalid file
/// counts as unsafe.
fn runs_commands(theme_json: &str) -> bool {
    fn walk(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Object(m) => {
                m.get("type")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t.eq_ignore_ascii_case("command"))
                    || m.values().any(walk)
            }
            serde_json::Value::Array(a) => a.iter().any(walk),
            _ => false,
        }
    }
    match serde_json::from_str::<serde_json::Value>(theme_json) {
        Ok(v) => walk(&v),
        Err(_) => true,
    }
}

/// Themes rendered as soon as the picker opens (R-04): the featured ones
/// that exist, and the selected one. The rest render when highlighted.
pub fn eager_previews(names: &[String], sel: usize) -> Vec<usize> {
    let mut out = vec![sel.min(names.len().saturating_sub(1))];
    for (i, n) in names.iter().enumerate() {
        if FEATURED.contains(&n.as_str()) && !out.contains(&i) {
            out.push(i);
        }
    }
    if names.is_empty() {
        out.clear();
    }
    out
}

/// The PowerShell 5 profile Bayan's default shell reads.
pub fn profile_path() -> PathBuf {
    match std::env::var_os("USERPROFILE") {
        Some(h) => PathBuf::from(h)
            .join("Documents")
            .join("WindowsPowerShell")
            .join("Microsoft.PowerShell_profile.ps1"),
        None => PathBuf::from("Microsoft.PowerShell_profile.ps1"),
    }
}

/// Theme names present on disk: featured ones first, then the rest sorted.
pub fn available() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(themes_dir()) else {
        return Vec::new(); // oh-my-posh not installed is not an error here
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|f| f.strip_suffix(".omp.json").map(str::to_string))
        .filter(|n| is_safe_theme_name(n))
        .collect();
    names.sort();
    order_featured_first(names)
}

/// BAYAN-005: a theme stem is spliced into the `--config "..."` line of
/// `$PROFILE`, inside PowerShell DOUBLE quotes (they must stay double: the
/// managed line keeps `$env:USERPROFILE` portable). Double quotes expand
/// `$x`, `$(...)` and backtick escapes, so a file NAME carrying them would
/// become code in every profile-loading PowerShell. Only `[A-Za-z0-9._-]`
/// is accepted — none of `$`, backtick, quotes, spaces or path separators
/// can be written that way — and a name may not start with `-` or `.`.
pub fn is_safe_theme_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && !n.starts_with(['-', '.'])
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Featured names (in FEATURED order) that exist, then everything else as
/// given. Split out so the ordering rule is testable without a filesystem.
fn order_featured_first(names: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = FEATURED
        .iter()
        .filter(|f| names.iter().any(|n| n == *f))
        .map(|f| f.to_string())
        .collect();
    out.extend(
        names
            .into_iter()
            .filter(|n| !FEATURED.contains(&n.as_str())),
    );
    out
}

/// The theme name in the managed `$PROFILE` line, if there is one.
pub fn current() -> Option<String> {
    let txt = std::fs::read_to_string(profile_path()).ok()?;
    current_from(&txt)
}

/// Pull the theme name out of profile text. Separated from I/O so the parsing
/// — the part that can be wrong — is testable.
fn current_from(txt: &str) -> Option<String> {
    let i = txt.find("oh-my-posh init pwsh --config")?;
    let rest = &txt[i..];
    let a = rest.find('"')? + 1;
    let b = rest[a..].find('"')? + a;
    let path = &rest[a..b];
    let file = path.rsplit(['\\', '/']).next()?;
    file.strip_suffix(".omp.json").map(str::to_string)
}

/// Rewrite the managed line's `--config` path, leaving every other line alone.
/// Returns the new text, or None if there is no managed line to update.
fn rewrite_config(txt: &str, theme: &str) -> Option<String> {
    if !is_safe_theme_name(theme) {
        return None;
    }
    let i = txt.find("oh-my-posh init pwsh --config")?;
    let rest = &txt[i..];
    let a = rest.find('"')? + 1;
    let b = rest[a..].find('"')? + a;
    let old = &rest[a..b];
    // keep whatever directory form the user already had, swap only the file —
    // a profile written with $env:USERPROFILE must stay portable
    let dir_end = old.rfind(['\\', '/']).map(|p| p + 1).unwrap_or(0);
    let new_path = format!("{}{}.omp.json", &old[..dir_end], theme);
    let mut out = String::with_capacity(txt.len() + 16);
    out.push_str(&txt[..i + a]);
    out.push_str(&new_path);
    out.push_str(&txt[i + b..]);
    Some(out)
}

/// Point `$PROFILE`'s managed line at `theme`. New shells pick it up.
pub fn set_theme(theme: &str) -> Result<(), String> {
    if !is_safe_theme_name(theme) {
        return Err(format!(
            "refusing theme name {theme:?}: only A-Z a-z 0-9 . _ -"
        ));
    }
    let p = profile_path();
    let txt = std::fs::read_to_string(&p).map_err(|e| format!("read profile: {e}"))?;
    let new = rewrite_config(&txt, theme).ok_or("no managed oh-my-posh line in the profile")?;
    std::fs::write(&p, new).map_err(|e| format!("write profile: {e}"))
}

/// Render a theme the way it will actually appear: ask oh-my-posh for the
/// primary prompt and hand back its raw ANSI. None when the binary is absent,
/// which the picker shows as a plain name rather than a fake preview.
pub fn preview(theme: &str) -> Option<Vec<u8>> {
    if !is_safe_theme_name(theme) {
        return None;
    }
    let cfg = themes_dir().join(format!("{theme}.omp.json"));
    if !cfg.is_file() {
        return None;
    }
    let json = std::fs::read_to_string(&cfg).ok()?;
    if runs_commands(&json) {
        return None; // R-04: shown by name only, never executed by a preview
    }
    let exe = posh_exe()?; // R-05: never a bare name
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["print", "primary", "--shell", "pwsh", "--config"])
        .arg(&cfg);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = run_with_timeout(cmd, PREVIEW_TIMEOUT)?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(out.stdout)
}

/// A preview that takes longer than this is not worth freezing the window
/// for: the picker shows the name alone instead (the same as "no binary").
const PREVIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = concat!(
        "chcp 65001 > $null\n",
        "# --- EasyTer: Oh My Posh prompt ---\n",
        "if (Get-Command oh-my-posh -ErrorAction SilentlyContinue) {\n",
        "  oh-my-posh init pwsh --config \"$env:USERPROFILE\\.poshthemes\\night-owl.omp.json\" | Invoke-Expression\n",
        "}\n",
        "# a line the user wrote and we must never touch\n",
    );

    #[test]
    fn the_current_theme_is_read_from_the_managed_line() {
        assert_eq!(current_from(PROFILE).as_deref(), Some("night-owl"));
        assert_eq!(current_from("no posh here").as_deref(), None);
        // a forward-slash path is just as valid
        let fwd = "oh-my-posh init pwsh --config \"C:/themes/pure.omp.json\" | Invoke-Expression";
        assert_eq!(current_from(fwd).as_deref(), Some("pure"));
    }

    #[test]
    fn applying_touches_only_the_managed_line() {
        let out = rewrite_config(PROFILE, "kali").unwrap();
        assert_eq!(current_from(&out).as_deref(), Some("kali"));
        // every other line survives byte for byte — this is the whole promise
        assert!(out.contains("chcp 65001 > $null"));
        assert!(out.contains("a line the user wrote and we must never touch"));
        assert!(out.contains("if (Get-Command oh-my-posh"));
        assert_eq!(out.lines().count(), PROFILE.lines().count());
        // the directory form is preserved, so a portable profile stays portable
        assert!(
            out.contains("$env:USERPROFILE"),
            "absolute path was injected: {out}"
        );
    }

    #[test]
    fn theme_names_that_powershell_would_expand_are_refused() {
        // BAYAN-005: nothing that double-quoted PowerShell expands survives
        for bad in [
            "x$(calc)",
            "a$env:PATH",
            "tick`n",
            "quote\"",
            "single'q",
            "sp ace",
            "back\\slash",
            "fwd/slash",
            "-flag",
            ".hidden",
            "",
            "\u{202E}rlo",
        ] {
            assert!(!is_safe_theme_name(bad), "{bad:?} must be refused");
            assert_eq!(
                rewrite_config(PROFILE, bad),
                None,
                "{bad:?} reached the profile"
            );
        }
        for good in [
            "night-owl",
            "powerlevel10k_rainbow",
            "1_shell",
            "M365Princess",
            "the-unnamed.v2",
        ] {
            assert!(is_safe_theme_name(good), "{good:?} is a real theme name");
        }
        assert!(set_theme("x$(calc)").is_err());
        assert_eq!(preview("x$(calc)"), None);
    }

    #[test]
    fn a_profile_without_the_managed_line_is_refused_not_mangled() {
        assert!(rewrite_config("# nothing here\n", "kali").is_none());
    }

    #[test]
    fn featured_themes_lead_and_the_rest_follow_sorted() {
        let names = vec![
            "aaa".to_string(),
            "kali".to_string(),
            "zzz".to_string(),
            "pure".to_string(),
            "atomic".to_string(),
        ];
        let out = order_featured_first(names);
        // FEATURED order, not alphabetical, for the ones that are featured
        assert_eq!(
            &out[..3],
            &["kali".to_string(), "atomic".to_string(), "pure".to_string()]
        );
        // then the others, in the order given (already sorted by the caller)
        assert_eq!(&out[3..], &["aaa".to_string(), "zzz".to_string()]);
        // a featured name that is not installed must not appear
        assert!(!out.contains(&"paradox".to_string()));
    }

    /// A sleeping child must time out even when the network is unavailable.
    #[test]
    fn a_hung_preview_is_killed_at_the_deadline() {
        let mut slow = std::process::Command::new("powershell.exe");
        slow.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 30",
        ]);
        let t0 = std::time::Instant::now();
        let out = run_with_timeout(slow, std::time::Duration::from_millis(400));
        assert!(out.is_none(), "a hung child must yield None");
        assert!(t0.elapsed() >= std::time::Duration::from_millis(400));
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "took {:?}",
            t0.elapsed()
        );
        // and a quick child still returns its output
        let mut quick = std::process::Command::new("cmd");
        quick.args(["/c", "echo", "hi"]);
        let out = run_with_timeout(quick, std::time::Duration::from_secs(5)).expect("output");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    /// The deadline must cover the DRAIN too. oh-my-posh shells out, so a
    /// grandchild can outlive it still holding the inherited stdout pipe —
    /// and read_to_end waits for every writer. Joining that unconditionally
    /// hung the UI thread for as long as the grandchild lived.
    #[test]
    fn a_grandchild_holding_the_pipe_cannot_outlast_the_deadline() {
        // cmd exits at once; the detached ping inherits stdout and holds it
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/c", "start /b ping -n 20 127.0.0.1"]);
        let t0 = std::time::Instant::now();
        let _ = run_with_timeout(cmd, std::time::Duration::from_millis(300));
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "drain outlived the deadline: {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn themes_path_must_be_local() {
        // R-04: UNC, device and verbatim themes paths fall back to the default
        let yes = |_: &str| true;
        let default = themes_dir_from(None, yes);
        for bad in [r"\\server\themes", r"\\?\C:\t", r"\\.\x"] {
            assert_eq!(themes_dir_from(Some(bad), yes), default, "{bad}");
        }
        // a local dir that the probe refuses (remote drive, reparse point)
        assert_eq!(themes_dir_from(Some(r"Z:\t"), |_| false), default);
        assert_eq!(
            themes_dir_from(Some(r"D:\themes"), yes),
            PathBuf::from(r"D:\themes")
        );
    }

    #[test]
    fn oh_my_posh_is_resolved_to_an_absolute_path_outside_bayans_folder() {
        // R-05
        let own = PathBuf::from(r"C:\Users\a\Downloads");
        let path = vec![
            PathBuf::from("."),
            PathBuf::from(r"relative\bin"),
            own.clone(),
            PathBuf::from(r"\\server\tools"),
            PathBuf::from(r"C:\Tools\posh"),
        ];
        let everywhere = |_: &std::path::Path| true;
        assert_eq!(
            posh_exe_in(None, &path, Some(&own), everywhere),
            Some(PathBuf::from(r"C:\Tools\posh\oh-my-posh.exe"))
        );
        // the install location wins over PATH
        let local = PathBuf::from(r"C:\Users\a\AppData\Local\Programs\oh-my-posh\bin");
        assert_eq!(
            posh_exe_in(Some(local.clone()), &path, Some(&own), everywhere),
            Some(local.join("oh-my-posh.exe"))
        );
        // nothing found: no preview, not a bare-name spawn
        assert_eq!(posh_exe_in(None, &path, Some(&own), |_| false), None);
    }

    #[test]
    fn themes_that_run_commands_are_not_previewed() {
        // R-04
        let cmd = r#"{"blocks":[{"segments":[{"type":"path"},{"type":"command","properties":{"command":"x"}}]}]}"#;
        assert!(runs_commands(cmd));
        assert!(runs_commands(
            r#"{"blocks":[{"segments":[{"type":"Command"}]}]}"#
        ));
        assert!(!runs_commands(
            r#"{"blocks":[{"segments":[{"type":"path"},{"type":"git"}]}]}"#
        ));
        assert!(runs_commands("not json"), "unreadable counts as unsafe");
    }

    #[test]
    fn only_featured_and_selected_themes_render_on_open() {
        // R-04
        let names: Vec<String> = ["aaa", "kali", "zzz", "pure", "mmm"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(eager_previews(&names, 4), vec![4, 1, 3]);
        assert_eq!(eager_previews(&names, 1), vec![1, 3]);
        assert!(eager_previews(&[], 0).is_empty());
    }

    #[test]
    fn a_missing_themes_directory_yields_no_themes() {
        // guards the "oh-my-posh not installed" path: empty, never a panic
        assert!(order_featured_first(Vec::new()).is_empty());
    }
}
