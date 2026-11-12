//! SSH as a tab: the Windows OpenSSH client, started in a new ConPTY tab.
//!
//! `ssh.exe` is `GetSystemDirectoryW()\OpenSSH\ssh.exe`. A copy on `PATH`,
//! next to `bayan.exe`, or named by an environment variable is never used —
//! the same rule as `wsl.exe`, including when the process is elevated and
//! PATH is the attacker's.
//!
//! The palette lists hosts from config.json's `ssh_hosts` and from
//! `~/.ssh/config` `Host` lines. Wildcard patterns and `Match` blocks are
//! not destinations. The argv is built here: a host that starts with `-`
//! is rejected, and `--` stands before the destination so a host cannot
//! become an ssh option. Passwords are not a parameter and are not stored;
//! the client uses the agent and keys.

use std::path::{Path, PathBuf};

/// Palette / menu label. The Arabic side is the string the UI shows by default.
pub fn label_open() -> &'static str {
    crate::lang::tr("فتح SSH", "Open SSH")
}

/// An elevated window does not offer SSH. `~/.ssh/config` can name a
/// `ProxyCommand`, and that would otherwise run as admin.
pub fn offered(elevated: bool) -> bool {
    !elevated
}

/// Why a destination was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshError {
    /// Empty, a wildcard, a leading `-`, or a character that would split
    /// or quote-break the command line.
    BadHost,
    /// System32 `\OpenSSH\ssh.exe` is not there. PATH is not a fallback.
    NoClient,
}

/// One palette row: what we pass to ssh, and the tab's title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostItem {
    /// Token after `--`: a config alias, or `user@host`.
    pub token: String,
    /// Tab title. The host, without `user@`.
    pub title: String,
}

/// A resolved launch: absolute `ssh.exe` plus an argv that cannot inject options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshLaunch {
    pub title: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// One command line for the local ConPTY path (`split_command` round-trips it).
    pub command_line: String,
}

/// `system_dir\OpenSSH\ssh.exe`. `system_dir` is whatever `GetSystemDirectoryW`
/// returned; this function does not search anywhere else.
pub fn openssh_exe(system_dir: &Path) -> PathBuf {
    system_dir.join("OpenSSH").join("ssh.exe")
}

/// Absolute `System32\OpenSSH\ssh.exe` when `is_file` says it is there.
/// A relative directory is refused: that is the app-dir / PATH search this
/// exists to avoid. No environment variable is read.
pub fn resolve_openssh(
    system_dir: Option<&Path>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let dir = system_dir.filter(|d| d.is_absolute())?;
    let exe = openssh_exe(dir);
    is_file(&exe).then_some(exe)
}

#[cfg(windows)]
pub fn system32_dir() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260];
    // SAFETY: the length passed is `buf`'s own, so the write stays inside it.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) };
    if n == 0 || n as usize >= buf.len() {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsString::from_wide(
        &buf[..n as usize],
    )))
}

#[cfg(not(windows))]
pub fn system32_dir() -> Option<PathBuf> {
    None
}

/// Destination pieces. `port` is ours (a `u16`), never a string from the host token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
}

impl SshTarget {
    pub fn host(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            user: None,
            port: None,
        }
    }
}

/// Argv for `ssh.exe`, not including the program. `-p <port>` when we have
/// a port, then `--`, then `user@host` or `host`. The destination is one
/// argument and never starts with `-`.
pub fn build_ssh_args(target: &SshTarget) -> Result<Vec<String>, SshError> {
    validate_piece(&target.host)?;
    if let Some(user) = &target.user {
        validate_piece(user)?;
        if user.contains('@') {
            return Err(SshError::BadHost);
        }
    }
    let mut args = Vec::new();
    if let Some(port) = target.port {
        if port == 0 {
            return Err(SshError::BadHost);
        }
        args.push("-p".to_string());
        args.push(port.to_string());
    }
    args.push("--".to_string());
    let dest = match &target.user {
        Some(user) => format!("{user}@{}", target.host),
        None => target.host.clone(),
    };
    args.push(dest);
    Ok(args)
}

/// A `Host` token or a typed `user@host`. Wildcards never parse.
pub fn target_from_token(token: &str) -> Result<SshTarget, SshError> {
    let token = token.trim();
    if let Some((user, host)) = token.split_once('@') {
        if host.contains('@') || user.is_empty() {
            return Err(SshError::BadHost);
        }
        validate_piece(user)?;
        validate_piece(host)?;
        Ok(SshTarget {
            host: host.to_string(),
            user: Some(user.to_string()),
            port: None,
        })
    } else {
        validate_piece(token)?;
        Ok(SshTarget::host(token))
    }
}

fn validate_piece(s: &str) -> Result<(), SshError> {
    let n = s.chars().count();
    if !(1..=253).contains(&n) || s.starts_with('-') || s == "--" {
        return Err(SshError::BadHost);
    }
    // No whitespace, quotes, or globbing: one argv slot, and `split_command`
    // cannot cut it in two. `*` `?` `[` `!` are ssh patterns, not names.
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        Ok(())
    } else {
        Err(SshError::BadHost)
    }
}

/// Command line whose program is `exe` and whose arguments are `args`.
/// Arguments are already single safe tokens; a program path with a space
/// is quoted the way `split_command` unquotes it.
pub fn command_line(exe: &Path, args: &[String]) -> String {
    let prog = exe.display().to_string();
    let mut line = if prog.contains([' ', '\t']) {
        format!("\"{prog}\"")
    } else {
        prog
    };
    for arg in args {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

/// Resolve `ssh.exe` and build the argv for `token`. `is_file` is injected
/// so the path rule is testable without OpenSSH installed.
pub fn prepare_launch(
    system_dir: Option<&Path>,
    is_file: impl Fn(&Path) -> bool,
    token: &str,
) -> Result<SshLaunch, SshError> {
    let program = resolve_openssh(system_dir, is_file).ok_or(SshError::NoClient)?;
    let target = target_from_token(token)?;
    let args = build_ssh_args(&target)?;
    let command_line = command_line(&program, &args);
    Ok(SshLaunch {
        title: target.host,
        program,
        args,
        command_line,
    })
}

/// `Host` names from an OpenSSH config file. `Match` blocks contribute
/// nothing. A pattern token (`*`, `?`, `!`, `[`) is skipped; a concrete
/// name on the same `Host` line is kept. `HostName` is not a separate row —
/// ssh itself applies the config when we pass the alias.
pub fn parse_ssh_config(text: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for line in text.lines() {
        let mut tokens = tokens(line);
        if tokens.is_empty() {
            continue;
        }
        let keyword = tokens.remove(0);
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        for token in tokens {
            if is_pattern(&token) {
                continue;
            }
            if validate_piece(&token).is_err() {
                continue;
            }
            if !hosts.iter().any(|h: &String| h == &token) {
                hosts.push(token);
            }
        }
    }
    hosts
}

fn is_pattern(token: &str) -> bool {
    token.contains(['*', '?', '!', '['])
}

/// Split a config line into tokens. `#` starts a comment outside quotes.
/// Quotes are grouping, not part of the token.
fn tokens(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = false;
    for c in line.chars() {
        if c == '"' {
            quote = !quote;
            continue;
        }
        if c == '#' && !quote {
            break;
        }
        if c.is_whitespace() && !quote {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Config.json hosts first, then names parsed from `~/.ssh/config`.
/// Unsafe tokens (a leading `-`, a wildcard) are dropped, not escaped.
pub fn host_list(configured: &[String], file_text: &str) -> Vec<HostItem> {
    let mut items = Vec::new();
    for token in configured {
        push_host(&mut items, token);
    }
    for token in parse_ssh_config(file_text) {
        push_host(&mut items, &token);
    }
    items
}

fn push_host(items: &mut Vec<HostItem>, token: &str) {
    let Ok(target) = target_from_token(token) else {
        return;
    };
    let token = match &target.user {
        Some(user) => format!("{user}@{}", target.host),
        None => target.host.clone(),
    };
    if items.iter().any(|h| h.token == token) {
        return;
    }
    items.push(HostItem {
        token,
        title: target.host,
    });
}

/// `~/.ssh/config`, or empty when it is missing or unreadable. Callers that
/// must not touch the home directory (tests) pass the text in themselves.
pub fn read_user_config() -> String {
    let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) else {
        return String::new();
    };
    let path = Path::new(&home).join(".ssh").join("config");
    std::fs::read_to_string(path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys() -> &'static Path {
        Path::new(r"C:\Windows\System32")
    }

    #[test]
    fn ssh_args_reject_option_injection_and_place_double_dash_before_the_destination() {
        let args = build_ssh_args(&SshTarget {
            host: "example.com".into(),
            user: Some("a".into()),
            port: Some(2222),
        })
        .unwrap();
        assert_eq!(
            args,
            vec![
                "-p".to_string(),
                "2222".to_string(),
                "--".to_string(),
                "a@example.com".to_string()
            ]
        );
        let dash = args.iter().position(|a| a == "--").unwrap();
        assert!(dash < args.len() - 1, "-- must precede the destination");
        assert!(!args[dash + 1].starts_with('-'));
        // the only option before -- is the port we formatted ourselves
        assert_eq!(&args[..dash], ["-p".to_string(), "2222".to_string()]);

        let plain = build_ssh_args(&SshTarget::host("example.com")).unwrap();
        assert_eq!(plain, vec!["--".to_string(), "example.com".to_string()]);

        for bad in [
            "-oProxyCommand=calc",
            "--",
            "-v",
            "",
            "example.com -o evil",
            "host\n-o",
            "host\t-o",
            "a\"b",
            "a;b",
            "*.example.com",
            "good?.com",
            "host!",
            "-example.com",
            "user@",
            "@host",
            "a@b@c",
        ] {
            assert!(
                target_from_token(bad).is_err(),
                "accepted an injectable host: {bad:?}"
            );
            assert!(
                build_ssh_args(&SshTarget::host(bad)).is_err(),
                "built argv for {bad:?}"
            );
        }
    }

    #[test]
    fn ssh_exe_is_the_system_openssh_client_not_path() {
        let exe = openssh_exe(sys());
        assert_eq!(exe, sys().join("OpenSSH").join("ssh.exe"));
        assert!(exe.is_absolute());
        // PATH and a planted ssh.exe are not arguments of the resolver.
        // Even if the probe would accept them, the only candidate is System32.
        let planted = Path::new(r"C:\Users\a\Downloads\ssh.exe");
        let got = resolve_openssh(Some(sys()), |p| p == planted || p == exe);
        assert_eq!(got.as_deref(), Some(exe.as_path()));
        assert_ne!(got.as_deref(), Some(planted));
        // a relative directory is the app-dir hole
        assert_eq!(resolve_openssh(Some(Path::new("System32")), |_| true), None);
        assert_eq!(resolve_openssh(None, |_| true), None);
        assert_eq!(
            resolve_openssh(Some(sys()), |_| false),
            None,
            "missing OpenSSH is not a PATH search"
        );

        // the production resolver has no PATH / env lookup to regress into
        let src = include_str!("ssh.rs");
        let prod = src.split("mod tests").next().unwrap_or(src);
        assert!(!prod.contains("std::env::var(\"PATH\")"));
        assert!(!prod.contains("split_paths"));
        assert!(!prod.contains("which"));
    }

    #[test]
    fn ssh_config_hosts_skip_wildcards_and_match() {
        let text = r#"
# comment
Host alpha beta
    HostName real.example
    User me

Host *
    User nope

Host *.example.com
    User x

Host good?.com
Host keep
Match host *.internal exec "calc"
    User admin
Host "gamma"
Host -inject
Host ok extra*
host Second
"#;
        assert_eq!(
            parse_ssh_config(text),
            vec![
                "alpha".to_string(),
                "beta".to_string(),
                "keep".to_string(),
                "gamma".to_string(),
                "ok".to_string(),
                "Second".to_string(),
            ]
        );
    }

    #[test]
    fn ssh_command_line_round_trips_through_split_command() {
        let exe = openssh_exe(sys());
        let launch = prepare_launch(Some(sys()), |p| p == exe, "me@example.com").unwrap();
        assert_eq!(launch.title, "example.com");
        assert_eq!(launch.program, exe);
        assert!(launch.args.iter().any(|a| a == "--"));
        assert!(
            !launch
                .args
                .iter()
                .any(|a| a.to_ascii_lowercase().contains("password")),
            "argv must not carry a password flag: {:?}",
            launch.args
        );
        let (prog, args) = crate::term::split_command(&launch.command_line);
        assert_eq!(prog, exe.display().to_string());
        assert_eq!(args, launch.args);
        let dash = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[dash + 1], "me@example.com");
    }

    #[test]
    fn ssh_launch_never_has_a_password_argument() {
        let exe = openssh_exe(sys());
        let launch = prepare_launch(Some(sys()), |p| p == exe, "example.com").unwrap();
        let rendered = format!("{launch:?}");
        assert!(!rendered.to_ascii_lowercase().contains("password"));
        assert!(prepare_launch(Some(sys()), |_| false, "example.com").is_err());
        assert!(prepare_launch(Some(sys()), |p| p == exe, "-oProxyCommand=calc").is_err());
    }

    #[test]
    fn configured_hosts_merge_with_the_file_and_drop_unsafe_tokens() {
        let file = "Host fromfile\nHost *\nMatch all\n  User root\n";
        let items = host_list(&["-evil".into(), "fromcfg".into(), "fromfile".into()], file);
        let tokens: Vec<&str> = items.iter().map(|h| h.token.as_str()).collect();
        assert_eq!(tokens, vec!["fromcfg", "fromfile"]);
        assert!(items
            .iter()
            .all(|h| h.title == h.token || h.title == "example.com"));
        let titled = host_list(&["me@example.com".into()], "");
        assert_eq!(titled[0].title, "example.com");
        assert_eq!(titled[0].token, "me@example.com");
    }

    #[test]
    fn arabic_ssh_palette_label() {
        crate::lang::set(crate::lang::Lang::Ar);
        assert_eq!(label_open(), "فتح SSH");
        crate::lang::set(crate::lang::Lang::En);
        assert_eq!(label_open(), "Open SSH");
    }

    #[test]
    fn elevated_window_does_not_offer_ssh() {
        assert!(offered(false));
        assert!(
            !offered(true),
            "an elevated window must not run ~/.ssh/config as admin"
        );
    }
}
