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
/// Bytes one PTY `read` pulls. Named so the OSC 52 transport bound and the
/// reader share it. A terminated OSC 52 in one read is about 48 KiB, or
/// about 51 KiB when the opener is still inside [`MAX_CARRY`].
const PTY_READ: usize = 64 * 1024;

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
/// depend on these marks. `__by_osc7` adds the standard OSC 7 `file://`
/// report beside 9;9 (empty host, so `file:///C:/…`); cmd/clink has the
/// matching snippet in the README, because cmd is launched bare.
const PS_MARKS: &str = "function global:__by_osc7{if($PWD.Provider.Name -ne 'FileSystem'){return ''};\
$p=$PWD.ProviderPath.Replace('\\','/');if($p -match '^[A-Za-z]:'){$p='/'+$p};$e='';\
foreach($x in [Text.Encoding]::UTF8.GetBytes($p)){if($x -eq 47 -or ($x -ge 48 -and $x -le 57) -or ($x -ge 65 -and $x -le 90) -or ($x -ge 97 -and $x -le 122) -or $x -eq 45 -or $x -eq 46 -or $x -eq 95 -or $x -eq 126 -or $x -eq 58){$e+=[char]$x}else{$e+=('%{0:X2}' -f $x)}};[char]27+']7;file://'+$e+[char]7};\
function global:__by_wrap{if(-not $global:__BY_SI){$global:__BY_SI=$true;\
$global:__BY_OP=$function:prompt;function global:prompt{\
$c=$global:LASTEXITCODE;if($null -eq $c){if($?){$c=0}else{$c=1}};\
\"$([char]27)]133;D;$c$([char]7)$([char]27)]133;A$([char]7)\
$([char]27)]9;9;$($PWD.ProviderPath)$([char]7)$(__by_osc7)\"\
+(& $global:__BY_OP)+\"$([char]27)]133;B$([char]7)\"}}}; __by_wrap";

/// The PowerShell prompt wrapper injected at spawn. Tests check that OSC 7
/// stayed in it; the README quotes the same idea for cmd/clink.
pub(crate) fn shell_integration_ps() -> &'static str {
    PS_MARKS
}

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
    /// working directory from OSC 7 (`file://`) or OSC 9;9
    pub cwd: Option<String>,
    /// BDSM (implicit/explicit) and SCP paragraph direction for this pane.
    pub bidi: crate::bidi_ctl::BidiState,
    /// Half-parsed BDSM/SCP/RIS held across PTY reads.
    pub(crate) esc_filter: crate::bidi_ctl::StreamFilter,
    /// command blocks (OSC 133 A/D): gutter lights + prompt jumping
    pub marks: Vec<CmdMark>,
    /// history lines evicted past the scrollback cap (+ ED3-cleared lines):
    /// keeps mark positions anchored to content forever
    pub evicted: u64,
    /// when the running command was submitted — feeds the finish notification
    pub cmd_started: Option<std::time::Instant>,
    /// The chunk contained RIS (ESC c). The UI clears this and forgets
    /// which pane it had told "you are focused", so a 1004 that comes
    /// back in the same read still hears CSI I.
    pub saw_ris: bool,
}

impl SessionMeta {
    /// Directory last reported by OSC 7 or OSC 9;9.
    ///
    /// OSC 8 hyperlink resolution joins a relative `file:line` target against
    /// this. It is the report as accepted, not a promise the directory still
    /// exists — [`Self::spawn_cwd`] is what a new tab or split uses.
    pub fn reported_cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Reported directory, if it is a local drive path and it exists right
    /// now. [`accept_local_cwd`] runs before `is_dir`, so a UNC or `\\?\`
    /// path is never probed. A missing directory falls through to the
    /// launcher directory.
    pub fn spawn_cwd(&self) -> Option<String> {
        self.reported_cwd()
            .and_then(accept_local_cwd)
            .filter(|p| crate::osc7::directory_exists(p))
    }
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

/// Decoded OSC 52 backstop. One scan cannot actually deliver this much.
///
/// The reader pulls [`PTY_READ`] (64 KiB). [`incomplete_tail_start`] only
/// carries an OSC whose ESC still sits in the last [`MAX_CARRY`] (4 KiB)
/// bytes, so a *terminated* sequence has to fit in one scan buffer. For the
/// shortest opener `\x1b]52;c;` (7) plus a BEL (1), that is 49146 bytes on
/// one read (~48 KiB) and 52218 bytes when that opener is still inside the
/// carry (~51 KiB). This 64 KiB constant is only the ceiling `scan_osc52`
/// enforces once the bytes are already in one buffer.
pub(crate) const OSC52_MAX_BYTES: usize = 64 * 1024;

/// Decoded bytes a terminated `OSC 52 ; c ; … BEL` can occupy in a scan of
/// `scan_len` bytes. The selection field `c` is the shortest one; a longer
/// field leaves even less room. Test-only: the reader never calls it.
#[cfg(test)]
fn osc52_transport_room(scan_len: usize) -> usize {
    const WRAP: usize = 7 + 1; // ESC ] 5 2 ; c ;  + BEL
    (scan_len.saturating_sub(WRAP) / 4) * 3
}

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
/// All three must hold: the user set `osc52` to `"copy"`, the window is
/// focused, and the sequence came from the focused pane of the active tab.
/// A background tab or an unfocused split must not win the clipboard.
/// There is no flag here that allows a read.
pub fn allow_osc52_write(
    copy_enabled: bool,
    window_focused: bool,
    from_focused_pane: bool,
) -> bool {
    copy_enabled && window_focused && from_focused_pane
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

fn merge_chunk(dst: &mut ChunkEvents, src: ChunkEvents) {
    if src.clipboard.is_some() {
        dst.clipboard = src.clipboard;
    }
    if src.cwd.is_some() {
        dst.cwd = src.cwd;
    }
    if src.finished.is_some() {
        dst.finished = src.finished;
    }
}

/// Carry-merge, scan, split-feed one slice the BiDi filter has already
/// decided the parser should see. OSC 133 markers are consumed here
/// (alacritty would discard them anyway); segments between them feed the
/// emulator, with the cursor read exactly at each mark.
fn process_bytes<T: EventListener>(
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
    // ESC c is RIS. It can share a read with the 1004 that follows it,
    // and the UI only sees the final mode — so the flag has to be here.
    if buf.windows(2).any(|w| w == b"\x1bc") {
        meta.saw_ris = true;
    }
    ev.clipboard = scan_osc52(&buf);
    if let Some(c) = scan_cwd(&buf) {
        meta.cwd = Some(c.clone());
        ev.cwd = Some(c);
    }
    // OSC 7 wins only when it sits later in this slice than OSC 9;9. A
    // rejected file:// must not wipe a directory the other report just set.
    if let Some((at, cwd)) = crate::osc7::last_accepted(&buf, crate::osc7::cached_local_names()) {
        let later = crate::osc7::last_prefix_index(&buf, b"\x1b]9;9;")
            .map(|i| at > i)
            .unwrap_or(true);
        if later {
            meta.cwd = Some(cwd.clone());
            ev.cwd = Some(cwd);
        }
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

/// One PTY read. BDSM (`CSI 8 h` / `CSI 8 l`), SCP (`CSI Ps SP k`) and RIS
/// (`ESC c`) are peeled off first — alacritty_terminal 0.26 does not act on
/// the first two — and applied between the byte slices they used to sit in,
/// so a mode change and the text around it stay in order. An alternate-screen
/// switch resets BiDi state after the slice that caused it is parsed.
pub(crate) fn process_chunk<T: EventListener>(
    parser: &mut Processor,
    t: &mut Term<T>,
    meta: &mut SessionMeta,
    carry: &mut Vec<u8>,
    data: &[u8],
) -> ChunkEvents {
    let items = meta.esc_filter.pull(data);
    let mut ev = ChunkEvents::default();
    for item in items {
        match item {
            crate::bidi_ctl::StreamItem::Bytes(bytes) => {
                if bytes.is_empty() {
                    continue;
                }
                let was_alt = t.mode().contains(TermMode::ALT_SCREEN);
                let part = process_bytes(parser, t, meta, carry, &bytes);
                if was_alt != t.mode().contains(TermMode::ALT_SCREEN) {
                    meta.bidi.reset();
                }
                merge_chunk(&mut ev, part);
            }
            crate::bidi_ctl::StreamItem::Bdsm(mode) => meta.bidi.mode = mode,
            crate::bidi_ctl::StreamItem::Scp(dir) => {
                let (abs, col) = crate::bidi_ctl::cursor_abs(t, meta.evicted);
                meta.bidi.note_scp(dir, abs, col);
            }
            crate::bidi_ctl::StreamItem::Ris => meta.bidi.reset(),
        }
    }
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
                    powershell_escape_single(&theme)
                ));
            }
            setup.push(' ');
            setup.push_str(shell_integration_ps());
            cmd.arg(setup);
        } else if shell_family(shell) == ShellFamily::Cmd {
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
            let mut buf = [0u8; PTY_READ];
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
                            let _ = proxy.send_event(UserEvent::ClipboardSet(id, txt));
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

/// Why a paste or a drop is held for the confirm card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteRisk {
    /// CR, LF, CRLF, NEL, U+2028, or U+2029.
    Enter,
    /// A control, a C1 byte, or a bidi / zero-width spoof.
    Control,
    EnterAndControl,
    /// More than 2000 characters.
    TooLong,
    /// The shell has not reported OSC 133 B. Used by drag-drop, not by the guard.
    NotAtPrompt,
    /// The focused program is on the alternate screen (vim, less).
    AltScreen,
}

impl PasteRisk {
    /// Card copy. Arabic, matching the rest of the confirm UI.
    pub fn label(self) -> &'static str {
        match self {
            PasteRisk::Enter => "يحتوي على Enter",
            PasteRisk::Control => "يحتوي على أحرف تحكم",
            PasteRisk::EnterAndControl => "يحتوي على Enter وأحرف تحكم",
            PasteRisk::TooLong => "النص أطول من الحد",
            PasteRisk::NotAtPrompt => "ليس في موجّه الأوامر",
            PasteRisk::AltScreen => "البرنامج في وضع الشاشة الكاملة",
        }
    }
}

/// Counts and the reason, when confirmation is due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasteGuard {
    pub lines: usize,
    pub chars: usize,
    pub risk: PasteRisk,
}

/// Multi-line / huge pastes can execute commands on arrival (EasyTer's paste
/// protection). `Some` when confirmation is due.
///
/// CR is Enter on ConPTY, and the default PowerShell is started `-NoProfile`
/// so PSReadLine never turns bracketed paste on. LF-only detection let a
/// clipboard of `command\r` run on paste. Line breaks are CR, LF, CRLF, NEL,
/// U+2028 and U+2029. Any other control except a tab takes the same confirm
/// path (ESC would otherwise smuggle a sequence), and so do the bidi
/// overrides U+202A..=U+202E and the isolates U+2066..=U+2069, plus the
/// zero-width characters U+200B, U+FEFF, and U+2060 — none of those are
/// `is_control`, and a raw RLO in the card would spoof the preview. LRM
/// (U+200E), RLM (U+200F), and ALM (U+061C) do not confirm: Arabic copied
/// from the web is full of them. The size cap counts characters: a line of
/// Arabic is not "huge" because UTF-8 uses two bytes.
///
/// Bracketed paste is intentionally not forced from our side. The guard runs
/// first either way; injecting `\x1b[?2004h` into a host that does not
/// understand the wrappers would type the wrappers into the prompt.
pub fn needs_paste_guard(text: &str) -> Option<PasteGuard> {
    let mut breaks = 0usize;
    let mut ends_with_break = false;
    let mut saw_break = false;
    let mut saw_control = false;
    let mut chars = 0usize;
    let mut iter = text.chars().peekable();
    while let Some(c) = iter.next() {
        chars += 1;
        if c == '\r' {
            breaks += 1;
            ends_with_break = true;
            saw_break = true;
            if iter.peek() == Some(&'\n') {
                iter.next();
                chars += 1;
            }
            continue;
        }
        if is_line_break(c) {
            breaks += 1;
            ends_with_break = true;
            saw_break = true;
            continue;
        }
        ends_with_break = false;
        if c != '\t' && (c.is_control() || is_paste_spoof(c)) {
            saw_control = true;
        }
    }
    if saw_break || saw_control || chars > 2000 {
        let lines = breaks + usize::from(!ends_with_break);
        let risk = if saw_break && saw_control {
            PasteRisk::EnterAndControl
        } else if saw_break {
            PasteRisk::Enter
        } else if saw_control {
            PasteRisk::Control
        } else {
            PasteRisk::TooLong
        };
        Some(PasteGuard { lines, chars, risk })
    } else {
        None
    }
}

/// LF, NEL, line separator, paragraph separator. CR is handled by the caller
/// so a following LF can be swallowed as one CRLF.
fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\u{0085}' | '\u{2028}' | '\u{2029}')
}

/// Format characters that reorder or hide text. They are not controls, so
/// `is_control` misses them, and a confirm card that draws them raw can be
/// made to say the opposite of the bytes that would be sent.
///
/// LRM, RLM, and ALM are not in this set: a paste of ordinary Arabic should
/// not stop for them. A dropped filename still confirms on those marks; see
/// [`drop_needs_confirm`]. ZWJ (U+200D) and ZWNJ (U+200C) stay allowed either
/// way: Arabic and Persian need them.
fn is_paste_spoof(c: char) -> bool {
    paste_spoof_label(c).is_some() && !is_bidi_mark(c)
}

/// Marks that Arabic text from the web carries constantly. Not a paste
/// trigger. Still a confirm on a dropped filename, where one mark can hide
/// the rest of the name.
fn is_bidi_mark(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}')
}

fn paste_spoof_label(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{061C}' => "<ALM>",
        '\u{200B}' => "<ZWSP>",
        '\u{200E}' => "<LRM>",
        '\u{200F}' => "<RLM>",
        '\u{202A}' => "<LRE>",
        '\u{202B}' => "<RLE>",
        '\u{202C}' => "<PDF>",
        '\u{202D}' => "<LRO>",
        '\u{202E}' => "<RLO>",
        '\u{2060}' => "<WJ>",
        '\u{2066}' => "<LRI>",
        '\u{2067}' => "<RLI>",
        '\u{2068}' => "<FSI>",
        '\u{2069}' => "<PDI>",
        '\u{FEFF}' => "<BOM>",
        _ => return None,
    })
}

/// Which quoting rules a spawned command line needs. Unknown programs follow
/// PowerShell, because that is the shell Bayan starts when nothing else matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellFamily {
    PowerShell,
    Cmd,
    Posix,
    Fish,
    /// Nushell raw strings. Not PowerShell: `'` is not the quote character.
    Nu,
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
        "fish" => ShellFamily::Fish,
        "nu" => ShellFamily::Nu,
        "bash" | "sh" | "zsh" | "wsl" | "dash" => ShellFamily::Posix,
        // `git-bash.exe` is bash. Matching the substring "bash" would also
        // catch `notbash`, so the hyphen is part of the rule.
        n if n.ends_with("-bash") => ShellFamily::Posix,
        _ => ShellFamily::PowerShell,
    }
}

/// PowerShell single-quote characters. U+2018..=U+201B are quotes too: a
/// dropped file named `a’;calc;’.txt` otherwise runs `calc`.
fn is_powershell_single_quote(c: char) -> bool {
    matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}')
}

/// Double every PowerShell single quote, ASCII and smart. Shared by drop
/// quoting and the oh-my-posh `--config '…'` argument, which is the same literal.
fn powershell_escape_single(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if is_powershell_single_quote(c) {
            out.push(c);
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

/// Fish single quotes are literal except `\'` and `\\`. Escape `\` first,
/// then `'`. The other order turns `C:\d\';calc;#.txt` into
/// `'C:\d\\';calc;#.txt'`: fish collapses `\\` to one backslash, the quote
/// closes, and `;calc` runs. A doubled trailing backslash also keeps a drive
/// root (`C:\`) from escaping the closer, so the path is not rewritten to `C:\.`.
fn quote_fish(p: &str) -> String {
    p.replace('\\', r"\\").replace('\'', r"\'")
}

/// Nushell raw string. The hash run is one longer than any `'#…` already in
/// the path, so the content cannot close the literal.
fn quote_nu(p: &str) -> String {
    let mut n = 1usize;
    while p.contains(&format!("'{}", "#".repeat(n))) {
        n += 1;
    }
    let h = "#".repeat(n);
    format!("r{h}'{p}'{h}")
}

/// A file dropped onto a pane, quoted as a literal for `shell`, plus a trailing
/// space. `None` when the path contains a control character (or, for cmd, a
/// character that expands inside quotes) — those are not typed at all.
///
/// PowerShell single quotes are literal except for `'` — and the smart
/// quotes U+2018..=U+201B, which PowerShell treats as `'` too. Each is
/// doubled. cmd double quotes stop `&` from being a separator, but `%` and
/// `!` still expand, so a name containing either is refused. Git Bash and
/// WSL take POSIX single quotes (`'` → `'\''`). fish doubles `\` and then
/// escapes `'`. nushell takes a raw string (`r#'…'#`). `git-bash.exe` is
/// bash, not PowerShell.
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
        ShellFamily::PowerShell => format!("'{}'", powershell_escape_single(&p)),
        ShellFamily::Posix => format!("'{}'", p.replace('\'', r"'\''")),
        ShellFamily::Fish => format!("'{}'", quote_fish(&p)),
        ShellFamily::Nu => quote_nu(&p),
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

/// Prepare clipboard text for the PTY.
///
/// Line breaks become one carriage return (CRLF is one Enter). Tab stays.
/// Every other C0 (including every ESC), DEL, and every C1 byte is removed,
/// and so are the bidi overrides and zero-width characters the confirm card
/// flags. A single `.replace("\x1b[201~")` is not enough: `\x1b[20\x1b[201~1~`
/// still contains an ESC after that pass, and the wrapped paste ends early.
/// Stripping the controls leaves `[20[201~1~`, which cannot close the bracket.
///
/// Bracketed paste then wraps the stripped body. The guard still runs first;
/// this function is what a confirmed paste, and a drop into a program that
/// asked for bracketed paste, actually writes.
pub fn normalize_paste(text: &str, bracketed: bool) -> String {
    let mut body = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            body.push('\r');
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            continue;
        }
        if is_line_break(c) {
            body.push('\r');
            continue;
        }
        if c == '\t' {
            body.push('\t');
            continue;
        }
        if c.is_control() || is_paste_spoof(c) {
            continue;
        }
        body.push(c);
    }
    if bracketed {
        format!("\x1b[200~{body}\x1b[201~")
    } else {
        body
    }
}

/// A drop is typed straight in only at a cooked shell prompt whose text does
/// not itself need the paste guard. The alternate screen (vim, less), a shell
/// that has not reported OSC 133 B (cmd inside PowerShell never does), and a
/// name containing a bidi override, a zero-width character, or a directional
/// mark (LRM, RLM, ALM) go through the confirm card. Those marks do not
/// raise the card for a paste. In a filename they still do: quoting keeps
/// them, and they can hide the rest of the name.
pub fn drop_needs_confirm(alt_screen: bool, at_prompt: bool, text: &str) -> bool {
    alt_screen || !at_prompt || needs_paste_guard(text).is_some() || text.chars().any(is_bidi_mark)
}

/// The confirm card's one-line preview. Controls become the visible pictures
/// (␍ ␛ ␊ ␉); bidi overrides and zero-width characters become ASCII markers
/// so the card cannot be reordered by the text it is asking about.
pub fn paste_preview(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut n = 0usize;
    for c in text.chars() {
        let rep = paste_preview_char(c);
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

fn paste_preview_char(c: char) -> String {
    if let Some(label) = paste_spoof_label(c) {
        return label.to_string();
    }
    match c {
        // C0 control pictures: U+2400 + code. CR is ␍, ESC is ␛, LF is ␊, tab is ␉.
        c if (c as u32) <= 0x1F => char::from_u32(0x2400 + c as u32).unwrap().to_string(),
        '\u{007F}' => "\u{2421}".to_string(),
        c if c.is_control() => format!("<U+{:04X}>", c as u32),
        c => c.to_string(),
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
        // bracketed: wrapped. The ESC is gone; the leftover "[201~" cannot
        // close the bracket the way the whole end-marker used to.
        assert_eq!(
            normalize_paste("safe\x1b[201~evil\n", true),
            "\x1b[200~safe[201~evil\r\x1b[201~"
        );
    }

    /// A single `.replace("\x1b[201~")` turns `\x1b[20\x1b[201~1~` back into
    /// `\x1b[201~`. Every ESC has to go, and the card has to show the bytes.
    #[test]
    fn paste_strips_nested_bracket_end() {
        let out = normalize_paste("\x1b[20\x1b[201~1~", true);
        let body = out
            .strip_prefix("\x1b[200~")
            .and_then(|s| s.strip_suffix("\x1b[201~"))
            .expect("wrapped");
        assert_eq!(body, "[20[201~1~");
        assert!(!body.chars().any(|c| c == '\u{1b}'));
        // other C0 (BEL) and C1 (U+009B, the 8-bit CSI) do not survive; a tab does
        assert_eq!(normalize_paste("a\u{7}b\u{9b}c\t", false), "abc\t");
        assert_eq!(paste_preview("echo\r\x1b[2J", 48), "echo␍␛[2J");
        assert_eq!(
            needs_paste_guard("echo\r\x1b[2J").map(|g| g.risk),
            Some(PasteRisk::EnterAndControl)
        );
    }

    #[test]
    fn paste_guard_triggers_on_danger_only() {
        assert_eq!(needs_paste_guard("ls -la"), None);
        assert_eq!(
            needs_paste_guard("line1\nline2").map(|g| (g.lines, g.chars)),
            Some((2, 11))
        );
        // a single trailing newline still counts as multi-line intent
        assert_eq!(needs_paste_guard("rm -rf x\n").map(|g| g.lines), Some(1));
        let huge = "x".repeat(3000);
        assert_eq!(needs_paste_guard(&huge).map(|g| g.lines), Some(1));
        assert_eq!(needs_paste_guard(&huge).map(|g| g.risk), Some(PasteRisk::TooLong));
    }

    /// BAYAN-002: CR is Enter. A short CR-separated paste must ask, and so
    /// must other controls. The size cap is in characters, not UTF-8 bytes.
    #[test]
    fn paste_guard_treats_cr_and_controls_as_dangerous() {
        assert_eq!(
            needs_paste_guard("echo hi\recho pwned\r").map(|g| (g.lines, g.chars)),
            Some((2, 19))
        );
        assert_eq!(
            needs_paste_guard("whoami\r").map(|g| (g.lines, g.chars)),
            Some((1, 7))
        );
        // under the old 2000-byte cap, and still a submit
        assert!(needs_paste_guard(&"whoami\r".repeat(200)).is_some());
        // CRLF is one break, not two
        assert_eq!(
            needs_paste_guard("a\r\nb").map(|g| (g.lines, g.chars)),
            Some((2, 4))
        );
        assert_eq!(
            needs_paste_guard("a\u{2028}b").map(|g| (g.lines, g.chars)),
            Some((2, 3))
        );
        assert_eq!(
            needs_paste_guard("a\u{2029}b").map(|g| (g.lines, g.chars)),
            Some((2, 3))
        );
        assert_eq!(
            needs_paste_guard("a\u{0085}b").map(|g| (g.lines, g.chars)),
            Some((2, 3))
        );
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

    /// Overrides and zero-width characters are not `is_control`. A confirmed
    /// paste must not send them. LRM, RLM, and ALM are not in this set.
    /// ZWJ and ZWNJ stay: shaping needs them.
    #[test]
    fn paste_guard_flags_bidi_overrides_and_zero_width() {
        for (c, label) in [
            ('\u{200B}', "<ZWSP>"),
            ('\u{202A}', "<LRE>"),
            ('\u{202B}', "<RLE>"),
            ('\u{202C}', "<PDF>"),
            ('\u{202D}', "<LRO>"),
            ('\u{202E}', "<RLO>"),
            ('\u{2060}', "<WJ>"),
            ('\u{2066}', "<LRI>"),
            ('\u{2067}', "<RLI>"),
            ('\u{2068}', "<FSI>"),
            ('\u{2069}', "<PDI>"),
            ('\u{FEFF}', "<BOM>"),
        ] {
            let text = format!("ab{c}cd");
            assert_eq!(
                needs_paste_guard(&text).map(|g| g.risk),
                Some(PasteRisk::Control),
                "{label} must confirm"
            );
            assert_eq!(normalize_paste(&text, false), "abcd");
            assert_eq!(paste_preview(&text, 48), format!("ab{label}cd"));
        }
        assert_eq!(needs_paste_guard("a\u{200D}b\u{200C}c"), None, "ZWJ and ZWNJ stay");
        assert_eq!(normalize_paste("a\u{200D}b\u{200C}c", false), "a\u{200D}b\u{200C}c");
    }

    /// Arabic copied from the web is full of LRM, RLM, and ALM. Those marks
    /// paste straight through. An override still stops for the card.
    #[test]
    fn arabic_with_lrm_and_rlm_pastes_without_a_card() {
        let text = "السلام\u{200E} عليكم\u{200F} و\u{061C}رحمة";
        assert_eq!(needs_paste_guard(text), None);
        assert_eq!(normalize_paste(text, false), text);
        for (c, label) in [('\u{200E}', "<LRM>"), ('\u{200F}', "<RLM>"), ('\u{061C}', "<ALM>")] {
            let marked = format!("مرحبا{c}بك");
            assert_eq!(needs_paste_guard(&marked), None, "{label}");
            assert_eq!(normalize_paste(&marked, false), marked);
            // the card still draws the mark as text, so a mixed paste cannot
            // be reordered by a mark that did not itself open the card
            assert_eq!(paste_preview(&marked, 48), format!("مرحبا{label}بك"));
        }
        assert_eq!(
            needs_paste_guard("السلام\u{202E}عليكم").map(|g| g.risk),
            Some(PasteRisk::Control)
        );
        assert_eq!(normalize_paste("السلام\u{202E}عليكم", false), "السلامعليكم");
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
            dropped_path_arg(Path::new(r"C:\tmp\$(calc).txt"), "elvish.exe").as_deref(),
            Some(r"'C:\tmp\$(calc).txt' ")
        );
        // CR/LF in a name would be Enter. Refuse for every shell.
        assert_eq!(dropped_path_arg(Path::new("C:\\tmp\\a\nb.txt"), "powershell.exe"), None);
        assert_eq!(dropped_path_arg(Path::new("C:\\tmp\\a\rb.txt"), "cmd.exe"), None);
        assert_eq!(dropped_path_arg(Path::new("C:\\tmp\\a\rb.txt"), "wsl.exe -d Ubuntu"), None);
    }

    /// PowerShell treats U+2018..=U+201B as `'`. Doubling them is the same
    /// rule as the ASCII quote, including the unknown-shell fallback and the
    /// oh-my-posh theme argument (both call `powershell_escape_single`).
    #[test]
    fn powershell_doubles_smart_single_quotes() {
        use std::path::Path;
        let name = "C:\\tmp\\a\u{2019};calc;\u{2019}.txt";
        let doubled = "'C:\\tmp\\a\u{2019}\u{2019};calc;\u{2019}\u{2019}.txt' ";
        assert_eq!(
            dropped_path_arg(Path::new(name), "powershell.exe").as_deref(),
            Some(doubled)
        );
        let mixed = format!(
            "C:\\tmp\\{}a{}b{}c{}.txt",
            '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'
        );
        for shell in ["powershell.exe", "pwsh.exe", "elvish.exe"] {
            let q = dropped_path_arg(Path::new(&mixed), shell).unwrap();
            for cp in ['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'] {
                assert_eq!(
                    q.chars().filter(|c| *c == cp).count(),
                    2,
                    "{shell} left {cp:?} single in {q}"
                );
            }
        }
        // the theme path interpolates this into `--config '…'`
        assert_eq!(
            powershell_escape_single("D:\\themes\\o'night\u{2019}.omp.json"),
            "D:\\themes\\o''night\u{2019}\u{2019}.omp.json"
        );
    }

    /// `git-bash.exe` is bash. fish rejects `'\\''`. A full path to cmd.exe
    /// is cmd, same as the bare name `shell_family` uses for the spawn branch.
    #[test]
    fn shell_family_quotes_git_bash_fish_and_cmd() {
        use std::path::Path;
        assert_eq!(shell_family(r"C:\Program Files\Git\git-bash.exe"), ShellFamily::Posix);
        assert_eq!(shell_family("notbash.exe"), ShellFamily::PowerShell);
        assert_eq!(shell_family(r"C:\Windows\System32\cmd.exe"), ShellFamily::Cmd);
        assert_eq!(shell_family("cmd.exe /K chcp 65001"), ShellFamily::Cmd);
        assert_eq!(shell_family("fish.exe"), ShellFamily::Fish);
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\o'brien.txt"), "git-bash.exe").as_deref(),
            Some("'C:\\o'\\''brien.txt' ")
        );
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\o'brien.txt"), "fish.exe").as_deref(),
            Some(r"'C:\\o\'brien.txt' ")
        );
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\"), "fish.exe").as_deref(),
            Some(r"'C:\\' ")
        );
    }

    /// Fish collapses `\\` inside single quotes. Escaping the apostrophe
    /// first makes `C:\d\';calc;#.txt` close the quote and run `calc`.
    #[test]
    fn fish_doubles_backslash_before_escaping_quote() {
        use std::path::Path;
        let attack = "C:\\d\\';calc;#.txt";
        let quoted = dropped_path_arg(Path::new(attack), "fish.exe").unwrap();
        assert_eq!(quoted, r"'C:\\d\\\';calc;#.txt' ");
        assert_eq!(fish_single_quote_value(&quoted), attack);
        // neighbours: apostrophe with no slash in front of it, a plain path,
        // and a drive root (the old `C:\.` rewrite is gone)
        for path in [r"C:\o'brien.txt", r"C:\tmp\file.txt", r"C:\"] {
            let q = dropped_path_arg(Path::new(path), "fish.exe").unwrap();
            assert_eq!(fish_single_quote_value(&q), path, "{q}");
        }
    }

    /// What fish single quotes mean: `\\` is one backslash, `\'` is one quote,
    /// and a bare `'` would have closed the literal.
    fn fish_single_quote_value(typed: &str) -> String {
        let s = typed.trim();
        assert!(s.starts_with('\'') && s.ends_with('\''), "{typed}");
        let body = &s[1..s.len() - 1];
        let mut out = String::new();
        let mut it = body.chars();
        while let Some(c) = it.next() {
            if c == '\\' {
                out.push(it.next().expect("dangling fish escape"));
            } else {
                assert_ne!(c, '\'', "quote closed early in {typed}");
                out.push(c);
            }
        }
        out
    }

    /// nushell raw strings, with enough `#` that a `'#` in the name cannot close it.
    #[test]
    fn nu_quotes_with_a_raw_string() {
        use std::path::Path;
        assert_eq!(shell_family("nu.exe"), ShellFamily::Nu);
        assert_eq!(shell_family(r"C:\Program Files\nu\nu.exe"), ShellFamily::Nu);
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\tmp\$(calc).txt"), "nu.exe").as_deref(),
            Some(r"r#'C:\tmp\$(calc).txt'# ")
        );
        let smart = "C:\\tmp\\a\u{2019};calc;\u{2019}.txt";
        assert_eq!(
            dropped_path_arg(Path::new(smart), "nu.exe").as_deref(),
            Some("r#'C:\\tmp\\a\u{2019};calc;\u{2019}.txt'# ")
        );
        // one hash would end the literal at `'#`
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\a'#calc.txt"), "nu.exe").as_deref(),
            Some(r"r##'C:\a'#calc.txt'## ")
        );
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\o'brien.txt"), "nu.exe").as_deref(),
            Some(r"r#'C:\o'brien.txt'# ")
        );
    }

    /// Alt-screen and a shell that is not at its prompt must confirm. A drop
    /// into a program that asked for bracketed paste is wrapped after quoting.
    #[test]
    fn drop_off_prompt_confirms_and_bracketed_wraps() {
        use std::path::Path;
        assert!(drop_needs_confirm(true, true, "safe"), "alt screen, even at a prompt mark");
        assert!(drop_needs_confirm(false, false, "safe"), "no OSC 133 B");
        assert!(!drop_needs_confirm(false, true, r"'C:\safe.txt' "), "cooked prompt types immediately");
        // RLO / ZWSP in the name would be stripped on the way to the shell
        // with no card. A prompt does not make that silent.
        let rlo = dropped_path_arg(Path::new("C:\\d\\a\u{202E}b.txt"), "powershell.exe").unwrap();
        assert!(drop_needs_confirm(false, true, &rlo), "RLO at the prompt still confirms");
        let zwsp = dropped_path_arg(Path::new("C:\\d\\a\u{200B}b.txt"), "cmd.exe").unwrap();
        assert!(drop_needs_confirm(false, true, &zwsp));
        let zwj = dropped_path_arg(Path::new("C:\\d\\a\u{200D}b.txt"), "powershell.exe").unwrap();
        assert!(!drop_needs_confirm(false, true, &zwj), "ZWJ is part of the name");
        // LRM / RLM / ALM do not confirm a paste. In a filename they still do.
        for mark in ['\u{200E}', '\u{200F}', '\u{061C}'] {
            let raw = format!("C:\\d\\a{mark}b.txt");
            let quoted = dropped_path_arg(Path::new(&raw), "powershell.exe").unwrap();
            assert!(
                drop_needs_confirm(false, true, &quoted),
                "{mark:?} in a dropped name still confirms"
            );
        }
        let arg = dropped_path_arg(Path::new(r"C:\tmp\a&calc.exe"), "powershell.exe").unwrap();
        let wrapped = normalize_paste(&arg, true);
        assert!(wrapped.starts_with("\x1b[200~"), "{wrapped:?}");
        assert!(wrapped.ends_with("\x1b[201~"), "{wrapped:?}");
        assert!(wrapped.contains(r"'C:\tmp\a&calc.exe'"), "{wrapped:?}");
        assert_eq!(normalize_paste(&arg, false), arg);
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
    fn ris_is_flagged_even_when_focus_mode_comes_back() {
        let (mut t, mut p, mut meta, mut carry) = test_term();
        // RIS, then 1004 in the same read. The final mode has focus
        // reporting; the router still has to forget the previous CSI I.
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1bc\x1b[?1004h",
        );
        assert!(meta.saw_ris);
        assert!(t.mode().contains(TermMode::FOCUS_IN_OUT));
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
        carried.extend(std::iter::repeat_n(b'A', 5000));
        assert_eq!(scan_osc52(&carried), None);

        assert!(!allow_osc52_write(false, true, true), "default is off");
        assert!(!allow_osc52_write(false, false, true));
        assert!(!allow_osc52_write(true, false, true), "unfocused opt-in still does not write");
        assert!(allow_osc52_write(true, true, true));
    }

    /// A background tab or an unfocused split must not set the clipboard,
    /// even when copy is enabled and the window itself is focused.
    #[test]
    fn osc52_write_only_from_the_focused_pane() {
        assert!(!allow_osc52_write(true, true, false));
        assert!(!allow_osc52_write(true, false, false));
        assert!(allow_osc52_write(true, true, true));
    }

    /// The 64 KiB constant is a decode backstop. One scan sees ~48 KiB, or
    /// ~51 KiB when the opener is still in the 4 KiB carry.
    #[test]
    fn osc52_visible_cap_is_the_scan_buffer() {
        assert_eq!(osc52_transport_room(PTY_READ), 49_146);
        assert_eq!(osc52_transport_room(PTY_READ + MAX_CARRY), 52_218);
        assert!(OSC52_MAX_BYTES > osc52_transport_room(PTY_READ + MAX_CARRY));
        assert_eq!(PTY_READ, 64 * 1024);
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
