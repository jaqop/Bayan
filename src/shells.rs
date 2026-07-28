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
    /// Command line handed to ConPTY.
    pub command: String,
}

impl Shell {
    fn new(name: &str, command: impl Into<String>) -> Self {
        Self { name: name.to_string(), command: command.into() }
    }
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
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
        .lines()
        .map(|l| l.trim().trim_matches('\0').trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// The 8.3 short path. ConPTY fails on paths containing spaces, and every
/// default Git install lives under "Program Files".
#[cfg(windows)]
fn short_path(p: &str) -> String {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    let wide: Vec<u16> = std::ffi::OsStr::new(p).encode_wide().chain(Some(0)).collect();
    let mut buf = [0u16; 260];
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
    let Some(paths) = std::env::var_os("PATH") else { return false };
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
        Ok(o) => parse_wsl_list(&o.stdout),
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
    v.push(Shell::new("PowerShell", "powershell.exe"));
    v.push(Shell::new("PowerShell (سريع · بلا profile)", "powershell.exe -NoProfile"));

    if on_path("pwsh") {
        v.push(Shell::new("PowerShell 7", "pwsh.exe -NoLogo"));
    }

    v.push(Shell::new("Command Prompt", "cmd.exe"));

    if let Some(p) = pick_git_bash(|p| Path::new(p).exists()) {
        v.push(Shell::new("Git Bash", format!("{} --login -i", short_path(p))));
    }

    for d in wsl_distros() {
        v.push(Shell::new(&format!("WSL · {d}"), format!("wsl.exe -d {d}")));
    }

    v
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let raw: Vec<u8> = "Ubuntu".encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        assert_eq!(parse_wsl_list(&raw), vec!["Ubuntu"]);
        assert_ne!(String::from_utf8_lossy(&raw).trim(), "Ubuntu");
    }

    #[test]
    fn detection_always_offers_a_working_shell() {
        let found = detect();
        // powershell.exe and cmd.exe ship with Windows: never an empty list
        assert!(found.iter().any(|s| s.command == "powershell.exe"));
        assert!(found.iter().any(|s| s.command == "cmd.exe"));
        // EasyTer's order: the $PROFILE-loading entry comes first
        assert_eq!(found[0].command, "powershell.exe");
        assert_eq!(found[1].command, "powershell.exe -NoProfile");
        // no duplicate command lines
        let mut cmds: Vec<&str> = found.iter().map(|s| s.command.as_str()).collect();
        cmds.sort_unstable();
        let before = cmds.len();
        cmds.dedup();
        assert_eq!(cmds.len(), before, "duplicate shell commands: {found:?}");
    }
}


/// Friendly label for a stored command line, for the settings panel. Falls
/// back to a trimmed command so a hand-edited config.json still reads sanely.
pub fn label_for(command: &str) -> String {
    detect()
        .into_iter()
        .find(|s| s.command == command)
        .map(|s| s.name)
        .unwrap_or_else(|| command.trim_end_matches(".exe").to_string())
}
