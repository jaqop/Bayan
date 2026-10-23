//! PTY session + terminal state: ConPTY via portable-pty, VT emulation via
//! alacritty_terminal. Mirrors EasyTer's proven architecture: a reader thread
//! feeds the emulator behind a mutex and nudges the UI thread to repaint.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Column;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use base64::Engine;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use winit::event_loop::EventLoopProxy;

use crate::UserEvent;

pub const DEFAULT_SHELL: &str = "powershell.exe";
const MAX_CARRY: usize = 4096;

/// Shells the settings panel offers. Delegates to `shells::detect`, which
/// finds what is actually installed — PowerShell with and without $PROFILE,
/// pwsh when present, cmd, Git Bash, and every WSL distribution. Ported from
/// EasyTer, whose generator this mirrors.
pub fn shell_choices() -> Vec<String> {
    crate::shells::detect().into_iter().map(|s| s.command).collect()
}

/// The directory Bayan itself was started in — a shortcut's "Start in" field,
/// which is the standard way to say where a terminal should open.
///
/// It used to be ignored outright, with the reasoning that Windows often hands
/// a process `C:\Windows\System32`, which nobody wants a shell to open in.
/// True, but throwing away every launcher directory to dodge that one case
/// silently broke the shortcut field. So: honour it, unless it sits under the
/// Windows directory.
fn launcher_dir() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    if !cwd.is_dir() {
        return None;
    }
    let s = cwd.to_str()?.to_string();
    let win = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    if is_under(&s, &win) {
        return None;
    }
    Some(s)
}

/// Case-insensitive "is `path` inside `root`" — Windows paths are not
/// case-sensitive, so a plain starts_with would miss `c:\windows\system32`.
fn is_under(path: &str, root: &str) -> bool {
    let (p, r) = (path.to_lowercase().replace('/', "\\"), root.to_lowercase().replace('/', "\\"));
    let r = r.trim_end_matches('\\');
    p == r || p.starts_with(&format!("{r}\\"))
}

/// The PowerShell family gets the full treatment (UTF-8, prompt theme,
/// OSC 133 marks); other shells launch bare and the mark-driven features
/// (command lights, prompt jumps, Claude auto-detect) degrade gracefully.
fn is_powershell(shell: &str) -> bool {
    // `shell` is a COMMAND LINE now, not a program name: detection emits
    // "powershell.exe -NoProfile", "wsl.exe -d Ubuntu", "…bash.exe --login -i".
    let (prog, _) = split_command(shell);
    let base = prog.rsplit(['\\', '/']).next().unwrap_or(&prog).to_string();
    base.eq_ignore_ascii_case("powershell.exe") || base.eq_ignore_ascii_case("pwsh.exe")
}

/// Split a stored command line into program + arguments.
///
/// Splitting on the first space is WRONG: a hand-edited config may hold
/// `C:\Program Files\PowerShell\7\pwsh.exe`, whose program name contains two
/// spaces. So find the program by its `.exe` boundary (or an explicit quote),
/// and only then treat the remainder as arguments.
pub fn split_command(cmd: &str) -> (String, Vec<String>) {
    let c = cmd.trim();
    let split_args = |s: &str| s.split_whitespace().map(str::to_string).collect();
    // "quoted program" args…
    if let Some(rest) = c.strip_prefix('"') {
        if let Some(i) = rest.find('"') {
            return (rest[..i].to_string(), split_args(&rest[i + 1..]));
        }
    }
    // …otherwise the program ends at the first ".exe"
    if let Some(i) = c.to_lowercase().find(".exe") {
        let end = i + 4;
        return (c[..end].to_string(), split_args(&c[end..]));
    }
    let mut it = c.split_whitespace();
    (it.next().unwrap_or(c).to_string(), it.map(str::to_string).collect())
}

/// OSC 133 prompt wrapper injected into PowerShell (EasyTer's __et_wrap,
/// ported): D=command end (exit code), A=prompt start, 9;9=cwd report,
/// B=input begins. Command detection — and Claude mode's auto-enable —
/// depend on these marks.
const PS_MARKS: &str = "function global:__by_wrap{if(-not $global:__BY_SI){$global:__BY_SI=$true;\
$global:__BY_OP=$function:prompt;function global:prompt{\
$c=$global:LASTEXITCODE;if($null -eq $c){if($?){$c=0}else{$c=1}};\
\"$([char]27)]133;D;$c$([char]7)$([char]27)]133;A$([char]7)\
$([char]27)]9;9;$($PWD.ProviderPath)$([char]7)\"\
+(& $global:__BY_OP)+\"$([char]27)]133;B$([char]7)\"}}}; __by_wrap";

/// One command block: where its prompt sits and how the command ended.
/// `abs` is GLOBAL: evicted lines + history size + cursor row at mark time —
/// stable across scrolling AND past the scrollback cap (EasyTer's dropped
/// counter, reborn; see the eviction accounting in feed_counted).
pub struct CmdMark {
    pub abs: u64,
    pub exit: Option<i32>,
}

/// Shared shell-integration state, updated by the reader thread's scanners.
/// Lock order everywhere: term FIRST, then meta.
#[derive(Default)]
pub struct SessionMeta {
    /// idle at an interactive prompt (OSC 133 B seen, no command running)
    pub at_prompt: bool,
    /// column where prompt input begins (from OSC 133 B)
    pub prompt_col: Option<usize>,
    /// the command line currently executing ("" = idle) — feeds cmd_is_claude
    pub running_cmd: String,
    /// working directory from OSC 9;9
    pub cwd: Option<String>,
    /// command blocks (OSC 133 A/D): gutter lights + prompt jumping
    pub marks: Vec<CmdMark>,
    /// history lines evicted past the scrollback cap (+ ED3-cleared lines):
    /// keeps mark positions anchored to content forever
    pub evicted: u64,
    /// when the running command was submitted — feeds the finish notification
    pub cmd_started: Option<std::time::Instant>,
}

/// Events one PTY chunk produced, delivered to the UI thread by the reader.
#[derive(Default)]
pub(crate) struct ChunkEvents {
    pub clipboard: Option<String>,
    pub cwd: Option<String>,
    /// a LONG command just ended (ok?) — worth a notification if unfocused
    pub finished: Option<bool>,
}

/// Commands shorter than this end silently (EasyTer's threshold).
const NOTIFY_AFTER: std::time::Duration = std::time::Duration::from_secs(6);

/// Where an incomplete trailing escape sequence starts (carried to the next
/// read so scanners never see a split marker), or data.len() if none.
/// EasyTer's INCOMPLETE_TAIL_RE + MAX_CARRY semantics, byte-scan edition.
pub(crate) fn incomplete_tail_start(data: &[u8]) -> usize {
    let len = data.len();
    let start = len.saturating_sub(MAX_CARRY);
    let Some(rel) = data[start..].iter().rposition(|&b| b == 0x1b) else {
        return len;
    };
    let i = start + rel;
    let rest = &data[i + 1..];
    match rest.first() {
        // bare ESC at the end: may be half of an OSC's ESC-\ terminator —
        // if an unterminated OSC opener precedes it, carry from the opener
        None => osc_opener_before(data, i).unwrap_or(i),
        Some(b'[') => {
            if rest[1..]
                .iter()
                .all(|b| matches!(b, b'0'..=b'9' | b';' | b'?' | b'<' | b'>' | b'='))
            {
                i
            } else {
                len
            }
        }
        Some(b']') => {
            if rest.contains(&0x07) {
                len // BEL present: the OSC is complete
            } else {
                i
            }
        }
        _ => len,
    }
}

fn osc_opener_before(data: &[u8], esc: usize) -> Option<usize> {
    let start = esc.saturating_sub(MAX_CARRY);
    let rel = data[start..esc].iter().rposition(|&b| b == 0x1b)?;
    let j = start + rel;
    if data.get(j + 1) == Some(&b']') && !data[j..esc].contains(&0x07) {
        Some(j)
    } else {
        None
    }
}

/// OSC 133 markers in a chunk: (start, end, kind, exit code for D marks).
/// The carry logic above guarantees whole sequences.
fn find_osc133(data: &[u8]) -> Vec<(usize, usize, u8, Option<i32>)> {
    const P: &[u8] = b"\x1b]133;";
    let mut out = Vec::new();
    let mut i = 0;
    while i + P.len() < data.len() {
        if &data[i..i + P.len()] == P {
            let kind = data[i + P.len()];
            let mut j = i + P.len();
            let mut end = None;
            while j < data.len() {
                if data[j] == 0x07 {
                    end = Some(j + 1);
                    break;
                }
                if data[j] == 0x1b && data.get(j + 1) == Some(&b'\\') {
                    end = Some(j + 2);
                    break;
                }
                j += 1;
            }
            match end {
                Some(e) => {
                    // "D;<code>" carries the exit status
                    let exit = data[i + P.len() + 1..j]
                        .strip_prefix(b";")
                        .and_then(|p| std::str::from_utf8(p).ok())
                        .and_then(|p| p.split(';').next())
                        .and_then(|p| p.parse().ok());
                    out.push((i, e, kind, exit));
                    i = e;
                    continue;
                }
                None => break,
            }
        }
        i += 1;
    }
    out
}

/// Decoded OSC 52 payload cap. One PTY read is 64 KiB; anything larger is
/// not a yank, it is a clipboard bomb. Checked before the base64 decode.
pub(crate) const OSC52_MAX_BYTES: usize = 64 * 1024;

/// Payload of the LAST *terminated* occurrence of an OSC prefix.
///
/// BEL (`0x07`) or ST (`ESC \`) is required. An ESC that merely starts the
/// next sequence does not count: a program must not finish an OSC by running
/// out of the carry window or by bumping into the following CSI. An earlier
/// terminated sequence still wins when a later one is left open.
fn last_osc_payload<'a>(data: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let mut found = None;
    let mut i = 0;
    while i + prefix.len() <= data.len() {
        if &data[i..i + prefix.len()] == prefix {
            let s = i + prefix.len();
            let mut j = s;
            while j < data.len() && data[j] != 0x07 && data[j] != 0x1b {
                j += 1;
            }
            if data.get(j) == Some(&0x07) {
                found = Some(&data[s..j]);
                i = j + 1;
                continue;
            }
            if data.get(j) == Some(&0x1b) && data.get(j + 1) == Some(&b'\\') {
                found = Some(&data[s..j]);
                i = j + 2;
                continue;
            }
            // Unterminated. Step past this opener and keep the previous hit.
            i = s;
            continue;
        }
        i += 1;
    }
    found
}

/// OSC 52: a program asked to set the clipboard.
///
/// Write-only. A query (`?`) returns `None` and nothing in this process
/// ever replies with clipboard bytes. The caller still has to pass
/// [`allow_osc52_write`]: this function only parses.
fn scan_osc52(data: &[u8]) -> Option<String> {
    let payload = last_osc_payload(data, b"\x1b]52;")?;
    // skip the selection field ("c;", "p;", ...)
    let b64 = &payload[payload.iter().position(|&b| b == b';')? + 1..];
    let encoded = std::str::from_utf8(b64).ok()?.trim();
    // '?' is the clipboard query. Do not decode it, and do not answer it.
    if encoded.is_empty() || encoded.starts_with('?') {
        return None;
    }
    // 4 output chars per 3 input bytes, plus padding. Reject before decode.
    let max_encoded = OSC52_MAX_BYTES.div_ceil(3) * 4;
    if encoded.len() > max_encoded {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    if bytes.is_empty() || bytes.len() > OSC52_MAX_BYTES {
        return None;
    }
    let txt = String::from_utf8_lossy(&bytes).into_owned();
    (!txt.is_empty()).then_some(txt)
}

/// Whether an OSC 52 payload may replace the OS clipboard.
///
/// Both must hold: the user set `osc52` to `"copy"`, and the window is
/// focused. Unfocused writes are how a background session pastejacks the
/// desktop. There is no flag here that allows a read.
pub fn allow_osc52_write(copy_enabled: bool, focused: bool) -> bool {
    copy_enabled && focused
}

/// Longest cwd we will store or start a process in. Real Windows paths fit;
/// a multi-kilobyte string in `session.json` is not a directory.
const MAX_CWD_CHARS: usize = 4096;

/// A working directory that is safe to probe and to persist.
///
/// Returns a normalized drive-letter path (`C:\…`), or `None`. UNC
/// (`\\server\share`, `//server/share`), device and extended paths
/// (`\\?\`, `\\.\`, including `\\?\UNC\…`), and the NT prefix `\??\` are
/// rejected here, **before** any `exists` / `is_dir` / `canonicalize` call —
/// on Windows those open SMB and can leak NTLM. OSC 7 (`file://…`) should
/// parse the URL and then call this; do not grow a second checker.
pub fn accept_local_cwd(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s.chars().count() > MAX_CWD_CHARS {
        return None;
    }
    if s.chars().any(|c| c.is_control()) {
        return None;
    }
    if unc_or_device(s) {
        return None;
    }
    let mut chars = s.chars();
    let drive = chars.next()?;
    if !drive.is_ascii_alphabetic() || chars.next() != Some(':') {
        return None;
    }
    match chars.next() {
        Some('\\' | '/') => {}
        _ => return None,
    }
    let mut norm: String = s.chars().map(|c| if c == '/' { '\\' } else { c }).collect();
    // Keep the drive root (`C:\`). A trailing slash on anything longer is
    // noise, and on cmd it can also escape a closing quote.
    if norm.len() > 3 {
        while norm.ends_with('\\') {
            norm.pop();
        }
    }
    if unc_or_device(&norm) {
        return None;
    }
    Some(norm)
}

/// `\\…` (UNC, `\\?\`, `\\.\`) or `\??\…`. Slashes are normalized first so
/// `//server/share` and `//?/UNC/…` take the same path as their backslash form.
fn unc_or_device(s: &str) -> bool {
    let norm: String = s.chars().map(|c| if c == '/' { '\\' } else { c }).collect();
    let lower = norm.to_ascii_lowercase();
    lower.starts_with("\\\\") || lower.starts_with("\\??\\")
}

/// OSC 9;9: working-directory report (quoted or bare Windows path).
/// Unusable paths are dropped, not stored — see [`accept_local_cwd`].
fn scan_cwd(data: &[u8]) -> Option<String> {
    let payload = last_osc_payload(data, b"\x1b]9;9;")?;
    let s = String::from_utf8_lossy(payload);
    let s = s.trim().trim_matches('"').trim();
    accept_local_cwd(s)
}

fn handle_osc133<T: EventListener>(
    t: &Term<T>,
    meta: &mut SessionMeta,
    kind: u8,
    exit: Option<i32>,
    ev: &mut ChunkEvents,
) {
    match kind {
        b'A' => {
            meta.running_cmd.clear();
            // a new prompt starts on the cursor's row: record its block
            // GLOBALLY (evicted + history + row — never drifts)
            let abs = meta.evicted
                + t.grid().history_size() as u64
                + t.grid().cursor.point.line.0.max(0) as u64;
            meta.marks.push(CmdMark { abs, exit: None });
            if meta.marks.len() > 2000 {
                meta.marks.drain(..1000); // EasyTer's trim
            }
        }
        b'D' => {
            meta.running_cmd.clear();
            if let Some(m) = meta.marks.last_mut() {
                m.exit = exit;
            }
            // a long-running command finished: notify if nobody's watching
            if let Some(t0) = meta.cmd_started.take() {
                if t0.elapsed() >= NOTIFY_AFTER {
                    ev.finished = Some(exit.unwrap_or(0) == 0);
                }
            }
        }
        b'B' => {
            meta.at_prompt = true;
            meta.prompt_col = Some(t.grid().cursor.point.column.0);
        }
        _ => {}
    }
}

/// Feed one segment and count history evictions EXACTLY. The trick: Grid's
/// scroll_up() advances display_offset by the scrolled amount whenever the
/// offset is non-zero (that's how "stay scrolled up" works) — so park the
/// offset at 1 for the duration of the feed, read how far it moved, and
/// subtract the history growth. What's left is lines evicted past the cap.
/// (Saturates only if one segment scrolls more than the whole scrollback.)
fn feed_counted<T: EventListener>(
    parser: &mut Processor,
    t: &mut Term<T>,
    meta: &mut SessionMeta,
    bytes: &[u8],
) {
    if bytes.is_empty() {
        return;
    }
    let alt = t.mode().contains(TermMode::ALT_SCREEN);
    let hist_before = t.grid().history_size();
    let off_before = t.grid().display_offset();
    let parked = !alt && off_before == 0 && hist_before > 0;
    if parked {
        t.scroll_display(Scroll::Delta(1));
    }
    // vte 0.15 takes the whole slice (it batches internally) instead of one
    // byte per call. The counter brackets the entire feed, so this is a
    // straight speedup, not a change in what gets measured.
    parser.advance(t, bytes);
    let off_after = t.grid().display_offset();
    let hist_after = t.grid().history_size();
    let base = if parked { 1 } else { off_before };
    let scrolled = off_after.saturating_sub(base) as i64;
    if parked {
        t.scroll_display(Scroll::Bottom);
    }
    // history that shrank (ED3/RIS) counts as evicted too, EasyTer-style
    let growth = hist_after as i64 - hist_before as i64;
    let evict = scrolled - growth;
    if evict > 0 {
        meta.evicted += evict as u64;
    }
}

/// Carry-merge, scan, split-feed one PTY read. The markers themselves are
/// consumed here (alacritty would discard them anyway); segments between
/// them feed the emulator, with the cursor read exactly at each mark.
pub(crate) fn process_chunk<T: EventListener>(
    parser: &mut Processor,
    t: &mut Term<T>,
    meta: &mut SessionMeta,
    carry: &mut Vec<u8>,
    data: &[u8],
) -> ChunkEvents {
    let mut buf = std::mem::take(carry);
    buf.extend_from_slice(data);
    let cut = incomplete_tail_start(&buf);
    *carry = buf.split_off(cut);
    let mut ev = ChunkEvents::default();
    if buf.is_empty() {
        return ev;
    }
    ev.clipboard = scan_osc52(&buf);
    if let Some(c) = scan_cwd(&buf) {
        meta.cwd = Some(c.clone());
        ev.cwd = Some(c);
    }
    let mut pos = 0;
    for (s, e, kind, exit) in find_osc133(&buf) {
        feed_counted(parser, t, meta, &buf[pos..s]);
        handle_osc133(t, meta, kind, exit, &mut ev);
        pos = e;
    }
    feed_counted(parser, t, meta, &buf[pos..]);
    ev
}

/// On Enter at a prompt, read the typed command straight off the screen
/// (prompt echo included, so history-recalled/edited lines are captured
/// correctly too — EasyTer's technique). Feeds cmd_is_claude.
pub(crate) fn capture_command_core<T: EventListener>(t: &Term<T>, meta: &mut SessionMeta) {
    if t.mode().contains(TermMode::ALT_SCREEN) || !meta.at_prompt {
        return;
    }
    let Some(pcol) = meta.prompt_col else {
        return;
    };
    meta.at_prompt = false;
    let cur = t.grid().cursor.point;
    if cur.column.0 <= pcol {
        return;
    }
    let row = &t.grid()[cur.line];
    let mut s = String::new();
    for c in pcol..cur.column.0 {
        s.push(row[Column(c)].c);
    }
    let s = s.trim();
    if !s.is_empty() {
        meta.running_cmd = s.to_string();
        meta.cmd_started = Some(std::time::Instant::now());
    }
}

/// Pull the oh-my-posh theme path out of a PowerShell profile line like
///   oh-my-posh init pwsh --config "C:\...\night-owl.omp.json" | Invoke-Expression
pub fn parse_posh_config(profile_text: &str) -> Option<String> {
    for line in profile_text.lines() {
        let l = line.trim();
        if l.starts_with('#') || !l.contains("oh-my-posh") || !l.contains("--config") {
            continue;
        }
        let rest = l[l.find("--config")? + "--config".len()..].trim_start();
        let path = if let Some(q) = rest.strip_prefix('"') {
            q.split('"').next()
        } else if let Some(q) = rest.strip_prefix('\'') {
            q.split('\'').next()
        } else {
            rest.split_whitespace().next()
        };
        if let Some(p) = path {
            if !p.is_empty() {
                return Some(p.to_string());
            }
        }
    }
    None
}

/// The user's oh-my-posh theme: POSH_THEME env first, else auto-detected
/// from the PowerShell profile. Lets Bayan skip the profile (-NoProfile is
/// the single biggest avoidable startup cost — the EasyTer lesson) while
/// keeping the prompt the user actually uses.
/// Expand the environment references a hand-written $PROFILE may hold.
/// Bayan checks the config path on disk before injecting it, and a
/// portable profile stores "$env:USERPROFILE\\.poshthemes\\..." rather than
/// an absolute path — checking that literally always fails, which silently
/// drops the prompt. (Self-inflicted: making the profile portable is what
/// exposed this.)
fn expand_env(path: &str) -> String {
    let mut out = path.to_string();
    for (needle, var) in [
        ("$env:USERPROFILE", "USERPROFILE"),
        ("$env:HOME", "USERPROFILE"),
        ("$HOME", "USERPROFILE"),
        ("%USERPROFILE%", "USERPROFILE"),
        ("$env:LOCALAPPDATA", "LOCALAPPDATA"),
    ] {
        if let Some(v) = std::env::var_os(var).and_then(|v| v.into_string().ok()) {
            out = out.replace(needle, &v);
        }
    }
    // a leading ~ is the other common shorthand
    if let Some(rest) = out.strip_prefix("~") {
        if let Some(h) = std::env::var_os("USERPROFILE").and_then(|v| v.into_string().ok()) {
            out = format!("{h}{rest}");
        }
    }
    out
}

fn detect_posh_theme() -> Option<String> {
    if let Ok(t) = std::env::var("POSH_THEME") {
        if !t.is_empty() && std::path::Path::new(&t).exists() {
            return Some(t);
        }
    }
    let home = std::env::var("USERPROFILE").ok()?;
    for rel in [
        r"Documents\PowerShell\Microsoft.PowerShell_profile.ps1",
        r"Documents\WindowsPowerShell\Microsoft.PowerShell_profile.ps1",
    ] {
        let p = std::path::Path::new(&home).join(rel);
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Some(cfg) = parse_posh_config(&text) {
                let cfg = expand_env(&cfg);
                if std::path::Path::new(&cfg).exists() {
                    return Some(cfg);
                }
            }
        }
    }
    None
}

/// Grid dimensions handed to alacritty_terminal.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct TermSize {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Routes emulator events to the winit loop, tagged with the owning tab's
/// id. PtyWrite matters most: TUIs (Claude Code among them) send ESC[6n and
/// block waiting for the cursor position reply — the exact lesson EasyTer's
/// ReportingScreen taught us; the reply must reach the RIGHT tab's PTY.
#[derive(Clone)]
pub struct EventProxy(pub EventLoopProxy<UserEvent>, pub u64);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let mapped = match event {
            Event::PtyWrite(text) => UserEvent::PtyWrite(self.1, text),
            Event::Wakeup => UserEvent::Wakeup(self.1),
            // the bell: Claude Code rings it when waiting for an approval —
            // the cockpit's attention signal
            Event::Bell => UserEvent::Bell(self.1),
            Event::Exit => UserEvent::Exit(self.1),
            _ => return,
        };
        let _ = self.0.send_event(mapped);
    }
}

pub struct Session {
    pub term: Arc<Mutex<Term<EventProxy>>>,
    pub meta: Arc<Mutex<SessionMeta>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// Command line this pane was spawned with. Drag-drop quoting follows it,
    /// not whatever the settings say now — the live shell is the one reading.
    shell: String,
}

impl Session {
    pub fn spawn(
        size: TermSize,
        proxy: EventLoopProxy<UserEvent>,
        id: u64,
        start_cwd: Option<&str>,
        scrollback: usize,
        shell: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize {
            rows: size.rows as u16,
            cols: size.cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        // program + args, or a shell with switches would be spawned as one
        // absurd filename ("powershell.exe -NoProfile" is not a program)
        let (prog, args) = split_command(shell);
        let mut cmd = CommandBuilder::new(prog);
        for a in &args {
            cmd.arg(a);
        }
        // requested dir (a restored tab, or "new tab inherits the cwd"),
        // else the launcher's directory, else home.
        // accept_local_cwd runs BEFORE is_dir: a UNC path must not be probed.
        let dir = start_cwd
            .and_then(accept_local_cwd)
            .filter(|d| std::path::Path::new(d).is_dir())
            .or_else(launcher_dir)
            .or_else(|| std::env::var("USERPROFILE").ok());
        if let Some(d) = dir {
            cmd.cwd(d);
        }
        if is_powershell(shell) {
            // -NoProfile: the profile is seconds of avoidable startup (it
            // re-runs oh-my-posh, chcp, modules). Self-provide what it gave
            // us: UTF-8 (Arabic!) and the user's prompt theme, auto-detected.
            cmd.args(["-NoProfile", "-NoExit", "-Command"]);
            let mut setup = String::from(
                "$OutputEncoding=[Console]::InputEncoding=[Console]::OutputEncoding=\
                 [Text.UTF8Encoding]::new();",
            );
            if let Some(theme) = detect_posh_theme() {
                setup.push_str(&format!(
                    " $ErrorActionPreference='SilentlyContinue'; \
                     & {{oh-my-posh init powershell --config '{}' | Invoke-Expression}} 2>$null; \
                     $ErrorActionPreference='Continue';",
                    theme.replace('\'', "''")
                ));
            }
            setup.push(' ');
            setup.push_str(PS_MARKS);
            cmd.arg(setup);
        } else if shell.eq_ignore_ascii_case("cmd.exe") {
            // UTF-8 code page so Arabic paths survive; no OSC 133 marks
            cmd.args(["/K", "chcp 65001>nul"]);
        }
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        let term = Arc::new(Mutex::new(Term::new(
            config,
            &size,
            EventProxy(proxy.clone(), id),
        )));

        let meta = Arc::new(Mutex::new(SessionMeta::default()));
        let term2 = Arc::clone(&term);
        let meta2 = Arc::clone(&meta);
        std::thread::spawn(move || {
            let mut parser: Processor = Processor::new();
            let mut carry: Vec<u8> = Vec::new();
            let mut buf = [0u8; 65536];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let ev = {
                            let mut t = term2.lock().unwrap();
                            let mut m = meta2.lock().unwrap();
                            process_chunk(&mut parser, &mut t, &mut m, &mut carry, &buf[..n])
                        };
                        if let Some(txt) = ev.clipboard {
                            let _ = proxy.send_event(UserEvent::ClipboardSet(txt));
                        }
                        if let Some(cwd) = ev.cwd {
                            let _ = proxy.send_event(UserEvent::Cwd(id, cwd));
                        }
                        if let Some(ok) = ev.finished {
                            let _ = proxy.send_event(UserEvent::CommandFinished(id, ok));
                        }
                        if proxy.send_event(UserEvent::Wakeup(id)).is_err() {
                            break; // the event loop is gone
                        }
                    }
                }
            }
            let _ = proxy.send_event(UserEvent::Exit(id));
        });

        Ok(Self {
            term,
            meta,
            writer,
            master: pair.master,
            child,
            shell: shell.to_string(),
        })
    }

    /// The command line passed to [`Session::spawn`], for shell-specific quoting.
    pub fn shell(&self) -> &str {
        &self.shell
    }

    /// Terminate the shell (closing a tab must not leave orphans).
    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    pub fn write(&mut self, bytes: &[u8]) {
        // Enter at a prompt starts a command: read it off the screen now,
        // before the shell reacts (Claude mode auto-enable needs the name)
        if bytes.contains(&b'\r') {
            let t = self.term.lock().unwrap();
            let mut m = self.meta.lock().unwrap();
            capture_command_core(&t, &mut m);
        }
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub fn resize(&mut self, size: TermSize) {
        let _ = self.master.resize(PtySize {
            rows: size.rows as u16,
            cols: size.cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.term.lock().unwrap().resize(size);
    }
}

/// Multi-line / huge pastes can execute commands on arrival (EasyTer's paste
/// protection). Some((lines, chars)) when confirmation is due.
///
/// CR is Enter on ConPTY, and the default PowerShell is started `-NoProfile`
/// so PSReadLine never turns bracketed paste on. LF-only detection let a
/// clipboard of `command\r` run on paste. Line breaks are CR, LF, CRLF, NEL,
/// U+2028 and U+2029. Any other control except a tab takes the same confirm
/// path (ESC would otherwise smuggle a sequence). The size cap counts
/// characters: a line of Arabic is not "huge" because UTF-8 uses two bytes.
///
/// Bracketed paste is intentionally not forced from our side. The guard runs
/// first either way; injecting `\x1b[?2004h` into a host that does not
/// understand the wrappers would type the wrappers into the prompt.
pub fn needs_paste_guard(text: &str) -> Option<(usize, usize)> {
    let mut breaks = 0usize;
    let mut ends_with_break = false;
    let mut dangerous = false;
    let mut chars = 0usize;
    let mut iter = text.chars().peekable();
    while let Some(c) = iter.next() {
        chars += 1;
        if c == '\r' {
            breaks += 1;
            ends_with_break = true;
            dangerous = true;
            if iter.peek() == Some(&'\n') {
                iter.next();
                chars += 1;
            }
            continue;
        }
        if matches!(c, '\n' | '\u{0085}' | '\u{2028}' | '\u{2029}') {
            breaks += 1;
            ends_with_break = true;
            dangerous = true;
            continue;
        }
        ends_with_break = false;
        if c != '\t' && c.is_control() {
            dangerous = true;
        }
    }
    if dangerous || chars > 2000 {
        let lines = breaks + usize::from(!ends_with_break);
        Some((lines, chars))
    } else {
        None
    }
}

/// Which quoting rules a spawned command line needs. Unknown programs follow
/// PowerShell, because that is the shell Bayan starts when nothing else matches.
enum ShellFamily {
    PowerShell,
    Cmd,
    Posix,
}

fn shell_family(shell: &str) -> ShellFamily {
    let (prog, _) = split_command(shell);
    let base = prog.rsplit(['\\', '/']).next().unwrap_or(&prog);
    let name = base.to_ascii_lowercase();
    let name = match name.strip_suffix(".exe") {
        Some(stripped) => stripped.to_string(),
        None => name,
    };
    match name.as_str() {
        "powershell" | "pwsh" => ShellFamily::PowerShell,
        "cmd" => ShellFamily::Cmd,
        "bash" | "sh" | "zsh" | "fish" | "wsl" | "dash" => ShellFamily::Posix,
        _ => ShellFamily::PowerShell,
    }
}

/// A file dropped onto a pane, quoted as a literal for `shell`, plus a trailing
/// space. `None` when the path contains a control character (or, for cmd, a
/// character that expands inside quotes) — those are not typed at all.
///
/// PowerShell single quotes are literal except for `'` doubled. cmd double
/// quotes stop `&` from being a separator, but `%` and `!` still expand, so a
/// name containing either is refused. Git Bash and WSL take POSIX single
/// quotes (`'` → `'\''`).
pub fn dropped_path_arg(path: &std::path::Path, shell: &str) -> Option<String> {
    let raw = path.to_string_lossy();
    if raw.chars().any(|c| c.is_control()) {
        return None;
    }
    let mut p = raw.replace('/', "\\");
    if p.is_empty() {
        return None;
    }
    if p.len() > 3 && p.ends_with('\\') {
        p.pop();
    }
    let quoted = match shell_family(shell) {
        ShellFamily::PowerShell => format!("'{}'", p.replace('\'', "''")),
        ShellFamily::Posix => format!("'{}'", p.replace('\'', r"'\''")),
        ShellFamily::Cmd => {
            // `%VAR%` and delayed-expansion `!VAR!` survive double quotes.
            // `"` cannot occur in a Windows filename; if it did, it would
            // close the quotes. Refuse rather than hand the shell a breakout.
            if p.chars().any(|c| matches!(c, '%' | '!' | '"')) {
                return None;
            }
            // A trailing backslash escapes cmd's closing quote. Longer paths
            // already lost that slash. A drive root (`C:\`) is the one we
            // keep, and `C:\.` is the same directory without the escape.
            if p.ends_with('\\') {
                format!("\"{p}.\"")
            } else {
                format!("\"{p}\"")
            }
        }
    };
    Some(format!("{quoted} "))
}

/// Prepare clipboard text for the PTY: newlines become carriage returns, and
/// under bracketed paste the payload is wrapped — with any embedded end
/// marker stripped so pasted content can't terminate paste mode early and
/// smuggle the remainder in as executed input (EasyTer's paste-injection fix).
pub fn normalize_paste(text: &str, bracketed: bool) -> String {
    let body = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        format!("\x1b[200~{}\x1b[201~", body.replace("\x1b[201~", ""))
    } else {
        body
    }
}

/// Escape a literal so RegexSearch treats it verbatim (search is literal
/// text, not regex, from the user's point of view).
pub fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if r"\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_normalizes_newlines_and_blocks_injection() {
        assert_eq!(normalize_paste("a\r\nb\nc", false), "a\rb\rc");
        // bracketed: wrapped, and an embedded end marker cannot break out
        assert_eq!(
            normalize_paste("safe\x1b[201~evil\n", true),
            "\x1b[200~safeevil\r\x1b[201~"
        );
    }

    #[test]
    fn paste_guard_triggers_on_danger_only() {
        assert_eq!(needs_paste_guard("ls -la"), None);
        assert_eq!(needs_paste_guard("line1\nline2"), Some((2, 11)));
        // a single trailing newline still counts as multi-line intent
        assert_eq!(needs_paste_guard("rm -rf x\n").map(|g| g.0), Some(1));
        let huge = "x".repeat(3000);
        assert_eq!(needs_paste_guard(&huge).map(|g| g.0), Some(1));
    }

    /// BAYAN-002: CR is Enter. A short CR-separated paste must ask, and so
    /// must other controls. The size cap is in characters, not UTF-8 bytes.
    #[test]
    fn paste_guard_treats_cr_and_controls_as_dangerous() {
        assert_eq!(needs_paste_guard("echo hi\recho pwned\r"), Some((2, 19)));
        assert_eq!(needs_paste_guard("whoami\r"), Some((1, 7)));
        // under the old 2000-byte cap, and still a submit
        assert!(needs_paste_guard(&"whoami\r".repeat(200)).is_some());
        // CRLF is one break, not two
        assert_eq!(needs_paste_guard("a\r\nb"), Some((2, 4)));
        assert_eq!(needs_paste_guard("a\u{2028}b"), Some((2, 3)));
        assert_eq!(needs_paste_guard("a\u{2029}b"), Some((2, 3)));
        assert_eq!(needs_paste_guard("a\u{0085}b"), Some((2, 3)));
        assert!(needs_paste_guard("echo \u{1b}[2J").is_some());
        assert!(needs_paste_guard("del\u{7f}").is_some());
        // a tab is indentation, not a command
        assert_eq!(needs_paste_guard("col1\tcol2"), None);
        let arabic = "م".repeat(1500);
        assert_eq!(arabic.len(), 3000, "two bytes per letter");
        assert_eq!(needs_paste_guard(&arabic), None);
        assert_eq!(needs_paste_guard(&"x".repeat(2000)), None);
        assert!(needs_paste_guard(&"x".repeat(2001)).is_some());
    }

    #[test]
    fn dropped_paths_are_shell_ready() {
        use std::path::Path;
        // PowerShell literals, including a path with no spaces: an unquoted
        // `$` is not "safe enough".
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\tools\app.exe"), "powershell.exe").as_deref(),
            Some(r"'C:\tools\app.exe' ")
        );
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\My Files\doc.txt"), "powershell.exe").as_deref(),
            Some(r"'C:\My Files\doc.txt' ")
        );
    }

    /// BAYAN-004: `$(…)`, `&`, and backticks must be literals, per shell.
    #[test]
    fn dropped_paths_are_quoted_for_the_running_shell() {
        use std::path::Path;
        // PowerShell expands `$(…)` and backticks inside double quotes, and
        // an unquoted path is worse. Single quotes, apostrophe doubled.
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\tmp\$(calc).txt"), "powershell.exe").as_deref(),
            Some(r"'C:\tmp\$(calc).txt' ")
        );
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\My Files\$(calc).txt"), "pwsh.exe -NoLogo").as_deref(),
            Some(r"'C:\My Files\$(calc).txt' ")
        );
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\tmp\a`ncalc.txt"), "powershell.exe -NoProfile").as_deref(),
            Some(r"'C:\tmp\a`ncalc.txt' ")
        );
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\o'$(calc).txt"), "powershell.exe").as_deref(),
            Some(r"'C:\o''$(calc).txt' ")
        );
        // cmd: `&` is a separator until the path is quoted. `%` and `!` still
        // expand inside those quotes, so the drop is refused.
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\tmp\a&calc.exe"), "cmd.exe").as_deref(),
            Some("\"C:\\tmp\\a&calc.exe\" ")
        );
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\tools\app.exe"), "cmd.exe").as_deref(),
            Some(r#""C:\tools\app.exe" "#)
        );
        // `C:\` quoted naively is `"C:\"` — the backslash eats the quote.
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\"), "cmd.exe").as_deref(),
            Some(r#""C:\." "#)
        );
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\tmp\a&calc.exe"), r"C:\Windows\System32\cmd.exe")
                .as_deref(),
            Some(r#""C:\tmp\a&calc.exe" "#)
        );
        assert_eq!(dropped_path_arg(Path::new(r"C:\tmp\%COMSPEC%.txt"), "cmd.exe"), None);
        assert_eq!(dropped_path_arg(Path::new(r"C:\tmp\!PATH!.txt"), "cmd.exe"), None);
        // Git Bash and WSL are POSIX shells. `'\''` is the embedded apostrophe.
        let bash = r"C:\Program Files\Git\usr\bin\bash.exe --login -i";
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\tmp\$(calc).txt"), bash).as_deref(),
            Some(r"'C:\tmp\$(calc).txt' ")
        );
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\o'brien.txt"), "wsl.exe -d Ubuntu").as_deref(),
            Some("'C:\\o'\\''brien.txt' ")
        );
        // an unknown program follows PowerShell quoting, not "no quotes"
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\tmp\$(calc).txt"), "nu.exe").as_deref(),
            Some(r"'C:\tmp\$(calc).txt' ")
        );
        // CR/LF in a name would be Enter. Refuse for every shell.
        assert_eq!(dropped_path_arg(Path::new("C:\\tmp\\a\nb.txt"), "powershell.exe"), None);
        assert_eq!(dropped_path_arg(Path::new("C:\\tmp\\a\rb.txt"), "cmd.exe"), None);
        assert_eq!(dropped_path_arg(Path::new("C:\\tmp\\a\rb.txt"), "wsl.exe -d Ubuntu"), None);
    }

    #[test]
    fn shell_choices_come_from_detection() {
        // The old contract was "the installed trio" and capped the list at 3.
        // Detection replaced it: Git Bash and every WSL distro now appear too,
        // so the cap is gone deliberately, not by accident.
        let c = shell_choices();
        assert_eq!(c[0], DEFAULT_SHELL, "PowerShell 5 leads (always present)");
        assert!(c.contains(&"cmd.exe".to_string()));
        assert_eq!(c, crate::shells::detect().into_iter().map(|s| s.command).collect::<Vec<_>>());
        let mut sorted = c.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), c.len(), "cycling must not repeat a shell: {c:?}");
        // family detection drives the setup injection
        assert!(is_powershell("powershell.exe"));
        assert!(is_powershell(r"C:\Program Files\PowerShell\7\pwsh.exe"));
        assert!(!is_powershell("cmd.exe"));
        assert!(!is_powershell("nu.exe"));
        // the no-profile variant is still the PowerShell family: it must keep
        // UTF-8 setup and OSC 133 marks, or command lights die on that entry
        assert!(is_powershell("powershell.exe -NoProfile"));
    }

    #[test]
    fn a_portable_profile_path_still_resolves() {
        // the regression this guards: making $PROFILE portable replaced an
        // absolute path with "$env:USERPROFILE\\..." — and the existence
        // check on that literal string always fails, silently dropping the
        // prompt theme with no error anywhere
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        assert!(!home.is_empty(), "test needs USERPROFILE");
        for form in [
            "$env:USERPROFILE\\.poshthemes\\kali.omp.json",
            "%USERPROFILE%\\.poshthemes\\kali.omp.json",
            "~\\.poshthemes\\kali.omp.json",
        ] {
            let out = expand_env(form);
            assert!(out.starts_with(&home), "{form} did not expand: {out}");
            assert!(!out.contains("$env:") && !out.contains('%'), "leftover: {out}");
        }
        // an already-absolute path passes through untouched
        let abs = format!("{home}\\.poshthemes\\pure.omp.json");
        assert_eq!(expand_env(&abs), abs);
    }

    #[test]
    fn the_launcher_directory_is_honoured_except_under_windows() {
        // a shortcut's "Start in" is the standard way to pick the open folder
        assert!(!is_under(r"C:\Users\Admin\Desktop", r"C:\Windows"));
        // ...but a process handed System32 must fall back, and Windows paths
        // are case-insensitive, so a plain starts_with would miss these
        assert!(is_under(r"C:\Windows\System32", r"C:\Windows"));
        assert!(is_under(r"c:\windows\system32", r"C:\Windows"));
        assert!(is_under(r"C:\WINDOWS\System32\WindowsPowerShell", r"c:\Windows"));
        // the root itself counts as inside it
        assert!(is_under(r"C:\Windows", r"C:\Windows"));
        // a sibling that merely shares a prefix does NOT — the trap a naive
        // starts_with falls into
        assert!(!is_under(r"C:\WindowsApps\thing", r"C:\Windows"));
        // forward slashes normalise to the same answer
        assert!(is_under("C:/Windows/System32", r"C:\Windows"));
    }

    #[test]
    fn command_lines_split_into_program_and_args() {
        // guards: CommandBuilder::new(whole_line) would try to spawn a program
        // literally named "powershell.exe -NoProfile"
        assert_eq!(split_command("cmd.exe"), ("cmd.exe".into(), vec![]));
        assert_eq!(
            split_command("powershell.exe -NoProfile"),
            ("powershell.exe".into(), vec!["-NoProfile".to_string()])
        );
        assert_eq!(
            split_command(r"C:\PROGRA~1\Git\usr\bin\bash.exe --login -i"),
            (r"C:\PROGRA~1\Git\usr\bin\bash.exe".into(),
             vec!["--login".to_string(), "-i".to_string()])
        );
        assert_eq!(
            split_command("wsl.exe -d Ubuntu"),
            ("wsl.exe".into(), vec!["-d".to_string(), "Ubuntu".to_string()])
        );
        // a program path WITH spaces must survive: splitting on the first
        // space would yield "C:\Program" and break is_powershell
        assert_eq!(
            split_command(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            (r"C:\Program Files\PowerShell\7\pwsh.exe".into(), vec![])
        );
        // every detected shell splits to a real program
        for c in shell_choices() {
            let (prog, _) = split_command(&c);
            assert!(prog.to_lowercase().ends_with(".exe"), "bad program in {c:?}");
        }
    }

    #[test]
    fn regex_escape_makes_literals() {
        assert_eq!(regex_escape("a.b*c"), r"a\.b\*c");
        assert_eq!(regex_escape("plain"), "plain");
        assert_eq!(regex_escape("x(1)[2]{3}"), r"x\(1\)\[2\]\{3\}");
    }

    use alacritty_terminal::event::VoidListener;

    fn test_term() -> (Term<VoidListener>, Processor, SessionMeta, Vec<u8>) {
        let size = TermSize { cols: 40, rows: 6 };
        (
            Term::new(Config::default(), &size, VoidListener),
            Processor::new(),
            SessionMeta::default(),
            Vec::new(),
        )
    }

    #[test]
    fn carry_holds_back_split_sequences() {
        // split CSI
        assert_eq!(incomplete_tail_start(b"abc\x1b[1;5"), 3);
        // complete CSI: nothing carried
        let csi = b"abc\x1b[1;5C";
        assert_eq!(incomplete_tail_start(csi), csi.len());
        // unterminated OSC: carried from its opener
        assert_eq!(incomplete_tail_start(b"xy\x1b]133;"), 2);
        // OSC ending in a bare ESC (split ESC-\ terminator): from the opener
        assert_eq!(incomplete_tail_start(b"xy\x1b]9;9;C:\\U\x1b"), 2);
        // BEL-terminated OSC is complete
        let done = b"xy\x1b]133;A\x07z";
        assert_eq!(incomplete_tail_start(done), done.len());
        // plain text carries nothing
        assert_eq!(incomplete_tail_start(b"hello"), 5);
    }

    #[test]
    fn osc133_split_across_reads_still_marks_the_prompt() {
        let (mut t, mut p, mut meta, mut carry) = test_term();
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"PS> \x1b]133;");
        assert!(!meta.at_prompt, "marker not complete yet");
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"B\x07");
        assert!(meta.at_prompt);
        assert_eq!(meta.prompt_col, Some(4)); // input begins after "PS> "
    }

    #[test]
    fn command_capture_reads_the_typed_line() {
        let (mut t, mut p, mut meta, mut carry) = test_term();
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"> \x1b]133;B\x07");
        // the shell echoes what the user typed
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"claude --continue");
        capture_command_core(&t, &mut meta);
        assert_eq!(meta.running_cmd, "claude --continue");
        assert!(!meta.at_prompt);
        // OSC 133 D (command finished) clears it
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]133;D;0\x07");
        assert!(meta.running_cmd.is_empty());
    }

    /// The whole Claude-mode enabling chain, GUI-free: prompt mark, typed
    /// command captured off the screen, alternate screen entered — the two
    /// conditions claude_active() checks are both true.
    #[test]
    fn claude_pipeline_end_to_end() {
        let (mut t, mut p, mut meta, mut carry) = test_term();
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"> \x1b]133;B\x07");
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"claude");
        capture_command_core(&t, &mut meta);
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?1049h");
        assert!(t.mode().contains(TermMode::ALT_SCREEN));
        assert!(crate::bidi::cmd_is_claude(&meta.running_cmd));
        // leaving the TUI: alt screen off, D mark clears the command
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?1049l\x1b]133;D;0\x07");
        assert!(!t.mode().contains(TermMode::ALT_SCREEN));
        assert!(meta.running_cmd.is_empty());
    }

    /// The eviction counter (EasyTer's dropped counter, alacritty edition):
    /// flood far past a tiny scrollback cap and verify marks stay anchored.
    #[test]
    fn marks_survive_scrollback_eviction() {
        let size = TermSize { cols: 20, rows: 4 };
        let config = Config { scrolling_history: 8, ..Config::default() };
        let mut t = Term::new(config, &size, VoidListener);
        let mut p: Processor = Processor::new();
        let mut meta = SessionMeta::default();
        let mut carry = Vec::new();
        // 30 lines through a 4-row screen: 27 scroll into an 8-line history
        for i in 0..30 {
            let line = format!("L{i}\r\n");
            process_chunk(&mut p, &mut t, &mut meta, &mut carry, line.as_bytes());
        }
        assert_eq!(t.grid().history_size(), 8, "history is at its cap");
        assert_eq!(meta.evicted, 19, "27 scrolled - 8 kept = 19 evicted");
        // a new prompt mark lands exactly on the cursor row in grid space
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]133;A\x07");
        let m = meta.marks.last().unwrap();
        let grid_line = (m.abs - meta.evicted) as i64 - t.grid().history_size() as i64;
        assert_eq!(grid_line, t.grid().cursor.point.line.0 as i64);
        // the display offset was left untouched by the parking trick
        assert_eq!(t.grid().display_offset(), 0);
    }

    #[test]
    fn osc52_and_cwd_scan_with_split_payloads() {
        let (mut t, mut p, mut meta, mut carry) = test_term();
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]52;c;aGVsbG8=");
        assert!(ev.clipboard.is_none(), "unterminated: carried");
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x07tail");
        assert_eq!(ev.clipboard.as_deref(), Some("hello"));

        let ev = process_chunk(
            &mut p, &mut t, &mut meta, &mut carry,
            b"\x1b]9;9;\"C:\\Users\\Admin\\Bayan\\\"\x07",
        );
        assert_eq!(ev.cwd.as_deref(), Some(r"C:\Users\Admin\Bayan"));
        assert_eq!(meta.cwd.as_deref(), Some(r"C:\Users\Admin\Bayan"));
    }

    /// BAYAN-001: no BEL/ST means no clipboard write, a query is never a
    /// payload, and the decoded text is capped. Opt-in is a separate gate.
    #[test]
    fn osc52_requires_a_terminator_and_never_reads() {
        assert_eq!(scan_osc52(b"\x1b]52;c;aGVsbG8="), None, "unterminated");
        // the ESC of a following CSI is not a terminator either
        assert_eq!(scan_osc52(b"\x1b]52;c;aGVsbG8=\x1b[0m"), None);
        assert_eq!(scan_osc52(b"\x1b]52;c;aGVsbG8=\x07").as_deref(), Some("hello"));
        assert_eq!(scan_osc52(b"\x1b]52;c;aGVsbG8=\x1b\\").as_deref(), Some("hello"));
        // query forms. None of these may decode, and none is a reply.
        assert_eq!(scan_osc52(b"\x1b]52;c;?\x07"), None);
        assert_eq!(scan_osc52(b"\x1b]52;c;?\x1b\\"), None);
        assert_eq!(scan_osc52(b"\x1b]52;?\x07"), None);
        assert_eq!(scan_osc52(b"\x1b]52;c;\x07"), None, "empty write");
        // a later open sequence must not hide an earlier finished one
        assert_eq!(
            scan_osc52(b"\x1b]52;c;aGVsbG8=\x07\x1b]52;c;IQ==").as_deref(),
            Some("hello")
        );
        assert_eq!(
            scan_osc52(b"\x1b]52;c;aGVsbG8=\x07\x1b]52;c;IQ==\x07").as_deref(),
            Some("!")
        );
        // past the cap, even with a real BEL
        let mut big = b"\x1b]52;c;".to_vec();
        big.extend(base64::engine::general_purpose::STANDARD.encode(vec![b'A'; OSC52_MAX_BYTES + 1]).as_bytes());
        big.push(0x07);
        assert_eq!(scan_osc52(&big), None);
        let mut at_cap = b"\x1b]52;c;".to_vec();
        at_cap.extend(base64::engine::general_purpose::STANDARD.encode(vec![b'B'; OSC52_MAX_BYTES]).as_bytes());
        at_cap.push(0x07);
        assert_eq!(scan_osc52(&at_cap).map(|s| s.len()), Some(OSC52_MAX_BYTES));
        // longer than the carry window, still unterminated: not a write
        let mut carried = b"\x1b]52;c;".to_vec();
        carried.extend(std::iter::repeat(b'A').take(5000));
        assert_eq!(scan_osc52(&carried), None);

        assert!(!allow_osc52_write(false, true), "default is off");
        assert!(!allow_osc52_write(false, false));
        assert!(!allow_osc52_write(true, false), "unfocused opt-in still does not write");
        assert!(allow_osc52_write(true, true));
    }

    /// BAYAN-003: UNC and device paths are not directories we will probe.
    #[test]
    fn local_cwd_rejects_unc_and_device_paths() {
        for bad in [
            r"\\evil.example\share\secret",
            r"\\evil.example\share",
            r"//evil.example/share",
            r"\\?\UNC\evil\share",
            r"\\?\unc\evil\share",
            r"\\.\UNC\evil\share",
            r"\\?\C:\Windows",
            r"\\.\C:\Windows",
            r"\\.\pipe\bayan",
            r"\\.\COM1",
            r"\??\UNC\evil\share",
            r"\??\C:\Windows",
            r"//?/UNC/evil/share",
            r"C:relative",
            "",
            "   ",
            "C:\\has\r\nline",
        ] {
            assert_eq!(accept_local_cwd(bad), None, "accepted {bad:?}");
        }
        assert_eq!(
            accept_local_cwd(r"C:\Users\Admin\Bayan\").as_deref(),
            Some(r"C:\Users\Admin\Bayan")
        );
        assert_eq!(accept_local_cwd("C:/Users/Admin").as_deref(), Some(r"C:\Users\Admin"));
        assert_eq!(accept_local_cwd(r"D:\").as_deref(), Some(r"D:\"));
        let long = format!(r"C:\{}", "a".repeat(5000));
        assert_eq!(accept_local_cwd(&long), None);

        let osc = |p: &str| {
            let mut v = b"\x1b]9;9;".to_vec();
            v.extend_from_slice(p.as_bytes());
            v.push(0x07);
            v
        };
        assert_eq!(scan_cwd(&osc(r"\\evil.example\share\secret")), None);
        assert_eq!(scan_cwd(&osc(r#""\\evil.example\share""#)), None);
        assert_eq!(scan_cwd(&osc(r"\\?\UNC\evil\share")), None);
        assert_eq!(
            scan_cwd(&osc(r"C:\Users\Admin\Bayan\")).as_deref(),
            Some(r"C:\Users\Admin\Bayan")
        );

        // a good report survives a later UNC one: nothing to probe, nothing to store
        let (mut t, mut p, mut meta, mut carry) = test_term();
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, &osc(r"C:\work"));
        assert_eq!(ev.cwd.as_deref(), Some(r"C:\work"));
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, &osc(r"\\evil\share"));
        assert!(ev.cwd.is_none());
        assert_eq!(meta.cwd.as_deref(), Some(r"C:\work"));
    }

    #[test]
    fn posh_config_parses_common_profile_lines() {
        assert_eq!(
            parse_posh_config(
                r#"oh-my-posh init pwsh --config "C:\Users\Admin\.poshthemes\night-owl.omp.json" | Invoke-Expression"#
            ),
            Some(r"C:\Users\Admin\.poshthemes\night-owl.omp.json".to_string())
        );
        assert_eq!(
            parse_posh_config("oh-my-posh init powershell --config 'D:\\t\\x.omp.json' | iex"),
            Some(r"D:\t\x.omp.json".to_string())
        );
        assert_eq!(
            parse_posh_config("oh-my-posh init pwsh --config C:\\plain\\path.omp.json | iex"),
            Some(r"C:\plain\path.omp.json".to_string())
        );
        // commented-out lines and unrelated profiles yield nothing
        assert_eq!(
            parse_posh_config("# oh-my-posh init pwsh --config x.json\nSet-Alias g git"),
            None
        );
    }
}
