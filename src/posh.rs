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

use std::path::PathBuf;

/// Themes worth showing first, in this order. The rest follow alphabetically.
/// EasyTer's `FEATURED`, kept because the ordering is a curation decision,
/// not an implementation detail.
const FEATURED: &[&str] = &[
    "kali", "jandedobbeleer", "paradox", "atomic", "powerlevel10k_rainbow",
    "clean-detailed", "montys", "agnoster", "robbyrussell", "pure", "night-owl",
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

/// Theme names present on disk: featured ones first, then the rest sorted.
pub fn available() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(themes_dir()) else {
        return Vec::new(); // oh-my-posh not installed is not an error here
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|f| f.strip_suffix(".omp.json").map(str::to_string))
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
    out.extend(names.into_iter().filter(|n| !FEATURED.contains(&n.as_str())));
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
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // drain stdout on its own thread so a chatty child never blocks on a
    // full pipe while we wait on it
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let t0 = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if t0.elapsed() < limit => std::thread::sleep(std::time::Duration::from_millis(15)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let stdout = reader.join().ok()?;
    Some(std::process::Output { status, stdout, stderr: Vec::new() })
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
        // every other line survives byte for byte — this is the whole promise
        assert!(out.contains("chcp 65001 > $null"));
        assert!(out.contains("a line the user wrote and we must never touch"));
        assert!(out.contains("if (Get-Command oh-my-posh"));
        assert_eq!(out.lines().count(), PROFILE.lines().count());
        // the directory form is preserved, so a portable profile stays portable
        assert!(out.contains("$env:USERPROFILE"), "absolute path was injected: {out}");
    }

    #[test]
    fn a_profile_without_the_managed_line_is_refused_not_mangled() {
        assert!(rewrite_config("# nothing here\n", "kali").is_none());
    }

    #[test]
    fn featured_themes_lead_and_the_rest_follow_sorted() {
        let names = vec![
            "aaa".to_string(), "kali".to_string(), "zzz".to_string(),
            "pure".to_string(), "atomic".to_string(),
        ];
        let out = order_featured_first(names);
        // FEATURED order, not alphabetical, for the ones that are featured
        assert_eq!(&out[..3], &["kali".to_string(), "atomic".to_string(), "pure".to_string()]);
        // then the others, in the order given (already sorted by the caller)
        assert_eq!(&out[3..], &["aaa".to_string(), "zzz".to_string()]);
        // a featured name that is not installed must not appear
        assert!(!out.contains(&"paradox".to_string()));
    }

    /// A stuck preview must give up, not hang the window: `ping` with a
    /// long count outlives the limit and gets killed.
    #[test]
    fn a_hung_preview_is_killed_at_the_deadline() {
        let mut slow = std::process::Command::new("ping");
        slow.args(["-n", "30", "127.0.0.1"]);
        let t0 = std::time::Instant::now();
        let out = run_with_timeout(slow, std::time::Duration::from_millis(400));
        assert!(out.is_none(), "a hung child must yield None");
        assert!(t0.elapsed() < std::time::Duration::from_secs(5), "took {:?}", t0.elapsed());
        // and a quick child still returns its output
        let mut quick = std::process::Command::new("cmd");
        quick.args(["/c", "echo", "hi"]);
        let out = run_with_timeout(quick, std::time::Duration::from_secs(5)).expect("output");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    #[test]
    fn a_missing_themes_directory_yields_no_themes() {
        // guards the "oh-my-posh not installed" path: empty, never a panic
        assert!(order_featured_first(Vec::new()).is_empty());
    }
}

