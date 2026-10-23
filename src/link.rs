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
//! Opening is deliberately narrow. `http` and `https` go to `ShellExecuteW`
//! with the URI as the single `lpFile`. A `file` URI association-opens only
//! a local drive path whose extension is on the document/media allowlist
//! (`.txt`, `.md`, `.pdf`, images, …). Everything else, including no
//! extension, is revealed in Explorer (`/select`). `javascript:`, `ms-*`,
//! `search-ms:`, `data:`, a custom protocol, a UNC or device path, an
//! alternate data stream, and a path that breaks out of `/select,"…"` are
//! refused on a card. A `path:line` location goes to the editor's
//! `Command` argv, or to that same reveal when no editor is configured —
//! never to the file's own association. Control characters and bidi
//! overrides are refused too, including when they only appear after
//! percent-decoding, and nothing longer than [`MAX_TARGET`] bytes is
//! opened. `{file}` `{line}` `{col}` are substituted once, after the
//! template is split, so a path with spaces stays one argument and a
//! `{line}` inside the path is not rewritten. Disk checks and the open
//! itself run off the UI thread.
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

use std::collections::HashMap;
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
    /// PATHEXT or another extension Windows association-opens as a program.
    /// Association-open now reveals these instead of constructing this
    /// variant. The label stays so a card can still name the reason.
    #[allow(dead_code)]
    Blocked,
    /// The path is local, and it is not on disk.
    Missing,
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
            RefuseReason::Blocked => "لن يُفتح هذا النوع من الملفات",
            RefuseReason::Missing => "الملف غير موجود",
        }
    }
}

/// Shown on the confirm card when a URL carries userinfo. Enter opens it;
/// Esc dismisses. A [`RefuseReason`] card does not open on Enter.
pub const HOST_CONFIRM: &str = "تأكد من المضيف";

/// What a Ctrl+click should do. The window loop performs it; nothing here
/// builds a `cmd /c` string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// `ShellExecuteW` lpFile = this URI. http or https only.
    Browse(String),
    /// `ShellExecuteW` lpFile = this filesystem path. One argument.
    /// Only an allowlisted document or media extension, after `settle`
    /// has expanded an 8.3 name and re-checked the long path.
    OpenPath(String),
    /// `%WINDIR%\explorer.exe` with `/select,"<path>"` passed via
    /// `CommandExt::raw_arg`. Not an association open.
    Reveal(String),
    /// `Command::new(argv[0]).args(&argv[1..])`. No association fallback:
    /// a spawn failure is reported, and the file is not opened by its type.
    /// `path` is the file `settle` checks; it is not an association target.
    Edit { argv: Vec<String>, path: String },
    /// `path:line` with no editor chosen yet. `settle` looks up `code.exe`
    /// off the UI thread and either becomes [`Action::Edit`] or [`Action::Reveal`].
    EditDefault {
        path: String,
        line: u32,
        col: Option<u32>,
    },
    /// `http(s)` with userinfo, or a non-ASCII host. The card shows `host`
    /// first (punycode when the host was not ASCII). Enter opens `uri`.
    Confirm {
        uri: String,
        host: String,
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
            LinkKind::Osc { uri } | LinkKind::Url { uri } => host_tooltip(uri),
            LinkKind::File { path, line, col } => {
                let full = resolve_against_cwd(path, cwd).unwrap_or_else(|| path.clone());
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
                | '\u{00AD}'
                | '\u{034F}'
                | '\u{FEFF}'
        )
        || ('\u{2061}'..='\u{2064}').contains(&c)
        || ('\u{FFF9}'..='\u{FFFB}').contains(&c)
        || ('\u{E0000}'..='\u{E007F}').contains(&c)
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
/// that same field; callers do not grow a second source. The value has to
/// pass [`crate::term::accept_local_cwd`]: a UNC, `\\?\`, `\\.\`, or `\??\`
/// directory is dropped, and so is anything that is not a drive path.
/// Bidi and other forbidden characters are dropped here first — the cwd
/// gate only rejects controls.
pub fn cwd_hook(reported: Option<&str>) -> Option<String> {
    let c = reported?;
    if c.chars().any(is_forbidden) {
        return None;
    }
    crate::term::accept_local_cwd(c)
}

fn is_drive_abs(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// Join `path` onto `cwd` when the path is relative.
///
/// `None` when the result is not a local drive path: UNC and device paths,
/// a relative path with no pane cwd (Bayan's own cwd is not a fallback),
/// or a cwd [`cwd_hook`] already rejected. The returned path has been
/// through [`crate::term::accept_local_cwd`], so it starts with `X:\`.
pub fn resolve_against_cwd(path: &str, cwd: Option<&str>) -> Option<String> {
    if path.is_empty() || path.len() > MAX_TARGET || path.chars().any(is_forbidden) {
        return None;
    }
    let mut path = path.replace('/', "\\");
    if crate::term::unc_or_device(&path) {
        return None;
    }
    let joined = if is_drive_abs(&path) {
        path
    } else {
        // A single leading slash is the current-drive root. That still
        // depends on the process cwd, so it is not a path we open.
        if path.starts_with('\\') {
            return None;
        }
        while let Some(stripped) = path.strip_prefix(".\\") {
            path = stripped.to_string();
        }
        let cwd = cwd_hook(cwd)?;
        if path.is_empty() {
            cwd
        } else if cwd.ends_with('\\') {
            // Drive root already ends in a slash. A second one is `\\`,
            // which [`crate::term::accept_local_cwd`] rejects.
            format!("{cwd}{path}")
        } else {
            format!("{cwd}\\{path}")
        }
    };
    let full = crate::term::accept_local_cwd(&joined)?;
    if crate::term::unc_or_device(&full) || full.starts_with('-') {
        None
    } else {
        Some(full)
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

/// Decode well-formed `%HH` escapes. A stray `%` (`http://a/100%`) or a
/// `%` that is not followed by two hex digits stays a literal percent
/// sign — it is not a control character, and it is not a reason to refuse
/// the URI. Invalid UTF-8 after a real escape still yields `None`.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
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
/// drive path. The host is not the only check: the decoded path goes
/// through [`crate::term::accept_local_cwd`], so `file://///evil/share`,
/// a percent-encoded `\\evil\share`, and `\\?\UNC\…` are `None` before
/// anything stats the path.
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
    if decoded.chars().any(is_forbidden) {
        return None;
    }
    let path = if decoded.len() >= 3
        && decoded.as_bytes()[0] == b'/'
        && decoded.as_bytes()[1].is_ascii_alphabetic()
        && decoded.as_bytes()[2] == b':'
    {
        decoded[1..].to_string()
    } else {
        decoded
    };
    // A query or fragment is not part of the filesystem path. Leaving it
    // on would also hide a trailing `.exe`.
    let path = path.split(['?', '#']).next().unwrap_or(&path);
    let path = path.replace('/', "\\");
    if path.is_empty() || path.len() > MAX_TARGET {
        return None;
    }
    let full = crate::term::accept_local_cwd(&path)?;
    if crate::term::unc_or_device(&full) {
        None
    } else {
        Some(full)
    }
}

/// Host of an `http(s)` URI, and whether userinfo (`@`) was present.
/// The host is the part after the last `@`, so `good.com@evil.com` is
/// `evil.com`. The host is percent-decoded once before it is returned, so
/// `https://%D0%B0pple.com/` is the Cyrillic homograph. A `%` that is still
/// there after that decode is not a hostname.
fn http_authority(uri: &str) -> Option<(String, bool)> {
    let scheme = scheme_name(uri)?;
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
        return None;
    }
    let after = &uri[scheme.len()..];
    let rest = after.strip_prefix("://")?;
    if uri.contains('\\') || rest.is_empty() {
        return None;
    }
    let hostport = rest.split(['/', '?', '#']).next().unwrap_or("");
    if hostport.is_empty() {
        return None;
    }
    let has_userinfo = hostport.contains('@');
    let host = hostport.rsplit('@').next().unwrap_or(hostport);
    let host = if let Some(inner) = host.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or(host)
    };
    let host = decode_http_host(host)?;
    Some((host, has_userinfo))
}

/// One percent-decode, then the non-ASCII check in [`plan_uri`]. A leftover
/// `%` is rejected. A colon that the decode introduced is a smuggled port;
/// an IPv6 literal already contained its colons.
fn decode_http_host(raw: &str) -> Option<String> {
    let host = percent_decode(raw)?;
    if host.is_empty() || host == "." || host.contains('%') {
        return None;
    }
    if host.chars().any(|c| {
        c.is_whitespace() || is_forbidden(c) || matches!(c, '/' | '\\' | '@' | '?' | '#')
    }) {
        return None;
    }
    if host.contains(':') && !raw.contains(':') {
        return None;
    }
    Some(host)
}

/// RFC 3492 punycode of one label, without the `xn--` prefix.
/// ASCII letters are lowercased first. Overflow yields `None`.
fn punycode_encode(input: &str) -> Option<String> {
    const BASE: u32 = 36;
    const TMIN: u32 = 1;
    const TMAX: u32 = 26;
    const SKEW: u32 = 38;
    const DAMP: u32 = 700;
    const INITIAL_BIAS: u32 = 72;
    const INITIAL_N: u32 = 128;

    fn adapt(mut delta: u32, numpoints: u32, first: bool) -> u32 {
        delta /= if first { DAMP } else { 2 };
        delta += delta / numpoints;
        let mut k = 0u32;
        while delta > ((BASE - TMIN) * TMAX) / 2 {
            delta /= BASE - TMIN;
            k += BASE;
        }
        k + ((BASE - TMIN + 1) * delta) / (delta + SKEW)
    }

    fn digit(d: u32) -> char {
        if d < 26 {
            char::from(b'a' + d as u8)
        } else {
            char::from(b'0' + (d - 26) as u8)
        }
    }

    let cps: Vec<u32> = input
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase() as u32
            } else {
                c as u32
            }
        })
        .collect();
    let mut out = String::new();
    let mut basic = 0u32;
    for &c in &cps {
        if c < 128 {
            out.push(char::from_u32(c)?);
            basic += 1;
        }
    }
    let len = cps.len() as u32;
    if basic > 0 && basic < len {
        out.push('-');
    }
    let mut n = INITIAL_N;
    let mut delta = 0u32;
    let mut bias = INITIAL_BIAS;
    let mut h = basic;
    while h < len {
        let m = cps.iter().copied().filter(|&c| c >= n).min()?;
        let gap = m.checked_sub(n)?;
        delta = delta.checked_add(gap.checked_mul(h + 1)?)?;
        n = m;
        for &c in &cps {
            if c < n {
                delta = delta.checked_add(1)?;
            } else if c == n {
                let mut q = delta;
                let mut k = BASE;
                loop {
                    let t = if k <= bias {
                        TMIN
                    } else if k >= bias.saturating_add(TMAX) {
                        TMAX
                    } else {
                        k - bias
                    };
                    if q < t {
                        break;
                    }
                    let base_t = BASE - t;
                    out.push(digit(t + (q - t) % base_t));
                    q = (q - t) / base_t;
                    k = k.saturating_add(BASE);
                }
                out.push(digit(q));
                bias = adapt(delta, h + 1, h == basic);
                delta = 0;
                h += 1;
            }
        }
        delta = delta.checked_add(1)?;
        n = n.checked_add(1)?;
    }
    Some(out)
}

fn punycode_label(label: &str) -> Option<String> {
    if label.is_empty() {
        return None;
    }
    if label.is_ascii() {
        Some(label.to_ascii_lowercase())
    } else {
        punycode_encode(label).map(|body| format!("xn--{body}"))
    }
}

/// Each dot-label encoded. `None` when a label is empty or the encoder overflows.
fn punycode_host(host: &str) -> Option<String> {
    let mut labels = Vec::new();
    for label in host.split('.') {
        labels.push(punycode_label(label)?);
    }
    Some(labels.join("."))
}

/// Replace the host of an `http(s)` URI, keeping userinfo, port, path,
/// query, and fragment.
fn rewrite_host(uri: &str, ascii_host: &str) -> String {
    let Some(scheme) = scheme_name(uri) else {
        return uri.to_string();
    };
    let after = &uri[scheme.len()..];
    let Some(rest) = after.strip_prefix("://") else {
        return uri.to_string();
    };
    let hostport = rest.split(['/', '?', '#']).next().unwrap_or("");
    let suffix = &rest[hostport.len()..];
    let (userinfo, host_and_port) = if let Some(at) = hostport.rfind('@') {
        (&hostport[..=at], &hostport[at + 1..])
    } else {
        ("", hostport)
    };
    let port = if host_and_port.starts_with('[') {
        ""
    } else if let Some(idx) = host_and_port.rfind(':') {
        &host_and_port[idx..]
    } else {
        ""
    };
    let mut out = String::new();
    out.push_str(&uri[..scheme.len()]);
    out.push_str("://");
    out.push_str(userinfo);
    out.push_str(ascii_host);
    out.push_str(port);
    out.push_str(suffix);
    out
}

/// Tooltip text. Userinfo and a non-ASCII host lead with the host that
/// will actually be opened (punycode, when a label is not ASCII), so a
/// label of `good.com` cannot hide `evil.com` and an IDN cannot hide its
/// ASCII form.
fn host_tooltip(uri: &str) -> String {
    match http_authority(uri) {
        Some((host, has_userinfo)) if has_userinfo || !host.is_ascii() => {
            let shown = punycode_host(&host).unwrap_or(host);
            preview(&format!("{shown} — {uri}"), 200)
        }
        _ => preview(uri, 200),
    }
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
        match http_authority(uri) {
            Some((host, has_userinfo)) => {
                let non_ascii = !host.is_ascii();
                if has_userinfo || non_ascii {
                    let shown = if non_ascii {
                        match punycode_host(&host) {
                            Some(ascii) if ascii.is_ascii() => ascii,
                            _ => return refuse(RefuseReason::Scheme, uri),
                        }
                    } else {
                        host
                    };
                    let opened = if non_ascii {
                        rewrite_host(uri, &shown)
                    } else {
                        uri.to_string()
                    };
                    Action::Confirm {
                        uri: opened,
                        host: shown,
                    }
                } else {
                    Action::Browse(uri.to_string())
                }
            }
            None => refuse(RefuseReason::Scheme, uri),
        }
    } else if scheme.eq_ignore_ascii_case("file") {
        match file_uri_path(uri) {
            Some(path) => plan_local_open(&path),
            None => refuse(RefuseReason::BadFile, uri),
        }
    } else {
        refuse(RefuseReason::Scheme, uri)
    }
}

/// Association-open of a local file URI, as an intent. `exists` and 8.3
/// expansion run later in [`settle`], on the link thread. Only the
/// document/media allowlist may become [`Action::OpenPath`]; every other
/// extension, including none, is [`Action::Reveal`].
fn plan_local_open(path: &str) -> Action {
    if crate::term::unc_or_device(path) {
        return refuse(RefuseReason::BadFile, path);
    }
    if is_allowlisted(path) {
        Action::OpenPath(path.to_string())
    } else {
        Action::Reveal(path.to_string())
    }
}

fn local_file_exists(path: &str) -> bool {
    // A UNC stat opens SMB. Refuse that even if a caller skipped the gate.
    if crate::term::unc_or_device(path) {
        return false;
    }
    Path::new(path).exists()
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
    let Some(full) = resolve_against_cwd(path, cwd) else {
        return refuse(RefuseReason::BadFile, path);
    };
    // `path:line` never association-opens the file. A shell template is
    // refused before any disk check, so a missing file cannot hide `cmd /c`.
    // Whether the file exists is [`settle`]'s job, on the link thread.
    let configured = editor.map(str::trim).filter(|s| !s.is_empty());
    if let Some(template) = configured {
        match expand_editor(template, &full, Some(line), col) {
            Some(argv) if !is_shell_program(&argv[0]) => Action::Edit { argv, path: full },
            _ => refuse(RefuseReason::Editor, template),
        }
    } else if let Some(exe) = code_exe.map(str::trim).filter(|s| !s.is_empty()) {
        if is_shell_program(exe) {
            return refuse(RefuseReason::Editor, exe);
        }
        match expand_editor("code -g {file}:{line}:{col}", &full, Some(line), col) {
            Some(mut argv) => {
                argv[0] = exe.to_string();
                if is_shell_program(&argv[0]) {
                    return refuse(RefuseReason::Editor, exe);
                }
                Action::Edit { argv, path: full }
            }
            None => refuse(RefuseReason::Editor, exe),
        }
    } else {
        Action::EditDefault {
            path: full,
            line,
            col,
        }
    }
}

fn is_shell_program(prog: &str) -> bool {
    let base = prog.rsplit(['\\', '/']).next().unwrap_or(prog);
    let base = base.to_ascii_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base);
    matches!(
        base,
        "cmd"
            | "powershell"
            | "pwsh"
            | "bash"
            | "sh"
            | "wsl"
            | "fish"
            | "mshta"
            | "wscript"
            | "cscript"
            | "rundll32"
            | "regsvr32"
            | "msiexec"
            | "mmc"
            | "hh"
    )
}

/// Extensions `ShellExecuteW` `"open"` may receive. Source files are not
/// here: a `path:line` link goes to the editor, and a bare `file` URI of
/// a source file is revealed. `.svg` and `.csv` are revealed too.
const ALLOWED_EXTS: &[&str] = &[
    ".txt", ".md", ".log", ".json", ".pdf", ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp",
];

/// Filename used for the extension check. Trailing dots and spaces are
/// ignored the way Windows ignores them. A colon in the leaf is an
/// alternate data stream (`a.txt:evil.exe`); it matches no allowlisted
/// extension, and [`crate::term::accept_local_cwd`] rejects the path.
fn leaf_for_extension(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let file = file.trim_end_matches([' ', '.']);
    if file.contains(':') {
        return String::new();
    }
    file.to_ascii_lowercase()
}

fn ext_matches(leaf: &str, ext: &str) -> bool {
    leaf.len() > ext.len() && leaf.ends_with(ext)
}

fn is_allowlisted(path: &str) -> bool {
    let leaf = leaf_for_extension(path);
    ALLOWED_EXTS.iter().any(|ext| ext_matches(&leaf, ext))
}

fn has_short_component(path: &str) -> bool {
    path.split(['\\', '/']).any(|comp| comp.contains('~'))
}

/// `GetLongPathNameW` returns `\\?\` paths. `\\?\UNC\…` becomes a UNC
/// (`\\…`), which the local-drive gate then refuses. `\\?\C:\…` drops the prefix.
fn strip_verbatim(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    if lower.starts_with(r"\\?\unc\") {
        format!(r"\\{}", &path[r"\\?\UNC\".len()..])
    } else if lower.starts_with(r"\\?\") {
        path[r"\\?\".len()..].to_string()
    } else {
        path.to_string()
    }
}

/// Expand an 8.3 name when the file is already known to exist. `None`
/// means the name could not be expanded; a `~` component in that case is
/// revealed rather than association-opened.
fn win_long_path(path: &str) -> Option<String> {
    #[cfg(windows)]
    {
        use std::ffi::{OsStr, OsString};
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use windows_sys::Win32::Storage::FileSystem::GetLongPathNameW;
        let wide: Vec<u16> = OsStr::new(path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut buf = vec![0u16; 32_768];
        let n = unsafe { GetLongPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        if n == 0 || n as usize >= buf.len() {
            return None;
        }
        let os = OsString::from_wide(&buf[..n as usize]);
        Some(strip_verbatim(&os.to_string_lossy()))
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

enum DiskPath {
    /// Re-classify by the allowlist (the long path, when expansion worked).
    Ready(String),
    /// A `~` component that could not be expanded. Always reveal.
    ForceReveal(String),
}

fn prepare_disk<E, L>(path: &str, exists: &E, long_path: &L) -> Result<DiskPath, Action>
where
    E: Fn(&str) -> bool,
    L: Fn(&str) -> Option<String>,
{
    let Some(path) = crate::term::accept_local_cwd(path) else {
        return Err(refuse(RefuseReason::BadFile, path));
    };
    if crate::term::unc_or_device(&path) {
        return Err(refuse(RefuseReason::BadFile, &path));
    }
    if !exists(&path) {
        if has_short_component(&path) {
            return Ok(DiskPath::ForceReveal(path));
        }
        return Err(refuse(RefuseReason::Missing, &path));
    }
    match long_path(&path) {
        Some(long) => {
            let long = strip_verbatim(&long);
            let Some(long) = crate::term::accept_local_cwd(&long) else {
                return Err(refuse(RefuseReason::BadFile, &long));
            };
            if crate::term::unc_or_device(&long) {
                return Err(refuse(RefuseReason::BadFile, &long));
            }
            Ok(DiskPath::Ready(long))
        }
        None if has_short_component(&path) => Ok(DiskPath::ForceReveal(path)),
        None => Ok(DiskPath::Ready(path)),
    }
}

fn classify_open(path: String) -> Action {
    if is_allowlisted(&path) {
        Action::OpenPath(path)
    } else {
        Action::Reveal(path)
    }
}

fn rewrite_edit_path(argv: Vec<String>, from: &str, to: &str) -> Vec<String> {
    if from == to {
        argv
    } else {
        argv.into_iter().map(|arg| arg.replace(from, to)).collect()
    }
}

fn settle_edit_default<E, L, C>(
    path: String,
    line: u32,
    col: Option<u32>,
    exists: &E,
    long_path: &L,
    code_exe: &C,
) -> Action
where
    E: Fn(&str) -> bool,
    L: Fn(&str) -> Option<String>,
    C: Fn() -> Option<String>,
{
    let path = match prepare_disk(&path, exists, long_path) {
        Ok(DiskPath::ForceReveal(path)) => return Action::Reveal(path),
        Ok(DiskPath::Ready(path)) => path,
        Err(refusal) => return refusal,
    };
    // Still never association-open. No editor means reveal.
    if let Some(exe) = code_exe() {
        let exe = exe.trim();
        if !exe.is_empty() && !is_shell_program(exe) {
            if let Some(mut argv) =
                expand_editor("code -g {file}:{line}:{col}", &path, Some(line), col)
            {
                argv[0] = exe.to_string();
                if !is_shell_program(&argv[0]) {
                    return Action::Edit { argv, path };
                }
            }
        }
    }
    Action::Reveal(path)
}

/// Apply `exists` and 8.3 expansion, then the allowlist, off the UI thread.
/// `long_path` returns the expanded path (already without a `\\?\` prefix,
/// or with one — it is stripped). `None` means expansion failed.
pub fn settle<E, L, C>(action: Action, exists: E, long_path: L, code_exe: C) -> Action
where
    E: Fn(&str) -> bool,
    L: Fn(&str) -> Option<String>,
    C: Fn() -> Option<String>,
{
    match action {
        Action::OpenPath(path) | Action::Reveal(path) => match prepare_disk(&path, &exists, &long_path)
        {
            Ok(DiskPath::ForceReveal(path)) => Action::Reveal(path),
            Ok(DiskPath::Ready(path)) => classify_open(path),
            Err(refusal) => refusal,
        },
        Action::Edit { argv, path } => match prepare_disk(&path, &exists, &long_path) {
            Err(refusal) => refusal,
            Ok(DiskPath::ForceReveal(path)) => Action::Reveal(path),
            Ok(DiskPath::Ready(full)) => {
                let argv = rewrite_edit_path(argv, &path, &full);
                Action::Edit { argv, path: full }
            }
        },
        Action::EditDefault { path, line, col } => {
            settle_edit_default(path, line, col, &exists, &long_path, &code_exe)
        }
        other => other,
    }
}

pub fn settle_live(action: Action) -> Action {
    settle(action, local_file_exists, win_long_path, || {
        find_code_exe().map(|p| p.to_string_lossy().into_owned())
    })
}

/// Apartment COM, before `ShellExecuteW`. Called on the `bayan-link` thread.
pub fn init_link_thread() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        unsafe {
            let _hr = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        }
    }
}

/// Absolute `explorer.exe`. `WINDIR` is used only when it is a local drive
/// path whose last component is `Windows`. Otherwise `C:\Windows\explorer.exe`.
pub fn explorer_from_windir(windir: Option<&str>) -> String {
    const FALLBACK: &str = r"C:\Windows\explorer.exe";
    let Some(dir) = windir.map(str::trim).filter(|s| !s.is_empty()) else {
        return FALLBACK.to_string();
    };
    let Some(norm) = crate::term::accept_local_cwd(dir) else {
        return FALLBACK.to_string();
    };
    let leaf = norm.rsplit('\\').next().unwrap_or("");
    if leaf.eq_ignore_ascii_case("Windows") {
        format!(r"{norm}\explorer.exe")
    } else {
        FALLBACK.to_string()
    }
}

pub fn explorer_program() -> String {
    explorer_from_windir(std::env::var("WINDIR").ok().as_deref())
}

/// One raw argument: `/select,"<path>"`. The path has already rejected
/// comma and quote, so this cannot break out of the quotes.
pub fn select_arg(path: &str) -> String {
    format!(r#"/select,"{path}""#)
}

pub fn spawn_reveal(path: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new(explorer_program());
        cmd.raw_arg(select_arg(path));
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        cmd.spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "explorer",
        ))
    }
}

/// Browse, association-open, editor, and explorer run off the UI thread.
/// A refusal and a confirm card are decided on it. [`Action::EditDefault`]
/// is included so `exists` and `find_code_exe` run on the link thread.
pub fn runs_off_ui_thread(action: &Action) -> bool {
    matches!(
        action,
        Action::Browse(_)
            | Action::OpenPath(_)
            | Action::Reveal(_)
            | Action::Edit { .. }
            | Action::EditDefault { .. }
    )
}

pub fn perform_open(action: &Action) -> bool {
    match action {
        Action::Browse(target) | Action::OpenPath(target) => shell_execute(target),
        Action::Reveal(path) => spawn_reveal(path).is_ok(),
        Action::Edit { argv, .. } => spawn_editor(argv).is_ok(),
        Action::EditDefault { .. } | Action::Confirm { .. } | Action::Refuse { .. } => false,
    }
}

/// Enter on a userinfo card opens. Enter on a refusal dismisses. Esc
/// always dismisses. Any other key is ignored, so it is not typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardDecision {
    Ignore,
    Dismiss,
    Open,
}

pub fn card_decision(enter: bool, escape: bool, may_open: bool) -> CardDecision {
    if escape {
        CardDecision::Dismiss
    } else if enter && may_open {
        CardDecision::Open
    } else if enter {
        CardDecision::Dismiss
    } else {
        CardDecision::Ignore
    }
}

/// Title prefix. The confirm card and the refuse card do not share one.
pub fn card_title_prefix(may_open: bool) -> &'static str {
    if may_open {
        "تأكد قبل الفتح"
    } else {
        "رابط مرفوض"
    }
}

/// Arabic line, then English. Enter opens only on the confirm card.
pub fn card_key_lines(may_open: bool) -> (&'static str, &'static str) {
    if may_open {
        ("Enter يفتح · Esc يغلق", "Enter opens · Esc closes")
    } else {
        ("Enter يغلق · Esc يغلق", "Enter closes · Esc closes")
    }
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
        // One pass. The substituted path is not scanned again, so a
        // `{line}` that is part of the filename stays literal.
        let filled = subst_once(&tok, file, &line_s, &col_s);
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

fn subst_once(tok: &str, file: &str, line: &str, col: &str) -> String {
    let mut out = String::new();
    let mut rest = tok;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("{file}") {
            out.push_str(file);
            rest = after;
            continue;
        }
        if line.is_empty() {
            if let Some(after) = rest.strip_prefix(":{line}") {
                rest = after;
                continue;
            }
        }
        if col.is_empty() {
            if let Some(after) = rest.strip_prefix(":{col}") {
                rest = after;
                continue;
            }
        }
        if let Some(after) = rest.strip_prefix("{line}") {
            out.push_str(line);
            rest = after;
            continue;
        }
        if let Some(after) = rest.strip_prefix("{col}") {
            out.push_str(col);
            rest = after;
            continue;
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
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
/// does not do. `None` means a `path:line` link opens its folder with
/// `explorer.exe /select` instead of an editor.
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

#[derive(Clone)]
struct RowCell {
    col: usize,
    width: usize,
    ch: char,
    osc_id: Option<String>,
    osc_uri: Option<String>,
    /// HIDDEN cells are not part of a clickable span.
    hidden: bool,
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
        let hidden = cell.flags.intersects(Flags::HIDDEN);
        // A hidden cell keeps no hyperlink. Copying the URI onto a blank
        // would leave that column clickable.
        let href = if hidden { None } else { cell.hyperlink() };
        let ch = if hidden { ' ' } else { cell.c };
        cells.push(RowCell {
            col: cell.point.column.0,
            width,
            ch,
            osc_id: href.as_ref().map(|h| h.id().to_string()),
            osc_uri: href
                .as_ref()
                .map(|h| h.uri().to_string())
                .filter(|u| !u.is_empty()),
            hidden,
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
        if cells[i].hidden {
            i += 1;
            continue;
        }
        let Some(uri) = cells[i].osc_uri.clone() else {
            i += 1;
            continue;
        };
        let id = cells[i].osc_id.clone();
        let start = cells[i].col;
        let mut end = cells[i].col + cells[i].width;
        i += 1;
        while i < cells.len()
            && !cells[i].hidden
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

fn previous_token_is_path(text: &str, chars: &[(usize, char)], i: usize) -> bool {
    if i == 0 || !chars[i - 1].1.is_whitespace() {
        return false;
    }
    let mut k = i - 1;
    while k > 0 && chars[k - 1].1.is_whitespace() {
        k -= 1;
    }
    if k == 0 {
        return false;
    }
    let mut t = k;
    while t > 0 && !chars[t - 1].1.is_whitespace() {
        t -= 1;
    }
    let prev = &text[chars[t].0..chars[k].0];
    let drive =
        prev.len() >= 2 && prev.as_bytes()[0].is_ascii_alphabetic() && prev.as_bytes()[1] == b':';
    prev.contains('\\') || prev.contains('/') || drive
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
    // `C:\x y\a.rs:12` stops at the space and would open `y\a.rs`. If the
    // token before that space is itself a path, this match is the tail of
    // a spaced path — drop it. `see src/main.rs:10` stays: `see` is a word.
    if previous_token_is_path(text, chars, i) {
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

/// Spans for one pane, keyed by viewport row. The window invalidates a
/// pane when its PTY wakes, when that pane scrolls, and on relayout —
/// hover and click then reuse the row instead of locking the terminal.
#[derive(Clone, Debug, Default)]
pub struct SpanCache {
    panes: HashMap<u64, HashMap<usize, Vec<LinkSpan>>>,
}

impl SpanCache {
    pub fn get(&self, pane: u64, row: usize) -> Option<Vec<LinkSpan>> {
        self.panes.get(&pane)?.get(&row).cloned()
    }

    pub fn insert(&mut self, pane: u64, row: usize, spans: Vec<LinkSpan>) {
        self.panes.entry(pane).or_default().insert(row, spans);
    }

    pub fn invalidate_pane(&mut self, pane: u64) {
        self.panes.remove(&pane);
    }

    pub fn clear(&mut self) {
        self.panes.clear();
    }
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
                hidden: false,
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
            file_uri_path("file:///C:/src/main.rs").as_deref(),
            Some(r"C:\src\main.rs")
        );
        assert_eq!(
            file_uri_path("file://localhost/C:/Program%20Files/a.rs").as_deref(),
            Some(r"C:\Program Files\a.rs")
        );
        // A source file is not on the association allowlist. Reveal, and
        // the exists check is deferred to settle.
        assert!(matches!(
            plan_uri("file:///C:/bayan-qa-missing/no-such.rs"),
            Action::Reveal(ref path) if path == r"C:\bayan-qa-missing\no-such.rs"
        ));
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
        assert_eq!(cwd_hook(Some(r"  C:\src  ")).as_deref(), Some(r"C:\src"));
        assert_eq!(cwd_hook(Some("")), None);
        assert_eq!(cwd_hook(None), None);
        assert_eq!(cwd_hook(Some("C:\\src\u{202E}")), None);
        assert_eq!(cwd_hook(Some(r"\\evil\share")), None);
        assert_eq!(
            resolve_against_cwd("src/main.rs", Some(r"C:\work")).as_deref(),
            Some(r"C:\work\src\main.rs")
        );
        assert_eq!(
            resolve_against_cwd(r".\src\main.rs", Some(r"C:\work")).as_deref(),
            Some(r"C:\work\src\main.rs")
        );
        // Drive root keeps its slash; joining must not produce `\\`.
        assert_eq!(
            resolve_against_cwd(r"src\a.rs", Some(r"D:\")).as_deref(),
            Some(r"D:\src\a.rs")
        );
        // The file is not on disk. The intent is still Edit; settle is
        // what raises the missing-file card, so the UI thread does not stat.
        let spans = links_in_row(&row("src/main.rs:10:4"));
        match spans[0].activate(Some(r"C:\work"), Some("code -g {file}:{line}:{col}"), None) {
            Action::Edit { path, .. } => {
                assert_eq!(path, r"C:\work\src\main.rs");
                assert!(matches!(
                    settle(
                        Action::Edit {
                            argv: vec!["code".into()],
                            path: path.clone(),
                        },
                        |_| false,
                        |_| None,
                        || None,
                    ),
                    Action::Refuse {
                        reason: RefuseReason::Missing,
                        ..
                    }
                ));
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

    /// A drive path under the temp dir, created for the test. Each call
    /// gets its own directory so the suite can run in parallel.
    fn scratch(name: &str) -> (String, String) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("bayan-link-qa-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, b"x").unwrap();
        let raw = path.display().to_string();
        let raw = raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string();
        let full = crate::term::accept_local_cwd(&raw).expect("temp file is a drive path");
        let cwd = full
            .rsplit_once('\\')
            .map(|(d, _)| d.to_string())
            .expect("parent");
        (cwd, full)
    }

    #[test]
    fn unc_paths_are_not_opened() {
        for uri in [
            "file://///evil/share/x.txt",
            "file:///%5C%5Cevil%5Cshare%5Cx.txt",
            "file://localhost/%5C%5Cevil%5Cshare%5Cx.txt",
            "file://localhost/%2F%2Fevil%2Fshare%2Fx.txt",
            r"file:///\\evil\share\x",
            r"file:///\\?\UNC\evil\s",
            r"file:///\\.\UNC\evil\s",
            r"file:///\??\UNC\evil\s",
        ] {
            assert!(
                file_uri_path(uri).is_none(),
                "decoded a UNC path from {uri}"
            );
            assert!(
                matches!(
                    plan_uri(uri),
                    Action::Refuse {
                        reason: RefuseReason::BadFile,
                        ..
                    }
                ),
                "{uri} -> {:?}",
                plan_uri(uri)
            );
        }
        for path in [
            r"\\?\UNC\evil\s",
            r"\\evil\share\a.rs",
            "//evil/share/a.rs",
            r"\\.\pipe\bayan",
            r"\??\C:\Windows\a.rs",
            r"\\?\C:\Windows\a.rs",
        ] {
            assert_eq!(resolve_against_cwd(path, Some(r"C:\work")), None, "{path}");
            assert!(!local_file_exists(path), "stat of {path}");
        }
        assert_eq!(cwd_hook(Some(r"\\evil\share")), None);
        assert_eq!(cwd_hook(Some(r"//evil/share")), None);
        assert_eq!(cwd_hook(Some(r"\\?\UNC\evil\s")), None);
        assert_eq!(cwd_hook(Some(r"\\.\C:\x")), None);
        assert_eq!(cwd_hook(Some(r"\??\C:\x")), None);

        let spans = links_in_row(&row(r"\\evil\share\a.rs:12"));
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert!(matches!(
            spans[0].activate(Some(r"C:\work"), Some("code -g {file}:{line}:{col}"), None),
            Action::Refuse {
                reason: RefuseReason::BadFile,
                ..
            }
        ));
        let spans = links_in_row(&row("//evil/share/a.rs:3"));
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert!(matches!(
            spans[0].activate(None, Some(r"C:\Program Files\Code.exe"), None),
            Action::Refuse {
                reason: RefuseReason::BadFile,
                ..
            }
        ));
        // Not an editor argument and not an association open.
        assert!(!matches!(
            spans[0].activate(Some(r"C:\work"), None, Some(r"C:\Code.exe")),
            Action::OpenPath(_) | Action::Edit { .. } | Action::Reveal(_) | Action::Browse(_)
        ));
    }

    #[test]
    fn dangerous_extensions_are_not_association_opened() {
        // The old blocklist, kept as a fixture, plus the extensions that
        // used to reach ShellExecuteW, plus PATHEXT's built-in default
        // and a custom PATHEXT-shaped extension. None of them are
        // association-opened. No extension is not, either.
        let exts = [
            ".com",
            ".exe",
            ".bat",
            ".cmd",
            ".lnk",
            ".url",
            ".hta",
            ".scr",
            ".msi",
            ".ps1",
            ".vbs",
            ".vbe",
            ".js",
            ".jse",
            ".wsf",
            ".wsh",
            ".cpl",
            ".appref-ms",
            ".library-ms",
            ".search-ms",
            ".settingcontent-ms",
            ".application",
            ".reg",
            ".pif",
            ".msc",
            ".chm",
            ".jar",
            ".diagcab",
            ".website",
            ".wsc",
            ".sct",
            ".xbap",
            ".mht",
            ".msp",
            ".msu",
            ".ppkg",
            ".msix",
            ".appx",
            ".appinstaller",
            ".rdp",
            ".theme",
            ".themepack",
            ".scf",
            ".searchConnector-ms",
            ".iso",
            ".img",
            ".vhd",
            ".vhdx",
            ".foo",
            ".csv",
            ".svg",
            "",
        ];
        for ext in exts {
            let uri = format!("file:///C:/qa/evil{ext}");
            match plan_uri(&uri) {
                Action::Reveal(path) => {
                    assert!(!path.to_ascii_lowercase().contains("cmd /c"), "{path}");
                    let settled = settle(
                        Action::Reveal(path),
                        |_| true,
                        |_| None,
                        || panic!("reveal must not search for an editor"),
                    );
                    assert!(
                        matches!(settled, Action::Reveal(_)),
                        "{ext} settled to {settled:?}"
                    );
                }
                Action::Refuse { .. } => {}
                other => panic!("{ext} -> {other:?}"),
            }
            if !ext.is_empty() {
                let upper = format!("file:///C:/qa/evil{}", ext.to_ascii_uppercase());
                assert!(
                    matches!(plan_uri(&upper), Action::Reveal(_) | Action::Refuse { .. }),
                    "{upper}"
                );
                assert!(!matches!(plan_uri(&upper), Action::OpenPath(_)), "{upper}");
            }
        }
        // Final extension wins. A trailing dot is ignored, the way Windows ignores it.
        assert!(matches!(
            plan_uri("file:///C:/qa/evil.exe.txt"),
            Action::OpenPath(ref p) if p.ends_with(r"evil.exe.txt")
        ));
        assert!(matches!(
            plan_uri("file:///C:/qa/evil.txt.exe"),
            Action::Reveal(_)
        ));
        assert!(matches!(
            plan_uri("file:///C:/qa/evil.exe."),
            Action::Reveal(_)
        ));
        // Enter on a refusal does not open. A confirm card is the one that does.
        assert_eq!(card_decision(true, false, false), CardDecision::Dismiss);
        assert_eq!(card_decision(false, true, false), CardDecision::Dismiss);
    }

    #[test]
    fn file_line_goes_to_the_editor_or_explorer_select() {
        let (cwd, bat) = scratch("setup.bat");
        let leaf = bat.rsplit('\\').next().unwrap();
        let spans = links_in_row(&row(&format!("{leaf}:1")));
        assert_eq!(spans.len(), 1, "{spans:?}");
        // No editor yet: the intent defers exists and code.exe to the link thread.
        match spans[0].activate(Some(&cwd), None, None) {
            Action::EditDefault { path, line, col } => {
                assert_eq!(path, bat);
                assert_eq!(line, 1);
                assert!(col.is_none());
                let settled = settle(
                    Action::EditDefault { path, line, col },
                    |_| true,
                    |_| None,
                    || None,
                );
                match settled {
                    Action::Reveal(path) => {
                        assert_eq!(path, bat);
                        assert_eq!(select_arg(&path), format!(r#"/select,"{path}""#));
                        let prog = explorer_from_windir(Some(r"C:\Windows"));
                        assert_eq!(prog, r"C:\Windows\explorer.exe");
                        assert!(!prog.to_ascii_lowercase().contains("cmd"));
                        assert!(!select_arg(&path).contains("/c"));
                        assert!(runs_off_ui_thread(&Action::Reveal(path)));
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
        match spans[0].activate(Some(&cwd), None, Some(r"C:\Program Files\Code.exe")) {
            Action::Edit { argv, path } => {
                assert_eq!(path, bat);
                assert_eq!(argv[0], r"C:\Program Files\Code.exe");
                assert_eq!(argv[1], "-g");
                assert_eq!(argv[2], format!("{bat}:1"));
                assert!(editor_command(&argv).is_some());
            }
            other => panic!("{other:?}"),
        }
        // Configured editor, still no association fallback.
        match spans[0].activate(Some(&cwd), Some("code -g {file}:{line}:{col}"), None) {
            Action::Edit { argv, path } => {
                assert_eq!(path, bat);
                assert_eq!(argv[0], "code");
                assert_eq!(argv[2], format!("{bat}:1"));
            }
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn relative_path_without_cwd_is_refused() {
        assert_eq!(resolve_against_cwd("src/main.rs", None), None);
        assert_eq!(resolve_against_cwd(r".\src\main.rs", None), None);
        let spans = links_in_row(&row("src/main.rs:10:4"));
        assert!(matches!(
            spans[0].activate(None, Some("code -g {file}:{line}:{col}"), None),
            Action::Refuse {
                reason: RefuseReason::BadFile,
                ..
            }
        ));
        assert!(matches!(
            spans[0].activate(None, None, Some(r"C:\Code.exe")),
            Action::Refuse {
                reason: RefuseReason::BadFile,
                ..
            }
        ));
        // A reported UNC cwd is not a directory to join onto.
        assert!(matches!(
            spans[0].activate(Some(r"\\evil\share"), Some("code -g {file}:{line}"), None),
            Action::Refuse {
                reason: RefuseReason::BadFile,
                ..
            }
        ));
    }

    #[test]
    fn missing_file_is_refused() {
        let missing_txt = plan_uri(r"file:///C:/bayan-qa-missing/no-such.txt");
        assert!(matches!(missing_txt, Action::OpenPath(_)), "{missing_txt:?}");
        assert!(matches!(
            settle(missing_txt, |_| false, |_| None, || None),
            Action::Refuse {
                reason: RefuseReason::Missing,
                ..
            }
        ));
        let spans = links_in_row(&row(r"C:\bayan-qa-missing\no-such.rs:3"));
        match spans[0].activate(Some(r"D:\other"), Some("code -g {file}:{line}"), None) {
            Action::Edit { path, .. } => {
                assert!(matches!(
                    settle(
                        Action::Edit {
                            argv: vec!["code".into()],
                            path,
                        },
                        |_| false,
                        |_| None,
                        || None,
                    ),
                    Action::Refuse {
                        reason: RefuseReason::Missing,
                        ..
                    }
                ));
            }
            other => panic!("{other:?}"),
        }
        match spans[0].activate(Some(r"D:\other"), None, None) {
            Action::EditDefault { path, line, col } => {
                assert!(matches!(
                    settle(
                        Action::EditDefault { path, line, col },
                        |_| false,
                        |_| None,
                        || None,
                    ),
                    Action::Refuse {
                        reason: RefuseReason::Missing,
                        ..
                    }
                ));
            }
            other => panic!("{other:?}"),
        }
        // A real text file is still an association intent. An executable
        // is revealed, even when it exists.
        let (txt_dir, txt) = scratch("note.txt");
        let uri = format!("file:///{}", txt.replace('\\', "/"));
        assert_eq!(plan_uri(&uri), Action::OpenPath(txt));
        let (exe_dir, exe) = scratch("evil.exe");
        let uri = format!("file:///{}", exe.replace('\\', "/"));
        match plan_uri(&uri) {
            Action::Reveal(path) => {
                assert_eq!(path, exe);
                assert!(matches!(
                    settle(Action::Reveal(path), |_| true, |_| None, || None),
                    Action::Reveal(_)
                ));
            }
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&txt_dir);
        let _ = std::fs::remove_dir_all(&exe_dir);
    }

    #[test]
    fn editor_placeholder_is_substituted_once() {
        let argv = expand_editor(
            "code -g {file}:{line}:{col}",
            r"C:\src\a{line}b.rs",
            Some(9),
            Some(2),
        )
        .unwrap();
        assert_eq!(argv[2], r"C:\src\a{line}b.rs:9:2");
        let argv = expand_editor("code {file}", r"C:\src\a{file}b.rs", None, None).unwrap();
        assert_eq!(argv[1], r"C:\src\a{file}b.rs");
        // The colon that only introduced an empty placeholder is still dropped,
        // and it is the template's colon, not a colon inside the path.
        let argv = expand_editor(
            "code -g {file}:{line}:{col}",
            r"C:\src\a{line}b.rs",
            Some(4),
            None,
        )
        .unwrap();
        assert_eq!(argv[2], r"C:\src\a{line}b.rs:4");
    }

    #[test]
    fn absolute_path_cannot_become_an_editor_flag() {
        let (cwd, full) = scratch("-n.rs");
        let spans = links_in_row(&row("-n.rs:2"));
        match spans[0].activate(Some(&cwd), Some("code -g {file}:{line}"), None) {
            Action::Edit { argv, path } => {
                assert_eq!(path, full);
                assert_eq!(argv[0], "code");
                assert_eq!(argv[1], "-g");
                assert_eq!(argv[2], format!("{full}:2"));
                assert!(argv[2].as_bytes()[1] == b':', "{argv:?}");
                assert!(!argv[2].starts_with('-'), "{argv:?}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            resolve_against_cwd("-n.rs", Some(&cwd)).as_deref(),
            Some(full.as_str())
        );
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn stray_percent_is_not_a_control_character() {
        assert_eq!(
            plan_uri("http://a/100%"),
            Action::Browse("http://a/100%".into())
        );
        // Not two hex digits: a literal percent, not "control characters".
        assert_eq!(
            plan_uri("https://example.com/%ZZ"),
            Action::Browse("https://example.com/%ZZ".into())
        );
        assert_eq!(
            plan_uri("https://example.com/%E"),
            Action::Browse("https://example.com/%E".into())
        );
        match plan_uri("https://example.com/%00") {
            Action::Refuse {
                reason: RefuseReason::ControlOrBidi,
                ..
            } => {}
            other => panic!("encoded NUL -> {other:?}"),
        }
        match plan_uri("https://example.com/%E2%80%AEhidden") {
            Action::Refuse {
                reason: RefuseReason::ControlOrBidi,
                shown,
            } => {
                // The card shows the escape, not a live RLO.
                assert!(shown.contains("%E2%80%AE"), "{shown}");
                assert!(!shown.contains('\u{202E}'), "{shown}");
            }
            other => panic!("encoded RLO -> {other:?}"),
        }
    }

    #[test]
    fn extra_invisible_characters_are_forbidden() {
        let chars = [
            '\u{00AD}',
            '\u{034F}',
            '\u{2061}',
            '\u{2062}',
            '\u{2063}',
            '\u{2064}',
            '\u{FFF9}',
            '\u{FFFA}',
            '\u{FFFB}',
            '\u{E0000}',
            '\u{E0001}',
            '\u{E007F}',
        ];
        for c in chars {
            assert!(is_forbidden(c), "U+{:04X}", c as u32);
            let uri = format!("https://example.com/{c}x");
            match plan_uri(&uri) {
                Action::Refuse {
                    reason: RefuseReason::ControlOrBidi,
                    shown,
                } => assert!(!shown.chars().any(is_forbidden), "{shown}"),
                other => panic!("U+{:04X} -> {other:?}", c as u32),
            }
        }
        assert!(!is_forbidden('a'));
        assert!(!is_forbidden('ع'));
    }

    #[test]
    fn path_with_a_space_is_not_a_partial_link() {
        assert!(
            files_in(r"C:\x y\a.rs:12").is_empty(),
            "the tail of a spaced path must not be a link"
        );
        assert!(files_in(r"C:\x y\a.rs(12, 1)").is_empty());
        assert_eq!(
            files_in("see src/main.rs:10"),
            vec![("src/main.rs".into(), 10, None, "src/main.rs:10".into())]
        );
        assert_eq!(
            files_in(r"error at C:\src\a.rs:10"),
            vec![(r"C:\src\a.rs".into(), 10, None, r"C:\src\a.rs:10".into())]
        );
    }

    #[test]
    fn hidden_cells_are_not_links() {
        let mut cells = row("click");
        for c in &mut cells {
            c.osc_id = Some("id".into());
            c.osc_uri = Some("https://example.com/secret".into());
        }
        cells[2].hidden = true;
        let spans = links_in_row(&cells);
        assert!(spans.iter().all(|s| !s.contains(2)), "{spans:?}");
        assert!(spans.iter().any(|s| s.contains(0)), "{spans:?}");
        assert!(spans.iter().any(|s| s.contains(4)), "{spans:?}");
        let mut all_hidden = cells.clone();
        for c in &mut all_hidden {
            c.hidden = true;
        }
        assert!(links_in_row(&all_hidden).is_empty());
    }

    #[test]
    fn link_row_cache_reuses_spans_until_invalidated() {
        let mut cache = SpanCache::default();
        let mut builds = 0u32;
        let sample = vec![LinkSpan {
            start_col: 0,
            end_col: 4,
            kind: LinkKind::Url {
                uri: "https://example.com".into(),
            },
        }];
        let first = {
            builds += 1;
            cache.insert(7, 0, sample.clone());
            cache.get(7, 0).unwrap()
        };
        assert_eq!(first, sample);
        assert_eq!(cache.get(7, 0), Some(sample.clone()));
        assert_eq!(builds, 1, "a hit must not rebuild the row");
        assert!(cache.get(7, 1).is_none());
        cache.invalidate_pane(7);
        assert!(cache.get(7, 0).is_none(), "damage drops the pane");
        cache.insert(7, 0, sample.clone());
        cache.insert(8, 0, sample.clone());
        cache.clear();
        assert!(cache.get(7, 0).is_none());
        assert!(cache.get(8, 0).is_none());
    }

    #[test]
    fn shell_helpers_include_script_hosts() {
        for name in [
            "mshta",
            "mshta.exe",
            r"C:\Windows\System32\mshta.exe",
            "wscript",
            "wscript.exe",
            "cscript",
            "cscript.exe",
            "rundll32",
            "rundll32.exe",
            r"C:\Windows\SysWOW64\rundll32.exe",
            "regsvr32",
            "regsvr32.exe",
            "msiexec",
            "msiexec.exe",
            "mmc",
            "mmc.exe",
            "hh",
            "hh.exe",
            r"C:\Windows\hh.exe",
        ] {
            assert!(
                editor_command(&[name.into(), r"C:\src\a.rs".into()]).is_none(),
                "{name} was accepted as an editor"
            );
        }
    }

    #[test]
    fn userinfo_highlights_the_real_host_and_asks() {
        let spans = links_in_row(&row("see http://good.com@evil.com/a next"));
        assert_eq!(spans.len(), 1, "{spans:?}");
        let tip = spans[0].tooltip(None);
        assert!(tip.starts_with("evil.com"), "{tip}");
        assert!(tip.contains("good.com@evil.com"), "{tip}");
        match spans[0].activate(None, None, None) {
            Action::Confirm { uri, host } => {
                assert_eq!(host, "evil.com");
                assert_eq!(uri, "http://good.com@evil.com/a");
                assert!(!runs_off_ui_thread(&Action::Confirm {
                    uri: uri.clone(),
                    host: host.clone(),
                }));
                assert_eq!(card_decision(true, false, true), CardDecision::Open);
                assert_eq!(card_decision(false, true, true), CardDecision::Dismiss);
                assert_eq!(card_decision(false, false, true), CardDecision::Ignore);
            }
            other => panic!("{other:?}"),
        }
        // No userinfo: open, and the tooltip is the URI itself.
        assert_eq!(
            plan_uri("https://evil.com/a"),
            Action::Browse("https://evil.com/a".into())
        );
        let plain = links_in_row(&row("https://evil.com/a"));
        assert_eq!(plain[0].tooltip(None), "https://evil.com/a");
        // user:pass@host is the same card. The host is after the last @.
        match plan_uri("http://user:secret@good.com@evil.com/x") {
            Action::Confirm { host, .. } => assert_eq!(host, "evil.com"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn opens_leave_the_ui_thread() {
        assert!(runs_off_ui_thread(&Action::Browse(
            "https://example.com".into()
        )));
        assert!(runs_off_ui_thread(&Action::OpenPath(r"C:\a.txt".into())));
        assert!(runs_off_ui_thread(&Action::Reveal(r"C:\a.txt".into())));
        assert!(runs_off_ui_thread(&Action::Edit {
            argv: vec!["code".into()],
            path: r"C:\a.txt".into(),
        }));
        assert!(runs_off_ui_thread(&Action::EditDefault {
            path: r"C:\a.rs".into(),
            line: 1,
            col: None,
        }));
        assert!(!runs_off_ui_thread(&Action::Refuse {
            reason: RefuseReason::Blocked,
            shown: String::new(),
        }));
        assert!(!runs_off_ui_thread(&Action::Confirm {
            uri: "http://a@b".into(),
            host: "b".into(),
        }));
        assert!(!perform_open(&Action::Refuse {
            reason: RefuseReason::Blocked,
            shown: "x".into(),
        }));
        assert!(!perform_open(&Action::Confirm {
            uri: "http://a@b".into(),
            host: "b".into(),
        }));
        assert!(!perform_open(&Action::EditDefault {
            path: r"C:\a.rs".into(),
            line: 1,
            col: None,
        }));
    }

    #[test]
    fn association_allowlist_opens_only_documents_and_media() {
        for ext in [
            ".txt", ".md", ".log", ".json", ".pdf", ".png", ".jpg", ".jpeg", ".gif", ".webp",
            ".bmp",
        ] {
            let uri = format!("file:///C:/qa/note{ext}");
            match plan_uri(&uri) {
                Action::OpenPath(path) => {
                    assert!(path.to_ascii_lowercase().ends_with(ext), "{path}");
                    assert!(matches!(
                        settle(Action::OpenPath(path.clone()), |_| false, |_| None, || None),
                        Action::Refuse {
                            reason: RefuseReason::Missing,
                            ..
                        }
                    ));
                    assert!(matches!(
                        settle(Action::OpenPath(path), |_| true, |_| None, || None),
                        Action::OpenPath(_)
                    ));
                }
                other => panic!("{ext} -> {other:?}"),
            }
            let upper = format!("file:///C:/qa/note{}", ext.to_ascii_uppercase());
            assert!(matches!(plan_uri(&upper), Action::OpenPath(_)), "{upper}");
        }
        for ext in [".csv", ".svg"] {
            for uri in [
                format!("file:///C:/qa/note{ext}"),
                format!("file:///C:/qa/note{}", ext.to_ascii_uppercase()),
            ] {
                assert!(
                    matches!(plan_uri(&uri), Action::Reveal(_)),
                    "{uri} -> {:?}",
                    plan_uri(&uri)
                );
                assert!(!matches!(plan_uri(&uri), Action::OpenPath(_)), "{uri}");
            }
        }
    }

    #[test]
    fn ads_colon_after_the_drive_is_rejected() {
        assert_eq!(crate::term::accept_local_cwd(r"C:\a.txt:evil.exe"), None);
        assert_eq!(crate::term::accept_local_cwd(r"C:\evil.exe::$DATA"), None);
        assert!(leaf_for_extension(r"C:\a.txt:evil.exe").is_empty());
        assert!(!leaf_for_extension(r"C:\a.txt:evil.exe").ends_with(".txt"));
        assert!(matches!(
            plan_uri("file:///C:/a.txt:evil.exe"),
            Action::Refuse {
                reason: RefuseReason::BadFile,
                ..
            }
        ));
        assert!(matches!(
            plan_uri("file:///C:/a.txt%3Aevil.exe"),
            Action::Refuse {
                reason: RefuseReason::BadFile,
                ..
            }
        ));
        assert!(!matches!(
            plan_uri("file:///C:/a.txt:evil.exe"),
            Action::OpenPath(_)
        ));
    }

    #[test]
    fn short_names_are_expanded_before_open() {
        let open = settle(
            Action::OpenPath(r"C:\X~1.APP".into()),
            |_| true,
            |_| Some(r"C:\notes.txt".into()),
            || None,
        );
        assert_eq!(open, Action::OpenPath(r"C:\notes.txt".into()));

        let exe = settle(
            Action::Reveal(r"C:\X~1.APP".into()),
            |_| true,
            |_| Some(r"C:\evil.exe".into()),
            || None,
        );
        assert!(matches!(exe, Action::Reveal(ref p) if p == r"C:\evil.exe"));

        let unexpanded = settle(
            Action::OpenPath(r"C:\X~1.TXT".into()),
            |_| true,
            |_| None,
            || None,
        );
        assert!(matches!(unexpanded, Action::Reveal(_)), "{unexpanded:?}");

        let calls = std::cell::RefCell::new(Vec::new());
        let unc = settle(
            Action::OpenPath(r"C:\X~1.APP".into()),
            |p| {
                assert!(!p.starts_with(r"\\"), "must not stat {p}");
                calls.borrow_mut().push(p.to_string());
                true
            },
            |_| Some(r"\\evil\share\a.txt".into()),
            || None,
        );
        assert!(matches!(
            unc,
            Action::Refuse {
                reason: RefuseReason::BadFile,
                ..
            }
        ));
        assert_eq!(calls.borrow().as_slice(), [r"C:\X~1.APP".to_string()]);

        let missing = settle(
            Action::OpenPath(r"C:\NOTEXIST~1.TXT".into()),
            |_| false,
            |_| panic!("a missing short name is not expanded"),
            || None,
        );
        assert!(
            matches!(missing, Action::Reveal(_)),
            "unexpanded tilde reveals, {missing:?}"
        );
    }

    #[test]
    fn explorer_select_rejects_injection() {
        for bad in [
            r"C:\x,/root,\\evil\share",
            r"C:\foo\\bar",
            r"C:\foo//bar",
            "C:\\foo\"bar",
            r"C:\..\..\evil",
        ] {
            assert_eq!(cwd_hook(Some(bad)), None, "{bad}");
            assert_eq!(crate::term::accept_local_cwd(bad), None, "{bad}");
        }
        assert_eq!(
            crate::term::accept_local_cwd(r"C:\a\..\b").as_deref(),
            Some(r"C:\b")
        );
        assert_eq!(
            explorer_from_windir(Some(r"C:\Windows")),
            r"C:\Windows\explorer.exe"
        );
        assert_eq!(
            explorer_from_windir(Some(r"D:\Windows")),
            r"D:\Windows\explorer.exe"
        );
        assert_eq!(
            explorer_from_windir(Some(r"C:\evil")),
            r"C:\Windows\explorer.exe"
        );
        assert_eq!(
            explorer_from_windir(Some(r"\\evil\share")),
            r"C:\Windows\explorer.exe"
        );
        assert_eq!(
            explorer_from_windir(Some(r"C:\Windows\..\evil")),
            r"C:\Windows\explorer.exe"
        );
        assert_eq!(explorer_from_windir(None), r"C:\Windows\explorer.exe");
        assert_eq!(
            select_arg(r"C:\qa\note.txt"),
            r#"/select,"C:\qa\note.txt""#
        );
        let prog = explorer_from_windir(Some(r"C:\Windows"));
        assert!(prog.to_ascii_lowercase().ends_with(r"\explorer.exe"));
        assert!(!prog.to_ascii_lowercase().contains("cmd"));
    }

    #[test]
    fn idn_host_confirms_with_punycode() {
        assert_eq!(punycode_encode("münchen").as_deref(), Some("mnchen-3ya"));
        assert_eq!(punycode_encode("bücher").as_deref(), Some("bcher-kva"));
        match plan_uri("http://münchen.example/a") {
            Action::Confirm { uri, host } => {
                assert_eq!(host, "xn--mnchen-3ya.example");
                assert!(uri.contains("xn--mnchen-3ya.example"), "{uri}");
                assert!(!uri.contains('ü'), "{uri}");
                assert_eq!(uri, "http://xn--mnchen-3ya.example/a");
            }
            other => panic!("{other:?}"),
        }
        match plan_uri("http://user:secret@münchen.example:8080/a?q=1") {
            Action::Confirm { uri, host } => {
                assert_eq!(host, "xn--mnchen-3ya.example");
                assert_eq!(uri, "http://user:secret@xn--mnchen-3ya.example:8080/a?q=1");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            plan_uri("http://xn--mnchen-3ya.example/a"),
            Action::Browse("http://xn--mnchen-3ya.example/a".into())
        );
        let tip = host_tooltip("http://münchen.example/a");
        assert!(tip.starts_with("xn--mnchen-3ya.example"), "{tip}");
    }

    #[test]
    fn card_keys_say_whether_enter_opens() {
        let (ar, en) = card_key_lines(true);
        assert!(ar.contains("Enter يفتح"), "{ar}");
        assert!(en.contains("Enter opens"), "{en}");
        assert_eq!(card_title_prefix(true), "تأكد قبل الفتح");
        let (ar, en) = card_key_lines(false);
        assert!(ar.contains("Enter يغلق"), "{ar}");
        assert!(en.contains("Enter closes"), "{en}");
        assert_eq!(card_title_prefix(false), "رابط مرفوض");
        assert_ne!(card_key_lines(true), card_key_lines(false));
        assert_eq!(card_decision(true, false, true), CardDecision::Open);
        assert_eq!(card_decision(true, false, false), CardDecision::Dismiss);
        assert_eq!(card_decision(false, true, true), CardDecision::Dismiss);
    }

    /// Already rejected by `file_uri_path` before any stat or `ShellExecuteW`.
    /// No `OpenPath`, and no reveal of a network path.
    fn assert_file_link_refused(uri: &str) {
        assert!(
            file_uri_path(uri).is_none(),
            "decoded a path from {uri}: {:?}",
            file_uri_path(uri)
        );
        match plan_uri(uri) {
            Action::Refuse {
                reason: RefuseReason::BadFile,
                ..
            } => {}
            Action::OpenPath(path) => panic!("{uri} OpenPath {path}"),
            Action::Reveal(path) => panic!("{uri} revealed {path}"),
            other => panic!("{uri} -> {other:?}"),
        }
    }

    #[test]
    fn file_five_slash_unc_is_refused() {
        assert_file_link_refused("file://///evil.example/share/secret");
    }

    #[test]
    fn file_localhost_encoded_slashes_are_refused() {
        assert_file_link_refused("file://localhost/%2F%2Fevil.example/share/x");
    }

    #[test]
    fn file_empty_host_encoded_slashes_are_refused() {
        assert_file_link_refused("file:///%2F%2Fevil.example/share/x");
    }

    #[test]
    fn file_localhost_encoded_backslashes_are_refused() {
        assert_file_link_refused("file://localhost/%5C%5Cevil.example/share/x");
    }

    #[test]
    fn file_unc_variants_are_refused() {
        for uri in [
            "file://localhost/%2f%2fevil.example/share/x.txt",
            "file://localhost/%2F%2fevil.example/share/x",
            "file://localhost/%5c%5cevil.example/share/x.txt",
            "file:///%2f%5cEvil.example/share/x.txt",
            "file://localhost/%5C%2Fevil.example/share/x",
            "file://localhost/%252F%252Fevil.example/share/x.txt",
            "file:///%252F%252Fevil.example/share/x",
            "file://localhost/%255C%255Cevil.example/share/x.txt",
            r"file:///\\evil.example\share\secret",
            r"file://localhost/\\evil.example\share\x",
            r"file:///\evil.example\share\x.txt",
            "file:////evil.example/share/secret",
            "file:////evil.example/share/secret.txt",
            "file://////evil.example/share/secret.txt",
            "file://?/UNC/evil.example/share/x",
            "file://?/UNC/evil.example/share/x.txt",
            "file:///?/UNC/evil.example/share/x.txt",
            r"file:///\\?\UNC\evil.example\share\x.txt",
            r"file:///\??\UNC\evil\s",
        ] {
            assert_file_link_refused(uri);
        }
    }

    #[test]
    fn percent_encoded_host_confirms_with_punycode() {
        match plan_uri("https://%D0%B0pple.com/") {
            Action::Confirm { uri, host } => {
                assert_eq!(host, "xn--pple-43d.com");
                assert_eq!(uri, "https://xn--pple-43d.com/");
                assert!(!uri.to_ascii_lowercase().contains("%d0%b0"), "{uri}");
            }
            other => panic!("{other:?}"),
        }
        let tip = host_tooltip("https://%D0%B0pple.com/");
        assert!(tip.starts_with("xn--pple-43d.com"), "{tip}");
        match plan_uri("http://good.com@%D0%B0pple.com/a") {
            Action::Confirm { uri, host } => {
                assert_eq!(host, "xn--pple-43d.com");
                assert_eq!(uri, "http://good.com@xn--pple-43d.com/a");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            plan_uri("https://100%.example/"),
            Action::Refuse {
                reason: RefuseReason::Scheme,
                ..
            }
        ));
        assert_eq!(
            plan_uri("https://apple%2ecom/a"),
            Action::Browse("https://apple%2ecom/a".into())
        );
        assert_eq!(
            plan_uri("https://[::1]/"),
            Action::Browse("https://[::1]/".into())
        );
    }
}
