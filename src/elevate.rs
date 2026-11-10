//! Run as administrator, only when the user asks.
//!
//! The request is the `--elevated` flag (the jump list's "تشغيل كمسؤول"
//! task, or the same flag typed at a prompt). The relaunch is
//! `ShellExecuteW` with the verb `runas`, which is the ordinary UAC
//! consent dialog. There is no manifest elevation, no scheduled task, no
//! COM elevation moniker, and no other verb.
//!
//! The elevated process is a new process. The only thing it is given is
//! the argv chosen for that click: `--elevated`, and `-p <profile name>`
//! when a profile was named. A folder, a `--` command, and a profile name
//! that is a path are refused here, before `ShellExecuteW`. The elevated
//! process looks the name up in its own shell detection. It does not read
//! a command line out of the argument, and this module does not write the
//! request to the environment, the registry, a pipe, or a file.
//!
//! `~/.bayan` is writable by the same user before UAC. An elevated window
//! does not load `plugins/` (a plugin `send` would reach the shell after the
//! confirm card) and does not use `config.json`'s `editor` template (that
//! string is a program to spawn). `session.json` is not restored and is not
//! written. `config.json`'s `shell` is not what new tabs run. Nothing under
//! `~/.bayan` is written, including `jumplist.stamp` and a settings save, so
//! a junction planted on that directory is not followed by an elevated
//! process. Keybindings in `config.json` only remap fixed actions; they have
//! no send or command string. There is no snippets file. Theme colours and
//! font names do not run anything.
//!
//! PowerShell is started with `-NoProfile`, and the elevated setup script
//! does not inject oh-my-posh. The theme path would come from `$PROFILE` or
//! `POSH_THEME`, and `oh-my-posh` would be resolved from the user's `PATH`.
//! `Ctrl+click` on `path:line` looks for `Code.exe` only under Program Files
//! and `%LOCALAPPDATA%\Programs\Microsoft VS Code`, not on `PATH`.

use std::path::Path;

use crate::cli::{self, Launch};

/// The only elevation verb this program uses.
pub const RUNAS_VERB: &str = "runas";

/// OS window title. The drawn chrome has no system title bar, so the
/// prefix is also painted in the tab strip.
pub fn window_title(elevated: bool) -> String {
    if elevated {
        format!(
            "{} · Bayan — بيان",
            crate::lang::tr("مسؤول", "Administrator")
        )
    } else {
        "Bayan — بيان".to_string()
    }
}

/// Whether startup may reopen `session.json`. An elevated window never does.
pub fn may_restore_saved_tabs(elevated: bool, restore_tabs: bool, has_saved_tabs: bool) -> bool {
    !elevated && restore_tabs && has_saved_tabs
}

/// Whether this process may create or replace a file under `~/.bayan`.
/// Session, settings, and the jump-list stamp all go through this.
pub fn may_write_bayan(elevated: bool) -> bool {
    !elevated
}

/// The command a new tab starts. Elevated windows never use `config_shell`.
pub fn spawn_shell_command(
    elevated: bool,
    profile_command: Option<&str>,
    installed: &[crate::shells::Shell],
    config_shell: &str,
) -> String {
    if elevated {
        profile_command
            .filter(|command| !command.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| crate::shells::elevated_default_command(installed))
    } else {
        config_shell.to_string()
    }
}

/// Title and body of the card shown when an elevated window opens.
pub fn admin_notice() -> (&'static str, &'static str) {
    (
        crate::lang::tr("نافذة مسؤول", "Administrator window"),
        crate::lang::tr(
            "لا تُحمَّل الإضافات ولا الجلسة، ولا تُحقن سمة الموجّه، ولا يُؤخذ المحرّر أو الصدفة من الإعدادات.",
            "Plugins, the saved session, the prompt theme, and the configured shell and editor are not used.",
        ),
    )
}

/// `config.json`'s editor template, or nothing when this window is elevated.
/// The template is a program argv. `Code.exe` is then resolved only from
/// the known install folders, not from `PATH`.
pub fn configured_editor(elevated: bool, editor: Option<&str>) -> Option<&str> {
    if elevated {
        None
    } else {
        editor.map(str::trim).filter(|s| !s.is_empty())
    }
}

/// The visible tab-strip word, or nothing when this window is not elevated.
pub fn title_badge(elevated: bool) -> Option<&'static str> {
    elevated.then(|| crate::lang::tr("مسؤول", "Administrator"))
}

/// A profile name that may cross the UAC boundary. A path, a quote, a
/// control character, or a second flag is not a name.
pub fn elevation_profile_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(refused());
    }
    if name.starts_with('-') || name.contains("--") {
        return Err(refused());
    }
    let bare = name.chars().all(|c| {
        !c.is_control()
            && !matches!(
                c,
                '\\' | '/'
                    | ':'
                    | '"'
                    | '\''
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'
                    | '\u{202b}'
                    | '\u{202c}'
                    | '\u{202d}'
                    | '\u{202e}'
                    | '\u{2066}'
                    | '\u{2067}'
                    | '\u{2068}'
                    | '\u{2069}'
            )
    });
    if !bare {
        return Err(refused());
    }
    Ok(name)
}

fn refused() -> String {
    "لا تُمرَّر أوامر أو مسارات إلى النافذة المرتفعة — an elevated window accepts only --elevated and -p <profile name>".to_string()
}

/// Argv for the elevated process. `Err` means do not relaunch: the request
/// included a command, a folder, or a profile name that is not a bare name.
/// The `Ok` vector never contains those, and it is the only payload
/// [`relaunch`] puts on the command line.
pub fn forward_args(launch: &Launch) -> Result<Vec<String>, String> {
    if !launch.elevated {
        return Err(refused());
    }
    if launch.command.is_some() || launch.directory.is_some() {
        return Err(refused());
    }
    let mut args = vec!["--elevated".to_string()];
    if let Some(name) = &launch.profile {
        args.push("-p".into());
        args.push(elevation_profile_name(name)?.to_string());
    }
    Ok(args)
}

/// `ShellExecuteW` lpParameters. Only the strings [`forward_args`] returned.
pub fn relaunch_parameters(args: &[String]) -> Result<String, String> {
    checked_forward(args)?;
    Ok(cli::join_windows_args(args))
}

fn checked_forward(args: &[String]) -> Result<(), String> {
    match args {
        [flag] if flag == "--elevated" => Ok(()),
        [flag, dash_p, name] if flag == "--elevated" && dash_p == "-p" => {
            elevation_profile_name(name).map(|_| ())
        }
        _ => Err(refused()),
    }
}

/// True when this process token is elevated (UAC's high integrity token),
/// not merely when the user is a member of Administrators. A failed query
/// is treated as not elevated, so the only way up is the UAC prompt.
pub fn process_is_elevated() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: the process handle from GetCurrentProcess is a pseudo-handle
    // and is not closed. OpenProcessToken writes one token handle, which
    // CloseHandle releases. GetTokenInformation writes into `elevation`,
    // whose size is passed explicitly.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut written = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut written,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Start `exe` elevated with `args` and return. The caller exits. This does
/// not wait, does not set a directory, and does not pass an environment
/// block — `ShellExecuteW` has no parameter for one.
pub fn relaunch(exe: &Path, args: &[String]) -> Result<(), String> {
    let parameters = relaunch_parameters(args)?;
    let exe = cli::exe_display(exe)?;
    shell_execute_runas(&exe, &parameters)
}

fn shell_execute_runas(exe: &str, parameters: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let verb = wide(RUNAS_VERB);
    let file = wide(exe);
    let params = wide(parameters);
    // SAFETY: the three buffers are NUL-terminated and live for the call.
    // The window and directory pointers are null: no owner window, and no
    // directory is forwarded. The verb is the fixed string `runas`.
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if (rc as isize) > 32 {
        Ok(())
    } else {
        Err(format!(
            "لم تتم الموافقة على التشغيل كمسؤول ({}) — administrator approval was not granted",
            rc as isize
        ))
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(profile: Option<&str>, directory: Option<&str>, command: Option<&[&str]>) -> Launch {
        Launch {
            directory: directory.map(str::to_string),
            profile: profile.map(str::to_string),
            command: command.map(|argv| argv.iter().map(|s| (*s).to_string()).collect()),
            elevated: true,
        }
    }

    #[test]
    fn forward_args_keep_a_profile_name_with_arabic_and_spaces() {
        let args = forward_args(&launch(Some("PowerShell سريع"), None, None)).unwrap();
        assert_eq!(
            args,
            vec![
                "--elevated".to_string(),
                "-p".to_string(),
                "PowerShell سريع".to_string()
            ]
        );
        let params = relaunch_parameters(&args).unwrap();
        assert_eq!(params, "--elevated -p \"PowerShell سريع\"");
        assert!(!params.contains('\\'));
        assert!(!params.contains("-- "));
        assert_eq!(RUNAS_VERB, "runas");
    }

    #[test]
    fn forward_args_reject_commands_and_paths() {
        let rejected = [
            launch(None, None, Some(&["cmd.exe", "/c", "whoami"])),
            launch(Some("PowerShell"), Some(r"C:\Users\me"), None),
            launch(Some(r"C:\Windows\System32\cmd.exe"), None, None),
            launch(Some(r"\\server\share"), None, None),
            launch(Some("D:/مجلد/bayan.exe"), None, None),
            launch(Some("--elevated"), None, None),
            launch(Some("name\nsecond"), None, None),
            launch(
                Some("PowerShell سريع"),
                None,
                Some(&[r"C:\مجلد العمل\ملف.py", "arg with space"]),
            ),
        ];
        for launch in &rejected {
            assert!(forward_args(launch).is_err(), "{launch:?}");
        }
        // A caller that skips forward_args still cannot build parameters
        // that smuggle a command or a path.
        assert!(
            relaunch_parameters(&["--elevated".into(), "--".into(), "cmd.exe".into()]).is_err()
        );
        assert!(relaunch_parameters(&["--elevated".into(), r"C:\Windows".into()]).is_err());
        assert!(
            relaunch_parameters(&["--elevated".into(), "-d".into(), r"C:\tmp".into()]).is_err()
        );
    }

    #[test]
    fn an_unelevated_launch_is_not_forwarded() {
        let mut launch = launch(Some("PowerShell"), None, None);
        launch.elevated = false;
        assert!(forward_args(&launch).is_err());
    }

    #[test]
    fn an_elevated_window_does_not_restore_or_write_the_session_or_its_home() {
        assert!(!may_restore_saved_tabs(true, true, true));
        assert!(!may_write_bayan(true));
        assert!(may_restore_saved_tabs(false, true, true));
        assert!(!may_restore_saved_tabs(false, false, true));
        assert!(!may_restore_saved_tabs(false, true, false));
        assert!(may_write_bayan(false));
    }

    #[test]
    fn an_elevated_new_tab_does_not_run_the_configured_shell() {
        let installed = [crate::shells::Shell {
            name: "PowerShell".into(),
            command: "powershell.exe".into(),
        }];
        let planted = r"C:\Users\me\payload.exe";
        assert_eq!(
            spawn_shell_command(true, None, &installed, planted),
            "powershell.exe"
        );
        assert_eq!(
            spawn_shell_command(true, Some("pwsh.exe -NoLogo"), &installed, planted),
            "pwsh.exe -NoLogo"
        );
        assert_eq!(
            spawn_shell_command(false, None, &installed, "pwsh.exe"),
            "pwsh.exe"
        );
        assert!(!spawn_shell_command(true, None, &installed, planted).contains("payload"));
    }

    #[test]
    fn the_admin_notice_says_what_is_left_out() {
        let (title, body) = admin_notice();
        assert!(title.contains("مسؤول") || title.contains("Administrator"));
        assert!(body.contains("الإضافات"));
        assert!(body.contains("الجلسة"));
        assert!(body.contains("سمة"));
        assert!(body.contains("المحرّر"));
    }

    #[test]
    fn an_elevated_window_does_not_keep_a_configured_editor_command() {
        assert_eq!(
            configured_editor(true, Some(r"C:\Users\me\planted.exe {file}")),
            None
        );
        assert_eq!(configured_editor(true, Some("   ")), None);
        assert_eq!(
            configured_editor(false, Some("code -g {file}:{line}")),
            Some("code -g {file}:{line}")
        );
        assert_eq!(configured_editor(false, None), None);
    }

    #[test]
    fn the_title_says_the_window_is_elevated() {
        assert_eq!(window_title(false), "Bayan — بيان");
        assert!(window_title(true).starts_with("مسؤول"));
        assert_eq!(title_badge(false), None);
        assert_eq!(title_badge(true), Some("مسؤول"));
    }

    #[test]
    fn elevation_is_only_shellexecute_runas() {
        let src = include_str!("elevate.rs");
        let prod = src.split("mod tests").next().unwrap();
        assert!(prod.contains("ShellExecuteW"));
        assert!(prod.contains("runas"));
        for banned in [
            "schtasks",
            "ITaskService",
            "Elevation:Administrator",
            "CMLuaUtil",
            "fodhelper",
            "computerdefaults",
            "autoElevate",
            "requireAdministrator",
            "ShellExecuteEx",
            "CreateProcess",
            "SetEnvironmentVariable",
            "RegSetValue",
        ] {
            assert!(
                !prod
                    .to_ascii_lowercase()
                    .contains(&banned.to_ascii_lowercase()),
                "{banned} must not be an elevation path"
            );
        }
    }
}
