//! Detect the shells actually installed on this machine, the way Windows
//! Terminal's generators do. Ported from EasyTer's `profiles.py`, which is
//! the behavioral specification — including the traps it paid for.
//!
//! The order below is EasyTer's order and is deliberate: PowerShell first
//! (it loads $PROFILE, so the user's oh-my-posh prompt appears), then the
//! no-profile variant for a fast boot, then pwsh only if present, cmd,
//! Git Bash, and finally the WSL distributions.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    /// Label for the settings list.
    pub name: String,
    /// Program passed to `CommandBuilder::new`. Not a command line.
    pub program: String,
    /// Arguments, each one `CommandBuilder::arg`. A WSL distro is one entry.
    pub args: Vec<String>,
}

impl Shell {
    fn new(name: &str, program: &str, args: &[&str]) -> Self {
        Self {
            name: name.to_string(),
            program: program.to_string(),
            args: args.iter().copied().map(str::to_string).collect(),
        }
    }

    /// Command line stored in config and shown in the shell cycler.
    ///
    /// This is a persistence token. Spawn does not recover a WSL distro by
    /// splitting it: `wsl_spawn_argv` keeps the name as one argument, and
    /// `wsl_shell` refuses a name that would not round-trip that way.
    pub fn command(&self) -> String {
        let mut s = self.program.clone();
        for a in &self.args {
            s.push(' ');
            s.push_str(a);
        }
        s
    }
}

/// A distro name safe to pass as the single argument after `wsl.exe -d`.
///
/// Empty, a leading `-` (another flag), whitespace, and control characters
/// are refused. Whitespace is not an argument separator on this path.
fn accept_wsl_distro(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first == '-' || first.is_whitespace() || first.is_control() {
        return false;
    }
    chars.all(|c| !c.is_whitespace() && !c.is_control())
}

/// One WSL distro, or `None` when the name must not be launched.
fn wsl_shell(distro: &str) -> Option<Shell> {
    if !accept_wsl_distro(distro) {
        return None;
    }
    Some(Shell::new(
        &format!("WSL · {distro}"),
        "wsl.exe",
        &["-d", distro],
    ))
}

fn wsl_shells_from(names: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<Shell> {
    names
        .into_iter()
        .filter_map(|n| wsl_shell(n.as_ref()))
        .collect()
}

/// Argv for a stored `wsl.exe -d <distro>` command line.
///
/// `Ok(None)` when `command` is not that form (the caller uses
/// `split_command`). `Ok(Some)` is `wsl.exe` plus one `-d` and the distro
/// as a single argument — the remainder is never whitespace-split.
/// `Err` when the line is that form but the distro name is rejected, so the
/// caller must not fall back to splitting it into further arguments.
pub(crate) fn wsl_spawn_argv(command: &str) -> Result<Option<(String, Vec<String>)>, &'static str> {
    let Some(distro) = command.strip_prefix("wsl.exe -d ") else {
        return Ok(None);
    };
    if !accept_wsl_distro(distro) {
        return Err("rejected WSL distro name");
    }
    Ok(Some((
        "wsl.exe".to_string(),
        vec!["-d".to_string(), distro.to_string()],
    )))
}

/// Git Bash candidates, in priority order. The first two are the REAL bash.
///
/// `bin\bash.exe` is a launcher: it re-spawns the shell in a way that detaches
/// it from the ConPTY pipe, so no prompt appears and no input arrives — the
/// user-visible symptom is "I can't type at all". `usr\bin\bash.exe` runs
/// interactively on the pipe. EasyTer paid for this discovery; keep the order.
const GIT_BASH_CANDIDATES: &[&str] = &[
    r"C:\Program Files\Git\usr\bin\bash.exe",
    r"C:\Program Files (x86)\Git\usr\bin\bash.exe",
    r"C:\Program Files\Git\bin\bash.exe", // launcher — last resort only
    r"C:\Program Files (x86)\Git\bin\bash.exe",
];

/// First existing candidate, in priority order. `exists` is injected so the
/// priority rule is testable without installing four copies of Git.
fn pick_git_bash<F: Fn(&str) -> bool>(exists: F) -> Option<&'static str> {
    GIT_BASH_CANDIDATES.iter().copied().find(|p| exists(p))
}

/// `wsl -l -q` writes UTF-16LE, and pads entries with NULs. Decoding it as
/// UTF-8 yields separated letters and empty-looking names.
fn parse_wsl_list(raw: &[u8]) -> Vec<String> {
    let (pairs, _odd_byte) = raw.as_chunks::<2>();
    let units: Vec<u16> = pairs.iter().map(|&c| u16::from_le_bytes(c)).collect();
    String::from_utf16_lossy(&units)
        .lines()
        .map(|l| l.trim().trim_matches('\0').trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn parse_wsl_result(success: bool, raw: &[u8]) -> Vec<String> {
    // WSL writes diagnostics to stdout too; only successful listings are names.
    if success {
        parse_wsl_list(raw)
    } else {
        Vec::new()
    }
}

/// The 8.3 short path. ConPTY fails on paths containing spaces, and every
/// default Git install lives under "Program Files".
#[cfg(windows)]
fn short_path(p: &str) -> String {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    let wide: Vec<u16> = std::ffi::OsStr::new(p)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut buf = [0u16; 260];
    // SAFETY: `wide` is NUL-terminated, and the length passed is `buf`'s own.
    let n = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if n == 0 || n as usize >= buf.len() {
        return p.to_string(); // failure is not fatal: the long path may still work
    }
    std::ffi::OsString::from_wide(&buf[..n as usize])
        .to_string_lossy()
        .into_owned()
}

#[cfg(not(windows))]
fn short_path(p: &str) -> String {
    p.to_string()
}

/// Is `prog` resolvable on PATH? (`shutil.which` equivalent, exe-only.)
fn on_path(prog: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let exe = dir.join(format!("{prog}.exe"));
        exe.is_file() || dir.join(prog).is_file()
    })
}

#[cfg(windows)]
fn wsl_distros() -> Vec<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("wsl.exe")
        .args(["-l", "-q"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => parse_wsl_result(o.status.success(), &o.stdout),
        Err(_) => Vec::new(), // no WSL installed is not an error
    }
}

#[cfg(not(windows))]
fn wsl_distros() -> Vec<String> {
    Vec::new()
}

/// Every shell present on this machine, in EasyTer's order.
pub fn detect() -> Vec<Shell> {
    let mut v = Vec::new();

    // $PROFILE is loaded, so the user's chosen oh-my-posh prompt shows up.
    v.push(Shell::new("PowerShell", "powershell.exe", &[]));
    v.push(Shell::new(
        "PowerShell سريع",
        "powershell.exe",
        &["-NoProfile"],
    ));

    if on_path("pwsh") {
        v.push(Shell::new("PowerShell 7", "pwsh.exe", &["-NoLogo"]));
    }

    v.push(Shell::new("Command Prompt", "cmd.exe", &[]));

    if let Some(p) = pick_git_bash(|p| Path::new(p).exists()) {
        let prog = short_path(p);
        v.push(Shell::new("Git Bash", &prog, &["--login", "-i"]));
    }

    // Distro names stay one argv entry. A name that is empty, flag-like,
    // or contains whitespace or controls is dropped, not formatted into a
    // command line and split later.
    v.extend(wsl_shells_from(wsl_distros()));

    v
}

/// Friendly label for a stored command line, for the settings panel. Falls
/// back to a trimmed command so a hand-edited config.json still reads sanely.
pub fn label_for(command: &str) -> String {
    detect()
        .into_iter()
        .find(|s| s.command() == command)
        .map(|s| s.name)
        .unwrap_or_else(|| command.trim_end_matches(".exe").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_wsl_discovery_does_not_offer_error_messages_as_shells() {
        let error: Vec<u8> = "Access is denied.\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert!(parse_wsl_result(false, &error).is_empty());
        let valid: Vec<u8> = "Ubuntu\r\nDebian\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(parse_wsl_result(true, &valid), vec!["Ubuntu", "Debian"]);
    }

    #[test]
    fn git_bash_prefers_usr_bin_over_the_launcher() {
        // both installed: usr\bin wins, because bin\bash.exe detaches from ConPTY
        let both = pick_git_bash(|p| p.contains(r"Git\usr\bin") || p.contains(r"Git\bin"));
        assert_eq!(both, Some(r"C:\Program Files\Git\usr\bin\bash.exe"));
        // only the launcher present: take it rather than offering nothing
        let only_launcher = pick_git_bash(|p| p == r"C:\Program Files\Git\bin\bash.exe");
        assert_eq!(only_launcher, Some(r"C:\Program Files\Git\bin\bash.exe"));
        // x86 usr\bin beats the 64-bit launcher — priority is by KIND, not by tree
        let x86 = pick_git_bash(|p| p.contains("(x86)") && p.contains(r"usr\bin"));
        assert_eq!(x86, Some(r"C:\Program Files (x86)\Git\usr\bin\bash.exe"));
        assert_eq!(pick_git_bash(|_| false), None);
    }

    #[test]
    fn wsl_output_is_utf16le_with_nul_padding() {
        // what `wsl -l -q` actually writes
        let mut raw = Vec::new();
        for ch in "Ubuntu\r\nDebian\r\n".encode_utf16() {
            raw.extend_from_slice(&ch.to_le_bytes());
        }
        assert_eq!(parse_wsl_list(&raw), vec!["Ubuntu", "Debian"]);
        // trailing NUL padding must not become an empty distro
        raw.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(parse_wsl_list(&raw), vec!["Ubuntu", "Debian"]);
        // no WSL: empty output, empty list — not a panic
        assert!(parse_wsl_list(&[]).is_empty());
        // odd byte count (truncated read) must not panic
        assert!(parse_wsl_list(&[0x55]).is_empty());
    }

    #[test]
    fn utf8_decoding_of_wsl_output_would_be_wrong() {
        // guards the bug this port exists to avoid: reading it as UTF-8 gives
        // NUL-separated letters, which strip() cannot rescue
        let raw: Vec<u8> = "Ubuntu"
            .encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        assert_eq!(parse_wsl_list(&raw), vec!["Ubuntu"]);
        assert_ne!(String::from_utf8_lossy(&raw).trim(), "Ubuntu");
    }

    #[test]
    fn shell_labels_stay_short_enough_for_the_settings_row() {
        // the settings shell cycler is ~12 cells wide; a long label used to
        // overflow it and collide with the row title. The renderer truncates
        // now, but a name that ALWAYS truncates is a name nobody can read.
        for s in detect() {
            assert!(
                s.name.chars().count() <= 20,
                "shell label too long for the cycler: {:?} ({} chars)",
                s.name,
                s.name.chars().count()
            );
        }
    }

    #[test]
    fn detection_always_offers_a_working_shell() {
        let found = detect();
        // powershell.exe and cmd.exe ship with Windows: never an empty list
        assert!(found.iter().any(|s| s.command() == "powershell.exe"));
        assert!(found.iter().any(|s| s.command() == "cmd.exe"));
        // EasyTer's order: the $PROFILE-loading entry comes first
        assert_eq!(found[0].command(), "powershell.exe");
        assert_eq!(found[1].command(), "powershell.exe -NoProfile");
        assert_eq!(found[0].args, Vec::<String>::new());
        assert_eq!(found[1].program, "powershell.exe");
        assert_eq!(found[1].args, vec!["-NoProfile".to_string()]);
        // no duplicate command lines
        let mut cmds: Vec<String> = found.iter().map(|s| s.command()).collect();
        cmds.sort_unstable();
        let before = cmds.len();
        cmds.dedup();
        assert_eq!(cmds.len(), before, "duplicate shell commands: {found:?}");
    }

    #[test]
    fn wsl_distro_is_one_argv_entry_after_a_single_dash_d() {
        let shells = wsl_shells_from(["Ubuntu-22.04", "Debian"]);
        assert_eq!(shells.len(), 2);
        for s in &shells {
            assert_eq!(s.program, "wsl.exe");
            assert_eq!(s.args.len(), 2, "{s:?}");
            assert_eq!(s.args[0], "-d");
            assert_eq!(
                s.args.iter().filter(|a| *a == "-d").count(),
                1,
                "exactly one -d: {s:?}"
            );
            let (prog, args) = wsl_spawn_argv(&s.command())
                .unwrap()
                .expect("a stored WSL shell is structured argv");
            assert_eq!(prog, s.program);
            assert_eq!(args, s.args);
        }
        assert_eq!(shells[0].args[1], "Ubuntu-22.04");
        assert_eq!(shells[0].command(), "wsl.exe -d Ubuntu-22.04");
        assert_eq!(shells[1].args[1], "Debian");
        // not a WSL line: the caller keeps its own splitter
        assert_eq!(wsl_spawn_argv("powershell.exe -NoProfile"), Ok(None));
        assert_eq!(wsl_spawn_argv("cmd.exe"), Ok(None));
    }

    #[test]
    fn wsl_distro_names_empty_dash_whitespace_or_control_are_rejected() {
        let rejected = [
            "",
            "-",
            "-d",
            "--exec",
            "-Ubuntu",
            "My Distro",
            "Ubuntu 22.04",
            " leading",
            "trailing ",
            "a\tb",
            "a\nb",
            "a\rb",
            "a\u{0000}b",
            "a\u{0001}b",
            "a\u{001b}b",
            "a\u{007f}b",
            " ",
            "Ubuntu -e calc.exe",
        ];
        let mut names = vec!["Ubuntu".to_string(), "kali-linux".to_string()];
        names.extend(rejected.iter().map(|s| (*s).to_string()));
        let shells = wsl_shells_from(&names);
        assert_eq!(shells.len(), 2, "{shells:?}");
        assert!(shells
            .iter()
            .all(|s| s.args[1] == "Ubuntu" || s.args[1] == "kali-linux"));
        for bad in rejected {
            assert!(wsl_shell(bad).is_none(), "accepted {bad:?}");
            let stored = format!("wsl.exe -d {bad}");
            assert!(
                wsl_spawn_argv(&stored).is_err(),
                "whitespace-split or launched {bad:?}: {stored:?}"
            );
        }
    }
}
