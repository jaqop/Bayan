//! Hyperlinks: OSC 8 from the emulator, plus URLs and compiler locations
//! detected on the visible row.
//!
//! alacritty_terminal 0.26 already parses `ESC ] 8 ; params ; URI ST` onto
//! the cells it covers. We read `Cell::hyperlink` and group a run of cells
//! that share one id into a span — the bytes are not parsed a second time.
//! Implicit spans fill the cells that have no OSC 8 link: `http`/`https`
//! URLs, and `path:line[:col]` / `path(line,col)` from rustc, gcc, MSVC
//! and grep.
//!
//! Opening is deliberately narrow. `http`, `https` and `file` go to
//! `ShellExecuteW` with the target as the single `lpFile` argument.
//! Anything else — `javascript:`, `ms-*`, `search-ms:`, `data:`, a custom
//! protocol — is refused and shown on a card. Control characters and bidi
//! overrides are refused too, including when they only appear after
//! percent-decoding, and nothing longer than [`MAX_TARGET`] bytes is
//! opened. The editor is a `Command` argument vector; `{file}` `{line}`
//! `{col}` are substituted after the template is split, so a path with
//! spaces stays one argument and the string is never handed to a shell.
//!
//! Mouse tracking still owns a plain click. Ctrl+click is the link
//! gesture even while tracking is on. Shift keeps the gesture for local
//! selection, and it wins over Ctrl. An overlay (the refused-link card,
//! the paste guard, settings, the cockpit) blocks the click entirely:
//! the link underneath is not opened. Hit-testing uses the same
//! visual-to-logical column map as [`crate::mouse::Route`]: a link on an
//! Arabic row is the cell under the glyph, not `x / cell_w`. The window
//! asks [`click_owner`] before [`crate::mouse::Route::press`].
//!
//! Relative file links resolve through [`cwd_hook`]. That reads the
//! directory OSC 9;9 already stores on the pane. OSC 7 (PR5) should write
//! the same field; this function is the only reader.

use std::path::Path;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Term;

/// Bytes, not characters. A link past this is refused whole.
pub const MAX_TARGET: usize = 2048;

/// Who handles a left click. Overlay, tracking, Shift and Ctrl are the
/// whole policy; the window loop does not invent another case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickOwner {
    /// Ctrl+click on a link. Not reported, and not a selection.
    Link,
    /// 1000/1002/1003 is on and Shift is not held. The program hears it.
    Program,
    /// Local selection. Shift always lands here, and so does a plain click
    /// when the program is not tracking the mouse.
    Local,
    /// A card or panel is up. The click opens nothing and starts no drag.
    Blocked,
}

/// `overlay` wins, then `shift`, then Ctrl+click on a link, then the
/// program, then selection. `overlay` is the window's
/// `overlay_blocks_mouse` (settings, cockpit, paste guard, close guard,
/// refused-link card).
pub fn click_owner(
    overlay: bool,
    tracking: bool,
    shift: bool,
    ctrl: bool,
    on_link: bool,
) -> ClickOwner {
    if overlay {
        ClickOwner::Blocked
    } else if shift {
        ClickOwner::Local
    } else if ctrl && on_link {
        ClickOwner::Link
    } else if tracking {
        ClickOwner::Program
    } else {
        ClickOwner::Local
    }
}

/// Why a target was not opened. The card shows [`RefuseReason::label`] and
/// a sanitized copy of the target — never a button that opens it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefuseReason {
    Scheme,
    ControlOrBidi,
    TooLong,
    Empty,
    BadFile,
    Editor,
}

impl RefuseReason {
    pub fn label(self) -> &'static str {
        match self {
            RefuseReason::Scheme => "المخطط غير مسموح",
            RefuseReason::ControlOrBidi => "يحتوي على أحرف تحكم",
            RefuseReason::TooLong => "الرابط أطول من الحد",
            RefuseReason::Empty => "رابط فارغ",
            RefuseReason::BadFile => "مسار الملف غير مقبول",
            RefuseReason::Editor => "تعذّر فتح المحرّر",
        }
    }
}

/// What a Ctrl+click should do. The window loop performs it; nothing here
/// builds a `cmd /c` string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// `ShellExecuteW` lpFile = this URI. http or https only.
    Browse(String),
    /// `ShellExecuteW` lpFile = this filesystem path. One argument.
    OpenPath(String),
    /// `Command::new(argv[0]).args(&argv[1..])`.
    /// `fallback_path` is set only for the default `code -g` choice: if
    /// that process fails to spawn, the file opens by association. A
    /// configured editor leaves it empty so a failure is reported.
    Edit {
        argv: Vec<String>,
        fallback_path: Option<String>,
    },
    Refuse {
        reason: RefuseReason,
        shown: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// OSC 8. `uri` is what the program attached, which may differ from
    /// the cells' text — the hover status shows `uri`.
    Osc {
        uri: String,
    },
    Url {
        uri: String,
    },
    File {
        path: String,
        line: u32,
        col: Option<u32>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkSpan {
    pub start_col: usize,
    /// Exclusive grid column.
    pub end_col: usize,
    pub kind: LinkKind,
}

impl LinkSpan {
    pub fn contains(&self, col: usize) -> bool {
        col >= self.start_col && col < self.end_col
    }

    /// The string the hover status draws. Bidi controls are replaced, so
    /// the status cannot be reordered by the link it is disclosing.
    pub fn tooltip(&self, cwd: Option<&str>) -> String {
        match &self.kind {
            LinkKind::Osc { uri } | LinkKind::Url { uri } => preview(uri, 200),
            LinkKind::File { path, line, col } => {
                let full = resolve_against_cwd(path, cwd);
                preview(&format_file(&full, *line, *col), 200)
            }
        }
    }

    pub fn activate(
        &self,
        cwd: Option<&str>,
        editor: Option<&str>,
        code_exe: Option<&str>,
    ) -> Action {
        match &self.kind {
            LinkKind::Osc { uri } | LinkKind::Url { uri } => plan_uri(uri),
            LinkKind::File { path, line, col } => {
                plan_file(path, *line, *col, cwd, editor, code_exe)
            }
        }
    }
}

fn format_file(path: &str, line: u32, col: Option<u32>) -> String {
    match col {
        Some(c) => format!("{path}:{line}:{c}"),
        None => format!("{path}:{line}"),
    }
}

/// A control, a bidi embedding/override/isolate, or a zero-width spoof.
/// `is_control` covers C0, DEL and C1. The rest are not controls, and a
/// raw RLO in a tooltip would say the opposite of the bytes underneath.
pub fn is_forbidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'
                | '\u{200C}'
                | '\u{200D}'
                | '\u{200E}'
                | '\u{200F}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202A}'
                | '\u{202B}'
                | '\u{202C}'
                | '\u{202D}'
                | '\u{202E}'
                | '\u{2060}'
                | '\u{2066}'
                | '\u{2067}'
                | '\u{2068}'
                | '\u{2069}'
                | '\u{FEFF}'
        )
}

fn marker(c: char) -> String {
    match c {
        '\u{202A}' => "<LRE>".to_string(),
        '\u{202B}' => "<RLE>".to_string(),
        '\u{202C}' => "<PDF>".to_string(),
        '\u{202D}' => "<LRO>".to_string(),
        '\u{202E}' => "<RLO>".to_string(),
        '\u{2066}' => "<LRI>".to_string(),
        '\u{2067}' => "<RLI>".to_string(),
        '\u{2068}' => "<FSI>".to_string(),
        '\u{2069}' => "<PDI>".to_string(),
        '\u{200B}' => "<ZWSP>".to_string(),
        '\u{200C}' => "<ZWNJ>".to_string(),
        '\u{200D}' => "<ZWJ>".to_string(),
        '\u{200E}' => "<LRM>".to_string(),
        '\u{200F}' => "<RLM>".to_string(),
        '\u{061C}' => "<ALM>".to_string(),
        '\u{FEFF}' => "<BOM>".to_string(),
        c if (c as u32) <= 0x1F => char::from_u32(0x2400 + c as u32).unwrap().to_string(),
        '\u{007F}' => "\u{2421}".to_string(),
        c => format!("<U+{:04X}>", c as u32),
    }
}

/// Visible copy of a target. Forbidden characters become ASCII markers.
pub fn preview(raw: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut n = 0usize;
    for c in raw.chars() {
        let rep = if is_forbidden(c) {
            marker(c)
        } else {
            c.to_string()
        };
        let extra = rep.chars().count();
        if n + extra > max_chars {
            out.push('…');
            break;
        }
        out.push_str(&rep);
        n += extra;
    }
    out
}

fn refuse(reason: RefuseReason, raw: &str) -> Action {
    Action::Refuse {
        reason,
        shown: preview(raw, 160),
    }
}

/// Directory a relative `file:line` resolves against.
///
/// `reported` is `SessionMeta::cwd` (OSC 9;9 today). OSC 7 should assign
/// that same field; callers do not grow a second source. Empty, too long,
/// or containing a control / bidi character yields `None` — the link then
/// keeps the relative path instead of joining a poisoned directory.
pub fn cwd_hook(reported: Option<&str>) -> Option<&str> {
    let c = reported.map(str::trim).filter(|s| !s.is_empty())?;
    if c.len() > MAX_TARGET || c.chars().any(is_forbidden) {
        None
    } else {
        Some(c)
    }
}

fn is_windows_abs(p: &str) -> bool {
    let b = p.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
    {
        return true;
    }
    p.starts_with('\\') || p.starts_with("//")
}

/// Join `path` onto `cwd` when the path is relative. Slashes become `\`.
/// An absolute path (drive, UNC, or root-relative) is returned alone.
pub fn resolve_against_cwd(path: &str, cwd: Option<&str>) -> String {
    let mut path = path.replace('/', "\\");
    if is_windows_abs(&path) {
        return path;
    }
    if let Some(stripped) = path.strip_prefix(".\\") {
        path = stripped.to_string();
    }
    let Some(cwd) = cwd_hook(cwd) else {
        return path;
    };
    let cwd = cwd.replace('/', "\\");
    let cwd = cwd.trim_end_matches('\\');
    if path.is_empty() {
        cwd.to_string()
    } else {
        format!("{cwd}\\{path}")
    }
}

fn scheme_name(uri: &str) -> Option<&str> {
    let (scheme, _) = uri.split_once(':')?;
    if scheme.len() <= 1 {
        return None;
    }
    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some(scheme)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                return None;
            }
            let h = hex_val(b[i + 1])?;
            let l = hex_val(b[i + 2])?;
            out.push((h << 4) | l);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn uri_shape_bad(uri: &str) -> Option<RefuseReason> {
    if uri.is_empty() {
        return Some(RefuseReason::Empty);
    }
    if uri.len() > MAX_TARGET {
        return Some(RefuseReason::TooLong);
    }
    if uri.chars().any(|c| is_forbidden(c) || c.is_whitespace()) {
        return Some(RefuseReason::ControlOrBidi);
    }
    match percent_decode(uri) {
        Some(decoded) if decoded.chars().any(is_forbidden) => Some(RefuseReason::ControlOrBidi),
        Some(_) => None,
        None => Some(RefuseReason::ControlOrBidi),
    }
}

/// `file:///C:/src/a.rs` and `file://localhost/C:/src/a.rs` become a local
/// path. A remote host is refused — ShellExecuteW would otherwise treat it
/// as a network location.
fn file_uri_path(uri: &str) -> Option<String> {
    let scheme = scheme_name(uri)?;
    if !scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    let after = uri[scheme.len()..].strip_prefix(':')?;
    if !after.starts_with("//") {
        return None;
    }
    let after = &after[2..];
    let raw = if let Some(rest) = after.strip_prefix('/') {
        rest
    } else {
        let (host, path) = after.split_once('/')?;
        if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
            return None;
        }
        path
    };
    let decoded = percent_decode(raw)?;
    let path = if decoded.len() >= 3
        && decoded.as_bytes()[0] == b'/'
        && decoded.as_bytes()[1].is_ascii_alphabetic()
        && decoded.as_bytes()[2] == b':'
    {
        decoded[1..].to_string()
    } else {
        decoded
    };
    if path.is_empty() || path.len() > MAX_TARGET || path.chars().any(is_forbidden) {
        return None;
    }
    Some(path.replace('/', "\\"))
}

fn http_host_ok(uri: &str, scheme: &str) -> bool {
    let after = &uri[scheme.len()..];
    let Some(rest) = after.strip_prefix("://") else {
        return false;
    };
    if uri.contains('\\') || rest.is_empty() {
        return false;
    }
    let hostport = rest.split(['/', '?', '#']).next().unwrap_or("");
    if hostport.is_empty() {
        return false;
    }
    let host = hostport.rsplit('@').next().unwrap_or(hostport);
    let host = if let Some(inner) = host.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or(host)
    };
    !host.is_empty() && host != "."
}

/// Allowlist. The `Ok` string is exactly what `ShellExecuteW` receives:
/// the URI for http(s), the local path for `file`.
pub fn plan_uri(uri: &str) -> Action {
    if let Some(reason) = uri_shape_bad(uri) {
        return refuse(reason, uri);
    }
    let Some(scheme) = scheme_name(uri) else {
        return refuse(RefuseReason::Scheme, uri);
    };
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        if http_host_ok(uri, scheme) {
            Action::Browse(uri.to_string())
        } else {
            refuse(RefuseReason::Scheme, uri)
        }
    } else if scheme.eq_ignore_ascii_case("file") {
        match file_uri_path(uri) {
            Some(path) => Action::OpenPath(path),
            None => refuse(RefuseReason::BadFile, uri),
        }
    } else {
        refuse(RefuseReason::Scheme, uri)
    }
}

fn plan_file(
    path: &str,
    line: u32,
    col: Option<u32>,
    cwd: Option<&str>,
    editor: Option<&str>,
    code_exe: Option<&str>,
) -> Action {
    if path.is_empty() {
        return refuse(RefuseReason::Empty, path);
    }
    if path.len() > MAX_TARGET || path.chars().any(is_forbidden) {
        let reason = if path.len() > MAX_TARGET {
            RefuseReason::TooLong
        } else {
            RefuseReason::ControlOrBidi
        };
        return refuse(reason, path);
    }
    if let Some(scheme) = scheme_name(path) {
        if !scheme.eq_ignore_ascii_case("file") {
            return refuse(RefuseReason::Scheme, path);
        }
    }
    let full = resolve_against_cwd(path, cwd);
    if full.len() > MAX_TARGET || full.chars().any(is_forbidden) {
        return refuse(RefuseReason::BadFile, &full);
    }
    let configured = editor.map(str::trim).filter(|s| !s.is_empty());
    if let Some(template) = configured {
        match expand_editor(template, &full, Some(line), col) {
            Some(argv) if !is_shell_program(&argv[0]) => Action::Edit {
                argv,
                fallback_path: None,
            },
            _ => refuse(RefuseReason::Editor, template),
        }
    } else if let Some(exe) = code_exe.map(str::trim).filter(|s| !s.is_empty()) {
        match expand_editor("code -g {file}:{line}:{col}", &full, Some(line), col) {
            Some(mut argv) => {
                argv[0] = exe.to_string();
                if is_shell_program(&argv[0]) {
                    return refuse(RefuseReason::Editor, exe);
                }
                Action::Edit {
                    argv,
                    fallback_path: Some(full),
                }
            }
            None => Action::OpenPath(full),
        }
    } else {
        Action::OpenPath(full)
    }
}

fn is_shell_program(prog: &str) -> bool {
    let base = prog.rsplit(['\\', '/']).next().unwrap_or(prog);
    let base = base.to_ascii_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base);
    matches!(
        base,
        "cmd" | "powershell" | "pwsh" | "bash" | "sh" | "wsl" | "fish"
    )
}

/// Split `template` on whitespace, honoring double quotes, then substitute
/// `{file}` `{line}` `{col}`. The result is an argument vector. A missing
/// column drops the colon that introduced `{col}`, so `file:10:` does not
/// leak out of `{file}:{line}:{col}`.
pub fn expand_editor(
    template: &str,
    file: &str,
    line: Option<u32>,
    col: Option<u32>,
) -> Option<Vec<String>> {
    if template.len() > MAX_TARGET || template.chars().any(is_forbidden) {
        return None;
    }
    if file.chars().any(is_forbidden) || file.len() > MAX_TARGET {
        return None;
    }
    let tokens = tokenize(template)?;
    let line_s = line.map(|n| n.to_string()).unwrap_or_default();
    let col_s = col.map(|n| n.to_string()).unwrap_or_default();
    let mut argv = Vec::with_capacity(tokens.len());
    for tok in tokens {
        let mut filled = tok
            .replace("{file}", file)
            .replace("{line}", &line_s)
            .replace("{col}", &col_s);
        if col_s.is_empty() && tok.contains("{col}") {
            if let Some(stripped) = filled.strip_suffix(':') {
                filled = stripped.to_string();
            }
        }
        if line_s.is_empty() && tok.contains("{line}") {
            if let Some(stripped) = filled.strip_suffix(':') {
                filled = stripped.to_string();
            }
        }
        if filled.is_empty() {
            continue;
        }
        if filled.chars().any(is_forbidden) {
            return None;
        }
        argv.push(filled);
    }
    if argv.is_empty() || argv[0].is_empty() {
        None
    } else {
        Some(argv)
    }
}

fn tokenize(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut in_quote = false;
    while let Some(c) = chars.next() {
        if in_quote {
            if c == '"' {
                in_quote = false;
                continue;
            }
            if c == '\\' && chars.peek() == Some(&'"') {
                cur.push(chars.next().unwrap());
                continue;
            }
            cur.push(c);
            continue;
        }
        if c == '"' {
            in_quote = true;
            continue;
        }
        if c.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        cur.push(c);
    }
    if in_quote {
        return None;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Some(out)
}

/// `Command` from an argument vector. Shell programs are refused here so
/// a template of `cmd /c start …` cannot be spawned by mistake.
pub fn editor_command(argv: &[String]) -> Option<std::process::Command> {
    let (prog, args) = argv.split_first()?;
    if prog.is_empty() || prog.chars().any(is_forbidden) || is_shell_program(prog) {
        return None;
    }
    let mut cmd = std::process::Command::new(prog);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    Some(cmd)
}

pub fn spawn_editor(argv: &[String]) -> std::io::Result<()> {
    let mut cmd = editor_command(argv)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "editor"))?;
    cmd.spawn()?;
    Ok(())
}

/// `code.exe` on PATH, or the `Code.exe` that sits next to the `code.cmd`
/// shim VS Code puts on PATH. A `.cmd` is not returned: `Command` would
/// not start it, and wrapping it in `cmd /c` is the thing this module
/// does not do. `None` means the caller opens the file by association.
pub fn find_code_exe() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    find_code_exe_in(std::env::split_paths(&path), &pathexts())
}

fn pathexts() -> Vec<String> {
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn find_code_exe_in<I, P>(dirs: I, exts: &[String]) -> Option<std::path::PathBuf>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    for dir in dirs {
        let dir = dir.as_ref();
        for name in ["code.exe", "Code.exe"] {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
        for ext in exts {
            let ext = if ext.starts_with('.') {
                ext.as_str()
            } else {
                continue;
            };
            if ext.eq_ignore_ascii_case(".exe") {
                let p = dir.join(format!("code{ext}"));
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        for cmd_name in ["code.cmd", "code.CMD", "code.bat", "code.BAT"] {
            let cmd = dir.join(cmd_name);
            if cmd.is_file() {
                if let Some(exe) = exe_near_cmd(&cmd) {
                    return Some(exe);
                }
            }
        }
    }
    None
}

fn exe_near_cmd(cmd: &Path) -> Option<std::path::PathBuf> {
    let dir = cmd.parent()?;
    let candidates = [
        dir.join("code.exe"),
        dir.join("Code.exe"),
        dir.join("..").join("Code.exe"),
        dir.join("..").join("code.exe"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

/// Open `target` with the shell association. `target` is `lpFile` alone.
/// `lpParameters` is null. This is not `cmd /c start`.
pub fn shell_execute(target: &str) -> bool {
    if target.is_empty() || target.len() > MAX_TARGET || target.chars().any(is_forbidden) {
        return false;
    }
    #[cfg(windows)]
    {
        shell_execute_w(target)
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        false
    }
}

#[cfg(windows)]
fn shell_execute_w(target: &str) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let op: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    let file: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    (rc as isize) > 32
}

struct RowCell {
    col: usize,
    width: usize,
    ch: char,
    osc_id: Option<String>,
    osc_uri: Option<String>,
}

struct CharLoc {
    byte: usize,
    byte_end: usize,
    col: usize,
    end_col: usize,
}

/// Links on one viewport row. OSC 8 comes from the cell; the rest is
/// scanned from the row's text, and never overwrites an OSC span.
pub fn links_on_row<T: EventListener>(term: &Term<T>, viewport_row: usize) -> Vec<LinkSpan> {
    let content = term.renderable_content();
    let off = content.display_offset as i32;
    let mut cells = Vec::new();
    for cell in content.display_iter {
        let vrow = cell.point.line.0 + off;
        if vrow != viewport_row as i32 {
            continue;
        }
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let width = if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        let href = cell.hyperlink();
        let ch = if cell.flags.intersects(Flags::HIDDEN) {
            ' '
        } else {
            cell.c
        };
        cells.push(RowCell {
            col: cell.point.column.0,
            width,
            ch,
            osc_id: href.as_ref().map(|h| h.id().to_string()),
            osc_uri: href
                .as_ref()
                .map(|h| h.uri().to_string())
                .filter(|u| !u.is_empty()),
        });
    }
    links_in_row(&cells)
}

fn links_in_row(cells: &[RowCell]) -> Vec<LinkSpan> {
    let mut spans = osc_spans(cells);
    let (text, map) = line_text(cells);
    for (start, end, uri) in find_urls(&text) {
        if let Some((c0, c1)) = cols_covering(&map, start, end) {
            if !range_overlaps(&spans, c0, c1) {
                spans.push(LinkSpan {
                    start_col: c0,
                    end_col: c1,
                    kind: LinkKind::Url { uri },
                });
            }
        }
    }
    for found in find_files(&text) {
        if let Some((c0, c1)) = cols_covering(&map, found.start_byte, found.end_byte) {
            if !range_overlaps(&spans, c0, c1) {
                spans.push(LinkSpan {
                    start_col: c0,
                    end_col: c1,
                    kind: LinkKind::File {
                        path: found.path,
                        line: found.line,
                        col: found.col,
                    },
                });
            }
        }
    }
    spans.sort_by_key(|s| s.start_col);
    spans
}

fn range_overlaps(spans: &[LinkSpan], start: usize, end: usize) -> bool {
    spans.iter().any(|s| start < s.end_col && s.start_col < end)
}

fn osc_spans(cells: &[RowCell]) -> Vec<LinkSpan> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < cells.len() {
        let Some(uri) = cells[i].osc_uri.clone() else {
            i += 1;
            continue;
        };
        let id = cells[i].osc_id.clone();
        let start = cells[i].col;
        let mut end = cells[i].col + cells[i].width;
        i += 1;
        while i < cells.len()
            && cells[i].osc_uri.as_deref() == Some(uri.as_str())
            && cells[i].osc_id == id
            && cells[i].col <= end
        {
            end = cells[i].col + cells[i].width;
            i += 1;
        }
        out.push(LinkSpan {
            start_col: start,
            end_col: end,
            kind: LinkKind::Osc { uri },
        });
    }
    out
}

fn line_text(cells: &[RowCell]) -> (String, Vec<CharLoc>) {
    let mut text = String::new();
    let mut map = Vec::new();
    let mut expect = cells.first().map(|c| c.col).unwrap_or(0);
    for c in cells {
        while expect < c.col {
            let byte = text.len();
            text.push(' ');
            map.push(CharLoc {
                byte,
                byte_end: text.len(),
                col: expect,
                end_col: expect + 1,
            });
            expect += 1;
        }
        let byte = text.len();
        text.push(c.ch);
        map.push(CharLoc {
            byte,
            byte_end: text.len(),
            col: c.col,
            end_col: c.col + c.width,
        });
        expect = c.col + c.width;
    }
    (text, map)
}

fn cols_covering(map: &[CharLoc], start: usize, end: usize) -> Option<(usize, usize)> {
    let mut lo = None;
    let mut hi = 0usize;
    for ch in map {
        if ch.byte < end && ch.byte_end > start {
            lo = Some(lo.unwrap_or(ch.col).min(ch.col));
            hi = hi.max(ch.end_col);
        }
    }
    lo.map(|l| (l, hi))
}

fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | ':'
                | '/'
                | '?'
                | '#'
                | '['
                | ']'
                | '@'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '%'
                | '='
        )
}

fn url_scheme_len(rest: &str) -> Option<usize> {
    // Byte compare. Slicing the `&str` at `scheme.len()` panics when the
    // next bytes are inside an Arabic letter (those are two bytes, and
    // "http://" is 7).
    let bytes = rest.as_bytes();
    for scheme in [b"https://".as_slice(), b"http://".as_slice()] {
        if bytes.len() >= scheme.len() && bytes[..scheme.len()].eq_ignore_ascii_case(scheme) {
            return Some(scheme.len());
        }
    }
    None
}

fn trim_url(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut end = bytes.len();
    while end > 0 {
        let c = bytes[end - 1];
        if matches!(c, b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' | b'"') {
            end -= 1;
            continue;
        }
        if matches!(c, b')' | b']' | b'}') {
            let open = match c {
                b')' => b'(',
                b']' => b'[',
                _ => b'{',
            };
            let body = &bytes[..end];
            let opens = body.iter().filter(|&&b| b == open).count();
            let closes = body.iter().filter(|&&b| b == c).count();
            if closes > opens {
                end -= 1;
                continue;
            }
        }
        break;
    }
    &s[..end]
}

fn good_url(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    let hostport = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = hostport.rsplit('@').next().unwrap_or(hostport);
    let host = if let Some(inner) = host.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or(host)
    };
    !host.is_empty() && host != "."
}

fn find_urls(text: &str) -> Vec<(usize, usize, String)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let b = chars[i].0;
        let rest = &text[b..];
        let prev_ok = i == 0 || !chars[i - 1].1.is_ascii_alphanumeric();
        if prev_ok {
            if let Some(scheme_len) = url_scheme_len(rest) {
                let mut j = i + scheme_len;
                while j < chars.len() && is_url_char(chars[j].1) {
                    j += 1;
                }
                let end_b = chars.get(j).map(|c| c.0).unwrap_or(text.len());
                let raw = &text[b..end_b];
                let trimmed = trim_url(raw);
                if good_url(trimmed) {
                    let trim_end = b + trimmed.len();
                    out.push((b, trim_end, trimmed.to_string()));
                    i = chars
                        .iter()
                        .position(|(cb, _)| *cb >= trim_end)
                        .unwrap_or(chars.len());
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

struct FileHit {
    start_byte: usize,
    end_byte: usize,
    path: String,
    line: u32,
    col: Option<u32>,
}

fn is_path_char(c: char) -> bool {
    if is_forbidden(c) || c.is_whitespace() {
        return false;
    }
    if c.is_ascii() {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '_' | '-' | '.' | '+' | '/' | '\\' | '~' | '@' | '%' | '$'
            )
    } else {
        c.is_alphanumeric()
    }
}

fn looks_like_path(p: &str) -> bool {
    if p.is_empty() || p == "." || p == ".." || p.len() > MAX_TARGET || p.contains("://") {
        return false;
    }
    let file = p.rsplit(['\\', '/']).next().unwrap_or(p);
    let has_sep = p.contains('\\') || p.contains('/');
    let dotted = if let Some(rest) = file.strip_prefix('.') {
        !rest.is_empty() && rest != "."
    } else {
        file.contains('.')
    };
    has_sep || dotted
}

fn take_path(text: &str, chars: &[(usize, char)], end: usize) -> Option<(usize, usize)> {
    if end == 0 {
        return None;
    }
    let mut i = end;
    while i > 0 {
        let ch = chars[i - 1].1;
        if ch == ':' {
            let letter_at = i.checked_sub(2);
            let letter_ok = letter_at.is_some_and(|at| chars[at].1.is_ascii_alphabetic());
            let sep_ok = chars.get(i).is_some_and(|c| c.1 == '\\' || c.1 == '/');
            let before_ok = match letter_at {
                Some(0) => true,
                Some(at) if at > 0 => !is_path_char(chars[at - 1].1),
                _ => false,
            };
            if letter_ok && sep_ok && before_ok {
                i -= 2;
            }
            break;
        }
        if !is_path_char(ch) {
            break;
        }
        i -= 1;
    }
    if i >= end {
        return None;
    }
    // Stopped on a colon we did not consume: the tail of `scheme://…`.
    if i > 0 && chars[i - 1].1 == ':' {
        return None;
    }
    let start = chars[i].0;
    let stop = chars[end].0;
    let path = &text[start..stop];
    if !looks_like_path(path) {
        return None;
    }
    Some((start, stop))
}

fn parse_u32(chars: &[(usize, char)], i: usize) -> Option<(u32, usize)> {
    if i >= chars.len() || !chars[i].1.is_ascii_digit() {
        return None;
    }
    let mut n: u32 = 0;
    let mut j = i;
    let mut digits = 0u32;
    while j < chars.len() && chars[j].1.is_ascii_digit() {
        digits += 1;
        if digits > 7 {
            return None;
        }
        let d = chars[j].1.to_digit(10)?;
        n = n.checked_mul(10)?.checked_add(d)?;
        j += 1;
    }
    if n == 0 || n > 9_999_999 {
        None
    } else {
        Some((n, j))
    }
}

fn find_files(text: &str) -> Vec<FileHit> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].1 == '(' {
            if let Some(hit) = try_paren(text, &chars, i) {
                i = index_at(&chars, hit.end_byte);
                out.push(hit);
                continue;
            }
        }
        if chars[i].1 == ':' {
            if let Some(hit) = try_colon(text, &chars, i) {
                i = index_at(&chars, hit.end_byte);
                out.push(hit);
                continue;
            }
        }
        i += 1;
    }
    out
}

fn index_at(chars: &[(usize, char)], byte: usize) -> usize {
    chars
        .iter()
        .position(|(b, _)| *b >= byte)
        .unwrap_or(chars.len())
}

fn try_colon(text: &str, chars: &[(usize, char)], i: usize) -> Option<FileHit> {
    let (line, after_line) = parse_u32(chars, i + 1)?;
    let (col, end_idx) = if chars.get(after_line).is_some_and(|c| c.1 == ':') {
        if let Some((col, after_col)) = parse_u32(chars, after_line + 1) {
            (Some(col), after_col)
        } else {
            (None, after_line)
        }
    } else {
        (None, after_line)
    };
    let (start, _) = take_path(text, chars, i)?;
    let end_byte = chars.get(end_idx).map(|c| c.0).unwrap_or(text.len());
    let path = text[start..chars[i].0].to_string();
    Some(FileHit {
        start_byte: start,
        end_byte,
        path,
        line,
        col,
    })
}

fn try_paren(text: &str, chars: &[(usize, char)], i: usize) -> Option<FileHit> {
    let (line, after_line) = parse_u32(chars, i + 1)?;
    let (col, after) = if chars.get(after_line).is_some_and(|c| c.1 == ',') {
        let mut k = after_line + 1;
        while chars.get(k).is_some_and(|c| c.1 == ' ') {
            k += 1;
        }
        let (col, after_col) = parse_u32(chars, k)?;
        (Some(col), after_col)
    } else {
        (None, after_line)
    };
    if chars.get(after).map(|c| c.1) != Some(')') {
        return None;
    }
    let (start, _) = take_path(text, chars, i)?;
    let end_idx = after + 1;
    let end_byte = chars.get(end_idx).map(|c| c.0).unwrap_or(text.len());
    let path = text[start..chars[i].0].to_string();
    Some(FileHit {
        start_byte: start,
        end_byte,
        path,
        line,
        col,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(s: &str) -> Vec<RowCell> {
        s.chars()
            .enumerate()
            .map(|(i, ch)| RowCell {
                col: i,
                width: 1,
                ch,
                osc_id: None,
                osc_uri: None,
            })
            .collect()
    }

    fn span_text(s: &str, span: &LinkSpan) -> String {
        s.chars()
            .skip(span.start_col)
            .take(span.end_col - span.start_col)
            .collect()
    }

    fn urls_in(s: &str) -> Vec<String> {
        links_in_row(&row(s))
            .into_iter()
            .filter_map(|sp| match sp.kind {
                LinkKind::Url { uri } => Some(uri),
                _ => None,
            })
            .collect()
    }

    fn files_in(s: &str) -> Vec<(String, u32, Option<u32>, String)> {
        let spans = links_in_row(&row(s));
        spans
            .iter()
            .filter_map(|sp| match &sp.kind {
                LinkKind::File { path, line, col } => {
                    Some((path.clone(), *line, *col, span_text(s, sp)))
                }
                _ => None,
            })
            .collect()
    }

    fn osc_row(bytes: &[u8]) -> Vec<LinkSpan> {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::Processor;
        let size = crate::term::TermSize { cols: 80, rows: 6 };
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        links_on_row(&term, 0)
    }

    #[test]
    fn osc8_open_and_close_become_spans() {
        // BEL terminator. The empty URI is the closer; " tail" is not a link.
        let spans = osc_row(b"\x1b]8;;https://example.com/a\x07click\x1b]8;;\x07 tail");
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!((spans[0].start_col, spans[0].end_col), (0, 5));
        match &spans[0].kind {
            LinkKind::Osc { uri } => assert_eq!(uri, "https://example.com/a"),
            other => panic!("{other:?}"),
        }
        // ST (ESC \) is the other terminator and yields the same span.
        let spans = osc_row(b"\x1b]8;;https://example.com/a\x1b\\click\x1b]8;;\x1b\\");
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].start_col, spans[0].end_col), (0, 5));
    }

    #[test]
    fn osc8_two_adjacent_links_stay_separate() {
        let bytes = b"\x1b]8;id=one;https://a.example\x07AA\x1b]8;;\x07 \x1b]8;id=two;https://b.example\x07BBB\x1b]8;;\x07";
        let spans = osc_row(bytes);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!((spans[0].start_col, spans[0].end_col), (0, 2));
        assert_eq!((spans[1].start_col, spans[1].end_col), (3, 6));
        match (&spans[0].kind, &spans[1].kind) {
            (LinkKind::Osc { uri: a }, LinkKind::Osc { uri: b }) => {
                assert_eq!(a, "https://a.example");
                assert_eq!(b, "https://b.example");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn osc8_tooltip_shows_the_uri_not_the_label() {
        let spans = osc_row(b"\x1b]8;;https://evil.example/phish\x07click here\x1b]8;;\x07");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].tooltip(None), "https://evil.example/phish");
        assert_ne!(spans[0].tooltip(None), "click here");
        // A bidi override inside the URI is visible as a marker, not as a reorder.
        let spans = osc_row(b"\x1b]8;;https://evil.example/\xE2\x80\xAEhidden\x07x\x1b]8;;\x07");
        let tip = spans[0].tooltip(None);
        assert!(!tip.contains('\u{202E}'), "{tip}");
        assert!(tip.contains("<RLO>"), "{tip}");
        assert!(matches!(
            spans[0].activate(None, None, None),
            Action::Refuse {
                reason: RefuseReason::ControlOrBidi,
                ..
            }
        ));
    }

    #[test]
    fn url_strips_trailing_punctuation() {
        for end in [".", ",", ";", ":", "!", "?", "'"] {
            let s = format!("see https://example.com/a{end} next");
            assert_eq!(
                urls_in(&s),
                vec!["https://example.com/a".to_string()],
                "trailing {end}"
            );
        }
        assert_eq!(
            urls_in("https://example.com/a?q=1&x=2"),
            vec!["https://example.com/a?q=1&x=2".to_string()]
        );
        assert_eq!(
            urls_in("HTTP://Example.COM/A"),
            vec!["HTTP://Example.COM/A".to_string()]
        );
        assert!(urls_in("nothing http:// here").is_empty());
    }

    #[test]
    fn url_keeps_balanced_parentheses() {
        assert_eq!(
            urls_in("see (https://example.com/a)."),
            vec!["https://example.com/a".to_string()]
        );
        assert_eq!(
            urls_in("https://en.wikipedia.org/wiki/Rust_(programming_language)."),
            vec!["https://en.wikipedia.org/wiki/Rust_(programming_language)".to_string()]
        );
    }

    #[test]
    fn url_stops_at_surrounding_arabic() {
        let spaced = "انظر https://example.com/docs الآن";
        assert_eq!(
            urls_in(spaced),
            vec!["https://example.com/docs".to_string()]
        );
        let spans = links_in_row(&row(spaced));
        assert_eq!(span_text(spaced, &spans[0]), "https://example.com/docs");
        // No space: Arabic still bounds the URL, it is not a URL character.
        let glued = "انظرhttps://example.com/docsالآن";
        assert_eq!(urls_in(glued), vec!["https://example.com/docs".to_string()]);
        assert!(urls_in(glued)[0].is_ascii());
    }

    #[test]
    fn file_line_formats_for_rustc_gcc_msvc_and_grep() {
        assert_eq!(
            files_in("src/main.rs:10:5"),
            vec![("src/main.rs".into(), 10, Some(5), "src/main.rs:10:5".into())]
        );
        assert_eq!(
            files_in("src/main.rs:10"),
            vec![("src/main.rs".into(), 10, None, "src/main.rs:10".into())]
        );
        // gcc: column, then the diagnostic. The message is not part of the span.
        assert_eq!(
            files_in("foo.c:12:5: error: bad"),
            vec![("foo.c".into(), 12, Some(5), "foo.c:12:5".into())]
        );
        assert_eq!(
            files_in("foo.c:12: error: bad"),
            vec![("foo.c".into(), 12, None, "foo.c:12".into())]
        );
        // MSVC
        assert_eq!(
            files_in(r"src\main.cpp(10,5): error C2065"),
            vec![(
                r"src\main.cpp".into(),
                10,
                Some(5),
                r"src\main.cpp(10,5)".into()
            )]
        );
        assert_eq!(
            files_in(r"src\main.cpp(10): error"),
            vec![(r"src\main.cpp".into(), 10, None, r"src\main.cpp(10)".into())]
        );
        assert_eq!(
            files_in(r"src\main.cpp(10, 5): error"),
            vec![(
                r"src\main.cpp".into(),
                10,
                Some(5),
                r"src\main.cpp(10, 5)".into()
            )]
        );
        // grep -n, and grep --column / rustc's `-->` line
        assert_eq!(
            files_in("src/main.rs:10:fn main() {"),
            vec![("src/main.rs".into(), 10, None, "src/main.rs:10".into())]
        );
        assert_eq!(
            files_in("src/main.rs:10:4:match"),
            vec![("src/main.rs".into(), 10, Some(4), "src/main.rs:10:4".into())]
        );
        assert_eq!(
            files_in("  --> src/lib.rs:20:1"),
            vec![("src/lib.rs".into(), 20, Some(1), "src/lib.rs:20:1".into())]
        );
        assert_eq!(
            files_in(r"C:\src\main.rs:10:5"),
            vec![(
                r"C:\src\main.rs".into(),
                10,
                Some(5),
                r"C:\src\main.rs:10:5".into()
            )]
        );
        assert_eq!(
            files_in("./src/main.rs:3"),
            vec![("./src/main.rs".into(), 3, None, "./src/main.rs:3".into())]
        );
        assert_eq!(
            files_in("../lib.rs:8:1"),
            vec![("../lib.rs".into(), 8, Some(1), "../lib.rs:8:1".into())]
        );
        // Not locations: a clock, a word with a colon, a URL port.
        assert!(files_in("12:30:00").is_empty());
        assert!(files_in("error:10").is_empty());
        assert_eq!(
            urls_in("https://example.com:8080/a"),
            vec!["https://example.com:8080/a".to_string()]
        );
        assert!(files_in("https://example.com:8080/a").is_empty());
    }

    #[test]
    fn scheme_allowlist_rejects_non_http_targets() {
        for uri in [
            "javascript:alert(1)",
            "JAVASCRIPT:alert(1)",
            "ms-settings:privacy",
            "ms-msdt:id=PCWDiagnostic",
            "search-ms:query=password",
            "data:text/html,<script>",
            "vscode://file/C:/src/main.rs:10",
            "custom:open",
        ] {
            match plan_uri(uri) {
                Action::Refuse {
                    reason: RefuseReason::Scheme,
                    shown,
                } => {
                    assert!(!shown.contains('\u{202E}'), "{uri} -> {shown}");
                    assert!(!shown.to_ascii_lowercase().contains("cmd /c"), "{shown}");
                }
                other => panic!("{uri} -> {other:?}"),
            }
        }
        assert_eq!(
            plan_uri("https://example.com/a"),
            Action::Browse("https://example.com/a".into())
        );
        assert_eq!(
            plan_uri("http://example.com"),
            Action::Browse("http://example.com".into())
        );
        // A backslash is not a path separator the browser and we agree on.
        assert!(matches!(
            plan_uri(r"https://example.com\@evil.com"),
            Action::Refuse {
                reason: RefuseReason::Scheme,
                ..
            }
        ));
        assert_eq!(
            plan_uri("file:///C:/src/main.rs"),
            Action::OpenPath(r"C:\src\main.rs".into())
        );
        assert_eq!(
            plan_uri("file://localhost/C:/Program%20Files/a.rs"),
            Action::OpenPath(r"C:\Program Files\a.rs".into())
        );
        // Remote file host: not a local path, not opened.
        assert!(matches!(
            plan_uri("file://evil.example/share/secret"),
            Action::Refuse {
                reason: RefuseReason::BadFile,
                ..
            }
        ));
        match plan_uri("https://example.com/a") {
            Action::Browse(uri) => {
                assert!(!uri.to_ascii_lowercase().contains("cmd"));
                assert!(!uri.contains("/c"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn control_and_bidi_characters_are_rejected() {
        for uri in [
            "https://example.com/\u{7}x",
            "https://example.com/\n",
            "https://example.com/\u{202E}moc.elpmaxe",
            "https://example.com/\u{202A}x",
            "https://example.com/\u{2066}x",
            "https://example.com/\u{2067}x",
            "https://example.com/\u{200B}x",
            "https://example.com/\u{200E}x",
            "https://example.com/\u{FEFF}x",
            "https://example.com/\u{061C}x",
            // percent-encoded RLO (UTF-8 E2 80 AE) and a NUL
            "https://example.com/%E2%80%AEhidden",
            "https://example.com/%00",
            "https://example.com/%ZZ",
            "https://example.com/%E",
        ] {
            match plan_uri(uri) {
                Action::Refuse {
                    reason: RefuseReason::ControlOrBidi,
                    shown,
                } => {
                    assert!(!shown.chars().any(is_forbidden), "{uri} -> {shown}");
                }
                other => panic!("{uri} -> {other:?}"),
            }
        }
        assert_eq!(preview("ab\u{202E}cd", 40), "ab<RLO>cd");
    }

    #[test]
    fn uri_over_2048_bytes_is_rejected() {
        let mut over = String::from("https://example.com/");
        over.push_str(&"a".repeat(MAX_TARGET));
        assert!(over.len() > MAX_TARGET);
        assert!(matches!(
            plan_uri(&over),
            Action::Refuse {
                reason: RefuseReason::TooLong,
                ..
            }
        ));
        let mut ok = String::from("https://example.com/");
        ok.push_str(&"a".repeat(MAX_TARGET - ok.len()));
        assert_eq!(ok.len(), MAX_TARGET);
        assert_eq!(plan_uri(&ok), Action::Browse(ok));
    }

    #[test]
    fn editor_template_expands_to_argv_not_a_shell() {
        let argv = expand_editor(
            r#"code -g {file}:{line}:{col}"#,
            r"C:\src\main.rs",
            Some(10),
            Some(4),
        )
        .unwrap();
        assert_eq!(
            argv,
            vec![
                "code".to_string(),
                "-g".to_string(),
                r"C:\src\main.rs:10:4".to_string()
            ]
        );
        let cmd = editor_command(&argv).unwrap();
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("code"));
        let args: Vec<_> = cmd.get_args().map(|a| a.to_owned()).collect();
        assert_eq!(
            args,
            vec![
                std::ffi::OsString::from("-g"),
                std::ffi::OsString::from(r"C:\src\main.rs:10:4"),
            ]
        );
        assert!(!cmd.get_args().any(|a| a == "/c" || a == "start"));

        // A quoted program with spaces is one argument. The path's spaces
        // stay inside the substituted argument, they do not split it.
        let argv = expand_editor(
            r#""C:\Program Files\Microsoft VS Code\Code.exe" -g {file}:{line}:{col}"#,
            r"C:\src\my file.rs",
            Some(12),
            Some(3),
        )
        .unwrap();
        assert_eq!(argv[0], r"C:\Program Files\Microsoft VS Code\Code.exe");
        assert_eq!(argv[2], r"C:\src\my file.rs:12:3");
        let cmd = editor_command(&argv).unwrap();
        assert_eq!(
            cmd.get_program(),
            std::ffi::OsStr::new(r"C:\Program Files\Microsoft VS Code\Code.exe")
        );
        assert_eq!(cmd.get_args().count(), 2);

        // No column: the colon that only introduced {col} is not left behind.
        let argv = expand_editor("code -g {file}:{line}:{col}", "a.rs", Some(4), None).unwrap();
        assert_eq!(argv[2], "a.rs:4");

        // A shell is not an editor, even written as an argument vector.
        assert!(editor_command(&[
            "cmd".into(),
            "/c".into(),
            "start".into(),
            "https://x".into()
        ])
        .is_none());
        assert!(editor_command(&[
            r"C:\Windows\System32\cmd.exe".into(),
            "/c".into(),
            "start".into(),
            r"C:\src\a.rs".into()
        ])
        .is_none());
        let spans = links_in_row(&row("src/main.rs:10:4"));
        assert!(matches!(
            spans[0].activate(Some(r"C:\work"), Some(r"cmd /c start {file}"), None),
            Action::Refuse {
                reason: RefuseReason::Editor,
                ..
            }
        ));
    }

    #[test]
    fn relative_path_resolves_against_cwd_hook() {
        assert_eq!(cwd_hook(Some(r"  C:\src  ")), Some(r"C:\src"));
        assert_eq!(cwd_hook(Some("")), None);
        assert_eq!(cwd_hook(None), None);
        assert_eq!(cwd_hook(Some("C:\\src\u{202E}")), None);

        let spans = links_in_row(&row("src/main.rs:10:4"));
        assert_eq!(
            spans[0].activate(Some(r"C:\work"), Some("code -g {file}:{line}:{col}"), None),
            Action::Edit {
                argv: vec![
                    "code".into(),
                    "-g".into(),
                    r"C:\work\src\main.rs:10:4".into()
                ],
                fallback_path: None,
            }
        );
        // `./` is the cwd itself, not a literal dot directory.
        let spans = links_in_row(&row("./src/main.rs:3"));
        match spans[0].activate(Some(r"C:\work"), None, None) {
            Action::OpenPath(path) => assert_eq!(path, r"C:\work\src\main.rs"),
            other => panic!("{other:?}"),
        }
        // Absolute wins over the pane cwd. No editor and no code.exe: association.
        let spans = links_in_row(&row(r"C:\src\main.rs:2:1"));
        match spans[0].activate(Some(r"D:\other"), None, None) {
            Action::OpenPath(path) => assert_eq!(path, r"C:\src\main.rs"),
            other => panic!("{other:?}"),
        }
        // Default editor is the resolved exe, one argv element, with the
        // file as the association fallback if that process fails to spawn.
        let spans = links_in_row(&row("src/main.rs:10:4"));
        match spans[0].activate(Some(r"C:\work"), None, Some(r"C:\Program Files\Code.exe")) {
            Action::Edit {
                argv,
                fallback_path,
            } => {
                assert_eq!(argv[0], r"C:\Program Files\Code.exe");
                assert_eq!(argv[1], "-g");
                assert_eq!(argv[2], r"C:\work\src\main.rs:10:4");
                assert_eq!(fallback_path.as_deref(), Some(r"C:\work\src\main.rs"));
                assert!(editor_command(&argv).is_some());
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            spans[0].tooltip(Some(r"C:\work")),
            r"C:\work\src\main.rs:10:4"
        );
    }

    #[test]
    fn code_cmd_resolves_to_the_sibling_code_exe() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("bayan-link-{n}"));
        let bin = root.join("bin");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("code.cmd"), b"@echo off\r\n").unwrap();
        std::fs::write(root.join("Code.exe"), b"MZ").unwrap();
        let found = find_code_exe_in(std::iter::once(&bin), &[".EXE".into(), ".CMD".into()]);
        let found = found.expect("sibling Code.exe");
        assert_eq!(found.file_name().and_then(|s| s.to_str()), Some("Code.exe"));
        let _ = std::fs::remove_dir_all(&root);
        // A cmd shim with no real exe is not an editor we can spawn.
        // Its parent must not be the directory that holds Code.exe above:
        // `..\Code.exe` is how the shim case resolves, and Windows will
        // see that file.
        let alone_root = std::env::temp_dir().join(format!("bayan-link-nocode-{n}"));
        let alone = alone_root.join("bin");
        let _ = std::fs::remove_dir_all(&alone_root);
        std::fs::create_dir_all(&alone).unwrap();
        std::fs::write(alone.join("code.cmd"), b"@echo off\r\n").unwrap();
        assert!(find_code_exe_in(std::iter::once(&alone), &[".CMD".into()]).is_none());
        let _ = std::fs::remove_dir_all(&alone_root);
    }

    #[test]
    fn rtl_click_lands_on_the_link_under_the_glyph() {
        use crate::mouse::{column_for_click, GlyphBox};
        // The link is logical columns 3 and 4. The shaped RTL line draws
        // those on the left — the same boxes the mouse-reporting hit test
        // uses. `x / cell_w` at x = 2 would be column 0, which is not the link.
        let span = LinkSpan {
            start_col: 3,
            end_col: 5,
            kind: LinkKind::Url {
                uri: "https://example.com".into(),
            },
        };
        let boxes = [
            GlyphBox {
                x0: 40.0,
                x1: 50.0,
                col: 0,
            },
            GlyphBox {
                x0: 0.0,
                x1: 10.0,
                col: 4,
            },
            GlyphBox {
                x0: 10.0,
                x1: 20.0,
                col: 3,
            },
        ];
        let left = column_for_click(2.0, &boxes, 10.0, 5, 80, 0);
        assert_eq!(left, 4);
        assert_eq!((2.0_f32 / 10.0).floor() as usize, 0);
        assert!(span.contains(left));
        assert!(!span.contains(0), "a grid multiply would miss the link");
        let right = column_for_click(45.0, &boxes, 10.0, 5, 80, 0);
        assert_eq!(right, 0);
        assert!(!span.contains(right));
        // An LTR run is not mirrored onto the link.
        let ltr = [
            GlyphBox {
                x0: 0.0,
                x1: 10.0,
                col: 0,
            },
            GlyphBox {
                x0: 10.0,
                x1: 20.0,
                col: 1,
            },
        ];
        assert!(!span.contains(column_for_click(1.0, &ltr, 10.0, 2, 80, 0)));
    }

    #[test]
    fn ctrl_click_opens_a_link_and_shift_keeps_selection() {
        // Tracking on: a plain click is the program's, even on a link.
        assert_eq!(
            click_owner(false, true, false, false, true),
            ClickOwner::Program
        );
        // Ctrl+click is the link, tracking or not.
        assert_eq!(
            click_owner(false, true, false, true, true),
            ClickOwner::Link
        );
        assert_eq!(
            click_owner(false, false, false, true, true),
            ClickOwner::Link
        );
        // Shift keeps local selection, and it wins over Ctrl.
        assert_eq!(
            click_owner(false, true, true, false, true),
            ClickOwner::Local
        );
        assert_eq!(
            click_owner(false, true, true, true, true),
            ClickOwner::Local
        );
        assert_eq!(
            click_owner(false, false, true, true, true),
            ClickOwner::Local
        );
        // Ctrl+click on a cell that is not a link still reports.
        assert_eq!(
            click_owner(false, true, false, true, false),
            ClickOwner::Program
        );
        // No tracking, no ctrl: selection, link or not.
        assert_eq!(
            click_owner(false, false, false, false, true),
            ClickOwner::Local
        );
        assert_eq!(
            click_owner(false, false, false, false, false),
            ClickOwner::Local
        );
    }

    #[test]
    fn overlay_blocks_a_ctrl_click_on_a_link() {
        // The refused-link card, the paste guard, settings, the cockpit:
        // Ctrl+click underneath does not open, and it does not fall
        // through into a selection or a mouse report.
        assert_eq!(
            click_owner(true, true, false, true, true),
            ClickOwner::Blocked
        );
        assert_eq!(
            click_owner(true, false, false, true, true),
            ClickOwner::Blocked
        );
        assert_eq!(
            click_owner(true, true, true, true, true),
            ClickOwner::Blocked
        );
        assert_eq!(
            click_owner(true, false, false, false, false),
            ClickOwner::Blocked
        );
    }

    #[test]
    fn route_reports_ctrl_as_a_modifier_the_link_decision_is_separate() {
        // Route does not know about links. Ctrl is a modifier bit on the
        // report. The window asks click_owner first and only calls press
        // when the owner is the program — the cell is the same one.
        let mut route = crate::mouse::Route::default();
        let proto = crate::mouse::Proto {
            clicks: true,
            sgr: true,
            ..crate::mouse::Proto::default()
        };
        let cell = crate::mouse::Cell {
            pane: 1,
            row: 0,
            col: 4,
        };
        let out = route.press(
            cell,
            crate::mouse::Btn::Left,
            proto,
            crate::mouse::Mods {
                ctrl: true,
                ..crate::mouse::Mods::default()
            },
        );
        let bytes = out.expect("tracking consumes the press").expect("SGR");
        assert_eq!(bytes.pane, 1);
        assert_eq!(bytes.bytes, b"\x1b[<16;5;1M");
        assert_eq!(
            click_owner(false, true, false, true, true),
            ClickOwner::Link
        );
    }

    #[test]
    fn editor_config_key_round_trips() {
        let cfg: crate::config::UserConfig =
            serde_json::from_str(r#"{"editor":"code -g {file}:{line}:{col}"}"#).unwrap();
        assert_eq!(cfg.editor.as_deref(), Some("code -g {file}:{line}:{col}"));
        assert!(crate::config::UserConfig::default().editor.is_none());
    }
}
