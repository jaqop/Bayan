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
        Self {
            name: name.to_string(),
            command: command.into(),
        }
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

/// N-03: `wsl.exe` by bare name. `Command` on Windows searches the directory
/// of `bayan.exe` before System32, so a copy planted beside the program
/// (Downloads, `target\release`) would run at every launch. Discovery and
/// later spawns use `GetSystemDirectoryW()\wsl.exe` or they do not run.
pub(crate) fn wsl_program() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        resolve_wsl_exe(system32_dir().as_deref(), |p| p.is_file())
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Absolute `System32\wsl.exe` when `is_file` says it is there. A relative
/// directory is refused: that is the app-dir search this exists to avoid.
fn resolve_wsl_exe(
    system32: Option<&Path>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<std::path::PathBuf> {
    let dir = system32.filter(|d| d.is_absolute())?;
    let exe = dir.join("wsl.exe");
    is_file(&exe).then_some(exe)
}

/// A saved shell line of `wsl` / `wsl.exe` with no directory. An absolute
/// path is already decided and is left alone.
fn is_bare_wsl(prog: &str) -> bool {
    if prog.contains(['\\', '/', '"']) {
        return false;
    }
    prog.eq_ignore_ascii_case("wsl.exe") || prog.eq_ignore_ascii_case("wsl")
}

/// Replace a bare WSL name with the System32 copy. Anything else passes
/// through. `None` means "do not spawn": there is no System32 `wsl.exe`
/// to run, and the bare name is not a fallback.
pub(crate) fn resolve_spawn_program(prog: String) -> Option<String> {
    if !is_bare_wsl(&prog) {
        return Some(prog);
    }
    resolve_bare_wsl(&prog, wsl_program().as_deref())
}

fn resolve_bare_wsl(prog: &str, system_wsl: Option<&Path>) -> Option<String> {
    if !is_bare_wsl(prog) {
        return Some(prog.to_string());
    }
    let exe = system_wsl.filter(|p| p.is_absolute())?;
    Some(exe.display().to_string())
}

/// Command line stored for a distro. A path with a space is quoted so
/// `split_command` keeps it as one program.
fn format_wsl_invocation(exe: &Path, distro: &str) -> String {
    let p = exe.display().to_string();
    if p.contains([' ', '\t']) {
        format!("\"{p}\" -d {distro}")
    } else {
        format!("{p} -d {distro}")
    }
}

#[cfg(windows)]
fn system32_dir() -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260];
    // SAFETY: the length passed is `buf`'s own, so the write stays inside it.
    // Zero, or a length that does not fit, skips WSL instead of spawning a
    // bare name.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) };
    if n == 0 || n as usize >= buf.len() {
        return None;
    }
    Some(std::path::PathBuf::from(std::ffi::OsString::from_wide(
        &buf[..n as usize],
    )))
}

#[cfg(windows)]
fn wsl_distros_from(exe: &Path) -> Vec<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new(exe)
        .args(["-l", "-q"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => parse_wsl_result(o.status.success(), &o.stdout),
        Err(_) => Vec::new(), // no WSL installed is not an error
    }
}

#[cfg(not(windows))]
fn wsl_distros_from(_exe: &Path) -> Vec<String> {
    Vec::new()
}

fn wsl_entries() -> Vec<Shell> {
    let Some(exe) = wsl_program() else {
        return Vec::new();
    };
    wsl_distros_from(&exe)
        .into_iter()
        .filter(|d| is_safe_distro_name(d))
        .map(|d| Shell::new(&format!("WSL · {d}"), format_wsl_invocation(&exe, &d)))
        .collect()
}

/// BAYAN-008: the stored command line is split on whitespace at spawn, so
/// a distro name must be exactly one argument that cannot read as a flag.
/// WSL itself only creates names from `[A-Za-z0-9._-]`; anything else (a
/// space, a leading `-`, a quote, a control or bidi character) is skipped
/// rather than turned into extra `wsl.exe` arguments, so the profile the UI
/// shows is always the command that runs.
fn is_safe_distro_name(d: &str) -> bool {
    !d.is_empty()
        && d.len() <= 64
        && !d.starts_with('-')
        && d.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Every shell present on this machine, in EasyTer's order.
pub fn detect() -> Vec<Shell> {
    let mut v = Vec::new();

    // $PROFILE is loaded, so the user's chosen oh-my-posh prompt shows up.
    v.push(Shell::new("PowerShell", "powershell.exe"));
    v.push(Shell::new("PowerShell سريع", "powershell.exe -NoProfile"));

    if on_path("pwsh") {
        v.push(Shell::new("PowerShell 7", "pwsh.exe -NoLogo"));
    }

    v.push(Shell::new("Command Prompt", "cmd.exe"));

    if let Some(p) = pick_git_bash(|p| Path::new(p).exists()) {
        v.push(Shell::new(
            "Git Bash",
            format!("{} --login -i", short_path(p)),
        ));
    }

    v.extend(wsl_entries());

    v
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
    fn wsl_names_that_would_split_into_extra_args_are_skipped() {
        for ok in ["Ubuntu", "Ubuntu-22.04", "kali-linux", "my_distro.v2"] {
            assert!(is_safe_distro_name(ok), "{ok}");
            let (prog, args) = crate::term::split_command(&format!("wsl.exe -d {ok}"));
            assert_eq!((prog.as_str(), args.len()), ("wsl.exe", 2), "{ok}");
        }
        for bad in [
            "",
            "-u root",
            "--exec",
            "Ubuntu --exec calc",
            "a\"b",
            "a\tb",
            "x\u{202E}y",
        ] {
            assert!(!is_safe_distro_name(bad), "{bad:?}");
        }
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

    #[test]
    fn wsl_is_the_system32_copy_or_it_is_not_spawned() {
        let sys = Path::new(r"C:\Windows\System32\wsl.exe");
        assert_eq!(
            resolve_wsl_exe(Some(Path::new(r"C:\Windows\System32")), |p| p == sys),
            Some(sys.to_path_buf())
        );
        // a relative directory is the app-dir hole, even if a file is "there"
        assert_eq!(resolve_wsl_exe(Some(Path::new("system32")), |_| true), None);
        assert_eq!(resolve_wsl_exe(None, |_| true), None);
        assert_eq!(
            resolve_wsl_exe(Some(Path::new(r"C:\Windows\System32")), |_| false),
            None
        );

        assert_eq!(
            resolve_bare_wsl("wsl.exe", Some(sys)).as_deref(),
            Some(r"C:\Windows\System32\wsl.exe")
        );
        assert_eq!(
            resolve_bare_wsl("WSL.EXE", Some(sys)).as_deref(),
            Some(r"C:\Windows\System32\wsl.exe")
        );
        assert_eq!(
            resolve_bare_wsl("wsl", Some(sys)).as_deref(),
            Some(r"C:\Windows\System32\wsl.exe")
        );
        // no System32 copy: refuse, do not keep the bare name
        assert_eq!(resolve_bare_wsl("wsl.exe", None), None);
        assert_eq!(
            resolve_bare_wsl("wsl.exe", Some(Path::new(r"relative\wsl.exe"))),
            None
        );
        // other shells, and an already-absolute wsl, are unchanged
        assert_eq!(
            resolve_bare_wsl("powershell.exe", None).as_deref(),
            Some("powershell.exe")
        );
        assert_eq!(
            resolve_bare_wsl(r"C:\Windows\System32\wsl.exe", None).as_deref(),
            Some(r"C:\Windows\System32\wsl.exe")
        );

        let line = format_wsl_invocation(sys, "Ubuntu-22.04");
        let (prog, args) = crate::term::split_command(&line);
        assert_eq!(prog, r"C:\Windows\System32\wsl.exe");
        assert_eq!(args, ["-d", "Ubuntu-22.04"]);
        let spaced = format_wsl_invocation(Path::new(r"C:\Program Files\wsl.exe"), "Ubuntu");
        let (prog, args) = crate::term::split_command(&spaced);
        assert_eq!(prog, r"C:\Program Files\wsl.exe");
        assert_eq!(args, ["-d", "Ubuntu"]);
    }

    #[test]
    fn discovery_does_not_name_wsl_as_a_bare_command() {
        let src = include_str!("shells.rs");
        let prod = src.split("mod tests").next().unwrap_or(src);
        assert!(
            !prod.contains("Command::new(\"wsl.exe\")"),
            "wsl discovery must take the System32 path"
        );
        assert!(!prod.contains("Command::new(\"wsl\")"));
    }
}
