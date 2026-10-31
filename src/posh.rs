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
//! line the user has written is left untouched. The theme stem has to match
//! `[A-Za-z0-9._-]`: PowerShell expands `$` and `$(...)` inside the double
//! quotes that line used to carry, so a theme filename would otherwise become
//! code on every profile load. The path is written with
//! `term::powershell_escape_single`, the same literal escape spawn already uses.

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
/// itself honours it; otherwise the default install location.
pub fn themes_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("POSH_THEMES_PATH") {
        return PathBuf::from(p);
    }
    match std::env::var_os("USERPROFILE") {
        Some(h) => PathBuf::from(h).join(".poshthemes"),
        None => PathBuf::from(".poshthemes"),
    }
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

/// Theme stems that may be written into `$PROFILE`. Anything outside
/// `[A-Za-z0-9._-]` is rejected, including empty, so `$`, `(`, quotes, and
/// backticks never reach the profile. The picker uses the same rule.
fn theme_stem_allowed(theme: &str) -> bool {
    !theme.is_empty()
        && theme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `kali.omp.json` → `kali`, when the stem is allowlisted. Other names are
/// hidden: the picker must not offer a file `set_theme` would refuse.
fn theme_from_filename(file: &str) -> Option<String> {
    let stem = file.strip_suffix(".omp.json")?;
    theme_stem_allowed(stem).then(|| stem.to_string())
}

/// Theme names present on disk: featured ones first, then the rest sorted.
/// Stems outside `[A-Za-z0-9._-]` are omitted.
pub fn available() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(themes_dir()) else {
        return Vec::new(); // oh-my-posh not installed is not an error here
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|f| theme_from_filename(&f))
        .collect();
    names.sort();
    order_featured_first(names)
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

/// The marker of the one line `rewrite_config` is allowed to change.
const MANAGED_CONFIG: &str = "oh-my-posh init pwsh --config";

/// Pull the theme name out of profile text. Separated from I/O so the parsing
/// — the part that can be wrong — is testable.
fn current_from(txt: &str) -> Option<String> {
    let (_at, arg) = managed_arg(txt)?;
    let (path, _) = parse_config_argument(arg)?;
    let file = path.rsplit(['\\', '/']).next()?;
    file.strip_suffix(".omp.json").map(str::to_string)
}

/// Byte offset of the `--config` argument, and the text starting there.
fn managed_arg(txt: &str) -> Option<(usize, &str)> {
    let i = txt.find(MANAGED_CONFIG)?;
    let rest = &txt[i + MANAGED_CONFIG.len()..];
    let at = i + MANAGED_CONFIG.len() + leading_ws(rest);
    Some((at, &txt[at..]))
}

fn leading_ws(s: &str) -> usize {
    s.len() - s.trim_start().len()
}

/// One `--config` argument: a double-quoted path, a single-quoted path
/// (doubled quotes unescaped), or `($env:NAME + '\tail')` / `($HOME + '\tail')`.
/// The path is the logical text, with `$env:USERPROFILE` still unexpanded.
/// The `usize` is how many bytes of `s` the argument occupied.
pub(crate) fn parse_config_argument(s: &str) -> Option<(String, usize)> {
    if s.starts_with('"') {
        let rest = &s[1..];
        let end = rest.find('"')?;
        Some((rest[..end].to_string(), end + 2))
    } else if s.starts_with('\'') {
        parse_ps_single_quoted(s)
    } else if s.starts_with('(') {
        parse_ps_concat_arg(s)
    } else {
        None
    }
}

/// `'...'` with PowerShell's doubled single quotes (ASCII and smart) collapsed.
fn parse_ps_single_quoted(s: &str) -> Option<(String, usize)> {
    let mut chars = s.char_indices().peekable();
    let (_, open) = chars.next()?;
    if open != '\'' {
        return None;
    }
    let mut path = String::new();
    while let Some((i, c)) = chars.next() {
        if crate::term::is_powershell_single_quote(c) {
            if chars.peek().is_some_and(|(_, n)| *n == c) {
                path.push(c);
                chars.next();
                continue;
            }
            if c == '\'' {
                return Some((path, i + c.len_utf8()));
            }
        }
        path.push(c);
    }
    None
}

/// `($env:NAME + '\tail')` → logical path `$env:NAME\tail`.
fn parse_ps_concat_arg(s: &str) -> Option<(String, usize)> {
    if !s.starts_with('(') {
        return None;
    }
    let mut i = 1;
    i += leading_ws(&s[i..]);
    let (var, var_len) = split_env_token(&s[i..])?;
    i += var_len;
    i += leading_ws(&s[i..]);
    if !s[i..].starts_with('+') {
        return None;
    }
    i += 1;
    i += leading_ws(&s[i..]);
    let (literal, lit_len) = parse_ps_single_quoted(&s[i..])?;
    i += lit_len;
    i += leading_ws(&s[i..]);
    if !s[i..].starts_with(')') {
        return None;
    }
    i += 1;
    Some((format!("{var}{literal}"), i))
}

/// `$env:Identifier` or `$HOME` at the start of `s`. Not `$(...)` and not
/// `${...}`: those stay inside a single-quoted literal, where they cannot run.
fn split_env_token(s: &str) -> Option<(&str, usize)> {
    if let Some(rest) = s.strip_prefix("$env:") {
        let name_len = env_name_len(rest)?;
        let n = "$env:".len() + name_len;
        Some((&s[..n], n))
    } else if let Some(rest) = s.strip_prefix("$HOME") {
        if rest
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return None;
        }
        Some((&s[.."$HOME".len()], "$HOME".len()))
    } else {
        None
    }
}

fn env_name_len(s: &str) -> Option<usize> {
    let mut len = 0;
    for (i, c) in s.char_indices() {
        if i == 0 && !(c.is_ascii_alphabetic() || c == '_') {
            return None;
        }
        if c.is_ascii_alphanumeric() || c == '_' {
            len = i + c.len_utf8();
        } else {
            break;
        }
    }
    (len > 0).then_some(len)
}

/// The `--config` argument to write. A plain path is one single-quoted
/// literal. A leading `$env:NAME` or `$HOME` that the profile already used
/// stays outside the quotes and is joined to the single-quoted tail:
/// quoting `$env:USERPROFILE\...` would make the variable literal text, and
/// the prompt would not resolve in any other PowerShell. The tail, which
/// holds the theme filename, always goes through `powershell_escape_single`.
fn powershell_config_arg(path: &str) -> String {
    if let Some((var, var_len)) = split_env_token(path) {
        let rest = &path[var_len..];
        if rest.starts_with(['\\', '/']) {
            let escaped = crate::term::powershell_escape_single(rest);
            return format!("({var} + '{escaped}')");
        }
    }
    let escaped = crate::term::powershell_escape_single(path);
    format!("'{escaped}'")
}

/// Rewrite the managed line's `--config` path, leaving every other line alone.
/// Returns None when that line is absent or `theme` is outside `[A-Za-z0-9._-]`.
///
/// The directory form is preserved, so a profile written with
/// `$env:USERPROFILE` stays portable. Only the filename changes.
fn rewrite_config(txt: &str, theme: &str) -> Option<String> {
    if !theme_stem_allowed(theme) {
        return None;
    }
    let (arg_at, arg_txt) = managed_arg(txt)?;
    let (path, arg_len) = parse_config_argument(arg_txt)?;
    let dir_end = path.rfind(['\\', '/']).map(|p| p + 1).unwrap_or(0);
    let new_path = format!("{}{}.omp.json", &path[..dir_end], theme);
    let arg = powershell_config_arg(&new_path);
    let mut out = String::with_capacity(txt.len() + arg.len());
    out.push_str(&txt[..arg_at]);
    out.push_str(&arg);
    out.push_str(&txt[arg_at + arg_len..]);
    Some(out)
}

/// Point `$PROFILE`'s managed line at `theme`. New shells pick it up.
pub fn set_theme(theme: &str) -> Result<(), String> {
    if !theme_stem_allowed(theme) {
        return Err("theme name rejected".into());
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
    let cfg = themes_dir().join(format!("{theme}.omp.json"));
    if !cfg.is_file() {
        return None;
    }
    let mut cmd = std::process::Command::new("oh-my-posh");
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

/// `Command::output()` with a deadline. std has no wait-with-timeout, so poll
/// `try_wait` and kill on expiry; the previews ran on the UI thread with no
/// bound at all, so one stuck `oh-my-posh` (a broken theme, a slow shell
/// plugin it shells out to) hung Bayan for good.
fn run_with_timeout(
    mut cmd: std::process::Command,
    limit: std::time::Duration,
) -> Option<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // previews and tests never open a console
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // drain stdout on its own thread so a chatty child never blocks on a
    // full pipe while we wait on it
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let t0 = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if t0.elapsed() < limit => {
                std::thread::sleep(std::time::Duration::from_millis(15))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    // The child is gone, but read_to_end returns only when EVERY write end
    // of the pipe closes — and oh-my-posh shells out, so a grandchild can
    // still be holding the one it inherited. Joining that thread outright
    // would hang the UI thread for as long as the grandchild lives, which
    // is the exact freeze the deadline above exists to prevent. Bound this
    // wait too; a preview we cannot collect in time is a preview we skip.
    let left = limit
        .saturating_sub(t0.elapsed())
        .max(std::time::Duration::from_millis(250));
    let stdout = rx.recv_timeout(left).ok()?;
    Some(std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

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
        // the directory form is preserved and the tail is a single-quoted
        // literal. Quoting `$env:USERPROFILE` itself would stop it expanding.
        let expected = PROFILE.replace(
            "\"$env:USERPROFILE\\.poshthemes\\night-owl.omp.json\"",
            "($env:USERPROFILE + '\\.poshthemes\\kali.omp.json')",
        );
        assert_eq!(out, expected);
        // spawn reads the same line back as the expandable path
        assert_eq!(
            crate::term::parse_posh_config(&out).as_deref(),
            Some(r"$env:USERPROFILE\.poshthemes\kali.omp.json")
        );
    }

    #[test]
    fn a_profile_without_the_managed_line_is_refused_not_mangled() {
        assert!(rewrite_config("# nothing here\n", "kali").is_none());
    }

    #[test]
    fn rewrite_refuses_stems_outside_the_allowlist() {
        // the fixture really does have a managed line, so None below is the
        // allowlist and not a missing marker
        assert!(rewrite_config(PROFILE, "kali").is_some());
        for good in [
            "night-owl",
            "powerlevel10k_rainbow",
            "clean-detailed",
            "v1.2",
            "A_B",
        ] {
            assert!(rewrite_config(PROFILE, good).is_some(), "rejected {good}");
        }
        for bad in [
            "$(calc)",
            "a$b",
            "a(b)",
            "a)b",
            "night`ncalc",
            "a b",
            "a'b",
            "a\"b",
            "a;b",
            "a&b",
            "a\\b",
            "a/b",
            "عربي",
            "",
            "kali\n",
            "a\u{2019}b",
        ] {
            assert!(rewrite_config(PROFILE, bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn theme_filenames_outside_the_allowlist_are_hidden() {
        assert_eq!(
            theme_from_filename("kali.omp.json").as_deref(),
            Some("kali")
        );
        assert_eq!(
            theme_from_filename("night-owl.omp.json").as_deref(),
            Some("night-owl")
        );
        assert!(theme_from_filename("$(calc).omp.json").is_none());
        assert!(theme_from_filename("a(b).omp.json").is_none());
        assert!(theme_from_filename("a$b.omp.json").is_none());
        assert!(theme_from_filename("a`b.omp.json").is_none());
        assert!(theme_from_filename(".omp.json").is_none());
        assert!(theme_from_filename("kali.json").is_none());
    }

    #[test]
    fn an_absolute_config_path_is_one_single_quoted_literal() {
        let src = "oh-my-posh init pwsh --config \"C:/themes/pure.omp.json\" | Invoke-Expression\n";
        let out = rewrite_config(src, "kali").unwrap();
        let escaped = crate::term::powershell_escape_single("C:/themes/kali.omp.json");
        assert!(out.contains(&format!("--config '{escaped}'")), "{out}");
        assert!(!out.contains('"'), "double quotes remained: {out}");
        assert_eq!(current_from(&out).as_deref(), Some("kali"));
        assert_eq!(
            crate::term::parse_posh_config(&out).as_deref(),
            Some("C:/themes/kali.omp.json")
        );
    }

    #[test]
    fn a_quote_in_the_directory_is_escaped_with_powershell_escape_single() {
        let src = "oh-my-posh init pwsh --config \"C:\\o'brien\\pure.omp.json\" | iex\n";
        let out = rewrite_config(src, "kali").unwrap();
        let escaped = crate::term::powershell_escape_single("C:\\o'brien\\kali.omp.json");
        assert!(out.contains(&format!("--config '{escaped}'")), "{out}");
        assert_eq!(
            crate::term::parse_posh_config(&out).as_deref(),
            Some(r"C:\o'brien\kali.omp.json")
        );
        // applying again must not double the apostrophe a second time
        let again = rewrite_config(&out, "pure").unwrap();
        let escaped = crate::term::powershell_escape_single("C:\\o'brien\\pure.omp.json");
        assert!(again.contains(&format!("--config '{escaped}'")), "{again}");
        // a smart quote is a PowerShell quote too, so it is doubled the same way
        let smart = format!(
            "oh-my-posh init pwsh --config \"C:\\o{}brien\\pure.omp.json\" | iex\n",
            '\u{2019}'
        );
        let out = rewrite_config(&smart, "kali").unwrap();
        let path = format!("C:\\o{}brien\\kali.omp.json", '\u{2019}');
        let escaped = crate::term::powershell_escape_single(&path);
        assert!(out.contains(&format!("'{escaped}'")), "{out}");
        assert_eq!(
            crate::term::parse_posh_config(&out).as_deref(),
            Some(path.as_str())
        );
    }

    #[test]
    fn a_portable_profile_stays_expandable_and_a_second_apply_only_swaps_the_file() {
        let once = rewrite_config(PROFILE, "kali").unwrap();
        let twice = rewrite_config(&once, "pure").unwrap();
        assert!(
            twice.contains("($env:USERPROFILE + '\\.poshthemes\\pure.omp.json')"),
            "{twice}"
        );
        assert!(!twice.contains('"'), "{twice}");
        assert_eq!(current_from(&twice).as_deref(), Some("pure"));
        assert_eq!(
            crate::term::parse_posh_config(&twice).as_deref(),
            Some(r"$env:USERPROFILE\.poshthemes\pure.omp.json")
        );
        // $HOME and $env:POSH_THEMES_PATH are the other forms a profile uses
        let home =
            "oh-my-posh init pwsh --config \"$HOME\\.poshthemes\\night-owl.omp.json\" | iex\n";
        let out = rewrite_config(home, "kali").unwrap();
        assert!(
            out.contains("($HOME + '\\.poshthemes\\kali.omp.json')"),
            "{out}"
        );
        let themes =
            "oh-my-posh init pwsh --config \"$env:POSH_THEMES_PATH\\night-owl.omp.json\" | iex\n";
        let out = rewrite_config(themes, "kali").unwrap();
        assert!(
            out.contains("($env:POSH_THEMES_PATH + '\\kali.omp.json')"),
            "{out}"
        );
        assert_eq!(
            crate::term::parse_posh_config(&out).as_deref(),
            Some(r"$env:POSH_THEMES_PATH\kali.omp.json")
        );
    }

    #[test]
    fn a_subexpression_already_in_the_path_is_quoted_literal() {
        // the directory was already code. Rewriting must not leave it in
        // double quotes, and must not treat `$(...)` as a variable to keep.
        let src = "oh-my-posh init pwsh --config \"$($env:USERPROFILE)\\.poshthemes\\night-owl.omp.json\" | iex\n";
        let out = rewrite_config(src, "kali").unwrap();
        let escaped = crate::term::powershell_escape_single(
            "$($env:USERPROFILE)\\.poshthemes\\kali.omp.json",
        );
        assert!(out.contains(&format!("--config '{escaped}'")), "{out}");
        assert!(!out.contains("\"$("), "{out}");
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
    fn a_missing_themes_directory_yields_no_themes() {
        // guards the "oh-my-posh not installed" path: empty, never a panic
        assert!(order_featured_first(Vec::new()).is_empty());
    }
}
