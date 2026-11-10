//! OSC 133 shell integration for the shells besides the PowerShell wrapper
//! in `term.rs`.
//!
//! PowerShell gets its prompt function on the `-Command` line. Bash cannot:
//! `--rcfile` is the hook, and it *replaces* `~/.bashrc`, so the generated
//! file sources the user's rc first (when this process is not elevated) and
//! then sets `PROMPT_COMMAND`, `PS0` and `PS1`. Nothing here rewrites a file
//! in the user's home. WSL uses the same bash file, plus a `ZDOTDIR` for
//! zsh, after a launcher that reads the distro's login shell — a distro
//! whose shell is neither bash nor zsh is started as itself, with no
//! injection. cmd only has `PROMPT`, so it can emit A and B and cannot
//! report an exit code.
//!
//! The nonce travels in `BAYAN_PROMPT_NONCE`, the same variable PowerShell
//! uses, and the scripts unset it before a command runs. It is not written
//! into the rc file or the command line. An unknown program is left alone,
//! and `"shell_integration": false` turns the whole thing off.

use std::path::{Component, Path, PathBuf};

use crate::term::{fill_random, split_command, NONCE_ENV, NONCE_OFF};

/// Which shells can receive prompt marks. [`ShellKind::Unknown`] never does,
/// whatever the config says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellKind {
    PowerShell,
    Bash,
    Wsl,
    Cmd,
    Unknown,
}

/// Program name of `command`, which may be a full command line.
pub(crate) fn shell_kind(command: &str) -> ShellKind {
    let (prog, _) = split_command(command);
    let base = prog.rsplit(['\\', '/']).next().unwrap_or(&prog);
    let name = base.to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    match name {
        "powershell" | "pwsh" => ShellKind::PowerShell,
        "cmd" => ShellKind::Cmd,
        "wsl" => ShellKind::Wsl,
        // `git-bash.exe` is bash. A bare substring "bash" would also match
        // `notbash`, so the hyphen is part of that rule.
        "bash" | "git-bash" => ShellKind::Bash,
        n if n.ends_with("-bash") => ShellKind::Bash,
        // `sh` is not bash (it may be dash). Windows `zsh.exe` is not the
        // WSL case; zsh is handled inside the WSL launcher. Both stay unknown.
        _ => ShellKind::Unknown,
    }
}

/// Config on, and a real nonce. `NONCE_OFF` and an empty nonce inject
/// nothing: a missing nonce would make every `133` mark trusted.
pub(crate) fn injection_allowed(enabled: bool, nonce: &str) -> bool {
    enabled && !nonce.is_empty() && nonce != NONCE_OFF
}

/// One pane's injection, before any file is written. Paths in `args` are
/// the token `@file:<relative>`, filled in by [`materialize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    pub kind: ShellKind,
    /// Arguments after the program. The program itself is unchanged.
    pub args: Vec<String>,
    /// `(relative path, body)`. Empty for cmd, which needs no file.
    pub files: Vec<(String, String)>,
    /// Environment variable → relative path under the secure directory.
    /// WSL learns the Windows path this way; `WSLENV` translates it.
    pub path_env: Vec<(String, String)>,
    /// Literal environment entries. Includes the nonce variable.
    pub env: Vec<(String, String)>,
    /// Create the scripts outside the user profile, with a high integrity label.
    pub elevated: bool,
    /// `WSLENV` names that are Windows paths (`/up`).
    pub wslenv_path: Vec<String>,
    /// `WSLENV` names passed through unchanged (`/u`). The nonce is one.
    pub wslenv_plain: Vec<String>,
}

/// `None` when this command line must be started exactly as given: the
/// feature is off, the nonce is unusable, the shell is unknown, PowerShell
/// (it has its own wrapper), or `wsl.exe` was already given a command.
pub(crate) fn plan(command: &str, enabled: bool, elevated: bool, nonce: &str) -> Option<Plan> {
    if !injection_allowed(enabled, nonce) {
        return None;
    }
    let kind = shell_kind(command);
    let (_, args) = split_command(command);
    let env = vec![(NONCE_ENV.to_string(), nonce.to_string())];
    match kind {
        ShellKind::Bash => Some(bash_plan(&args, elevated, env)),
        ShellKind::Wsl => wsl_plan(&args, elevated, env),
        ShellKind::Cmd => Some(cmd_plan(&args, env)),
        ShellKind::PowerShell | ShellKind::Unknown => None,
    }
}

fn bash_plan(args: &[String], elevated: bool, env: Vec<(String, String)>) -> Plan {
    // Elevated: the rcfile is the only file bash will read (`--rcfile`
    // suppresses both `/etc/bash.bashrc` and `~/.bashrc`), and it does not
    // source either. A login profile would pull in `~/.bash_profile`.
    // `--norc` / `--noprofile` are the user asking for the same skip.
    let login = !elevated && is_login(args) && !has_flag(args, "--noprofile");
    let bashrc = !elevated && !has_flag(args, "--norc");
    Plan {
        kind: ShellKind::Bash,
        args: bash_args(args, "@file:bash.sh"),
        files: vec![("bash.sh".into(), bash_rc(login, bashrc))],
        path_env: Vec::new(),
        env,
        wslenv_path: Vec::new(),
        wslenv_plain: Vec::new(),
        elevated,
    }
}

fn wsl_plan(args: &[String], elevated: bool, env: Vec<(String, String)>) -> Option<Plan> {
    // `wsl -e` / `wsl -- cmd` is already a command, not an interactive login.
    if args.iter().any(|a| a == "-e" || a == "--exec" || a == "--") {
        return None;
    }
    let mut launch = args.to_vec();
    launch.push("-e".into());
    launch.push("/bin/sh".into());
    launch.push("-c".into());
    launch.push(wsl_launcher());
    // WSL's own interactive start is a login shell. The rcfile imitates
    // that, because `-e` would otherwise skip `/etc/profile`.
    let source_user = !elevated;
    Some(Plan {
        kind: ShellKind::Wsl,
        args: launch,
        files: vec![
            ("bash.sh".into(), bash_rc(source_user, source_user)),
            ("zsh/.zshenv".into(), zsh_env(source_user)),
            ("zsh/.zprofile".into(), zsh_profile(source_user)),
            ("zsh/.zshrc".into(), zsh_rc(source_user)),
            ("zsh/.zlogin".into(), zsh_login(source_user)),
        ],
        path_env: vec![
            ("BAYAN_BASH_RC".into(), "bash.sh".into()),
            ("BAYAN_ZDOTDIR".into(), "zsh".into()),
        ],
        env,
        wslenv_path: vec!["BAYAN_BASH_RC".into(), "BAYAN_ZDOTDIR".into()],
        wslenv_plain: vec![NONCE_ENV.into()],
        elevated,
    })
}

fn cmd_plan(args: &[String], env: Vec<(String, String)>) -> Plan {
    Plan {
        kind: ShellKind::Cmd,
        args: cmd_args(args, &cmd_setup()),
        files: Vec::new(),
        path_env: Vec::new(),
        env,
        wslenv_path: Vec::new(),
        wslenv_plain: Vec::new(),
        elevated: false,
    }
}

fn is_login(args: &[String]) -> bool {
    args.iter().any(|a| {
        a == "--login"
            || a == "-l"
            || (a.starts_with('-')
                && !a.starts_with("--")
                && a.chars().skip(1).all(|c| matches!(c, 'i' | 'l' | 's'))
                && a.contains('l'))
    })
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// `--login` is removed. Bash ignores `--rcfile` for a login shell, so the
/// rcfile sources `/etc/profile` itself when the original line asked for a
/// login. `-i` stays, or is added: without it bash would not be interactive
/// and would not print `PS1`.
fn bash_args(original: &[String], rc_token: &str) -> Vec<String> {
    let mut out = vec!["--rcfile".to_string(), rc_token.to_string()];
    let mut interactive = false;
    let mut i = 0;
    while i < original.len() {
        let a = &original[i];
        if a == "--rcfile" || a == "--init-file" {
            i += 2;
            continue;
        }
        if a == "--login" || a == "-l" {
            i += 1;
            continue;
        }
        if a == "-i" || a == "--interactive" {
            interactive = true;
            out.push(a.clone());
            i += 1;
            continue;
        }
        if let Some(flags) = a.strip_prefix('-').filter(|s| {
            !s.is_empty() && !s.starts_with('-') && s.chars().all(|c| matches!(c, 'i' | 'l' | 's'))
        }) {
            if flags.contains('i') {
                interactive = true;
                out.push("-i".into());
            }
            i += 1;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    if !interactive {
        out.push("-i".into());
    }
    out
}

fn cmd_args(original: &[String], setup: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut installed = false;
    while i < original.len() {
        let a = &original[i];
        if !installed && (a.eq_ignore_ascii_case("/k") || a.eq_ignore_ascii_case("/c")) {
            out.push(a.clone());
            i += 1;
            if i < original.len() {
                out.push(format!("{setup}&{}", original[i]));
                i += 1;
            } else {
                out.push(setup.to_string());
            }
            installed = true;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    if !installed {
        out.push("/K".into());
        out.push(setup.to_string());
    }
    out
}

/// `chcp` keeps Arabic paths readable. `$E` is ESC. cmd has no BEL in
/// `PROMPT`, so the terminator is ST (`ESC \`). There is no `D` and no `C`:
/// cmd prints `PROMPT` only when it draws the next prompt, and it has no
/// hook that runs as a command starts or that can see `ERRORLEVEL` without
/// the user typing it. `%BAYAN_PROMPT_NONCE%` expands once, into `PROMPT`,
/// and the variable is then cleared so a child does not inherit it under
/// that name. `PROMPT` itself still contains the nonce — cmd has nowhere
/// else to keep it — so a program in that pane can read it back.
fn cmd_setup() -> String {
    "chcp 65001>nul&prompt $E]133;A;%BAYAN_PROMPT_NONCE%$E\\$P$G$E]133;B;%BAYAN_PROMPT_NONCE%$E\\&set BAYAN_PROMPT_NONCE=".to_string()
}

fn bash_rc(login_profile: bool, bashrc: bool) -> String {
    let mut s = String::from(BASH_PRELUDE);
    if login_profile {
        s.push_str(BASH_LOGIN);
    }
    if bashrc {
        // A login profile on Git for Windows sets MSYSTEM and has already
        // sourced ~/.bashrc via /etc/bash.bashrc. Sourcing it again would
        // prepend PATH twice. Everywhere else MSYSTEM is unset and --rcfile
        // really did skip the file, so this is the load.
        if login_profile {
            s.push_str(BASH_BASHRC_UNLESS_GIT);
        } else {
            s.push_str(BASH_BASHRC);
        }
    }
    s.push_str(BASH_HOOKS);
    s
}

const BASH_PRELUDE: &str = r#"# Bayan shell integration for this pane. Deleted when the pane closes.
# The nonce is taken out of the environment before any user file runs, so a
# program the shell starts does not inherit it. It stays in an unexported
# variable, the same idea as PowerShell's $global:__BY_N.
if [ -n "${__BAYAN_SI:-}" ]; then
  return 0 2>/dev/null || true
fi
__BAYAN_SI=1
__BAYAN_NONCE=${BAYAN_PROMPT_NONCE-}
unset BAYAN_PROMPT_NONCE
if [ -z "$__BAYAN_NONCE" ]; then
  return 0 2>/dev/null || true
fi
"#;

const BASH_LOGIN: &str = r#"
# --login cannot be combined with --rcfile (bash ignores the rcfile). This
# is the load a login shell would have done.
if [ -r /etc/profile ]; then
  . /etc/profile
fi
if [ -r "$HOME/.bash_profile" ]; then
  . "$HOME/.bash_profile"
elif [ -r "$HOME/.bash_login" ]; then
  . "$HOME/.bash_login"
elif [ -r "$HOME/.profile" ]; then
  . "$HOME/.profile"
fi
"#;

const BASH_BASHRC: &str = r#"
if [ -r "$HOME/.bashrc" ]; then
  . "$HOME/.bashrc"
fi
"#;

const BASH_BASHRC_UNLESS_GIT: &str = r#"
if [ -r "$HOME/.bashrc" ] && [ -z "${MSYSTEM:-}" ]; then
  . "$HOME/.bashrc"
fi
"#;

/// Marks match PowerShell's wrapper: `D;<exit>;<nonce>`, `A;<nonce>`, then
/// the prompt text, then `B;<nonce>`. `C` is `PS0`, which bash prints after
/// the line is read and before it runs. `\[ \]` are bash's zero-width
/// markers so the escapes are not counted as columns and are not printed.
/// The user's `PROMPT_COMMAND` is saved first and `eval`'d after D and A,
/// which is when bash itself would have run it (before `PS1` is drawn).
const BASH_HOOKS: &str = r#"
__bayan_report_cwd() {
  __bayan_win=$PWD
  if command -v cygpath >/dev/null 2>&1; then
    __bayan_win=$(cygpath -w "$PWD" 2>/dev/null) || return 0
  elif command -v wslpath >/dev/null 2>&1; then
    __bayan_win=$(wslpath -w "$PWD" 2>/dev/null) || return 0
  fi
  case "$__bayan_win" in
    [A-Za-z]:*) ;;
    *) return 0 ;;
  esac
  case "$__bayan_win" in
    *$'\033'*|*$'\007'*|*$'\n'*|*$'\r'*) return 0 ;;
  esac
  printf '\033]9;9;%s\007' "$__bayan_win"
  __bayan_p=${__bayan_win//\\//}
  __bayan_enc=""
  __bayan_i=0
  while [ "$__bayan_i" -lt "${#__bayan_p}" ]; do
    __bayan_c=${__bayan_p:$__bayan_i:1}
    case "$__bayan_c" in
      [A-Za-z0-9._~/:-]) __bayan_enc="${__bayan_enc}${__bayan_c}" ;;
      " ") __bayan_enc="${__bayan_enc}%20" ;;
      *) return 0 ;;
    esac
    __bayan_i=$((__bayan_i + 1))
  done
  printf '\033]7;file:///%s\007' "$__bayan_enc"
}

__bayan_pc_items=()
if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == declare\ -a* ]]; then
  __bayan_pc_items=("${PROMPT_COMMAND[@]}")
elif [ -n "${PROMPT_COMMAND-}" ]; then
  __bayan_pc_items=("$PROMPT_COMMAND")
fi

__bayan_prompt_command() {
  local __bayan_ec=$?
  printf '\033]133;D;%s;%s\007' "$__bayan_ec" "$__BAYAN_NONCE"
  printf '\033]133;A;%s\007' "$__BAYAN_NONCE"
  __bayan_report_cwd
  local __bayan_cmd
  for __bayan_cmd in "${__bayan_pc_items[@]}"; do
    if [ -n "$__bayan_cmd" ]; then
      eval "$__bayan_cmd" || true
    fi
  done
  local __bayan_b
  __bayan_b=$'\[\033]133;B;'"${__BAYAN_NONCE}"$'\007\]'
  PS1=${PS1-}
  PS1=${PS1//"$__bayan_b"/}
  PS1="${PS1}${__bayan_b}"
  case "${PS0-}" in
    *']133;C;'*) ;;
    *) PS0="\[\033]133;C;${__BAYAN_NONCE}\007\]${PS0-}" ;;
  esac
  return 0
}
PROMPT_COMMAND=__bayan_prompt_command
case "${PS0-}" in
  *']133;C;'*) ;;
  *) PS0="\[\033]133;C;${__BAYAN_NONCE}\007\]${PS0-}" ;;
esac
"#;

fn zsh_env(source_user: bool) -> String {
    let mut s = String::from(ZSH_ENV_HEAD);
    if source_user {
        s.push_str("[[ -f \"$HOME/.zshenv\" ]] && source \"$HOME/.zshenv\"\n");
    }
    s
}

fn zsh_profile(source_user: bool) -> String {
    if source_user {
        "[[ -f \"$HOME/.zprofile\" ]] && source \"$HOME/.zprofile\"\n".into()
    } else {
        "# elevated: no user zprofile\n".into()
    }
}

fn zsh_rc(source_user: bool) -> String {
    let mut s = String::new();
    if source_user {
        s.push_str("[[ -f \"$HOME/.zshrc\" ]] && source \"$HOME/.zshrc\"\n");
    }
    s.push_str(ZSH_HOOKS);
    s.push_str("__bayan_ensure_hooks\n");
    s
}

fn zsh_login(source_user: bool) -> String {
    let mut s = String::new();
    if source_user {
        s.push_str("[[ -f \"$HOME/.zlogin\" ]] && source \"$HOME/.zlogin\"\n");
    }
    // A user .zlogin may have replaced precmd_functions. Put the marks back.
    s.push_str("__bayan_ensure_hooks\n");
    s
}

const ZSH_ENV_HEAD: &str = r#"# Bayan ZDOTDIR for this pane. zsh reads these files instead of ~/.zsh*.
if [[ -z ${__BAYAN_NONCE:-} ]]; then
  __BAYAN_NONCE=${BAYAN_PROMPT_NONCE-}
  unset BAYAN_PROMPT_NONCE
fi
"#;

const ZSH_HOOKS: &str = r#"
__bayan_report_cwd() {
  local win="$PWD"
  if (( $+commands[cygpath] )); then
    win="$(cygpath -w "$PWD" 2>/dev/null)" || return 0
  elif (( $+commands[wslpath] )); then
    win="$(wslpath -w "$PWD" 2>/dev/null)" || return 0
  fi
  case "$win" in
    [A-Za-z]:*) ;;
    *) return 0 ;;
  esac
  case "$win" in
    *$'\e'*|*$'\a'*|*$'\n'*|*$'\r'*) return 0 ;;
  esac
  printf '\033]9;9;%s\007' "$win"
  local p="${win//\\//}"
  p="${p// /%20}"
  case "$p" in
    *%*) ;;
    *) printf '\033]7;file:///%s\007' "$p" ;;
  esac
}

__bayan_precmd() {
  local ec=$?
  printf '\033]133;D;%s;%s\007' "$ec" "$__BAYAN_NONCE"
  printf '\033]133;A;%s\007' "$__BAYAN_NONCE"
  __bayan_report_cwd
}

__bayan_preexec() {
  printf '\033]133;C;%s\007' "$__BAYAN_NONCE"
}

__bayan_ps1() {
  local mark=$'%{\e]133;B;'"${__BAYAN_NONCE}"$'\a%}'
  PS1=${PS1//${mark}/}
  PS1="${PS1}${mark}"
}

__bayan_ensure_hooks() {
  precmd_functions=(${precmd_functions:#__bayan_precmd})
  precmd_functions=(${precmd_functions:#__bayan_ps1})
  preexec_functions=(${preexec_functions:#__bayan_preexec})
  precmd_functions=(__bayan_precmd $precmd_functions __bayan_ps1)
  preexec_functions=(__bayan_preexec $preexec_functions)
}
"#;

/// Runs under `wsl.exe -e /bin/sh -c`. POSIX `sh`, because the distro's
/// `/bin/sh` may be dash. The login shell comes from the passwd database,
/// not from assuming bash. `WSLENV` with `/p` rewrites the path variables
/// to `/mnt/…` before this starts; a value that still looks like `C:\…`
/// is handed to `wslpath`.
fn wsl_launcher() -> String {
    r#"to_unix() {
  _p=$1
  case "$_p" in
    [A-Za-z]:*)
      if command -v wslpath >/dev/null 2>&1; then
        wslpath -u "$_p" 2>/dev/null && return 0
      fi
      ;;
  esac
  printf '%s\n' "$_p"
}
rc=$(to_unix "${BAYAN_BASH_RC-}")
zdot=$(to_unix "${BAYAN_ZDOTDIR-}")
name=$(id -un 2>/dev/null) || name=${USER:-root}
line=
if command -v getent >/dev/null 2>&1; then
  line=$(getent passwd "$name" 2>/dev/null) || line=
fi
if [ -z "$line" ] && [ -r /etc/passwd ]; then
  while IFS= read -r row; do
    case "$row" in
      "$name":*) line=$row; break ;;
    esac
  done < /etc/passwd
fi
shell=${line##*:}
if [ -z "$shell" ]; then
  shell=${SHELL:-/bin/bash}
fi
base=${shell##*/}
case "$base" in
  bash)
    if [ -n "$rc" ] && [ -f "$rc" ]; then
      exec "$shell" --rcfile "$rc" -i
    fi
    ;;
  zsh)
    if [ -n "$zdot" ] && [ -d "$zdot" ]; then
      ZDOTDIR=$zdot
      export ZDOTDIR
      exec "$shell" -i -l
    fi
    ;;
esac
unset BAYAN_PROMPT_NONCE
unset BAYAN_BASH_RC
unset BAYAN_ZDOTDIR
unset ZDOTDIR
exec "$shell" -i
"#
    .to_string()
}

/// Append our `WSLENV` entries. An entry the user already has for the same
/// name is replaced, so a stale `/p`-less value cannot shadow the path
/// translation. Other entries are kept, in order.
pub(crate) fn merge_wslenv(existing: &str, add: &[&str]) -> String {
    let mut parts: Vec<String> = existing
        .split(':')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    for item in add {
        let name = item.split('/').next().unwrap_or(item);
        if let Some(pos) = parts.iter().position(|p| p.split('/').next() == Some(name)) {
            parts[pos] = (*item).to_string();
        } else {
            parts.push((*item).to_string());
        }
    }
    parts.join(":")
}

fn wslenv_additions(plan: &Plan) -> Vec<String> {
    let mut add = Vec::new();
    for name in &plan.wslenv_path {
        add.push(format!("{name}/up"));
    }
    for name in &plan.wslenv_plain {
        add.push(format!("{name}/u"));
    }
    add
}

/// What the terminal sees for one bash/zsh prompt redraw: D, A, the prompt
/// text, B. The same bytes PowerShell's wrapper writes, so the mark parser
/// treats them as the same command block. `C` is not part of the prompt; it
/// is printed earlier, when the command starts.
#[cfg(test)]
pub(crate) fn trusted_prompt_frame(exit: i32, nonce: &str, prompt: &str) -> Vec<u8> {
    let mut v = format!("\x1b]133;D;{exit};{nonce}\x07").into_bytes();
    v.extend(format!("\x1b]133;A;{nonce}\x07").into_bytes());
    v.extend(prompt.as_bytes());
    v.extend(format!("\x1b]133;B;{nonce}\x07").into_bytes());
    v
}

/// `PS0` / zsh `preexec`: command start. The parser records the mark and
/// does not treat it as a prompt boundary.
#[cfg(test)]
pub(crate) fn command_started_mark(nonce: &str) -> Vec<u8> {
    format!("\x1b]133;C;{nonce}\x07").into_bytes()
}

/// cmd's `PROMPT`. ST (`ESC \`) rather than BEL, because `$E` can emit ESC
/// and `\` is an ordinary prompt character. No D, so the block has no exit.
#[cfg(test)]
pub(crate) fn cmd_prompt_frame(nonce: &str, prompt: &str) -> Vec<u8> {
    let mut v = format!("\x1b]133;A;{nonce}\x1b\\").into_bytes();
    v.extend(prompt.as_bytes());
    v.extend(format!("\x1b]133;B;{nonce}\x1b\\").into_bytes());
    v
}

/// Arguments with `@file:` tokens replaced, environment filled in, and the
/// directory the files live in (None for cmd). The caller deletes that
/// directory when the pane closes.
pub(crate) struct Materialized {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub dir: Option<PathBuf>,
}

pub(crate) fn materialize(plan: &Plan) -> Result<Materialized, String> {
    let existing = std::env::var("WSLENV").unwrap_or_default();
    let additions: Vec<String> = wslenv_additions(plan);
    let addition_refs: Vec<&str> = additions.iter().map(String::as_str).collect();
    let mut env = plan.env.clone();
    if !additions.is_empty() {
        env.push(("WSLENV".into(), merge_wslenv(&existing, &addition_refs)));
    }
    if plan.files.is_empty() {
        return Ok(Materialized {
            args: plan.args.clone(),
            env,
            dir: None,
        });
    }
    let elevated = plan.elevated;
    // A SACL (the high integrity label) can be set only while
    // SeSecurityPrivilege is enabled. The elevated token has it, disabled.
    let _privilege = elevated.then(PrivilegeGuard::enable);
    let dir = create_integration_dir(elevated).map_err(|e| e.to_string())?;
    let cleanup = IntegrationDir(Some(dir.clone()));
    for (rel, body) in &plan.files {
        let path = resolve_rel(&dir, rel).ok_or_else(|| format!("refusing path {rel}"))?;
        if let Some(parent) = path.parent() {
            if parent != dir {
                match create_dir_secure(parent, elevated) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.to_string()),
                }
            }
        }
        write_secure(&path, body.as_bytes(), elevated).map_err(|e| e.to_string())?;
    }
    let mut args = Vec::with_capacity(plan.args.len());
    for arg in &plan.args {
        if let Some(rel) = arg.strip_prefix("@file:") {
            let path = resolve_rel(&dir, rel).ok_or_else(|| format!("refusing path {rel}"))?;
            args.push(path.display().to_string());
        } else {
            args.push(arg.clone());
        }
    }
    for (key, rel) in &plan.path_env {
        let path = resolve_rel(&dir, rel).ok_or_else(|| format!("refusing path {rel}"))?;
        env.push((key.clone(), path.display().to_string()));
    }
    let dir = cleanup.disarm();
    Ok(Materialized { args, env, dir })
}

fn resolve_rel(dir: &Path, rel: &str) -> Option<PathBuf> {
    if !rel_is_safe(rel) {
        return None;
    }
    Some(dir.join(rel))
}

fn rel_is_safe(rel: &str) -> bool {
    if rel.is_empty() || rel.len() > 80 {
        return false;
    }
    let path = Path::new(rel);
    let mut n = 0;
    for c in path.components() {
        match c {
            Component::Normal(s) => {
                let Some(s) = s.to_str() else {
                    return false;
                };
                if s.is_empty()
                    || s.contains(['\\', '/', ':', '\0'])
                    || s.chars().any(|ch| ch.is_control())
                {
                    return false;
                }
                n += 1;
            }
            _ => return false,
        }
    }
    (1..=2).contains(&n)
}

const DIR_PREFIX: &str = "bayan-si-";

fn integration_dirname(hex: &str) -> String {
    format!("{DIR_PREFIX}{hex}")
}

fn is_integration_dirname(name: &str) -> bool {
    let Some(hex) = name.strip_prefix(DIR_PREFIX) else {
        return false;
    };
    hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `dir` may be deleted only when it is one of our random names sitting
/// directly in the user temp directory or in ProgramData. Anything else
/// (a profile path, `C:\`, a name we did not mint) is left untouched.
pub(crate) fn dir_is_removable(dir: &Path, temp: &Path, programdata: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if !is_integration_dirname(name) {
        return false;
    }
    let Some(parent) = dir.parent() else {
        return false;
    };
    same_dir(parent, temp) || same_dir(parent, programdata)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    norm(a) == norm(b)
}

pub(crate) fn remove_integration_dir(dir: &Path) {
    if !dir_is_removable(dir, &std::env::temp_dir(), &programdata_dir()) {
        return;
    }
    for rel in [
        "bash.sh",
        "zsh/.zshenv",
        "zsh/.zprofile",
        "zsh/.zshrc",
        "zsh/.zlogin",
    ] {
        let _ = std::fs::remove_file(dir.join(rel));
    }
    let _ = std::fs::remove_dir(dir.join("zsh"));
    let _ = std::fs::remove_dir(dir);
}

/// Holds the directory until [`IntegrationDir::disarm`]. Dropping it early
/// (spawn failed) deletes the scripts.
pub(crate) struct IntegrationDir(pub Option<PathBuf>);

impl IntegrationDir {
    pub(crate) fn disarm(mut self) -> Option<PathBuf> {
        self.0.take()
    }
}

impl Drop for IntegrationDir {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            remove_integration_dir(&dir);
        }
    }
}

fn programdata_dir() -> PathBuf {
    std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
}

/// Where a new integration directory is created. Elevated files go in
/// ProgramData, not under the user profile: the user's temp directory is
/// writable by the medium-integrity token of the same account, which could
/// replace a script between create and the elevated shell reading it.
pub(crate) fn integration_parent(elevated: bool) -> PathBuf {
    if elevated {
        programdata_dir()
    } else {
        std::env::temp_dir()
    }
}

/// DACL Bayan puts on the directory and every file in it.
///
/// Not elevated: the current user only, protected so the directory does not
/// inherit a world-writable ACE. Elevated: SYSTEM and Administrators, plus
/// a high integrity label with no-write-up, so the filtered token cannot
/// rewrite the script the elevated shell is about to read.
pub(crate) fn security_sddl(elevated: bool) -> &'static str {
    if elevated {
        "D:P(A;;FA;;;SY)(A;;FA;;;BA)S:(ML;;NW;;;HI)"
    } else {
        "D:P(A;;FA;;;CU)"
    }
}

/// This process token is elevated (UAC's high token, including Explorer's
/// Run as administrator). A failed query is not elevated: the token bit is
/// the only signal. Tests pass the boolean into [`plan`] and do not call this.
pub(crate) fn is_elevated() -> bool {
    token_is_elevated()
}

/// Enables `SeSecurityPrivilege` for this function's lifetime, then puts it
/// back. Drop runs on every return, including a failed create.
struct PrivilegeGuard(bool);

impl PrivilegeGuard {
    fn enable() -> Self {
        Self(set_privilege("SeSecurityPrivilege", true))
    }
}

impl Drop for PrivilegeGuard {
    fn drop(&mut self) {
        if self.0 {
            let _ = set_privilege("SeSecurityPrivilege", false);
        }
    }
}

fn set_privilege(name: &str, enable: bool) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_SUCCESS};
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let wide = wide_str(name);
    // SAFETY: GetCurrentProcess is a pseudo-handle. OpenProcessToken writes
    // one token handle, closed below. LookupPrivilegeValueW writes one LUID.
    // AdjustTokenPrivileges takes that token and a one-entry privilege list
    // that outlives the call. GetLastError is read before CloseHandle, which
    // would otherwise replace it.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        ) == 0
        {
            return false;
        }
        let mut luid = windows_sys::Win32::Foundation::LUID {
            LowPart: 0,
            HighPart: 0,
        };
        if LookupPrivilegeValueW(std::ptr::null(), wide.as_ptr(), &mut luid) == 0 {
            CloseHandle(token);
            return false;
        }
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: if enable { SE_PRIVILEGE_ENABLED } else { 0 },
            }],
        };
        let ok = AdjustTokenPrivileges(
            token,
            0,
            &privileges,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        let err = GetLastError();
        CloseHandle(token);
        ok != 0 && err == ERROR_SUCCESS
    }
}

fn token_is_elevated() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: GetCurrentProcess returns a pseudo-handle that is not closed.
    // OpenProcessToken writes one token handle, and CloseHandle releases it.
    // GetTokenInformation writes into `elevation`; the size is the struct's.
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
        let _ = CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

fn random_hex() -> Option<String> {
    let mut bytes = [0u8; 16];
    if !fill_random(&mut bytes) {
        return None;
    }
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(b"0123456789abcdef"[(b >> 4) as usize]));
        s.push(char::from(b"0123456789abcdef"[(b & 0xf) as usize]));
    }
    Some(s)
}

fn create_integration_dir(elevated: bool) -> std::io::Result<PathBuf> {
    let parent = integration_parent(elevated);
    let mut last = std::io::Error::other("no random name");
    for _ in 0..4 {
        let Some(hex) = random_hex() else {
            return Err(std::io::Error::other("RtlGenRandom failed"));
        };
        let dir = parent.join(integration_dirname(&hex));
        match create_dir_secure(&dir, elevated) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

fn create_dir_secure(path: &Path, elevated: bool) -> std::io::Result<()> {
    with_security(elevated, |sa| {
        let wide = wide_path(path);
        // SAFETY: `wide` is NUL-terminated and outlives the call. `sa` points
        // at a SECURITY_ATTRIBUTES whose descriptor stays alive for the call.
        // A zero return is failure; the descriptor is freed by `with_security`.
        let ok =
            unsafe { windows_sys::Win32::Storage::FileSystem::CreateDirectoryW(wide.as_ptr(), sa) };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

fn write_secure(path: &Path, body: &[u8], elevated: bool) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    };

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const CREATE_NEW: u32 = 1;

    with_security(elevated, |sa| {
        let wide = wide_path(path);
        // SAFETY: `wide` is NUL-terminated. `sa` is the same shape as in
        // `create_dir_secure`. CREATE_NEW fails if the name exists, so a
        // raced file is not truncated. The share mode is read, not write.
        // INVALID_HANDLE_VALUE is the failure return. CloseHandle releases a
        // real handle.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ,
                sa,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let write = write_all(handle, body);
        // SAFETY: `handle` is the CreateFileW handle from above, not
        // INVALID_HANDLE_VALUE. CloseHandle releases it.
        unsafe { CloseHandle(handle) };
        write
    })
}

fn write_all(
    handle: windows_sys::Win32::Foundation::HANDLE,
    mut body: &[u8],
) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    while !body.is_empty() {
        let mut wrote = 0u32;
        // SAFETY: `body` is readable for `body.len()` bytes. `wrote` is a
        // u32 the call fills. A zero return is failure and nothing past
        // `wrote` is treated as stored.
        let ok = unsafe {
            WriteFile(
                handle,
                body.as_ptr(),
                body.len().min(u32::MAX as usize) as u32,
                &mut wrote,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if wrote == 0 {
            return Err(std::io::Error::other("short write"));
        }
        body = &body[wrote as usize..];
    }
    Ok(())
}

fn with_security<T>(
    elevated: bool,
    f: impl FnOnce(*const windows_sys::Win32::Security::SECURITY_ATTRIBUTES) -> std::io::Result<T>,
) -> std::io::Result<T> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    let sddl = wide_str(security_sddl(elevated));
    let mut sd = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated and outlives the call. `sd` receives
    // one LocalAlloc'd descriptor. A zero return leaves `sd` unset. LocalFree
    // releases it after `f` returns, including on error.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || sd.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let result = f(&sa);
    // SAFETY: `sd` is the descriptor ConvertString... just allocated. LocalFree
    // releases that one pointer. `f` has returned, so it no longer uses it.
    unsafe { LocalFree(sd.cast()) };
    result
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// DACL of `path` as an SDDL string. Label and owner are not requested, so
/// this does not need the security privilege.
#[cfg(test)]
pub(crate) fn dacl_sddl(path: &Path) -> std::io::Result<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    use windows_sys::Win32::Security::GetFileSecurityW;

    const DACL_SECURITY_INFORMATION: u32 = 0x4;
    let wide = wide_path(path);
    let mut needed = 0u32;
    // SAFETY: the first call passes a null buffer and a zero length, which
    // is the documented way to read `needed`. The second writes `needed`
    // bytes into `buf`. The converted string is LocalFree'd.
    unsafe {
        GetFileSecurityW(
            wide.as_ptr(),
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut buf = vec![0u8; needed as usize];
    // SAFETY: `buf` is writable for `needed` bytes. `wide` is NUL-terminated.
    let ok = unsafe {
        GetFileSecurityW(
            wide.as_ptr(),
            DACL_SECURITY_INFORMATION,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut out = std::ptr::null_mut();
    let mut out_len = 0u32;
    // SAFETY: `buf` holds the descriptor from GetFileSecurityW. `out` receives
    // one LocalAlloc'd wide string.
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            buf.as_mut_ptr().cast(),
            1,
            DACL_SECURITY_INFORMATION,
            &mut out,
            &mut out_len,
        )
    };
    if ok == 0 || out.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `out` is the wide string just allocated, and `out_len` counts
    // its code units including the trailing NUL. LocalFree releases it.
    let text = unsafe {
        let n = out_len.saturating_sub(1) as usize;
        let slice = std::slice::from_raw_parts(out, n);
        let text = String::from_utf16_lossy(slice);
        LocalFree(out.cast());
        text
    };
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "abc123deadbeef";

    fn bash_git() -> &'static str {
        r"C:\Program Files\Git\usr\bin\bash.exe --login -i"
    }

    fn file<'a>(plan: &'a Plan, rel: &str) -> &'a str {
        plan.files
            .iter()
            .find(|(n, _)| n == rel)
            .map(|(_, b)| b.as_str())
            .unwrap_or_else(|| panic!("missing {rel}"))
    }

    #[test]
    fn bash_rcfile_sources_the_user_bashrc_and_emits_osc_133() {
        let plan = plan(bash_git(), true, false, NONCE).expect("bash");
        assert_eq!(plan.kind, ShellKind::Bash);
        assert_eq!(plan.args[0], "--rcfile");
        assert_eq!(plan.args[1], "@file:bash.sh");
        assert!(plan.args.iter().any(|a| a == "-i"));
        assert!(!plan.args.iter().any(|a| a == "--login" || a == "-l"));
        let rc = file(&plan, "bash.sh");
        assert!(
            rc.contains("declare -p PROMPT_COMMAND"),
            "existing PROMPT_COMMAND must be read"
        );
        let save = rc.find("declare -p PROMPT_COMMAND").unwrap();
        let replace = rc.find("PROMPT_COMMAND=__bayan_prompt_command").unwrap();
        assert!(
            save < replace,
            "save the user's command before replacing it"
        );
        assert!(rc.contains("eval \"$__bayan_cmd\""));
        assert!(rc.contains(r"printf '\033]133;D;%s;%s\007'"));
        assert!(rc.contains(r"printf '\033]133;A;%s\007'"));
        assert!(rc.contains(r"\033]133;B;"));
        assert!(rc.contains(r"\033]133;C;"));
        assert!(rc.contains("unset BAYAN_PROMPT_NONCE"));
        assert!(rc.contains(r#". "$HOME/.bashrc""#), "{rc}");
        assert!(rc.contains("/etc/profile"), "login shell keeps its profile");
        assert!(rc.contains(r"\033]9;9;"));
        assert!(rc.contains(r"\033]7;file:///"));
        assert!(
            !rc.contains(NONCE),
            "nonce must not be written into the file"
        );
        assert!(!plan.args.iter().any(|a| a.contains(NONCE)));
    }

    #[test]
    fn a_non_login_bash_sources_bashrc_and_not_the_profile() {
        let plan = plan(r"C:\msys64\usr\bin\bash.exe -i", true, false, NONCE).unwrap();
        let rc = file(&plan, "bash.sh");
        assert!(rc.contains(r#". "$HOME/.bashrc""#));
        assert!(!rc.contains("/etc/profile"));
        assert!(plan.args.contains(&"-i".to_string()));
    }

    #[test]
    fn an_elevated_bash_does_not_source_user_rc_files() {
        let plan = plan(bash_git(), true, true, NONCE).unwrap();
        let rc = file(&plan, "bash.sh");
        assert!(!rc.contains("$HOME"), "{rc}");
        assert!(!rc.contains("/etc/profile"), "{rc}");
        assert!(!rc.contains(".bashrc"), "{rc}");
        assert!(rc.contains(r"printf '\033]133;D;%s;%s\007'"));
        assert_eq!(integration_parent(true), programdata_dir());
        let user = std::env::var("USERPROFILE").unwrap_or_default();
        if !user.is_empty() {
            assert!(
                !integration_parent(true).starts_with(&user),
                "elevated scripts must not sit in the user profile"
            );
        }
    }

    #[test]
    fn wsl_launcher_handles_bash_zsh_and_any_other_default_shell() {
        let cmd = r"C:\Windows\System32\wsl.exe -d Ubuntu-22.04";
        let got = plan(cmd, true, false, NONCE).expect("wsl");
        assert_eq!(got.args[0], "-d");
        assert_eq!(got.args[1], "Ubuntu-22.04");
        assert_eq!(got.args[2], "-e");
        assert_eq!(got.args[3], "/bin/sh");
        assert_eq!(got.args[4], "-c");
        let launch = &got.args[5];
        assert!(launch.contains("wslpath"), "{launch}");
        assert!(launch.contains("getent passwd"), "{launch}");
        assert!(launch.contains("/etc/passwd"), "{launch}");
        assert!(launch.contains("--rcfile"), "bash inside WSL");
        assert!(launch.contains("ZDOTDIR"), "zsh inside WSL");
        assert!(launch.contains("unset BAYAN_PROMPT_NONCE"));
        assert!(
            launch.contains("exec \"$shell\" -i\n"),
            "any other default shell starts with no rcfile"
        );
        let bash = file(&got, "bash.sh");
        assert!(bash.contains(r#". "$HOME/.bashrc""#));
        let zshrc = file(&got, "zsh/.zshrc");
        assert!(zshrc.contains(r#"source "$HOME/.zshrc""#));
        assert!(zshrc.contains(r"printf '\033]133;D;%s;%s\007'"));
        assert!(zshrc.contains(r"\033]133;C;"));
        assert!(zshrc.contains("__bayan_ensure_hooks"));
        assert!(file(&got, "zsh/.zshenv").contains("unset BAYAN_PROMPT_NONCE"));
        assert_eq!(
            got.wslenv_path,
            vec!["BAYAN_BASH_RC".to_string(), "BAYAN_ZDOTDIR".to_string()]
        );
        let merged = merge_wslenv(
            "WT_SESSION/u",
            &wslenv_additions(&got)
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        assert!(merged.starts_with("WT_SESSION/u:"));
        assert!(merged.contains("BAYAN_BASH_RC/up"));
        assert!(merged.contains("BAYAN_ZDOTDIR/up"));
        assert!(merged.contains("BAYAN_PROMPT_NONCE/u"));
        // a distro invocation that is already a command is not wrapped
        assert!(plan(
            r"C:\Windows\System32\wsl.exe -d Ubuntu -- exec fish",
            true,
            false,
            NONCE
        )
        .is_none());
    }

    #[test]
    fn elevated_wsl_does_not_source_the_linux_home() {
        let plan = plan(r"C:\Windows\System32\wsl.exe -d Debian", true, true, NONCE).unwrap();
        for (name, body) in &plan.files {
            assert!(
                !body.contains("$HOME"),
                "{name} sources a user file while elevated:\n{body}"
            );
        }
        assert!(file(&plan, "zsh/.zshrc").contains(r"printf '\033]133;A;%s\007'"));
    }

    #[test]
    fn cmd_prompt_emits_a_and_b_without_an_exit_code() {
        let got = plan("cmd.exe", true, false, NONCE).unwrap();
        assert!(got.files.is_empty(), "cmd does not need a script file");
        assert_eq!(got.args[0], "/K");
        let setup = &got.args[1];
        assert!(setup.contains("chcp 65001"), "{setup}");
        assert!(setup.contains("$E]133;A;%BAYAN_PROMPT_NONCE%"), "{setup}");
        assert!(setup.contains("$E]133;B;%BAYAN_PROMPT_NONCE%"), "{setup}");
        assert!(setup.contains("set BAYAN_PROMPT_NONCE="), "{setup}");
        assert!(!setup.contains("133;D"), "cmd cannot report an exit code");
        assert!(!setup.contains("133;C"), "cmd has no command-start hook");
        assert!(!setup.contains(NONCE), "the nonce stays in the environment");
        // an existing /K is extended, not given a second /K
        let again = plan(
            r"C:\Windows\System32\cmd.exe /K echo hi",
            true,
            false,
            NONCE,
        )
        .unwrap();
        assert_eq!(again.args[0], "/K");
        assert!(again.args[1].contains("echo hi"));
        assert!(again.args[1].starts_with("chcp 65001"));
    }

    #[test]
    fn shell_integration_off_injects_nothing() {
        for cmd in [
            "powershell.exe",
            "pwsh.exe -NoLogo",
            bash_git(),
            r"C:\Windows\System32\wsl.exe -d Ubuntu",
            "cmd.exe",
        ] {
            assert!(
                plan(cmd, false, false, NONCE).is_none(),
                "{cmd} injected while off"
            );
            assert!(!injection_allowed(false, NONCE));
        }
        assert!(!injection_allowed(true, NONCE_OFF));
        assert!(!injection_allowed(true, ""));
        assert!(injection_allowed(true, NONCE));
    }

    #[test]
    fn an_unknown_shell_is_never_injected() {
        for cmd in [
            "fish.exe",
            "nu.exe",
            "dash.exe",
            "sh.exe",
            r"C:\tools\custom.exe",
            "notbash.exe",
            r"C:\msys64\usr\bin\zsh.exe",
            "",
        ] {
            assert_eq!(shell_kind(cmd), ShellKind::Unknown, "{cmd}");
            assert!(
                plan(cmd, true, false, NONCE).is_none(),
                "{cmd} was injected"
            );
        }
        assert_eq!(
            shell_kind("powershell.exe -NoProfile"),
            ShellKind::PowerShell
        );
        assert!(plan("powershell.exe", true, false, NONCE).is_none());
        assert_eq!(
            shell_kind(r"C:\Program Files\Git\git-bash.exe"),
            ShellKind::Bash
        );
        assert_eq!(shell_kind("cmd.exe /K chcp 65001"), ShellKind::Cmd);
    }

    #[test]
    fn integration_descriptors_are_not_world_writable() {
        let user = security_sddl(false);
        let admin = security_sddl(true);
        for s in [user, admin] {
            assert!(s.contains("D:P"), "{s}");
            assert!(!s.contains("WD"), "Everyone must not be granted: {s}");
            assert!(!s.contains("AU"), "{s}");
            assert!(!s.contains("BU"), "{s}");
        }
        assert!(user.contains("CU"), "{user}");
        assert!(admin.contains("BA") && admin.contains("SY"), "{admin}");
        assert!(admin.contains("HI") && admin.contains("NW"), "{admin}");
        assert!(!admin.contains("CU"), "{admin}");
    }

    #[test]
    fn only_our_random_directory_is_removed() {
        let temp = Path::new(r"C:\Users\a\AppData\Local\Temp");
        let data = Path::new(r"C:\ProgramData");
        let good = "bayan-si-0123456789abcdef0123456789abcdef";
        assert!(dir_is_removable(&temp.join(good), temp, data));
        assert!(dir_is_removable(&data.join(good), temp, data));
        assert!(!dir_is_removable(
            &Path::new(r"C:\Users\a").join(good),
            temp,
            data
        ));
        assert!(!dir_is_removable(
            &Path::new(r"C:\Windows").join(good),
            temp,
            data
        ));
        assert!(!dir_is_removable(&temp.join("bayan-si-short"), temp, data));
        assert!(!dir_is_removable(&temp.join("not-ours"), temp, data));
        assert!(!dir_is_removable(Path::new(r"C:\"), temp, data));
    }

    #[test]
    fn file_tokens_cannot_escape_the_directory() {
        let dir = Path::new(r"C:\ProgramData\bayan-si-0123456789abcdef0123456789abcdef");
        assert_eq!(resolve_rel(dir, "bash.sh").unwrap(), dir.join("bash.sh"));
        assert_eq!(
            resolve_rel(dir, "zsh/.zshrc").unwrap(),
            dir.join("zsh").join(".zshrc")
        );
        for bad in [
            "../x",
            r"..\x",
            "/etc/passwd",
            r"C:\x",
            "a/b/c",
            "",
            "a\u{0000}b",
        ] {
            assert!(resolve_rel(dir, bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn wslenv_keeps_the_user_entries_and_replaces_ours() {
        assert_eq!(
            merge_wslenv(
                "FOO/p:BAYAN_BASH_RC/p:BAR/u",
                &["BAYAN_BASH_RC/up", "BAYAN_PROMPT_NONCE/u"]
            ),
            "FOO/p:BAYAN_BASH_RC/up:BAR/u:BAYAN_PROMPT_NONCE/u"
        );
        assert_eq!(merge_wslenv("", &["BAYAN_BASH_RC/up"]), "BAYAN_BASH_RC/up");
    }

    /// The directory really is created with that DACL, not only described.
    #[test]
    fn a_secure_directory_is_unique_and_not_world_writable() {
        let dir = create_integration_dir(false).expect("create");
        let sddl = dacl_sddl(&dir).expect("dacl");
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        remove_integration_dir(&dir);
        assert!(is_integration_dirname(&name), "{name}");
        assert!(!sddl.contains("WD"), "world-writable: {sddl}");
        assert!(!sddl.contains("AU"), "{sddl}");
        assert!(!std::fs::metadata(&dir).is_ok(), "cleanup left {dir:?}");
    }
}
