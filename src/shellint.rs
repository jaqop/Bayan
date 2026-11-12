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
//! report an exit code. Its marks carry no nonce and are untrusted, because
//! every child of cmd inherits `PROMPT`.
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
        // `git-bash.exe` is a mintty GUI launcher, not a shell we can hand
        // `--rcfile`. A bare substring "bash" would also match `notbash`.
        "bash" => ShellKind::Bash,
        n if n.ends_with("-bash") && n != "git-bash" => ShellKind::Bash,
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
        ShellKind::Bash => bash_plan(&args, elevated, env),
        ShellKind::Wsl => wsl_plan(&args, elevated, env),
        ShellKind::Cmd => Some(cmd_plan(&args)),
        ShellKind::PowerShell | ShellKind::Unknown => None,
    }
}

fn bash_plan(args: &[String], elevated: bool, mut env: Vec<(String, String)>) -> Option<Plan> {
    // `bash -c` and `bash script.sh` are not interactive prompts. Adding
    // `-i` would turn the script name into `$0` or a script argument.
    if !bash_is_interactive(args) {
        return None;
    }
    // A real login shell reads /etc/profile and one user profile, and does
    // not itself read ~/.bashrc. An elevated login does not read
    // /etc/profile: Git for Windows' copy runs git-prompt.sh, which
    // executes files the user can write. PATH and MSYSTEM are set from
    // this bash's install instead.
    let login = is_login(args) && !has_flag(args, "--noprofile");
    let user = !elevated && !has_flag(args, "--norc");
    if elevated {
        push_safe_readline(&mut env);
    }
    let mut bash_argv = bash_args(args, "@file:bash.sh");
    if elevated && !bash_argv.iter().any(|a| a == "--noprofile") {
        // Long options have to precede the first short option. bash
        // rejects `--noprofile` after `-i` (`--: invalid option`, exit 2)
        // and the pane never reaches a prompt.
        bash_argv.insert(0, "--noprofile".into());
    }
    Some(Plan {
        kind: ShellKind::Bash,
        args: bash_argv,
        files: vec![("bash.sh".into(), bash_rc(login, user, elevated))],
        path_env: Vec::new(),
        env,
        wslenv_path: Vec::new(),
        wslenv_plain: Vec::new(),
        elevated,
    })
}

/// readline and the history file are not affected by `--rcfile`. An elevated
/// shell must not open the user's `~/.inputrc` (a macro there runs as admin)
/// or append to a history file the user can rewrite.
fn push_safe_readline(env: &mut Vec<(String, String)>) {
    env.push(("INPUTRC".into(), "/dev/null".into()));
    env.push(("HISTFILE".into(), "/dev/null".into()));
}

fn wsl_plan(args: &[String], elevated: bool, mut env: Vec<(String, String)>) -> Option<Plan> {
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
    // that, because `-e` would otherwise skip `/etc/profile`. Elevated
    // reads that profile with HOME pointed at the rcfile directory.
    // After HOME is restored, bash-completion loads the user's
    // `completions/<cmd>` on Tab, so `BASH_COMPLETION_USER_DIR` stays
    // on the integration directory and `XDG_DATA_HOME` cannot send it
    // back to the user's files.
    let source_user = !elevated;
    if elevated {
        push_safe_readline(&mut env);
    }
    let mut plain = vec![NONCE_ENV.to_string()];
    if elevated {
        plain.push("INPUTRC".into());
        plain.push("HISTFILE".into());
    }
    Some(Plan {
        kind: ShellKind::Wsl,
        args: launch,
        files: vec![
            ("bash.sh".into(), bash_rc(true, source_user, false)),
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
        wslenv_plain: plain,
        elevated,
    })
}

/// No nonce in the environment: cmd would have to keep it in `PROMPT`,
/// which every child inherits.
fn cmd_plan(args: &[String]) -> Plan {
    Plan {
        kind: ShellKind::Cmd,
        args: cmd_args(args, &cmd_setup()),
        files: Vec::new(),
        path_env: Vec::new(),
        env: Vec::new(),
        wslenv_path: Vec::new(),
        wslenv_plain: Vec::new(),
        elevated: false,
    }
}

/// `-c` / a script path is a one-shot bash, not a prompt we can mark.
fn bash_is_interactive(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-c" || a == "--command" {
            return false;
        }
        if a == "--rcfile" || a == "--init-file" {
            i += 2;
            continue;
        }
        if let Some(flags) = a
            .strip_prefix('-')
            .filter(|s| !s.is_empty() && !s.starts_with('-'))
        {
            if flags.contains('c') {
                return false;
            }
            i += 1;
            continue;
        }
        if a.starts_with("--") {
            i += 1;
            continue;
        }
        // `bash script.sh` — the file name is not a prompt.
        return false;
    }
    true
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
    while i < original.len() {
        let a = &original[i];
        if a.eq_ignore_ascii_case("/k") || a.eq_ignore_ascii_case("/c") {
            // Everything after /K is one command. split_command broke
            // `echo hi` into two argv words; joining them back is what cmd
            // sees on a single command line.
            out.push(a.clone());
            let rest = original[i + 1..].join(" ");
            if rest.is_empty() {
                out.push(setup.to_string());
            } else {
                out.push(format!("{setup}&{rest}"));
            }
            return out;
        }
        out.push(a.clone());
        i += 1;
    }
    out.push("/K".into());
    out.push(setup.to_string());
    out
}

/// `chcp` is the same UTF-8 code page `term.rs` gives a cmd pane without
/// marks, so Arabic paths stay readable either way. `$E` is ESC. cmd has no
/// BEL in `PROMPT`, so the terminator is ST (`ESC \`). There is no `D` and
/// no `C`: cmd prints `PROMPT` only when it draws the next prompt, and it
/// has no hook that runs as a command starts or that can see `ERRORLEVEL`
/// without the user typing it.
///
/// The marks carry no nonce. `PROMPT` is inherited by every program the
/// pane runs, so a nonce in it could be read back and used to forge trusted
/// marks. `term.rs` treats these exactly like a forged mark: A resets a BiDi
/// mode the last command left on, and that is all. No block, exit, prompt
/// state or cwd trust comes from them. The user's `PROMPT` is wrapped, not replaced (cmd defines
/// `$P$G` itself when none is set).
///
/// No `"` and no trailing `\`: portable-pty quotes this argument and would
/// turn either into a backslash cmd keeps. shortcut: a raw `&` or `|` in the
/// user's `PROMPT` (rather than `$A`/`$B`) splits this line; quote it if
/// portable-pty ever passes an argument verbatim.
fn cmd_setup() -> String {
    r"prompt $E]133;A$E\%PROMPT%$E]133;B$E\&chcp 65001>nul".to_string()
}

/// `login` sources `/etc/profile` and, when `user` is set, the first of
/// `~/.bash_profile`, `~/.bash_login`, `~/.profile`. It does not also source
/// `~/.bashrc`: a login shell never does, and Ubuntu's profile already does,
/// so a second source ran ssh-agent and PATH setup twice. Non-login sources
/// `~/.bashrc` only, and only when `user` is set. `windows_elevated` is Git
/// Bash running as administrator: that profile is not read at all, because
/// its `git-prompt.sh` runs files under the user's home. WSL elevated
/// (`windows_elevated` false, `user` false) still reads the distro profile,
/// with `HOME` pointed at the rcfile directory for that read.
fn bash_rc(login: bool, user: bool, windows_elevated: bool) -> String {
    let mut s = String::from(BASH_PRELUDE);
    if login && !user && windows_elevated {
        s.push_str(BASH_ELEVATED_ENV);
    } else if login {
        if user {
            s.push_str(BASH_SYSTEM_PROFILE);
            s.push_str(BASH_USER_PROFILE);
        } else {
            s.push_str(BASH_SYSTEM_PROFILE_SAFE_HOME);
        }
    } else if user {
        s.push_str(BASH_BASHRC);
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

const BASH_SYSTEM_PROFILE: &str = r#"
# --login cannot be combined with --rcfile (bash ignores the rcfile).
# /etc/profile is what a login shell reads first. The user's own profile
# comes next and may source ~/.bashrc; this file does not.
if [ -r /etc/profile ]; then
  . /etc/profile
fi
"#;

const BASH_SYSTEM_PROFILE_SAFE_HOME: &str = r#"
# WSL elevated login. The distro's /etc/profile is not user-writable, but a
# snippet in it can name a file under $HOME. HOME is this rcfile's directory
# for that read (the protected integration directory), then restored.
# bash-completion's lazy loader then reads
# ${BASH_COMPLETION_USER_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/bash-completion}
# on Tab. The directory stays the integration directory, and clearing
# XDG_DATA_HOME means it cannot point back at the user's files.
__bayan_real_home=$HOME
__bayan_safe=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
__bayan_completion_dir=$__bayan_safe/bash-completion
HOME=$__bayan_safe
export HOME
if [ -r /etc/profile ]; then
  . /etc/profile
fi
HOME=$__bayan_real_home
export HOME
unset __bayan_real_home __bayan_safe
unset BASH_COMPLETION_USER_DIR
BASH_COMPLETION_USER_DIR=$__bayan_completion_dir
readonly BASH_COMPLETION_USER_DIR
unset XDG_DATA_HOME
unset __bayan_completion_dir
"#;

const BASH_ELEVATED_ENV: &str = r#"
# Elevated Git Bash does not source /etc/profile. Git for Windows' copy
# runs /etc/profile.d/git-prompt.sh, which sources ~/.config/git/git-prompt.sh
# and ~/bash_completion.d/*.bash. Moving HOME for that read did not stop
# the real profile from executing those files. PATH and MSYSTEM come from
# this bash's own install.
# A launcher that already chose MSYSTEM is kept. Paths are the MSYS
# root (/), not a cygpath result: //c/... is a network path and [ -d ]
# waits on it. Git for Windows 2.56 has /ucrt64 and no /mingw64.
if [ -z "${MSYSTEM:-}" ]; then
  if [ -d /ucrt64/bin ]; then
    MSYSTEM=UCRT64
  elif [ -d /mingw64/bin ]; then
    MSYSTEM=MINGW64
  elif [ -d /clangarm64/bin ]; then
    MSYSTEM=CLANGARM64
  elif [ -d /mingw32/bin ]; then
    MSYSTEM=MINGW32
  elif [ -d /clang64/bin ]; then
    MSYSTEM=CLANG64
  else
    MSYSTEM=MSYS
  fi
fi
case "$MSYSTEM" in
  MINGW32) PATH="/mingw32/bin:/usr/bin${PATH:+:$PATH}" ;;
  MINGW64) PATH="/mingw64/bin:/usr/bin${PATH:+:$PATH}" ;;
  CLANGARM64) PATH="/clangarm64/bin:/usr/bin${PATH:+:$PATH}" ;;
  CLANG64) PATH="/clang64/bin:/usr/bin${PATH:+:$PATH}" ;;
  UCRT64) PATH="/ucrt64/bin:/usr/bin${PATH:+:$PATH}" ;;
  *) PATH="/usr/bin${PATH:+:$PATH}" ;;
esac
export MSYSTEM PATH
# Do not cd to the rcfile path: a Windows path can look like a network
# share to MSYS and the cd then waits until the test kills it.
__bayan_here="${BASH_SOURCE[0]}"
__bayan_here="${__bayan_here//\\//}"
__bayan_completion_dir="${__bayan_here%/*}/bash-completion"
unset __bayan_here
unset BASH_COMPLETION_USER_DIR
BASH_COMPLETION_USER_DIR=$__bayan_completion_dir
readonly BASH_COMPLETION_USER_DIR
unset XDG_DATA_HOME
unset __bayan_completion_dir
# The system startup file already put a git prompt in PS1. That runs
# git on every prompt, and git.exe waits when this process has no
# console. A plain prompt still receives the OSC mark from the hooks.
PS1='$ '
"#;

const BASH_USER_PROFILE: &str = r#"
# The first user profile only, the same order bash uses. Do not also source
# ~/.bashrc: the profile does that when the user wants it (Ubuntu's
# ~/.profile, Git for Windows' ~/.bash_profile).
if [ -r "$HOME/.bash_profile" ]; then
  . "$HOME/.bash_profile"
elif [ -r "$HOME/.bash_login" ]; then
  . "$HOME/.bash_login"
elif [ -r "$HOME/.profile" ]; then
  . "$HOME/.profile"
fi
"#;

const BASH_BASHRC: &str = r#"
# Non-login interactive bash reads ~/.bashrc and not the profile.
if [ -r "$HOME/.bashrc" ]; then
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
# UTF-8 percent-encoding of one path. od is byte-wise, so an Arabic
# directory becomes %D8%A8… rather than being dropped. Unreserved bytes
# and the path separators stay literal.
__bayan_pct() {
  local b enc="" ch hex
  hex=$(printf '%s' "$1" | od -An -tx1)
  for b in $hex; do
    case "$b" in
      2d|2e|2f|3a|5f|7e|3[0-9]|4[1-9a-f]|5[0-9a]|6[1-9a-f]|7[0-9a])
        printf -v ch "\\x$b"
        enc="${enc}${ch}"
        ;;
      [0-9a-f][0-9a-f])
        enc="${enc}%${b^^}"
        ;;
    esac
  done
  printf '%s' "$enc"
}

__bayan_report_cwd() {
  if [ "$PWD" = "${__bayan_pwd_cache-}" ]; then
    printf '%s' "${__bayan_osc_cache-}"
    return 0
  fi
  # Cache misses too: a WSL home or a failed cygpath reports nothing, and
  # asking again on every prompt would fork a converter each time.
  __bayan_pwd_cache=$PWD
  __bayan_osc_cache=
  local __bayan_win="$PWD"
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
  local __bayan_p="${__bayan_win//\\//}"
  local __bayan_enc
  __bayan_enc=$(__bayan_pct "$__bayan_p")
  __bayan_osc_cache="$(printf '\033]9;9;%s\007\033]7;file:///%s\007' "$__bayan_win" "$__bayan_enc")"
  printf '%s' "$__bayan_osc_cache"
}

__bayan_ret() { return "$1"; }

__bayan_pc_items=()
if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == declare\ -a* ]]; then
  __bayan_pc_items=("${PROMPT_COMMAND[@]}")
elif [ -n "${PROMPT_COMMAND-}" ]; then
  __bayan_pc_items=("$PROMPT_COMMAND")
fi

__bayan_prompt_command() {
  # One builtin, so both expansions see the command that just finished.
  # Anything after this line (printf, cygpath) would leave $? at 0, and
  # starship / oh-my-posh would paint a green mark on a failed command.
  local __bayan_ec=${?} __bayan_pipe=("${PIPESTATUS[@]}")
  # Elevated Git Bash is started with HOME on the integration directory,
  # so a system profile cannot see the user's files. The real values
  # come back here, after every startup file has run.
  if [ -n "${BAYAN_REAL_HOME+x}" ]; then
    case "$BAYAN_REAL_HOME" in
      [A-Za-z]:*)
        if command -v cygpath >/dev/null 2>&1; then
          HOME=$(cygpath -u -- "$BAYAN_REAL_HOME" 2>/dev/null) || HOME=$BAYAN_REAL_HOME
        else
          HOME=$BAYAN_REAL_HOME
        fi
        ;;
      *) HOME=$BAYAN_REAL_HOME ;;
    esac
    export HOME
    unset BAYAN_REAL_HOME
  fi
  if [ -n "${BAYAN_REAL_USERPROFILE+x}" ]; then
    USERPROFILE=$BAYAN_REAL_USERPROFILE
    export USERPROFILE
    unset BAYAN_REAL_USERPROFILE
  fi
  printf '\033]133;D;%s;%s\007' "$__bayan_ec" "$__BAYAN_NONCE"
  printf '\033]133;A;%s\007' "$__BAYAN_NONCE"
  __bayan_report_cwd
  local __bayan_cmd
  for __bayan_cmd in "${__bayan_pc_items[@]}"; do
    if [ -n "$__bayan_cmd" ]; then
      __bayan_ret "$__bayan_ec"
      eval "$__bayan_cmd" || true
    fi
  done
  # \[ \] are PS1's zero-width markers. They are not for PS0: bash turns
  # them into literal SOH/STX there.
  local __bayan_b
  __bayan_b=$'\[\033]133;B;'"${__BAYAN_NONCE}"$'\007\]'
  PS1=${PS1-}
  PS1=${PS1//"$__bayan_b"/}
  PS1="${PS1}${__bayan_b}"
  case "${PS0-}" in
    *']133;C;'*) ;;
    *) PS0="$(printf '\033]133;C;%s\007' "$__BAYAN_NONCE")${PS0-}" ;;
  esac
  return 0
}
PROMPT_COMMAND=__bayan_prompt_command
case "${PS0-}" in
  *']133;C;'*) ;;
  *) PS0="$(printf '\033]133;C;%s\007' "$__BAYAN_NONCE")${PS0-}" ;;
esac
"#;

fn zsh_env(source_user: bool) -> String {
    let mut s = String::from(ZSH_ENV_HEAD);
    if source_user {
        s.push_str("__bayan_source_user .zshenv\n");
    }
    s
}

fn zsh_profile(source_user: bool) -> String {
    if source_user {
        "__bayan_source_user .zprofile\n".into()
    } else {
        "# elevated: no user zprofile\n".into()
    }
}

fn zsh_rc(source_user: bool) -> String {
    let mut s = String::new();
    if source_user {
        s.push_str("__bayan_source_user .zshrc\n");
    }
    s.push_str(ZSH_HOOKS);
    s.push_str("__bayan_ensure_hooks\n");
    // Login shells still have .zlogin to read from THIS directory. Restoring
    // ZDOTDIR here would make zsh open the user's .zlogin and skip ours.
    s.push_str("if [[ ! -o login ]]; then __bayan_finish_zdotdir; fi\n");
    s
}

fn zsh_login(source_user: bool) -> String {
    let mut s = String::new();
    if source_user {
        s.push_str("__bayan_source_user .zlogin\n");
    }
    // A user .zlogin may have replaced precmd_functions. Put the marks back,
    // then leave ZDOTDIR as the user left it so history and tmux are theirs.
    s.push_str("__bayan_ensure_hooks\n__bayan_finish_zdotdir\n");
    if !source_user {
        // After ZDOTDIR is restored, a login zsh reads ~/.zlogout on exit.
        // That file is user-writable. unsetopt rcs stops the exit read.
        // The marks are already installed.
        s.push_str("unsetopt rcs\n");
    }
    s
}

const ZSH_ENV_HEAD: &str = r#"# Bayan ZDOTDIR for this pane. zsh reads these files instead of ~/.zsh*.
# The directory is put back before the first prompt (__bayan_finish_zdotdir).
if [[ -z ${__BAYAN_NONCE:-} ]]; then
  __BAYAN_NONCE=${BAYAN_PROMPT_NONCE-}
  unset BAYAN_PROMPT_NONCE
fi

# Source one user startup file with ZDOTDIR pointed at their directory, then
# point it back here so the next file zsh opens is still ours. If their
# .zshenv sets ZDOTDIR (the XDG layout), that new value is what we keep.
__bayan_source_user() {
  local _file=$1
  local _saved=$ZDOTDIR
  local _dir=${BAYAN_USER_ZDOTDIR:-$HOME}
  if [[ -f "$_dir/$_file" ]]; then
    ZDOTDIR=$_dir
    source "$ZDOTDIR/$_file"
    # An XDG .zshenv sets ZDOTDIR to ~/.config/zsh. That value is the one
    # to keep. Leaving BAYAN_ZDOTDIR_UNSET set would drop it on the floor.
    if [[ $ZDOTDIR != $_dir ]]; then
      unset BAYAN_ZDOTDIR_UNSET
    fi
    BAYAN_USER_ZDOTDIR=$ZDOTDIR
    export BAYAN_USER_ZDOTDIR
  fi
  ZDOTDIR=$_saved
  export ZDOTDIR
}

__bayan_finish_zdotdir() {
  if [[ -n ${BAYAN_ZDOTDIR_UNSET-} ]]; then
    unset ZDOTDIR
  else
    ZDOTDIR=${BAYAN_USER_ZDOTDIR:-$HOME}
    export ZDOTDIR
  fi
  unset BAYAN_ZDOTDIR BAYAN_BASH_RC BAYAN_USER_ZDOTDIR BAYAN_ZDOTDIR_UNSET
}
"#;

const ZSH_HOOKS: &str = r#"
__bayan_pct() {
  local enc="" b ch hex
  hex=$(printf '%s' "$1" | od -An -tx1)
  for b in ${=hex}; do
    b=${(L)b}
    case $b in
      2d|2e|2f|3a|5f|7e|3[0-9]|4[1-9a-f]|5[0-9a]|6[1-9a-f]|7[0-9a])
        printf -v ch "\\x$b"
        enc+="$ch"
        ;;
      ??)
        enc+="%${(U)b}"
        ;;
    esac
  done
  print -rn -- "$enc"
}

__bayan_report_cwd() {
  if [[ $PWD == ${__bayan_pwd_cache-} ]]; then
    print -rn -- ${__bayan_osc_cache-}
    return 0
  fi
  # Cache misses too, so a Linux home does not fork wslpath every prompt.
  __bayan_pwd_cache=$PWD
  __bayan_osc_cache=
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
  local p="${win//\\//}"
  local enc
  enc=$(__bayan_pct "$p")
  __bayan_osc_cache="$(printf '\033]9;9;%s\007\033]7;file:///%s\007' "$win" "$enc")"
  print -rn -- "$__bayan_osc_cache"
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
# Remember the user's ZDOTDIR before the zsh branch replaces it. Unset means
# "zsh should use $HOME", which we restore by unsetting, not by exporting HOME.
if [ -n "${ZDOTDIR-}" ]; then
  export BAYAN_USER_ZDOTDIR="$ZDOTDIR"
  unset BAYAN_ZDOTDIR_UNSET
else
  export BAYAN_USER_ZDOTDIR="${HOME:-/}"
  export BAYAN_ZDOTDIR_UNSET=1
fi
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
/// and `\` is an ordinary prompt character. No D, so the block has no exit,
/// and no nonce (see [`cmd_setup`]).
#[cfg(test)]
pub(crate) fn cmd_prompt_frame(prompt: &str) -> Vec<u8> {
    let mut v = b"\x1b]133;A\x1b\\".to_vec();
    v.extend(prompt.as_bytes());
    v.extend(b"\x1b]133;B\x1b\\");
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
    if elevated && plan.kind == ShellKind::Bash {
        let real_home = std::env::var("HOME").ok();
        let real_profile = std::env::var("USERPROFILE").ok();
        pin_elevated_bash_home(
            &mut env,
            &dir,
            real_home.as_deref(),
            real_profile.as_deref(),
        );
    }
    let dir = cleanup.disarm();
    Ok(Materialized { args, env, dir })
}

/// Git for Windows compiles `SYS_BASHRC` to `/etc/bash.bashrc`, and that
/// file sources `git-prompt.sh` for every interactive shell that is not a
/// login shell. It runs before `--rcfile`. `--noprofile` does not stop it.
/// Point `HOME` and `USERPROFILE` at the integration directory so that
/// read cannot see the user's files. The first prompt puts the real
/// values back.
fn pin_elevated_bash_home(
    env: &mut Vec<(String, String)>,
    safe_dir: &Path,
    real_home: Option<&str>,
    real_userprofile: Option<&str>,
) {
    let safe = safe_dir.display().to_string();
    if let Some(home) = real_home.filter(|s| !s.is_empty()) {
        env.push(("BAYAN_REAL_HOME".into(), home.to_string()));
    } else if let Some(profile) = real_userprofile.filter(|s| !s.is_empty()) {
        env.push(("BAYAN_REAL_HOME".into(), profile.to_string()));
    }
    if let Some(profile) = real_userprofile.filter(|s| !s.is_empty()) {
        env.push(("BAYAN_REAL_USERPROFILE".into(), profile.to_string()));
    }
    env.push(("HOME".into(), safe.clone()));
    env.push(("USERPROFILE".into(), safe));
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
    remove_our_dir(dir);
}

/// Files only, one level, then the directory. A junction is not followed.
fn remove_our_dir(dir: &Path) {
    let _ = remove_files_here(dir);
    let zsh = dir.join("zsh");
    let _ = remove_files_here(&zsh);
    let _ = std::fs::remove_dir(&zsh);
    let _ = std::fs::remove_dir(dir);
}

fn remove_files_here(dir: &Path) -> std::io::Result<()> {
    for ent in std::fs::read_dir(dir)? {
        let ent = ent?;
        let path = ent.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() || meta.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Drop `bayan-si-*` directories left behind by a crashed process. Only a
/// name we mint, owned by this user, and last written more than 12 hours
/// ago. A pane that is still open created its directory moments ago.
pub(crate) fn sweep_stale_integration_dirs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let cutoff =
            std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(12 * 60 * 60));
        let Some(cutoff) = cutoff else {
            return;
        };
        // TokenOwner, not TokenUser. An elevated token's new directories are
        // owned by Administrators, which is the token owner, not the user SID.
        let Some(sid) = token_owner_sid().ok() else {
            return;
        };
        for parent in [std::env::temp_dir(), programdata_dir()] {
            sweep_parent(&parent, cutoff, &sid);
        }
    });
}

fn sweep_parent(parent: &Path, cutoff: std::time::SystemTime, sid: &str) {
    let Ok(rd) = std::fs::read_dir(parent) else {
        return;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        if !should_sweep(&path, cutoff, sid) {
            continue;
        }
        remove_our_dir(&path);
    }
}

fn should_sweep(path: &Path, cutoff: std::time::SystemTime, sid: &str) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if !is_integration_dirname(name) {
        return false;
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_dir() {
        return false;
    }
    let Ok(modified) = meta.modified() else {
        return false;
    };
    if modified > cutoff {
        return false;
    }
    owner_sid(path).as_deref() == Some(sid)
}

fn owner_sid(path: &Path) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;

    let wide = wide_path(path);
    // SAFETY: `wide` is NUL-terminated. GetNamedSecurityInfoW writes an
    // owner SID that points into `sd`, which LocalFree releases after the
    // string copy. ConvertSidToStringSidW allocates `out`, also LocalFree'd.
    unsafe {
        let mut owner = std::ptr::null_mut();
        let mut sd = std::ptr::null_mut();
        let err = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        );
        if err != 0 || owner.is_null() {
            if !sd.is_null() {
                LocalFree(sd.cast());
            }
            return None;
        }
        let mut out = std::ptr::null_mut();
        let ok = ConvertSidToStringSidW(owner, &mut out);
        LocalFree(sd.cast());
        if ok == 0 || out.is_null() {
            return None;
        }
        let mut n = 0usize;
        while *out.add(n) != 0 {
            n += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(out, n));
        LocalFree(out.cast());
        Some(text)
    }
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
    static CACHED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    CACHED.get_or_init(programdata_from_known_folder).clone()
}

/// `FOLDERID_ProgramData`, not the `PROGRAMDATA` environment variable. That
/// variable is per-user (`HKCU\Environment`), so a medium-integrity process
/// can point it at a directory it owns and then swap the script after we
/// create it.
fn programdata_from_known_folder() -> PathBuf {
    known_folder_programdata().unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
}

fn known_folder_programdata() -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath};

    // SAFETY: FOLDERID_ProgramData is a static GUID. A null token means the
    // current process. On success `ptr` is a CoTaskMemAlloc string; we copy
    // it and CoTaskMemFree it. A failing HRESULT leaves nothing to free.
    unsafe {
        let mut ptr = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(&FOLDERID_ProgramData, 0, std::ptr::null_mut(), &mut ptr);
        if hr != 0 || ptr.is_null() {
            return None;
        }
        let mut n = 0usize;
        while *ptr.add(n) != 0 {
            n += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, n));
        CoTaskMemFree(ptr.cast());
        if text.is_empty() {
            None
        } else {
            Some(PathBuf::from(text))
        }
    }
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
/// Not elevated: the current user's SID only, protected so the directory
/// does not inherit a world-writable ACE. `CU` is not an SDDL alias
/// (CreateDirectory returns error 1337), so the SID comes from the process
/// token. Elevated: SYSTEM and Administrators, plus a high integrity label
/// with no-write-up, so the filtered token cannot rewrite the script the
/// elevated shell is about to read.
pub(crate) fn security_sddl(elevated: bool) -> std::io::Result<String> {
    if elevated {
        Ok("D:P(A;;FA;;;SY)(A;;FA;;;BA)S:(ML;;NW;;;HI)".to_string())
    } else {
        Ok(format!("D:P(A;;FA;;;{})", current_user_sid()?))
    }
}

/// `S-1-…` form of the process token's user. That is the only SDDL trustee
/// that means "this account and nobody else".
fn current_user_sid() -> std::io::Result<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: GetCurrentProcess is a pseudo-handle. OpenProcessToken writes
    // one token, closed before return. The first GetTokenInformation sizes
    // the buffer; the second writes into `buf`, which is pointer-aligned.
    // TOKEN_USER.Sid points into that buffer and stays valid until
    // ConvertSidToStringSidW copies it. LocalFree releases the copy.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut needed = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            CloseHandle(token);
            return Err(std::io::Error::last_os_error());
        }
        let mut buf = vec![0u64; needed.div_ceil(8) as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            (buf.len() * std::mem::size_of::<u64>()) as u32,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        let mut out = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut out) == 0 || out.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut n = 0usize;
        while *out.add(n) != 0 {
            n += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(out, n));
        LocalFree(out.cast());
        if text.starts_with("S-1-") {
            Ok(text)
        } else {
            Err(std::io::Error::other("token user was not an SDDL sid"))
        }
    }
}

/// SID that owns objects this process creates. Elevated, that is usually
/// BUILTIN\Administrators rather than the user account.
fn token_owner_sid() -> std::io::Result<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenOwner, TOKEN_OWNER, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: same token and buffer rules as `current_user_sid`. TokenOwner's
    // SID points into `buf` until ConvertSidToStringSidW copies it.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut needed = 0u32;
        GetTokenInformation(token, TokenOwner, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            CloseHandle(token);
            return Err(std::io::Error::last_os_error());
        }
        let mut buf = vec![0u64; needed.div_ceil(8) as usize];
        let ok = GetTokenInformation(
            token,
            TokenOwner,
            buf.as_mut_ptr().cast(),
            (buf.len() * std::mem::size_of::<u64>()) as u32,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let owner = &*buf.as_ptr().cast::<TOKEN_OWNER>();
        let mut out = std::ptr::null_mut();
        if ConvertSidToStringSidW(owner.Owner, &mut out) == 0 || out.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut n = 0usize;
        while *out.add(n) != 0 {
            n += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(out, n));
        LocalFree(out.cast());
        if text.starts_with("S-1-") {
            Ok(text)
        } else {
            Err(std::io::Error::other("token owner was not an SDDL sid"))
        }
    }
}

/// This process token is elevated (UAC's high token, including Explorer's
/// Run as administrator). A failed query is treated as elevated: sourcing a
/// user rc file because the check broke is the failure mode this exists to
/// avoid. PR #59 carries its own copy of this check; when both are on
/// `bayan-plus`, delete this one and call that helper.
pub(crate) fn is_elevated() -> bool {
    elevated_from_query(token_is_elevated())
}

/// `None` is a failed query, not a clear "not elevated".
pub(crate) fn elevated_from_query(query: Option<bool>) -> bool {
    query.unwrap_or(true)
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

fn token_is_elevated() -> Option<bool> {
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
            return None;
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
        if ok == 0 {
            None
        } else {
            Some(elevation.TokenIsElevated != 0)
        }
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

    let sddl = wide_str(&security_sddl(elevated)?);
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
fn find_real_bash() -> Option<PathBuf> {
    for candidate in [
        r"C:\Program Files\Git\usr\bin\bash.exe",
        r"C:\Program Files (x86)\Git\usr\bin\bash.exe",
    ] {
        if Path::new(candidate).is_file() {
            return Some(PathBuf::from(candidate));
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let exe = dir.join("bash.exe");
        let text = exe.to_string_lossy().to_ascii_lowercase();
        (exe.is_file() && !text.contains("git-bash")).then_some(exe)
    })
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
        // A login shell does not read ~/.bashrc itself. The profile does,
        // once, when the user wrote it that way.
        assert!(
            !rc.contains(r#". "$HOME/.bashrc""#),
            "login rc must not source bashrc a second time: {rc}"
        );
        assert!(rc.contains("/etc/profile"), "login shell keeps its profile");
        assert!(rc.contains(r#". "$HOME/.bash_profile""#));
        let ret = rc.find("__bayan_ret \"$__bayan_ec\"").unwrap();
        let eval_at = rc.find("eval \"$__bayan_cmd\"").unwrap();
        assert!(ret < eval_at, "restore $? before the user's PROMPT_COMMAND");
        assert!(rc.contains("PIPESTATUS"), "capture the pipeline status");
        assert!(
            rc.contains("PS0=\"$(printf"),
            "PS0 is a raw OSC, not a PS1 \\[ \\] wrapper"
        );
        assert!(!rc.contains("PS0=\"\\["));
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
        assert!(
            !rc.contains("$HOME/"),
            "elevated rc must not name a file under the user's home: {rc}"
        );
        assert!(
            !rc.contains(". /etc/profile"),
            "elevated Git Bash must not source /etc/profile: {rc}"
        );
        assert!(rc.contains("MSYSTEM=UCRT64"), "{rc}");
        assert!(rc.contains("MSYSTEM=MINGW64"), "{rc}");
        let ucrt = rc.find("MSYSTEM=UCRT64").unwrap();
        let mingw = rc.find("MSYSTEM=MINGW64").unwrap();
        assert!(ucrt < mingw, "ucrt64 is preferred over mingw64: {rc}");
        let noprofile = plan
            .args
            .iter()
            .position(|a| a == "--noprofile")
            .expect("--noprofile");
        let interactive = plan.args.iter().position(|a| a == "-i").expect("-i");
        assert!(
            noprofile < interactive,
            "long options must precede -i, got {:?}",
            plan.args
        );
        assert!(!rc.contains(".bashrc"), "{rc}");
        assert!(rc.contains(r"printf '\033]133;D;%s;%s\007'"));
        assert_eq!(
            plan.env
                .iter()
                .find(|(k, _)| k == "INPUTRC")
                .map(|(_, v)| v.as_str()),
            Some("/dev/null")
        );
        assert_eq!(
            plan.env
                .iter()
                .find(|(k, _)| k == "HISTFILE")
                .map(|(_, v)| v.as_str()),
            Some("/dev/null")
        );
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
        assert!(
            !bash.contains(r#". "$HOME/.bashrc""#),
            "WSL login must not source bashrc twice"
        );
        assert!(bash.contains(r#". "$HOME/.profile""#));
        let zshrc = file(&got, "zsh/.zshrc");
        assert!(zshrc.contains("__bayan_source_user .zshrc"));
        assert!(zshrc.contains("__bayan_finish_zdotdir"));
        assert!(launch.contains("BAYAN_USER_ZDOTDIR"), "{launch}");
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
                !body.contains("__bayan_source_user ."),
                "{name} sources a user file while elevated:\n{body}"
            );
            assert!(
                !body.contains(". \"$HOME/"),
                "{name} sources a user file while elevated:\n{body}"
            );
        }
        assert!(plan.wslenv_plain.iter().any(|n| n == "INPUTRC"));
        assert!(plan.wslenv_plain.iter().any(|n| n == "HISTFILE"));
        assert!(file(&plan, "zsh/.zshrc").contains(r"printf '\033]133;A;%s\007'"));
    }

    #[test]
    fn cmd_prompt_emits_a_and_b_without_an_exit_code() {
        let got = plan("cmd.exe", true, false, NONCE).unwrap();
        assert!(got.files.is_empty(), "cmd does not need a script file");
        assert_eq!(got.args[0], "/K");
        let setup = &got.args[1];
        assert!(setup.contains(r"$E]133;A$E\"), "{setup}");
        assert!(setup.contains(r"$E]133;B$E\"), "{setup}");
        assert!(!setup.contains("133;D"), "cmd cannot report an exit code");
        assert!(!setup.contains("133;C"), "cmd has no command-start hook");
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
        assert!(again.args[1].starts_with(&cmd_setup()));
    }

    /// Every child of cmd inherits `PROMPT`. A nonce in it, or in a
    /// variable cmd expands into it, is readable by any program in the
    /// pane, which could then mint trusted marks.
    #[test]
    fn cmd_never_carries_the_nonce() {
        for line in ["cmd.exe", "cmd.exe /K echo hi"] {
            let got = plan(line, true, false, NONCE).unwrap();
            let joined = got.args.join(" ");
            assert!(!joined.contains(NONCE), "{joined}");
            assert!(!joined.contains(NONCE_ENV), "{joined}");
            assert!(
                got.env.iter().all(|(k, v)| k != NONCE_ENV && v != NONCE),
                "{:?}",
                got.env
            );
        }
    }

    /// The user's own `PROMPT` sits between A and B (cmd defines `$P$G`
    /// when none is set, so `%PROMPT%` always expands). The code page is
    /// the one Bayan already gives every cmd pane, not an extra one.
    #[test]
    fn cmd_wraps_the_users_prompt_and_keeps_the_pane_code_page() {
        let setup = cmd_setup();
        let a = setup.find("133;A").unwrap();
        let user = setup.find("%PROMPT%").expect("the user's PROMPT is kept");
        let b = setup.find("133;B").unwrap();
        assert!(a < user && user < b, "{setup}");
        assert!(!setup.contains("$P$G"), "do not replace the user's prompt");
        // portable-pty's argv quoting escapes `"` and doubles a final `\`
        assert!(!setup.contains('"') && !setup.ends_with('\\'), "{setup}");
        assert_eq!(setup.matches("chcp").count(), 1, "{setup}");
        assert!(setup.ends_with("&chcp 65001>nul"), "{setup}");
        let term = include_str!("term.rs");
        assert!(term.contains(r#"cmd.args(["/K", "chcp 65001>nul"])"#));
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
            ShellKind::Unknown,
            "git-bash.exe is a GUI launcher"
        );
        assert!(plan(r"C:\Program Files\Git\git-bash.exe", true, false, NONCE).is_none());
        assert_eq!(shell_kind("cmd.exe /K chcp 65001"), ShellKind::Cmd);
    }

    #[test]
    fn integration_descriptors_are_not_world_writable() {
        let user = security_sddl(false).expect("current user sid");
        let admin = security_sddl(true).expect("elevated sddl");
        for s in [&user, &admin] {
            assert!(s.contains("D:P"), "{s}");
            assert!(!s.contains("WD"), "Everyone must not be granted: {s}");
            assert!(!s.contains("AU"), "{s}");
            assert!(!s.contains("BU"), "{s}");
            // `CU` is not an SDDL alias; CreateDirectory rejects it.
            assert!(!s.contains(";;;CU)"), "{s}");
        }
        assert!(user.contains("S-1-"), "{user}");
        assert!(admin.contains("BA") && admin.contains("SY"), "{admin}");
        assert!(admin.contains("HI") && admin.contains("NW"), "{admin}");
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

    #[test]
    fn a_failed_elevation_query_is_treated_as_elevated() {
        // Fail closed. PR #59 has another copy of the token query; keep one
        // helper when both changes are on bayan-plus.
        assert!(elevated_from_query(None));
        assert!(elevated_from_query(Some(true)));
        assert!(!elevated_from_query(Some(false)));
    }

    #[test]
    fn one_shot_bash_and_the_gui_launcher_are_not_injected() {
        for cmd in [
            r"C:\Program Files\Git\usr\bin\bash.exe -c ls",
            r"C:\Program Files\Git\usr\bin\bash.exe -lc echo",
            r"C:\Program Files\Git\usr\bin\bash.exe script.sh",
            r"C:\Program Files\Git\git-bash.exe",
        ] {
            assert!(plan(cmd, true, false, NONCE).is_none(), "{cmd}");
        }
        let plain = plan(r"C:\Program Files\Git\usr\bin\bash.exe", true, false, NONCE).unwrap();
        assert!(plain.args.iter().any(|a| a == "-i"));
        assert!(plain.env.iter().all(|(k, _)| k != "INPUTRC"));
    }

    #[test]
    fn elevated_scripts_ignore_the_programdata_environment_variable() {
        let previous = std::env::var_os("PROGRAMDATA");
        std::env::set_var("PROGRAMDATA", r"C:\Users\me\planted-programdata");
        let parent = integration_parent(true);
        match previous {
            Some(value) => std::env::set_var("PROGRAMDATA", value),
            None => std::env::remove_var("PROGRAMDATA"),
        }
        assert_ne!(parent, PathBuf::from(r"C:\Users\me\planted-programdata"));
        assert!(
            parent
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("programdata"),
            "{parent:?}"
        );
    }

    #[test]
    fn cwd_reports_are_cached_and_percent_encode_every_byte() {
        let bash_plan = plan(bash_git(), true, false, NONCE).unwrap();
        let rc = file(&bash_plan, "bash.sh");
        assert!(
            rc.contains("__bayan_pwd_cache"),
            "cygpath once per directory"
        );
        assert!(
            rc.contains("od -An -tx1"),
            "encode UTF-8 bytes, not characters"
        );
        assert!(rc.contains("%${b^^}") || rc.contains("%${(U)b}"));
        let wsl = plan(r"C:\Windows\System32\wsl.exe -d Ubuntu", true, false, NONCE).unwrap();
        let zsh = file(&wsl, "zsh/.zshrc");
        assert!(zsh.contains("__bayan_pct"));
        assert!(zsh.contains("__bayan_pwd_cache"));
        // A miss is cached too: a WSL home (`\\wsl.localhost\…`) or a
        // failed cygpath must not fork a converter on every prompt.
        for (name, body) in [("bash", rc), ("zsh", zsh)] {
            let f = body.split("__bayan_report_cwd() {").nth(1).unwrap();
            let f = &f[..f.find("\n}\n").unwrap()];
            let stored = f.find("__bayan_pwd_cache=$PWD").expect(name);
            let first_return = f.find("|| return 0").expect(name);
            assert!(stored < first_return, "{name} caches only hits:\n{f}");
        }
    }

    #[test]
    fn stale_integration_directories_go_and_fresh_ones_stay() {
        let parent =
            std::env::temp_dir().join(format!("bayan-sweep-{}", random_hex().expect("random")));
        std::fs::create_dir(&parent).unwrap();
        let old = parent.join(integration_dirname(&"ab".repeat(16)));
        std::fs::create_dir(&old).unwrap();
        let sid = token_owner_sid().expect("token owner");
        assert_eq!(
            owner_sid(&old).as_deref(),
            Some(sid.as_str()),
            "a directory this process created is owned by the token owner"
        );
        // Rewriting a directory timestamp is Access Denied on the CI runner.
        // A cutoff behind "now" is a fresh pane; a cutoff ahead of "now" is
        // the same comparison as a 12-hour-old directory.
        let past = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(12 * 60 * 60))
            .unwrap();
        sweep_parent(&parent, past, &sid);
        assert!(
            old.exists(),
            "a directory from this run is not 12 hours old"
        );
        let future = std::time::SystemTime::now()
            .checked_add(std::time::Duration::from_secs(60))
            .unwrap();
        sweep_parent(&parent, future, "S-1-5-99");
        assert!(
            old.exists(),
            "another user's directory is not ours to delete"
        );
        sweep_parent(&parent, future, &sid);
        assert!(
            !old.exists(),
            "a directory older than the cutoff is removed"
        );
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Git Bash on the Windows runner executes the generated rcfile. A missing
    /// bash on a developer machine skips; CI must have it.
    #[test]
    fn real_bash_keeps_the_exit_status_and_sources_bashrc_once() {
        let Some(bash) = find_real_bash() else {
            if std::env::var_os("GITHUB_ACTIONS").is_some() {
                panic!("Git Bash is required on the Windows CI runner");
            }
            return;
        };
        let home =
            std::env::temp_dir().join(format!("bayan-bash-home-{}", random_hex().expect("random")));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir(&home).unwrap();
        std::fs::write(
            home.join(".bashrc"),
            "echo BASHRC >> \"$HOME/bashrc.count\"\nPROMPT_COMMAND='echo USER_PC_SAW=$?'\n",
        )
        .unwrap();
        let rc_path = home.join("bayan-rc.sh");
        let rc = bash_rc(false, true, false);
        std::fs::write(&rc_path, &rc).unwrap();
        // A non-login bash keeps the Windows PATH, which need not hold
        // Git's usr\bin (cygpath, od). A login shell's /etc/profile adds it.
        let mut path = std::ffi::OsString::from(bash.parent().unwrap());
        path.push(";");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        let mut child = std::process::Command::new(&bash)
            .arg("--rcfile")
            .arg(&rc_path)
            .arg("-i")
            .env("PATH", &path)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("BAYAN_PROMPT_NONCE", "n0nce")
            .env_remove("BASH_ENV")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn bash");
        let mut stdout = child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
            buf
        });
        let mut stdin = child.stdin.take().unwrap();
        use std::io::Write;
        writeln!(stdin, "false").unwrap();
        writeln!(stdin, "(exit 7)").unwrap();
        writeln!(stdin, "mkdir -p \"$HOME/بيان\"").unwrap();
        writeln!(stdin, "cd \"$HOME/بيان\"").unwrap();
        writeln!(stdin, "printf 'CD_DONE\\n'").unwrap();
        writeln!(stdin, "exit").unwrap();
        drop(stdin);
        let start = std::time::Instant::now();
        loop {
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            if start.elapsed() > std::time::Duration::from_secs(20) {
                let _ = child.kill();
                let _ = child.wait();
                panic!("bash did not exit");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let output = reader.join().expect("stdout");
        let text = String::from_utf8_lossy(&output);
        let count = std::fs::read_to_string(home.join("bashrc.count")).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(
            count.lines().filter(|l| l.contains("BASHRC")).count(),
            1,
            "bashrc once, output:\n{text}"
        );
        assert!(text.contains("USER_PC_SAW=1"), "{text}");
        assert!(text.contains("USER_PC_SAW=7"), "{text}");
        assert!(text.contains("]133;D;1;"), "{text}");
        assert!(text.contains("]133;D;7;"), "{text}");
        assert!(text.contains("CD_DONE"), "{text}");
        assert!(
            text.contains("%D8%A8"),
            "Arabic directory must be percent-encoded in OSC 7:\n{text}"
        );
        assert!(!text.contains('\u{1}'), "PS0 leaked SOH:\n{text}");
    }

    #[test]
    fn elevated_bash_pins_home_to_the_script_directory() {
        let dir = Path::new(r"C:\ProgramData\bayan-si-abc");
        let mut env = Vec::new();
        pin_elevated_bash_home(
            &mut env,
            dir,
            Some(r"C:\Users\someone"),
            Some(r"C:\Users\someone"),
        );
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "HOME")
                .map(|(_, v)| v.as_str()),
            Some(dir.to_str().unwrap())
        );
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "USERPROFILE")
                .map(|(_, v)| v.as_str()),
            Some(dir.to_str().unwrap())
        );
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "BAYAN_REAL_HOME")
                .map(|(_, v)| v.as_str()),
            Some(r"C:\Users\someone")
        );
    }

    #[test]
    fn materialize_pins_the_elevated_bash_home() {
        let plan = plan(bash_git(), true, true, NONCE).unwrap();
        let done = materialize(&plan).expect("elevated materialize");
        let dir = done.dir.clone().expect("integration dir");
        let home = done
            .env
            .iter()
            .find(|(k, _)| k == "HOME")
            .map(|(_, v)| v.clone());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            home.as_deref(),
            Some(dir.to_str().unwrap()),
            "removing pin_elevated_bash_home from materialize leaves HOME unset"
        );
    }

    #[test]
    fn elevated_completion_user_dir_is_the_integration_directory() {
        let git_plan = plan(bash_git(), true, true, NONCE).unwrap();
        let wsl_plan = plan(r"C:\Windows\System32\wsl.exe -d Ubuntu", true, true, NONCE).unwrap();
        let git = file(&git_plan, "bash.sh");
        let wsl = file(&wsl_plan, "bash.sh");
        for (name, rc) in [("git", git), ("wsl", wsl)] {
            assert!(
                rc.contains("__bayan_completion_dir=")
                    && (rc.contains(
                        r#"__bayan_completion_dir="${__bayan_here%/*}/bash-completion""#,
                    ) || rc.contains("__bayan_completion_dir=$__bayan_safe/bash-completion")),
                "{name} completion dir must be the rcfile directory:\n{rc}"
            );
            assert!(
                rc.contains("BASH_COMPLETION_USER_DIR=$__bayan_completion_dir"),
                "{name}"
            );
            assert!(rc.contains("unset XDG_DATA_HOME"), "{name}");
            assert!(rc.contains("readonly BASH_COMPLETION_USER_DIR"), "{name}");
            assert!(!rc.contains("BASH_COMPLETION_USER_DIR=$HOME"), "{name}");
            assert!(
                !rc.contains("BASH_COMPLETION_USER_DIR=${XDG_DATA_HOME"),
                "{name}"
            );
        }
    }

    #[test]
    fn elevated_git_bash_sets_path_without_sourcing_profile() {
        let rc = bash_rc(true, false, true);
        assert!(
            !rc.contains(". /etc/profile"),
            "elevated Git Bash must not source /etc/profile: {rc}"
        );
        let msystem = rc.find("MSYSTEM=MINGW64").expect("MSYSTEM is set");
        let prefix = rc.find("/mingw64").expect("mingw64 is on PATH");
        let hooks = rc.find("printf '\\033]133;D;%s;%s\\007'").unwrap();
        assert!(msystem < hooks && prefix < hooks, "{rc}");
        let saved = rc.find("BAYAN_REAL_HOME").expect("real HOME is restored");
        let user_pc = rc.find("eval \"$__bayan_cmd\"").unwrap();
        assert!(
            saved < user_pc,
            "HOME must be back before the user's prompt command"
        );
    }

    #[test]
    fn elevated_wsl_moves_home_before_reading_the_profile() {
        let plan = plan(r"C:\Windows\System32\wsl.exe -d Ubuntu", true, true, NONCE).unwrap();
        let wsl = file(&plan, "bash.sh");
        let saved = wsl.find("__bayan_real_home=$HOME").expect("save HOME");
        let safe = wsl
            .find("__bayan_safe=$(CDPATH=")
            .expect("rcfile directory");
        let moved = wsl.find("HOME=$__bayan_safe").expect("HOME moves");
        let sourced = wsl.find(". /etc/profile").expect("profile");
        let restored = wsl
            .find("HOME=$__bayan_real_home")
            .expect("HOME comes back");
        assert!(
            saved < safe && safe < moved && moved < sourced && sourced < restored,
            "{wsl}"
        );
    }

    #[test]
    fn elevated_zsh_login_stops_reading_rc_files_before_exit() {
        let elevated = plan(r"C:\Windows\System32\wsl.exe -d Ubuntu", true, true, NONCE).unwrap();
        let zlogin = file(&elevated, "zsh/.zlogin");
        assert!(
            zlogin.trim_end().ends_with("unsetopt rcs"),
            "exit would read ~/.zlogout: {zlogin}"
        );
        let ordinary = plan(r"C:\Windows\System32\wsl.exe -d Ubuntu", true, false, NONCE).unwrap();
        assert!(
            !file(&ordinary, "zsh/.zlogin").contains("unsetopt rcs"),
            "a normal login zsh still runs the user's .zlogout"
        );
    }

    #[test]
    fn a_user_zdotdir_change_clears_the_unset_flag() {
        let plan = plan(r"C:\Windows\System32\wsl.exe -d Ubuntu", true, false, NONCE).unwrap();
        let env = file(&plan, "zsh/.zshenv");
        let start = env.find("__bayan_source_user()").unwrap();
        let end = env.find("__bayan_finish_zdotdir()").unwrap();
        let body = &env[start..end];
        let sourced = body.find("source \"$ZDOTDIR/$_file\"").unwrap();
        let cleared = body.find("unset BAYAN_ZDOTDIR_UNSET").unwrap();
        assert!(sourced < cleared, "{body}");
    }

    /// Elevated Git Bash must not run the user's git-prompt or completion
    /// scripts. PATH and MSYSTEM are set without `/etc/profile`. Skip when
    /// bash is absent; CI has it.
    #[test]
    fn real_git_bash_elevated_profile_does_not_run_user_git_prompt() {
        let Some(bash) = find_real_bash() else {
            if std::env::var_os("GITHUB_ACTIONS").is_some() {
                panic!("Git Bash is required on the Windows CI runner");
            }
            return;
        };
        let hex = random_hex().expect("random");
        let home = std::env::temp_dir().join(format!("bayan-elev-home-{hex}"));
        let safe = std::env::temp_dir().join(format!("bayan-elev-rc-{hex}"));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&safe);
        std::fs::create_dir_all(home.join(".config/git")).unwrap();
        std::fs::create_dir_all(home.join("bash_completion.d")).unwrap();
        std::fs::create_dir_all(&safe).unwrap();
        let marker = home.join("PWNED");
        let marker_bash = marker.display().to_string().replace('\\', "/");
        std::fs::write(
            home.join(".config/git/git-prompt.sh"),
            format!("echo gitprompt > '{marker_bash}'\n"),
        )
        .unwrap();
        std::fs::write(
            home.join("bash_completion.d/pwn.bash"),
            format!("echo completion > '{marker_bash}'\n"),
        )
        .unwrap();
        let planned = plan(bash_git(), true, true, NONCE).unwrap();
        let stage = safe.join("stage");
        for (rel, body) in &planned.files {
            let path = safe.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            // Bookends so a hang says whether the rcfile started.
            let mut marked = String::from(
                "printf 'start si=%s\\n' \"${__BAYAN_SI-}\" >> \"$BAYAN_STAGE\"\nunset __BAYAN_SI\nprintf 'nonce2\\n' >> \"$BAYAN_STAGE\"\n",
            );
            let body = body
                .replace(
                    "unset BAYAN_PROMPT_NONCE\n",
                    "unset BAYAN_PROMPT_NONCE\nprintf 'nonce\\n' >> \"$BAYAN_STAGE\"\n",
                )
                .replace(
                    "export MSYSTEM PATH\n",
                    "export MSYSTEM PATH\nprintf 'path\\n' >> \"$BAYAN_STAGE\"\n",
                )
                .replace(
                    "__bayan_bin=\"${__bayan_bash%/*}\"",
                    "printf 'bin\\n' >> \"$BAYAN_STAGE\"\n__bayan_bin=\"${__bayan_bash%/*}\"",
                );
            marked.push_str(&body);
            marked.push_str("\nprintf 'end\\n' >> \"$BAYAN_STAGE\"\n");
            std::fs::write(path, marked).unwrap();
        }
        let home_str = home.display().to_string();
        let mut env = planned.env.clone();
        pin_elevated_bash_home(&mut env, &safe, Some(&home_str), Some(&home_str));
        let mut child = std::process::Command::new(&bash);
        for arg in &planned.args {
            if let Some(rel) = arg.strip_prefix("@file:") {
                child.arg(safe.join(rel));
            } else {
                child.arg(arg);
            }
        }
        for (key, value) in &env {
            child.env(key, value);
        }
        child
            .env("BAYAN_PROMPT_NONCE", "n0nce")
            .env(
                "BAYAN_STAGE",
                stage.display().to_string().replace('\\', "/"),
            )
            .env_remove("BASH_ENV")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = child.spawn().expect("spawn bash");
        let mut stdout = child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
            buf
        });
        let mut stdin = child.stdin.take().unwrap();
        use std::io::Write;
        let _ = writeln!(stdin, "printf 'MSYSTEM=%s\\n' \"$MSYSTEM\"");
        let _ = writeln!(stdin, "printf 'AFTER=%s\\n' \"$HOME\"");
        let _ = writeln!(
            stdin,
            "command -v ls >/dev/null && printf 'LS=ok\\n' || printf 'LS=missing\\n'"
        );
        let _ = writeln!(stdin, "exit");
        drop(stdin);
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().expect("wait") {
                break status;
            }
            if start.elapsed() > std::time::Duration::from_secs(20) {
                let _ = child.kill();
                let _ = child.wait();
                let output = reader.join().unwrap_or_default();
                let text = String::from_utf8_lossy(&output);
                let stage_text = std::fs::read_to_string(&stage).unwrap_or_default();
                panic!("bash did not exit\nstage={stage_text}\n{text}");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let output = reader.join().expect("stdout");
        let text = String::from_utf8_lossy(&output);
        let pwned = std::fs::read_to_string(&marker).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&safe);
        assert!(
            status.success() && !text.contains("invalid option"),
            "bash exited {status}: {text}"
        );
        assert!(
            pwned.is_empty(),
            "user git-prompt ran as the elevated shell: {pwned}\n{text}"
        );
        let bash_path = bash.to_string_lossy().to_ascii_lowercase();
        if bash_path.contains("\\git\\") || bash_path.contains("/git/") {
            assert!(
                text.contains("MSYSTEM=UCRT")
                    || text.contains("MSYSTEM=MINGW")
                    || text.contains("MSYSTEM=CLANG"),
                "Git install left MSYSTEM unset or MSYS:\n{text}"
            );
        } else {
            assert!(
                text.contains("MSYSTEM=UCRT")
                    || text.contains("MSYSTEM=MINGW")
                    || text.contains("MSYSTEM=CLANG")
                    || text.contains("MSYSTEM=MSYS"),
                "MSYSTEM was not set:\n{text}"
            );
        }
        assert!(text.contains("LS=ok"), "PATH has no ls:\n{text}");
        assert!(
            text.contains("bayan-elev-home-"),
            "HOME was not restored:\n{text}"
        );
        assert!(
            !text.contains("bayan-elev-rc-"),
            "HOME was left on the rcfile directory:\n{text}"
        );
    }
}
