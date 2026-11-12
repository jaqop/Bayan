//! Command line.
//!
//! `#45` already walks `argv` by hand for `-d` / `--cwd` / `--cwd=<folder>`
//! and refuses anything it does not know. This module is that walk, extended
//! with the profile, the command after `--`, `--help` / `--version`, and the
//! Explorer-menu switches. A second parser (clap, lexopt) would fork the
//! error text and the `--cwd=` form the existing test locks in, so the
//! directory arms below are the same ones `#45` shipped.

use std::ffi::OsString;
use std::path::Path;

/// Shown by `--help`. Arabic first, then the same switches in English.
/// The directory spellings are `#45`'s; the rest is new.
pub const HELP: &str = concat!(
    "بيان ",
    env!("CARGO_PKG_VERSION"),
    "\n\n",
    "\
الاستخدام:
  bayan [-d | --cwd <مجلد>] [-p | --profile <اسم>] [-- <أمر>...]
  bayan --new-window
  bayan --elevated [-p <اسم>]
  bayan --register-context-menu
  bayan --unregister-context-menu
  bayan --register-terminal-handoff
  bayan --unregister-terminal-handoff

  -d, --cwd <مجلد>       مجلد التبويب الذي يفتحه هذا الأمر
  -p, --profile <اسم>    صدفة ذلك التبويب (اسمها أو أمرها)
  -- <أمر>...            تشغيل هذا البرنامج بدل الصدفة
  --new-window           نافذة جديدة (قائمة شريط المهام)
  --elevated             إعادة التشغيل كمسؤول. يُبقى -p <اسم> فقط
  --register-context-menu
                         إضافة «افتح بيان هنا» للمستخدم الحالي
  --unregister-context-menu
                         إزالة ذلك البند من المستكشف
  --register-terminal-handoff
                         تسجيل تسليم الطرفية للمستخدم الحالي
  --unregister-terminal-handoff
                         إزالة ذلك التسجيل
  -h, --help             هذه المساعدة
  -V, --version          رقم الإصدار

Bayan ",
    env!("CARGO_PKG_VERSION"),
    "\n\n",
    "\
usage:
  bayan [-d | --cwd <folder>] [-p | --profile <name>] [-- <command>...]
  bayan --new-window
  bayan --elevated [-p | --profile <name>]
  bayan --register-context-menu
  bayan --unregister-context-menu
  bayan --register-terminal-handoff
  bayan --unregister-terminal-handoff

  -d, --cwd <folder>       Folder for the tab this command opens
      --cwd=<folder>       The same, as one argument
  -p, --profile <name>     Shell profile for that tab (its name or its command)
      --profile=<name>     The same, as one argument
      -- <command>...      Run this program in that tab instead of the shell
      --new-window         Open a window (the taskbar jump list's \"New window\")
      --elevated           Relaunch through UAC as administrator. Only an
                           optional -p <profile name> is kept; a folder and
                           a command after -- are refused
      --register-context-menu
                           Add «افتح بيان هنا» for the current user
      --unregister-context-menu
                           Remove that Explorer entry
      --register-terminal-handoff
                           Register the per-user ITerminalHandoff class
      --unregister-terminal-handoff
                           Remove that class
      --handoff            COM local server. Windows appends -Embedding
  -h, --help               Show this help
  -V, --version            Show the version

-d and --cwd are the folder switch from the working-directory change: a
relative folder is taken from the current one, and a UNC path, a device
path, a remote drive or a missing folder stops startup. A profile or a
command applies to that same tab. With saved tabs restored, those tabs
come back first and this tab is the active one.

--elevated never elevates by itself on a normal launch. It asks Windows
for a standard UAC prompt (ShellExecuteW verb \"runas\") and the elevated
process receives only --elevated and, if it was named, -p <profile name>.
The profile is resolved again from installed shells. Nothing is handed
over through the environment, the registry, a pipe, or a config file.

Explorer registration writes only
HKCU\\Software\\Classes\\Directory\\shell\\Bayan and
Directory\\Background\\shell\\Bayan. The command is
\"<exe>\" -d \"%V\\.\" so a drive root does not end in a backslash before
the closing quote. It does not write HKLM and does not ask for an
administrator. Windows 11's top-level menu needs package identity
(MSIX, a later installer); this entry shows under \"Show more options\".

Terminal handoff registration writes only the per-user COM class under
HKCU\\Software\\Classes\\CLSID. It does not write HKLM and it does not set
HKCU\\Console\\%%Startup, so it does not become the Windows default
terminal. See the handoff module for what an MSIX package would still
have to provide.
"
);

const USAGE: &str = "\
usage: bayan [-d | --cwd <folder>] [-p | --profile <name>] [-- <command>...]
       bayan --new-window | --elevated [-p <name>]
       bayan --register-context-menu | --unregister-context-menu
       bayan --register-terminal-handoff | --unregister-terminal-handoff
       bayan --help | --version";

/// What `argv` asked for, before the folder is checked on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    Launch(Launch),
    Help,
    Version,
    RegisterContextMenu,
    UnregisterContextMenu,
    RegisterTerminalHandoff,
    UnregisterTerminalHandoff,
    /// COM launched us (`--handoff`, and the `-Embedding` it appends).
    Handoff,
}

/// A normal startup. `directory` is still the raw text; [`gate_start_dir`]
/// is what accepts it. `profile` is the name the user typed, not a command
/// line. `command` is set only when `--` was present.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Launch {
    pub directory: Option<String>,
    pub profile: Option<String>,
    pub command: Option<Vec<String>>,
    /// `--elevated`: relaunch through UAC. The elevated process keeps only
    /// this flag and an optional profile name; see `elevate`.
    pub elevated: bool,
}

/// The process the launch tab starts. A `--` command is already split, so
/// a path with spaces stays one argument. Otherwise the profile's command
/// (or the configured shell) goes through the normal shell launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchProcess {
    pub command_line: String,
    pub raw_argv: Option<Vec<String>>,
}

pub fn parse<I, S>(args: I) -> Result<CliAction, String>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let text = |a: OsString| -> Result<String, String> {
        a.into_string()
            .map_err(|_| format!("an argument is not valid Unicode\n{USAGE}"))
    };
    let mut args = args.into_iter().map(Into::into);
    let mut directory = None;
    let mut profile = None;
    let mut command = None;
    let mut elevated = false;
    let mut new_window = false;
    let mut handoff = false;
    // `Some(true)` registers, `Some(false)` removes. Both at once is an error.
    let mut menu: Option<bool> = None;
    let mut handoff_reg: Option<bool> = None;

    while let Some(a) = args.next() {
        let a = text(a)?;
        if a == "--" {
            let mut cmd = Vec::new();
            for rest in args.by_ref() {
                cmd.push(text(rest)?);
            }
            if cmd.is_empty() || cmd[0].is_empty() {
                return Err(format!("missing command after --\n{USAGE}"));
            }
            command = Some(cmd);
            break;
        }
        match a.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "--register-context-menu" => {
                if menu.is_some() {
                    return Err(format!(
                        "--register-context-menu and --unregister-context-menu cannot be combined\n{USAGE}"
                    ));
                }
                menu = Some(true);
            }
            "--unregister-context-menu" => {
                if menu.is_some() {
                    return Err(format!(
                        "--register-context-menu and --unregister-context-menu cannot be combined\n{USAGE}"
                    ));
                }
                menu = Some(false);
            }
            "--register-terminal-handoff" => {
                if handoff_reg.is_some() {
                    return Err(format!(
                        "--register-terminal-handoff and --unregister-terminal-handoff cannot be combined\n{USAGE}"
                    ));
                }
                handoff_reg = Some(true);
            }
            "--unregister-terminal-handoff" => {
                if handoff_reg.is_some() {
                    return Err(format!(
                        "--register-terminal-handoff and --unregister-terminal-handoff cannot be combined\n{USAGE}"
                    ));
                }
                handoff_reg = Some(false);
            }
            "--handoff" | "-Embedding" => handoff = true,
            "--elevated" => elevated = true,
            // The jump list needs a non-empty argument list (AddUserTasks
            // rejects a shell link with no arguments). The flag is a launch.
            "--new-window" => new_window = true,
            // `#45`: `-d <folder>` and `--cwd <folder>`. The next token is
            // the folder even when it looks like a flag; a missing one errors.
            "-d" | "--cwd" => {
                let v = text(
                    args.next()
                        .ok_or_else(|| format!("{a} needs a folder\n{USAGE}"))?,
                )?;
                directory = Some(normalize_start_dir(&v));
            }
            "-p" | "--profile" => {
                let v = text(
                    args.next()
                        .ok_or_else(|| format!("{a} needs a profile name\n{USAGE}"))?,
                )?;
                if v.trim().is_empty() {
                    return Err(format!("{a} needs a profile name\n{USAGE}"));
                }
                profile = Some(v);
            }
            _ => match a.strip_prefix("--cwd=") {
                Some(v) => directory = Some(normalize_start_dir(v)),
                None => match a.strip_prefix("--profile=") {
                    Some(v) if !v.trim().is_empty() => profile = Some(v.to_string()),
                    Some(_) => {
                        return Err(format!("--profile needs a profile name\n{USAGE}"));
                    }
                    None => return Err(format!("unknown argument {a:?}\n{USAGE}")),
                },
            },
        }
    }

    let exclusive = menu.is_some() as u8 + handoff_reg.is_some() as u8 + u8::from(handoff);
    if exclusive > 1 {
        return Err(format!("those commands cannot be combined\n{USAGE}"));
    }
    let launch_bits = directory.is_some() || profile.is_some() || command.is_some() || elevated;
    if exclusive > 0 && (launch_bits || (new_window && (menu.is_some() || handoff_reg.is_some()))) {
        return Err(format!(
            "that command cannot be combined with other arguments\n{USAGE}"
        ));
    }
    // `--new-window --handoff` is not a COM launch.
    if handoff && new_window {
        return Err(format!(
            "that command cannot be combined with other arguments\n{USAGE}"
        ));
    }

    if let Some(enable) = menu {
        return Ok(if enable {
            CliAction::RegisterContextMenu
        } else {
            CliAction::UnregisterContextMenu
        });
    }
    if let Some(enable) = handoff_reg {
        return Ok(if enable {
            CliAction::RegisterTerminalHandoff
        } else {
            CliAction::UnregisterTerminalHandoff
        });
    }
    if handoff {
        return Ok(CliAction::Handoff);
    }

    Ok(CliAction::Launch(Launch {
        directory,
        profile,
        command,
        elevated,
    }))
}

/// Undo the two ways a drive root arrives broken.
///
/// `"<exe>" -d "%V"` at `C:\` is parsed by `CommandLineToArgvW` as `C:"`
/// (the backslash escapes the closing quote). The registered form
/// `"<exe>" -d "%V\."` expands that same root to `C:\\.` (the slash in `%V`
/// plus the slash in the template). A stray trailing quote is dropped, `X:`
/// after that quote is the drive root, and a repeated backslash on a drive
/// path is collapsed. A path that starts with `\\` is left alone so a UNC
/// folder is still refused.
pub fn normalize_start_dir(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = trimmed.strip_suffix('"').unwrap_or(trimmed);
    let bytes = stripped.as_bytes();
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return format!("{}:\\", stripped.chars().next().unwrap_or('C'));
    }
    if stripped.starts_with('\\') || stripped.starts_with('/') {
        return stripped.to_string();
    }
    let mut collapsed = stripped.to_string();
    while collapsed.contains("\\\\") {
        collapsed = collapsed.replace("\\\\", "\\");
    }
    collapsed
}

/// One argument the way `CommandLineToArgvW` parses it. A profile name with
/// spaces or Arabic stays one argument; a trailing backslash stays a
/// backslash (it is doubled so the closing quote is not escaped).
pub fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| matches!(c, ' ' | '\t' | '\n' | '"')) {
        return arg.to_string();
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            c => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(c);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

/// Join already-split arguments for `ShellExecuteW`'s parameter string.
pub fn join_windows_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| quote_windows_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `#45`'s folder gate: [`normalize_start_dir`], then `std::path::absolute`
/// (string work, so a UNC name is not opened here), then `accept_local_cwd`,
/// then `usable_spawn_dir`. A refused folder is an error, not a fall back to home.
pub fn gate_start_dir(raw: &str) -> Result<String, String> {
    let raw_owned = normalize_start_dir(raw);
    let raw = raw_owned.as_str();
    std::path::absolute(raw)
        .ok()
        .and_then(|p| p.to_str().and_then(crate::term::accept_local_cwd))
        .filter(|d| crate::term::usable_spawn_dir(d))
        .ok_or_else(|| format!("--cwd {raw:?} is not a local folder Bayan can open"))
}

/// The directory half of [`parse`], gated. What the `#45` test calls.
#[cfg(test)]
pub fn start_directory<I, S>(args: I) -> Result<Option<String>, String>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    match parse(args)? {
        CliAction::Launch(launch) => match launch.directory {
            Some(raw) => gate_start_dir(&raw).map(Some),
            None => Ok(None),
        },
        _ => Err(format!("that command does not open a folder\n{USAGE}")),
    }
}

pub fn launch_process(
    profile_command: Option<&str>,
    command: Option<&[String]>,
    default_shell: &str,
) -> LaunchProcess {
    if let Some(argv) = command {
        LaunchProcess {
            command_line: join_command(argv),
            raw_argv: Some(argv.to_vec()),
        }
    } else {
        LaunchProcess {
            command_line: profile_command.unwrap_or(default_shell).to_string(),
            raw_argv: None,
        }
    }
}

fn join_command(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if arg.contains([' ', '\t', '"']) {
                format!("\"{}\"", arg.replace('"', "\\\""))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Write `text` to the parent console when there is one (a `cmd` launch of
/// the window-subsystem binary). `false` means the caller should show a dialog.
pub fn write_console(text: &str) -> bool {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetConsoleMode, GetStdHandle, WriteConsoleW, ATTACH_PARENT_PROCESS,
        STD_ERROR_HANDLE,
    };

    // SAFETY: AttachConsole takes a process id and no pointers. Attaching
    // fails harmlessly when this process already has a console, or when the
    // parent has none (Explorer). GetStdHandle / GetConsoleMode / WriteConsoleW
    // then use the standard error handle only if it is a real console.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return false;
        }
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        if !text.ends_with('\n') {
            wide.push('\n' as u16);
        }
        let mut written = 0u32;
        WriteConsoleW(
            handle,
            wide.as_ptr(),
            wide.len() as u32,
            &mut written,
            std::ptr::null(),
        ) != 0
    }
}

/// The executable path, without the `\\?\` prefix `current_exe` sometimes
/// returns. A quote cannot be escaped inside the menu command.
pub fn exe_display(exe: &Path) -> Result<String, String> {
    let path = exe.to_str().ok_or("executable path is not valid Unicode")?;
    let path = match path.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest,
        _ => path,
    };
    if path.is_empty() || path.contains(['"', '\n', '\r']) {
        return Err("executable path cannot be quoted safely".into());
    }
    Ok(path.to_string())
}

#[cfg(test)]
mod tests {
    use std::os::windows::ffi::OsStringExt;

    use super::*;

    fn launch(args: &[&str]) -> Launch {
        match parse(args.iter().copied()).unwrap() {
            CliAction::Launch(launch) => launch,
            other => panic!("expected a launch, got {other:?}"),
        }
    }

    #[test]
    fn profile_command_help_and_version_parse() {
        let folder = r"D:\مجلد العمل\مشروع";
        let got = launch(&["-d", folder, "-p", "PowerShell سريع"]);
        assert_eq!(got.directory.as_deref(), Some(folder));
        assert_eq!(got.profile.as_deref(), Some("PowerShell سريع"));
        assert!(got.command.is_none());

        let got = launch(&["--cwd", folder, "--profile", "Command Prompt"]);
        assert_eq!(got.directory.as_deref(), Some(folder));
        assert_eq!(got.profile.as_deref(), Some("Command Prompt"));

        let joined = format!("--cwd={folder}");
        let profile = "--profile=PowerShell سريع".to_string();
        let got = launch(&[&joined, &profile]);
        assert_eq!(got.directory.as_deref(), Some(folder));
        assert_eq!(got.profile.as_deref(), Some("PowerShell سريع"));

        let script = r"C:\مسار فيه مسافة\ملف.py";
        let got = launch(&[
            "-p",
            "PowerShell",
            "--",
            "python.exe",
            script,
            "arg with space",
        ]);
        assert_eq!(
            got.command.as_deref(),
            Some(
                [
                    "python.exe".to_string(),
                    script.to_string(),
                    "arg with space".to_string()
                ]
                .as_slice()
            )
        );
        // `--` ends flags: `-d` here is the program, not a folder.
        let got = launch(&["--", "-d", folder]);
        assert!(got.directory.is_none());
        assert_eq!(
            got.command.as_deref(),
            Some(["-d".to_string(), folder.to_string()].as_slice())
        );

        assert_eq!(parse(["--help"]).unwrap(), CliAction::Help);
        assert_eq!(parse(["-h"]).unwrap(), CliAction::Help);
        assert_eq!(parse(["--version"]).unwrap(), CliAction::Version);
        assert_eq!(parse(["-V"]).unwrap(), CliAction::Version);
        assert_eq!(
            parse(["--register-context-menu"]).unwrap(),
            CliAction::RegisterContextMenu
        );
        assert_eq!(
            parse(["--unregister-context-menu"]).unwrap(),
            CliAction::UnregisterContextMenu
        );
        assert!(launch(&["--elevated"]).elevated);
        let elevated = launch(&["--elevated", "-p", "PowerShell سريع"]);
        assert!(elevated.elevated);
        assert_eq!(elevated.profile.as_deref(), Some("PowerShell سريع"));
        assert!(elevated.command.is_none());
        assert!(elevated.directory.is_none());
        let window = launch(&["--new-window"]);
        assert!(!window.elevated);
        assert!(window.profile.is_none());
        assert_eq!(parse(["--handoff"]).unwrap(), CliAction::Handoff);
        assert_eq!(parse(["-Embedding"]).unwrap(), CliAction::Handoff);
        assert_eq!(
            parse(["--handoff", "-Embedding"]).unwrap(),
            CliAction::Handoff
        );
        assert_eq!(
            parse(["--register-terminal-handoff"]).unwrap(),
            CliAction::RegisterTerminalHandoff
        );
        assert_eq!(
            parse(["--unregister-terminal-handoff"]).unwrap(),
            CliAction::UnregisterTerminalHandoff
        );
    }

    #[test]
    fn invalid_arguments_are_errors_and_do_not_panic() {
        for args in [
            vec!["--bogus"],
            vec!["-x"],
            vec!["-d"],
            vec!["--cwd"],
            vec!["-p"],
            vec!["--profile"],
            vec!["--profile="],
            vec!["-p", "   "],
            vec!["--"],
            vec!["--", ""],
            vec!["--register-context-menu", "--unregister-context-menu"],
            vec!["--register-context-menu", "-d", r"C:\Windows"],
            vec!["-p", "PowerShell", "--unregister-context-menu"],
            vec!["--handoff", "-p", "PowerShell"],
            vec!["--elevated", "--handoff"],
            vec!["--new-window", "--handoff"],
            vec![
                "--register-terminal-handoff",
                "--unregister-terminal-handoff",
            ],
            vec!["--register-context-menu", "--register-terminal-handoff"],
            vec!["--register-terminal-handoff", "-d", r"C:\Windows"],
            vec!["not-a-flag"],
        ] {
            assert!(parse(args.iter().copied()).is_err(), "{args:?}");
        }
        // an unpaired surrogate is not Unicode; parsing reports it
        let bad = OsString::from_wide(&[0xD800]);
        assert!(parse([bad]).is_err());
    }

    #[test]
    fn an_existing_folder_with_arabic_and_spaces_passes_the_cwd_gate() {
        let dir = std::env::temp_dir().join("bayan cwd اختبار");
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.to_str().expect("temp path is Unicode");
        let got = start_directory(["-d", raw]).unwrap();
        assert_eq!(
            got.as_deref(),
            crate::term::accept_local_cwd(raw).as_deref()
        );
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn the_launch_command_keeps_spaced_arguments_and_prefers_an_explicit_command() {
        let plain = launch_process(None, None, "powershell.exe");
        assert_eq!(plain.command_line, "powershell.exe");
        assert!(plain.raw_argv.is_none());

        let profile = launch_process(Some("pwsh.exe -NoLogo"), None, "powershell.exe");
        assert_eq!(profile.command_line, "pwsh.exe -NoLogo");
        assert!(profile.raw_argv.is_none());

        let argv = vec![
            "python.exe".into(),
            r"C:\مجلد العمل\ملف.py".into(),
            "arg with space".into(),
        ];
        let command = launch_process(Some("powershell.exe"), Some(&argv), "powershell.exe");
        assert_eq!(command.raw_argv.as_deref(), Some(argv.as_slice()));
        assert!(command.command_line.contains(r"مجلد العمل"));
        assert!(command.command_line.contains("arg with space"));
    }

    /// CommandLineToArgvW: backslashes are literal unless they precede a quote.
    /// An odd run before a quote emits a literal quote and does not close it,
    /// which is how `"C:\"` arrives as `C:"`.
    fn split_windows_command_line(cmd: &str) -> Vec<String> {
        let chars: Vec<char> = cmd.chars().collect();
        let mut i = 0;
        let mut args = Vec::new();
        while i < chars.len() {
            while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                i += 1;
            }
            if i >= chars.len() {
                break;
            }
            let mut cur = String::new();
            let mut in_quotes = false;
            while i < chars.len() {
                if chars[i] == '\\' {
                    let start = i;
                    while i < chars.len() && chars[i] == '\\' {
                        i += 1;
                    }
                    let slashes = i - start;
                    if i < chars.len() && chars[i] == '"' {
                        cur.extend(std::iter::repeat_n('\\', slashes / 2));
                        if slashes % 2 == 1 {
                            cur.push('"');
                            i += 1;
                        }
                    } else {
                        cur.extend(std::iter::repeat_n('\\', slashes));
                    }
                    continue;
                }
                if chars[i] == '"' {
                    in_quotes = !in_quotes;
                    i += 1;
                    continue;
                }
                if (chars[i] == ' ' || chars[i] == '\t') && !in_quotes {
                    break;
                }
                cur.push(chars[i]);
                i += 1;
            }
            args.push(cur);
        }
        args
    }

    #[test]
    fn the_menu_command_quotes_spaces_arabic_and_a_verbatim_prefix() {
        assert_eq!(
            exe_display(Path::new(r"\\?\C:\Program Files\بيان\bayan.exe")).unwrap(),
            r"C:\Program Files\بيان\bayan.exe"
        );
        assert!(exe_display(Path::new("C:\\a\"b\\bayan.exe")).is_err());
    }

    #[test]
    fn windows_argv_of_a_drive_root_a_spaced_path_a_unc_path_and_a_folder() {
        // `"C:\"` is what Explorer handed `-d "%V"` at a drive root.
        let broken = split_windows_command_line(r#""C:\bayan.exe" -d "C:\""#);
        assert_eq!(broken[2], "C:\"");
        assert_eq!(normalize_start_dir(&broken[2]), r"C:\");
        assert_eq!(gate_start_dir(&broken[2]).unwrap(), r"C:\");
        assert_eq!(launch(&["-d", "C:\""]).directory.as_deref(), Some(r"C:\"));

        // `%V\.` at `C:\` is `C:\\.` (the root's own slash plus the template's).
        let registered = crate::context_menu::context_menu_command(Path::new(r"C:\bayan.exe"))
            .unwrap()
            .replace("%V", r"C:\");
        let root = split_windows_command_line(&registered);
        assert_eq!(root[2], r"C:\\.");
        assert_eq!(normalize_start_dir(&root[2]), r"C:\.");
        assert_eq!(gate_start_dir(&root[2]).unwrap(), r"C:\");

        let spaced = split_windows_command_line(
            r#""C:\Program Files\Bayan\bayan.exe" -d "C:\Program Files\مشروع""#,
        );
        assert_eq!(spaced[0], r"C:\Program Files\Bayan\bayan.exe");
        assert_eq!(spaced[2], r"C:\Program Files\مشروع");
        assert_eq!(normalize_start_dir(&spaced[2]), r"C:\Program Files\مشروع");
        assert_eq!(
            crate::term::accept_local_cwd(&spaced[2]).as_deref(),
            Some(r"C:\Program Files\مشروع")
        );
        let spaced_menu =
            crate::context_menu::context_menu_command(Path::new(r"C:\Program Files\bayan.exe"))
                .unwrap()
                .replace("%V", r"C:\Program Files");
        let spaced_root = split_windows_command_line(&spaced_menu);
        assert_eq!(spaced_root[2], r"C:\Program Files\.");
        assert_eq!(
            crate::term::accept_local_cwd(&normalize_start_dir(&spaced_root[2])).as_deref(),
            Some(r"C:\Program Files")
        );

        let unc = split_windows_command_line(r#""C:\bayan.exe" -d "\\server\share\secret""#);
        assert_eq!(unc[2], r"\\server\share\secret");
        assert!(
            gate_start_dir(&unc[2]).is_err(),
            "a UNC folder is still refused"
        );

        let normal = split_windows_command_line(r#""C:\bayan.exe" -d "C:\Windows""#);
        assert_eq!(normal[2], r"C:\Windows");
        assert_eq!(
            gate_start_dir(&normal[2]).unwrap(),
            crate::term::accept_local_cwd(r"C:\Windows").unwrap()
        );
    }

    #[test]
    fn windows_quoting_keeps_arabic_spaces_and_a_trailing_backslash() {
        assert_eq!(quote_windows_arg("PowerShell"), "PowerShell");
        assert_eq!(quote_windows_arg("PowerShell سريع"), "\"PowerShell سريع\"");
        assert_eq!(
            quote_windows_arg(r"C:\مجلد العمل\bayan.exe"),
            "\"C:\\مجلد العمل\\bayan.exe\""
        );
        // The closing quote must not eat the final backslash.
        assert_eq!(
            quote_windows_arg(r"C:\Program Files\"),
            "\"C:\\Program Files\\\\\""
        );
        assert_eq!(quote_windows_arg("a\"b"), "\"a\\\"b\"");
        let joined =
            join_windows_args(&["--elevated".into(), "-p".into(), "PowerShell سريع".into()]);
        assert_eq!(joined, "--elevated -p \"PowerShell سريع\"");
    }
}
