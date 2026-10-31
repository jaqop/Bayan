//! PTY session + terminal state: ConPTY via portable-pty, VT emulation via
//! alacritty_terminal. Mirrors EasyTer's proven architecture: a reader thread
//! feeds the emulator behind a mutex and nudges the UI thread to repaint.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Column;
use alacritty_terminal::sync::FairMutex;
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
    crate::shells::detect()
        .into_iter()
        .map(|s| s.command)
        .collect()
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
    let (p, r) = (
        path.to_lowercase().replace('/', "\\"),
        root.to_lowercase().replace('/', "\\"),
    );
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
    (
        it.next().unwrap_or(c).to_string(),
        it.map(str::to_string).collect(),
    )
}

/// OSC 133 prompt wrapper injected into PowerShell (EasyTer's __et_wrap,
/// ported): D=command end (exit code), A=prompt start, 9;9=cwd report,
/// B=input begins. Command detection — and Claude mode's auto-enable —
/// depend on these marks. `__by_osc7` adds the standard OSC 7 `file://`
/// report beside 9;9 (empty host, so `file:///C:/…`); cmd/clink has the
/// matching snippet in the README, because cmd is launched bare.
const PS_HEAD: &str = "function global:__by_osc7{if($PWD.Provider.Name -ne 'FileSystem'){return ''};\
$p=$PWD.ProviderPath.Replace('\\','/');if($p -match '^[A-Za-z]:'){$p='/'+$p};$e='';\
foreach($x in [Text.Encoding]::UTF8.GetBytes($p)){if($x -eq 47 -or ($x -ge 48 -and $x -le 57) -or ($x -ge 65 -and $x -le 90) -or ($x -ge 97 -and $x -le 122) -or $x -eq 45 -or $x -eq 46 -or $x -eq 95 -or $x -eq 126 -or $x -eq 58){$e+=[char]$x}else{$e+=('%{0:X2}' -f $x)}};[char]27+']7;file://'+$e+[char]7};\
function global:__by_wrap{if(-not $global:__BY_SI){$global:__BY_SI=$true;\
$global:__BY_OP=$function:prompt;function global:prompt{\
$c=$global:LASTEXITCODE;if($null -eq $c){if($?){$c=0}else{$c=1}};\
\"$([char]27)]133;D;$c;";

const PS_MID: &str = "$([char]7)$([char]27)]133;A;";

const PS_TAIL: &str = "$([char]7)\
$([char]27)]9;9;$($PWD.ProviderPath)$([char]7)$(__by_osc7)\"\
+(& $global:__BY_OP)+\"$([char]27)]133;B$([char]7)\"}}}; __by_wrap";

/// The PowerShell prompt wrapper injected at spawn.
///
/// `nonce` is embedded in OSC 133 D and A (`D;<exit>;<nonce>`, `A;<nonce>`).
/// A report from a running command is stored only after an A that carries
/// the same nonce, so a spoofed `133;D` cannot authorize the OSC 7 that
/// follows it. Hex is safe inside the double-quoted prompt string. An empty
/// nonce keeps the old "any A/D" rule for tests. cmd/clink has no 133 marks;
/// its OSC 7 is still accepted while the pane is already idle.
pub(crate) fn shell_integration_ps(nonce: &str) -> String {
    let mut s =
        String::with_capacity(PS_HEAD.len() + PS_MID.len() + PS_TAIL.len() + nonce.len() * 2);
    s.push_str(PS_HEAD);
    s.push_str(nonce);
    s.push_str(PS_MID);
    s.push_str(nonce);
    s.push_str(PS_TAIL);
    s
}

#[link(name = "advapi32")]
extern "system" {
    /// `SystemFunction036` is `RtlGenRandom`. 32 hex characters from it go
    /// into OSC 133 so a program cannot mint a prompt-end mark.
    fn SystemFunction036(randombuffer: *mut core::ffi::c_void, randombufferlength: u32) -> i32;
}

fn fresh_prompt_nonce() -> String {
    let mut bytes = [0u8; 16];
    // SAFETY: the length passed is `bytes`'s own, so the write stays in bounds.
    let ok = unsafe { SystemFunction036(bytes.as_mut_ptr().cast(), bytes.len() as u32) != 0 };
    if !ok {
        let tick = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        bytes[..8].copy_from_slice(&tick.to_le_bytes());
        bytes[8..].copy_from_slice(&(std::process::id() as u64).to_le_bytes());
    }
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(b"0123456789abcdef"[(b >> 4) as usize]));
        s.push(char::from(b"0123456789abcdef"[(b & 0xf) as usize]));
    }
    s
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
    /// Global row of OSC 133 B; a command may wrap below this row.
    pub prompt_abs: Option<u64>,
    /// the command line currently executing ("" = idle) — feeds cmd_is_claude
    pub running_cmd: String,
    /// One-shot cleanup once a newly launched interactive Claude is ready.
    pub clean_claude_after: Option<u64>,
    /// First absolute row belonging to the current inline Claude screen.
    pub claude_from_abs: Option<u64>,
    /// working directory from OSC 9;9
    /// Enter was sent at the prompt, and a trusted OSC 133 A or D has not
    /// been seen since. Set even when the cursor never moved past the prompt
    /// column, so `running_cmd` stayed empty: paste and Enter in one chunk,
    /// Home then Enter, or a command that wrapped onto the next line. While
    /// this is set, OSC 7 and OSC 9;9 are held to the nonce the same way
    /// they are while `running_cmd` is set. Cleared only by a nonce-verified
    /// 133 A or D.
    pub cmd_pending: bool,
    /// working directory from OSC 7 (`file://`) or OSC 9;9.
    /// File links resolve through `link::cwd_hook` on this field. There is
    /// no second source.
    pub cwd: Option<String>,
    /// BDSM (implicit/explicit) and SCP paragraph direction for this pane.
    /// An `Arc` so a frame can snapshot it without cloning the stamp list.
    /// The reader thread uses [`Self::bidi_mut`], which copies only when a
    /// frame still holds the previous snapshot.
    pub bidi: std::sync::Arc<crate::bidi_ctl::BidiState>,
    /// Half-parsed BDSM/SCP/RIS held across PTY reads.
    pub(crate) esc_filter: crate::bidi_ctl::StreamFilter,
    /// command blocks (OSC 133 A/D): gutter lights + prompt jumping
    pub marks: Vec<CmdMark>,
    /// history lines evicted past the scrollback cap (+ ED3-cleared lines):
    /// keeps mark positions anchored to content forever
    pub evicted: u64,
    /// when the running command was submitted — feeds the finish notification
    pub cmd_started: Option<std::time::Instant>,
    /// User opted in (`keyboard.kitty_protocol`). Off: the parser is built
    /// with `kitty_keyboard: false`, so its stack is never touched, and a
    /// query is answered `CSI ?0u` by us. ConPTY may swallow the ESC of a
    /// CSI u sequence and turn Ctrl+C into literal text.
    pub kitty_protocol: bool,
    /// How many keyboard-mode levels we have actually pushed on this screen
    /// for sequences we recognized. Kitty's own cap is [`KITTY_STACK_CAP`].
    /// RIS (`ESC c`, the same bytes as [`SessionMeta::saw_ris`]) zeroes this
    /// and [`kitty_alt_depth`].
    pub kitty_depth: u16,
    /// The other screen's depth. 1049h/l swaps the stacks, so this swaps too.
    pub kitty_alt_depth: u16,
    /// The chunk contained RIS (ESC c). The UI clears this and forgets
    /// which pane it had told "you are focused", so a 1004 that comes
    /// back in the same read still hears CSI I. The keyboard-depth reset
    /// uses this same byte test, inside the feed, because the flag stays
    /// set until the UI reads it.
    pub saw_ris: bool,
    /// Secret mixed into OSC 133 A and D by the PowerShell prompt wrapper.
    /// Empty in tests: any A or D is trusted. A live session sets 32 hex
    /// characters at spawn. While a command is running, OSC 7 / OSC 9;9 are
    /// stored only after an A that carries this nonce. Clink's OSC 7 has no
    /// 133 and is still accepted when the pane is already idle.
    pub prompt_nonce: String,
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

    /// Copy-on-write for the reader thread. A frame holding the previous
    /// `Arc` keeps its snapshot; the next mutation does not rewrite it.
    pub(crate) fn bidi_mut(&mut self) -> &mut crate::bidi_ctl::BidiState {
        std::sync::Arc::make_mut(&mut self.bidi)
    }

    /// Reported directory, if it is a local drive path and it exists right
    /// now. [`accept_local_cwd`] runs before any probe, and [`usable_spawn_dir`]
    /// rejects a remote drive or a reparse point before `is_dir`. A missing
    /// directory falls through to the launcher directory.
    pub fn spawn_cwd(&self) -> Option<String> {
        self.reported_cwd()
            .and_then(accept_local_cwd)
            .filter(|p| usable_spawn_dir(p))
    }
}

/// kitty itself refuses to grow the keyboard-mode stack past 16. Recognized
/// pushes past this are dropped. Sequences the scanner does not recognize
/// still reach the parser; the crate patch makes those evict from the
/// keyboard stack instead of panicking.
const KITTY_STACK_CAP: u16 = 16;

/// Events one PTY chunk produced, delivered to the UI thread by the reader.
#[derive(Default)]
pub(crate) struct ChunkEvents {
    pub clean_claude: bool,
    pub clipboard: Option<String>,
    pub cwd: Option<String>,
    /// a LONG command just ended (ok?) — worth a notification if unfocused
    pub finished: Option<bool>,
    /// Replies alacritty did not emit. With the kitty protocol off the
    /// parser's `kitty_keyboard` flag is false, so `CSI ? u` is answered
    /// here with `CSI ?0u` and the reader writes it to the PTY.
    pub pty_replies: Vec<String>,
    /// the program asked for a desktop notification (title, body): OSC 9,
    /// OSC 777 or OSC 99 — what Claude Code, ntfy, zellij and friends emit
    pub notify: Option<(String, String)>,
    /// OSC 9;4 progress: Some(None) clears, Some(Some(p)) sets
    pub progress: Option<Option<Progress>>,
}

/// ConEmu's OSC 9;4 progress report, as Windows Terminal, Ghostty, WezTerm
/// and Konsole read it — and as Claude Code emits it while it works
/// (`terminalProgressBarEnabled`). The tab bar and the cockpit draw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub state: ProgressState,
    /// 0..=100; meaningful for Normal, Error and Paused
    pub percent: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressState {
    Normal,
    Error,
    Indeterminate,
    Paused,
}

/// Parse an OSC 9;4 payload ("st;pr" after the "4;"): st 0 clears, 1 normal
/// (pr%), 2 error, 3 indeterminate, 4 paused. Unknown states are ignored
/// rather than drawn as something they are not.
fn parse_progress(payload: &[u8]) -> Option<Option<Progress>> {
    let s = std::str::from_utf8(payload).ok()?;
    let mut it = s.split(';');
    let st: u8 = it.next()?.trim().parse().ok()?;
    let pr: u8 = it
        .next()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(0)
        .min(100);
    Some(match st {
        0 => None,
        1 => Some(Progress {
            state: ProgressState::Normal,
            percent: pr,
        }),
        2 => Some(Progress {
            state: ProgressState::Error,
            percent: pr,
        }),
        3 => Some(Progress {
            state: ProgressState::Indeterminate,
            percent: 0,
        }),
        4 => Some(Progress {
            state: ProgressState::Paused,
            percent: pr,
        }),
        _ => return None,
    })
}

/// The LAST OSC 9;4 in a chunk (a burst of progress updates collapses to
/// its final value — nobody needs the intermediate ones drawn).
fn scan_progress(data: &[u8]) -> Option<Option<Progress>> {
    let payload = last_osc_payload(data, b"\x1b]9;4;")?;
    parse_progress(payload)
}

/// Desktop-notification requests, three dialects, last one in the chunk wins:
/// - OSC 9;<message> — iTerm2 / ConEmu (digit-prefixed ConEmu subcommands
///   like 9;4; and 9;9; are NOT messages)
/// - OSC 777;notify;<title>;<body> — rxvt-unicode, what Ghostty and WezTerm speak
/// - OSC 99;<meta>;<payload> — kitty: `p=title`/`p=body` in the metadata
fn scan_notify(data: &[u8]) -> Option<(String, String)> {
    let mut best: Option<(usize, (String, String))> = None;
    let mut consider = |pos: usize, n: (String, String)| {
        if best.as_ref().is_none_or(|(p, _)| pos > *p) {
            best = Some((pos, n));
        }
    };
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
    // OSC 9;<message>
    let mut i = 0;
    while let Some(rel) = find(&data[i..], b"\x1b]9;") {
        let s = i + rel + 4;
        let e = osc_end(data, s);
        let payload = &data[s..e];
        // ConEmu subcommand (9;4;progress, 9;9;cwd, 9;12;...): not a message
        let is_subcommand = payload
            .iter()
            .position(|&b| b == b';')
            .is_some_and(|k| k > 0 && payload[..k].iter().all(u8::is_ascii_digit));
        if !is_subcommand && !payload.is_empty() {
            consider(s, ("Bayan".to_string(), text(payload)));
        }
        i = e.max(s);
    }
    // OSC 777;notify;title;body
    let mut i = 0;
    while let Some(rel) = find(&data[i..], b"\x1b]777;notify;") {
        let s = i + rel + 13;
        let e = osc_end(data, s);
        let payload = &data[s..e];
        let (title, body) = match payload.iter().position(|&b| b == b';') {
            Some(k) => (text(&payload[..k]), text(&payload[k + 1..])),
            None => ("Bayan".to_string(), text(payload)),
        };
        consider(s, (title, body));
        i = e.max(s);
    }
    // OSC 99;<k=v:k=v>;<payload>
    let mut i = 0;
    while let Some(rel) = find(&data[i..], b"\x1b]99;") {
        let s = i + rel + 5;
        let e = osc_end(data, s);
        let payload = &data[s..e];
        if let Some(k) = payload.iter().position(|&b| b == b';') {
            let meta = String::from_utf8_lossy(&payload[..k]);
            let is_title = meta.split(':').any(|kv| kv == "p=title");
            let body = text(&payload[k + 1..]);
            if !body.is_empty() {
                consider(
                    s,
                    if is_title {
                        (body, String::new())
                    } else {
                        ("Bayan".to_string(), body)
                    },
                );
            }
        }
        i = e.max(s);
    }
    best.map(|(_, n)| n)
}

/// End of an OSC payload that starts at `s`: the BEL / ESC, or the data end.
fn osc_end(data: &[u8], s: usize) -> usize {
    data[s..]
        .iter()
        .position(|&b| b == 0x07 || b == 0x1b)
        .map_or(data.len(), |p| s + p)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
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

/// OSC 133 markers in a chunk: (start, end, kind, exit code for D marks,
/// whether the payload carries the session nonce).
/// The carry logic above guarantees whole sequences.
fn find_osc133(data: &[u8], nonce: &str) -> Vec<(usize, usize, u8, Option<i32>, bool)> {
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
                    let payload = &data[i + P.len()..j];
                    let trusted = osc133_trusted(payload, nonce);
                    // "D;<code>" carries the exit status. A nonce field after
                    // the code is not part of the number.
                    let exit = data[i + P.len() + 1..j]
                        .strip_prefix(b";")
                        .and_then(|p| std::str::from_utf8(p).ok())
                        .and_then(|p| p.split(';').next())
                        .and_then(|p| p.parse().ok());
                    out.push((i, e, kind, exit, trusted));
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
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
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
/// (`\\?\`, `\\.\`, including `\\?\UNC\…`), the NT prefix `\??\`, a comma
/// or a quote, a `\\` or `//` anywhere, `..` that climbs past the drive,
/// DOS device names, alternate data streams, and bidi controls are rejected
/// here, **before** any `exists` / `is_dir` / `canonicalize` call — on
/// Windows those open SMB and can leak NTLM. `.` and a `..` that stays on
/// the drive are collapsed (`C:\a\..\b` becomes `C:\b`). OSC 7 parses the
/// URL and then calls this; do not grow a second checker.
pub fn accept_local_cwd(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s.chars().count() > MAX_CWD_CHARS {
        return None;
    }
    if s.chars()
        .any(|c| c.is_control() || crate::osc7::is_bidi_control(c))
    {
        return None;
    }
    // A comma or a quote breaks out of `explorer /select,"<path>"`.
    if s.contains(',') || s.contains('"') {
        return None;
    }
    if unc_or_device(s) {
        return None;
    }
    let mut norm: String = s.chars().map(|c| if c == '/' { '\\' } else { c }).collect();
    // Keep the drive root (`C:\`). A trailing slash on anything longer is
    // noise, and on cmd it can also escape a closing quote.
    if norm.len() > 3 {
        while norm.ends_with('\\') {
            norm.pop();
        }
    }
    if unc_or_device(&norm) || norm.contains("\\\\") {
        return None;
    }
    let mut chars = norm.chars();
    let drive = chars.next()?;
    if !drive.is_ascii_alphabetic() || chars.next() != Some(':') {
        return None;
    }
    if chars.next() != Some('\\') {
        return None;
    }
    // The drive letter's colon is the only one allowed. A later colon is
    // an alternate data stream (`a.txt:evil.exe`, `evil.exe::$DATA`).
    if norm.len() > 2 && norm[2..].contains(':') {
        return None;
    }
    let rest = if norm.len() <= 3 { "" } else { &norm[3..] };
    let mut stack: Vec<&str> = Vec::new();
    if !rest.is_empty() {
        for comp in rest.split('\\') {
            if comp.is_empty() {
                return None;
            }
            // A device component is rejected even when a later `..` would
            // have popped it (`C:\CON\..\Users`).
            if reserved_dos_name(comp) {
                return None;
            }
            if comp == "." {
                continue;
            }
            if comp == ".." {
                if stack.is_empty() {
                    return None;
                }
                stack.pop();
                continue;
            }
            stack.push(comp);
        }
    }
    let mut out = String::new();
    out.push(drive);
    out.push(':');
    out.push('\\');
    if !stack.is_empty() {
        out.push_str(&stack.join("\\"));
    }
    if out.chars().count() > MAX_CWD_CHARS || unc_or_device(&out) || out.contains("\\\\") {
        return None;
    }
    Some(out)
}

fn reserved_dos_name(component: &str) -> bool {
    // Windows strips spaces before the device check, so `CON .txt` is `CON`.
    let collapsed: String = component.chars().filter(|c| *c != ' ').collect();
    let trimmed = collapsed.trim_end_matches('.');
    if trimmed.is_empty() {
        return false;
    }
    let base = trimmed.split('.').next().unwrap_or(trimmed);
    let upper = base.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) {
        return true;
    }
    let rest = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"));
    let Some(rest) = rest else {
        return false;
    };
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => dos_device_digit(c),
        _ => false,
    }
}

/// ASCII `0`–`9`, plus the superscript digits Windows folds into that slot
/// (`⁰¹²³⁴⁵⁶⁷⁸⁹`, with ¹²³ in the Latin-1 block).
fn dos_device_digit(c: char) -> bool {
    matches!(
        c,
        '0'..='9'
            | '\u{2070}'
            | '\u{00B9}'
            | '\u{00B2}'
            | '\u{00B3}'
            | '\u{2074}'
            | '\u{2075}'
            | '\u{2076}'
            | '\u{2077}'
            | '\u{2078}'
            | '\u{2079}'
    )
}

/// Spawn-time probe. [`accept_local_cwd`] has already rejected UNC.
/// `GetDriveTypeW` on the drive root rejects a mapped remote drive before
/// `is_dir` opens it. A reparse point (symlink, junction, mount point) is
/// rejected without following it, because the target may be a UNC path.
/// Both calls are one syscall; nothing here canonicalizes.
pub(crate) fn usable_spawn_dir(path: &str) -> bool {
    drive_root_is_local(path) && !is_reparse_point(path) && crate::osc7::directory_exists(path)
}

fn wide_nul(path: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn drive_root_is_local(path: &str) -> bool {
    let mut chars = path.chars();
    let drive = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => c,
        _ => return false,
    };
    if chars.next() != Some(':') {
        return false;
    }
    let root = format!("{drive}:\\");
    let wide = wide_nul(&root);
    // DRIVE_REMOTE is 4. The constant lives in Win32_System_WindowsProgramming,
    // which this crate does not enable; the value is the Win32 one.
    const DRIVE_REMOTE: u32 = 4;
    // SAFETY: `wide` comes from `wide_nul`, so it is NUL-terminated.
    unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) != DRIVE_REMOTE }
}

fn is_reparse_point(path: &str) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    let wide = wide_nul(path);
    // SAFETY: `wide` comes from `wide_nul`, so it is NUL-terminated.
    let attr = unsafe { GetFileAttributesW(wide.as_ptr()) };
    attr != u32::MAX && attr & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// `\\…` (UNC, `\\?\`, `\\.\`) or `\??\…`. Slashes are normalized first so
/// `//server/share` and `//?/UNC/…` take the same path as their backslash form.
/// File links call this on the path they are about to open — the same gate
/// as [`accept_local_cwd`], not a second copy of the rules.
pub(crate) fn unc_or_device(s: &str) -> bool {
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

struct OscAt<'a> {
    number: &'a [u8],
    payload: &'a [u8],
    /// Index just past BEL or ST.
    end: usize,
}

/// One BEL/ST-terminated OSC that starts at `i` (`ESC ]`).
///
/// The payload is only this sequence. A later OSC in the same slice is a
/// separate call, so an untrusted report cannot be read back through a
/// "last match in the suffix" scan.
fn osc_at(data: &[u8], i: usize) -> Option<OscAt<'_>> {
    if data.get(i + 1) != Some(&b']') {
        return None;
    }
    let mut j = i + 2;
    while data.get(j) == Some(&b' ') {
        j += 1;
    }
    let num_start = j;
    while matches!(data.get(j), Some(b'0'..=b'9')) {
        j += 1;
    }
    if j == num_start {
        return None;
    }
    let number = &data[num_start..j];
    while data.get(j) == Some(&b' ') {
        j += 1;
    }
    if data.get(j) != Some(&b';') {
        return None;
    }
    j += 1;
    let pay = j;
    while j < data.len() && data[j] != 0x07 && data[j] != 0x1b {
        j += 1;
    }
    let end = if data.get(j) == Some(&0x07) {
        j + 1
    } else if data.get(j) == Some(&0x1b) && data.get(j + 1) == Some(&b'\\') {
        j + 2
    } else {
        return None;
    };
    Some(OscAt {
        number,
        payload: &data[pay..j],
        end,
    })
}

fn cwd_from_osc7(payload: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(payload).ok()?;
    crate::osc7::parse_osc7(text, crate::osc7::cached_local_names())
}

/// `A` or `D` whose `;`-separated fields contain `nonce`.
///
/// An empty nonce trusts every A and D, which is what the unit tests send.
/// A live nonce is 32 hex characters from [`fresh_prompt_nonce`].
fn osc133_trusted(payload: &[u8], nonce: &str) -> bool {
    let Some(&kind) = payload.first() else {
        return false;
    };
    if kind != b'A' && kind != b'D' {
        return false;
    }
    if nonce.is_empty() {
        return true;
    }
    let Ok(text) = std::str::from_utf8(payload) else {
        return false;
    };
    text.split(';').any(|field| field == nonce)
}

/// The last OSC 7 or OSC 9;9 in `data` that is allowed to change the cwd.
///
/// `running` and `alt` are the pane state at the start of the slice.
/// `running` is also true when Enter was sent at the prompt and no trusted
/// 133 A/D has cleared `cmd_pending` yet, even if the command text was not
/// captured. The prompt wrapper emits OSC 133 D, then A, then 9;9, then 7,
/// in one read. A trusted A or D clears `running` for reports that follow
/// them in this slice. With a session nonce, only an A that carries it arms
/// the report: a spoofed `133;D` leaves the command running and does not
/// authorize the OSC 7 after it. An empty nonce (tests) arms at the start,
/// so any A/D is enough. A pane that is already idle stays armed, which is
/// how clink's OSC 7 — no 133 at all — still updates the directory. A
/// `?1049` / `?47` /
/// `?1047` updates `alt` for reports after that CSI. RIS (`ESC c`) leaves
/// the alt screen.
///
/// An untrusted or rejected report does not replace `best`. The later of
/// two trusted reports wins.
fn trusted_cwd(data: &[u8], running: bool, alt: bool, nonce: &str) -> Option<String> {
    let mut running = running;
    let mut alt = alt;
    let mut armed = nonce.is_empty() || !running;
    let mut best: Option<String> = None;
    let mut i = 0;
    while i < data.len() {
        if data[i] != 0x1b {
            i += 1;
            continue;
        }
        if data.get(i + 1) == Some(&b'c') {
            alt = false;
            i += 2;
            continue;
        }
        if data.get(i + 1) == Some(&b'[') {
            if let Some((next, is_alt)) = scan_csi_alt(data, i) {
                if is_alt {
                    // h enters, l leaves. `scan_csi_alt` only says "this CSI
                    // is one of the three"; the final byte is the direction.
                    alt = data.get(next - 1) == Some(&b'h');
                }
                i = next;
                continue;
            }
        }
        if let Some(osc) = osc_at(data, i) {
            if !running && !alt && armed {
                let cwd = if osc.number == b"7" {
                    cwd_from_osc7(osc.payload)
                } else if osc.number == b"9" {
                    // The slice is this one sequence, so the "last 9;9" scan
                    // cannot see a later report.
                    scan_cwd(&data[i..osc.end])
                } else {
                    None
                };
                if let Some(cwd) = cwd {
                    best = Some(cwd);
                }
            }
            if osc.number == b"133" && osc133_trusted(osc.payload, nonce) {
                running = false;
                if osc.payload.first() == Some(&b'A') {
                    armed = true;
                }
            }
            i = osc.end;
            continue;
        }
        i += 1;
    }
    best
}

fn handle_osc133<T: EventListener>(
    t: &Term<T>,
    meta: &mut SessionMeta,
    kind: u8,
    exit: Option<i32>,
    trusted: bool,
    ev: &mut ChunkEvents,
) {
    match kind {
        b'A' => {
            // Prompt start. A BDSM/SCP from the command that just ended
            // must not survive onto this line, even when the A is spoofed.
            // Only a trusted A (the session nonce, or any A when the nonce
            // is empty) clears the command and records a mark.
            meta.bidi_mut().reset();
            if !trusted {
                return;
            }
            meta.running_cmd.clear();
            meta.clean_claude_after = None;
            meta.cmd_pending = false;
            // Back at a prompt, so nothing is running and any OSC 9;4 bar
            // still showing is stale — a program killed with Ctrl+C never
            // gets to send its own 9;4;0. An explicit report in this same
            // chunk wins, so a prompt followed by fresh progress is kept.
            if ev.progress.is_none() {
                ev.progress = Some(None);
            }
            meta.claude_from_abs = None;
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
            // Command finished. Same reset as A: `cat` of a file that
            // contains CSI 8 l must not leave the pane in explicit mode.
            // An untrusted D does not clear `running_cmd`, so the OSC 7
            // that a spoofed mark tries to authorize stays ignored.
            meta.bidi_mut().reset();
            if !trusted {
                return;
            }
            meta.running_cmd.clear();
            meta.clean_claude_after = None;
            meta.cmd_pending = false;
            // the command is over: its progress bar goes with it
            if ev.progress.is_none() {
                ev.progress = Some(None);
            }
            meta.claude_from_abs = None;
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
            meta.prompt_abs = Some(
                meta.evicted
                    + t.grid().history_size() as u64
                    + t.grid().cursor.point.line.0.max(0) as u64,
            );
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
    replies: &mut Vec<String>,
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
    // straight speedup, not a change in what gets measured. Queries are
    // split out so each one is answered from the mode the bytes before it
    // just established.
    advance_with_kitty_queries(parser, t, meta, bytes, replies);
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
    dst.clean_claude |= src.clean_claude;
    if src.clipboard.is_some() {
        dst.clipboard = src.clipboard;
    }
    if src.cwd.is_some() {
        dst.cwd = src.cwd;
    }
    if src.finished.is_some() {
        dst.finished = src.finished;
    }
    if src.notify.is_some() {
        dst.notify = src.notify;
    }
    // Last one wins, INCLUDING a clear: Some(None) is a real value here, so
    // this cannot test `is_some()` on the inner option.
    if src.progress.is_some() {
        dst.progress = src.progress;
    }
    // The BiDi filter splits one PTY read into several `Bytes` items. A
    // kitty `CSI ? u` answered in an earlier item must not be dropped when
    // a later item (the text after a stripped BDSM, for example) is merged
    // on top of it.
    dst.pty_replies.extend(src.pty_replies);
}

enum KittySeqKind {
    Push,
    Pop(u16),
    Set,
    Query,
}

struct KittySeq {
    end: usize,
    kind: KittySeqKind,
}

/// A complete `CSI <intermediate> <params> u` starting at `i`.
fn kitty_seq_at(data: &[u8], i: usize) -> Option<KittySeq> {
    if let Some(seq) = kitty_seq_strict(data, i) {
        return Some(seq);
    }
    // vte executes C0 inside a CSI and still completes it. `ESC [ ? NUL u`
    // is a query. The strict scanner misses the NUL, and with the protocol
    // off the parser will not answer, so this has to.
    let end = kitty_query_with_c0(data, i)?;
    Some(KittySeq {
        end,
        kind: KittySeqKind::Query,
    })
}

/// `0x00..=0x17`, `0x19`, `0x1C..=0x1F`. CAN, SUB and ESC abort the CSI.
fn c0_inside_csi(b: u8) -> bool {
    matches!(b, 0x00..=0x17 | 0x19 | 0x1C..=0x1F)
}

/// A `CSI ? u` query that may contain C0 bytes, the way vte still parses it.
fn kitty_query_with_c0(data: &[u8], i: usize) -> Option<usize> {
    if data.get(i) != Some(&0x1b) || data.get(i + 1) != Some(&b'[') {
        return None;
    }
    let mut j = i + 2;
    while j < data.len() && c0_inside_csi(data[j]) {
        j += 1;
    }
    if data.get(j) != Some(&b'?') {
        return None;
    }
    j += 1;
    while j < data.len() && (matches!(data[j], b'0'..=b'9' | b';') || c0_inside_csi(data[j])) {
        j += 1;
    }
    (data.get(j) == Some(&b'u')).then_some(j + 1)
}

fn kitty_seq_strict(data: &[u8], i: usize) -> Option<KittySeq> {
    if data.get(i) != Some(&0x1b) || data.get(i + 1) != Some(&b'[') {
        return None;
    }
    let inter = *data.get(i + 2)?;
    if !matches!(inter, b'<' | b'>' | b'=' | b'?') {
        return None;
    }
    let mut j = i + 3;
    let param_at = j;
    while j < data.len() && matches!(data[j], b'0'..=b'9' | b';') {
        j += 1;
    }
    if data.get(j) != Some(&b'u') {
        return None;
    }
    let params = &data[param_at..j];
    let kind = match inter {
        b'>' => KittySeqKind::Push,
        b'<' => KittySeqKind::Pop(kitty_pop_count(params)),
        b'=' => KittySeqKind::Set,
        _ => KittySeqKind::Query,
    };
    Some(KittySeq { end: j + 1, kind })
}

/// vte treats a missing or zero parameter as the default, which for pop is 1.
fn kitty_pop_count(params: &[u8]) -> u16 {
    let end = params
        .iter()
        .position(|&b| b == b';')
        .unwrap_or(params.len());
    if end == 0 {
        return 1;
    }
    match std::str::from_utf8(&params[..end])
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
    {
        Some(0) | None => 1,
        Some(n) => n,
    }
}

fn note_screen_swap<T: EventListener>(before_alt: bool, t: &Term<T>, meta: &mut SessionMeta) {
    if before_alt != t.mode().contains(TermMode::ALT_SCREEN) {
        std::mem::swap(&mut meta.kitty_depth, &mut meta.kitty_alt_depth);
    }
}

fn feed_plain<T: EventListener>(
    parser: &mut Processor,
    t: &mut Term<T>,
    meta: &mut SessionMeta,
    bytes: &[u8],
) {
    if bytes.is_empty() {
        return;
    }
    let before_alt = t.mode().contains(TermMode::ALT_SCREEN);
    parser.advance(t, bytes);
    note_screen_swap(before_alt, t, meta);
    // Same `ESC c` that [`process_chunk`] records in `saw_ris`. The parser
    // has just cleared both keyboard stacks. The counter has to follow, or
    // the next push is dropped because we still think we are at the cap.
    // It cannot watch the flag: that stays set until the UI reads it, and a
    // push in a later chunk must still count. `ESC c` is RIS even
    // mid-sequence: ESC aborts the CSI.
    if contains_ris(bytes) {
        meta.saw_ris = true;
        meta.kitty_depth = 0;
        meta.kitty_alt_depth = 0;
    }
}

/// RIS the way vte 0.15 sees it from Escape: `c` with no intermediates.
/// A second ESC restarts the sequence. C0 other than CAN/SUB is executed
/// and does not leave Escape. DEL and bytes >= 0x80 are ignored and stay,
/// so `ESC 0x80 c` is RIS.
fn contains_ris(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        i += 1;
        while i < bytes.len() {
            match bytes[i] {
                0x1b => break,
                0x18 | 0x1a => {
                    i += 1;
                    break;
                }
                0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f | 0x80..=0xff => i += 1,
                b'c' => return true,
                _ => {
                    i += 1;
                    break;
                }
            }
        }
    }
    false
}

/// `CSI =` updates `TermMode` and leaves the push/pop stack stale, which is
/// also what a later alt-screen swap restores. Pop the stale top and push
/// the mode bits now, so both stacks agree with the encoder before the next
/// `1049h`/`1049l`. A zero mode does not push onto an empty stack.
fn repair_keyboard_stack<T: EventListener>(
    parser: &mut Processor,
    t: &mut Term<T>,
    depth: &mut u16,
) {
    let flags = crate::keys::protocol_bits(*t.mode());
    parser.advance(t, b"\x1b[<u");
    *depth = depth.saturating_sub(1);
    let revealed = crate::keys::protocol_bits(*t.mode());
    if (flags != 0 || revealed != 0) && *depth < KITTY_STACK_CAP {
        let repair = format!("\x1b[>{flags}u");
        parser.advance(t, repair.as_bytes());
        *depth = depth.saturating_add(1);
    }
}

/// Drop keyboard-mode levels on the current screen only.
///
/// No-op when the protocol is off (the parser never recorded a stack) or
/// the counted depth is 0. A `1049h`/`1049l` visit of the other screen
/// clears the selection, clobbers DECSC, and wipes the alt grid on the way
/// back in — a prompt or Ctrl+Shift+K must not do that. The other screen's
/// stack stays where it is.
///
/// The pop count is the depth, not the flag bits. A stacked layer whose
/// mode is 0 sets no kitty bits, and skipping it would leave the level
/// under it in place. Flag bits from a sequence we did not count are not
/// a stack.
pub(crate) fn reset_keyboard_modes<T: EventListener>(
    parser: &mut Processor,
    t: &mut Term<T>,
    meta: &mut SessionMeta,
) {
    if !meta.kitty_protocol || meta.kitty_depth == 0 {
        return;
    }
    let pop = format!("\x1b[<{}u", meta.kitty_depth);
    parser.advance(t, pop.as_bytes());
    meta.kitty_depth = 0;
}

fn apply_kitty_seq<T: EventListener>(
    parser: &mut Processor,
    t: &mut Term<T>,
    meta: &mut SessionMeta,
    seq: &[u8],
    kind: KittySeqKind,
    replies: &mut Vec<String>,
) {
    if !meta.kitty_protocol {
        // The parser was built with kitty_keyboard false, so it would not
        // answer. The probe still gets the empty mode Rio reports.
        if matches!(kind, KittySeqKind::Query) {
            replies.push("\x1b[?0u".to_string());
        }
        return;
    }
    match kind {
        KittySeqKind::Push => {
            if meta.kitty_depth >= KITTY_STACK_CAP {
                return;
            }
            parser.advance(t, seq);
            meta.kitty_depth = meta.kitty_depth.saturating_add(1);
        }
        KittySeqKind::Pop(n) => {
            parser.advance(t, seq);
            meta.kitty_depth = meta.kitty_depth.saturating_sub(n);
        }
        KittySeqKind::Set => {
            parser.advance(t, seq);
            repair_keyboard_stack(parser, t, &mut meta.kitty_depth);
        }
        KittySeqKind::Query => {
            repair_keyboard_stack(parser, t, &mut meta.kitty_depth);
            parser.advance(t, seq);
        }
    }
}

/// Feed `bytes`, keeping the kitty stack in sync with what we actually let
/// the parser see.
fn advance_with_kitty_queries<T: EventListener>(
    parser: &mut Processor,
    t: &mut Term<T>,
    meta: &mut SessionMeta,
    bytes: &[u8],
    replies: &mut Vec<String>,
) {
    let mut i = 0;
    while i < bytes.len() {
        if let Some(seq) = kitty_seq_at(bytes, i) {
            apply_kitty_seq(parser, t, meta, &bytes[i..seq.end], seq.kind, replies);
            i = seq.end;
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len() {
            if bytes[j] == 0x1b && kitty_seq_at(bytes, j).is_some() {
                break;
            }
            j += 1;
        }
        feed_plain(parser, t, meta, &bytes[i..j]);
        i = j;
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
    if contains_ris(&buf) {
        meta.saw_ris = true;
    }
    ev.clipboard = scan_osc52(&buf);
    // OSC 7 and OSC 9;9 are trusted only at a shell prompt: no command
    // running, Enter not still pending, and not the alt screen. A report
    // that sits after a trusted OSC 133 A in this same slice is trusted
    // even if the command was still running when the slice started — that
    // is the prompt wrapper's own report. The later of the two trusted
    // reports wins. An untrusted or rejected one does not replace a
    // directory already stored.
    let nonce = meta.prompt_nonce.clone();
    let command_active = !meta.running_cmd.is_empty() || meta.cmd_pending;
    if let Some(cwd) = trusted_cwd(
        &buf,
        command_active,
        t.mode().contains(TermMode::ALT_SCREEN),
        &nonce,
    ) {
        meta.cwd = Some(cwd.clone());
        ev.cwd = Some(cwd);
    }
    ev.notify = scan_notify(&buf);
    ev.progress = scan_progress(&buf);
    let mut pos = 0;
    for (s, e, kind, exit, trusted) in find_osc133(&buf, &nonce) {
        feed_counted(parser, t, meta, &buf[pos..s], &mut ev.pty_replies);
        handle_osc133(t, meta, kind, exit, trusted, &mut ev);
        // A new prompt means the previous program is gone. A crashed TUI
        // never pops its keyboard flags; clear the current screen before
        // the shell types. Off, or an empty stack, this is a no-op. A
        // spoofed A (nonce mismatch) must not pop the stack.
        if kind == b'A' && trusted {
            reset_keyboard_modes(parser, t, meta);
        }
        pos = e;
    }
    feed_counted(parser, t, meta, &buf[pos..], &mut ev.pty_replies);
    if let Some(start_abs) = meta.clean_claude_after {
        let first = start_abs as i64 - meta.evicted as i64 - t.grid().history_size() as i64;
        if first < 0 {
            meta.clean_claude_after = None; // startup already scrolled out of view
        } else if t.mode().contains(TermMode::BRACKETED_PASTE) {
            let mut has_input = false;
            let mut has_rule = false;
            let mut has_banner = false;
            for line in first as i32..t.screen_lines() as i32 {
                let row = &t.grid()[alacritty_terminal::index::Line(line)];
                let text: String = (0..t.columns()).map(|c| row[Column(c)].c).collect();
                has_banner |= text.contains("Claude Code v");
                // --resume may paint the transcript and input directly,
                // without the startup version banner. Require its frame
                // as well as bracketed paste, and only inspect new rows.
                let trimmed = text.trim_start();
                has_input |= trimmed.starts_with(">\u{a0}") || trimmed.starts_with("❯\u{a0}");
                has_rule |= trimmed.starts_with("────────");
            }
            if has_banner || (has_input && has_rule) {
                meta.clean_claude_after = None;
                meta.claude_from_abs = Some(meta.evicted + t.grid().history_size() as u64);
                ev.clean_claude = true;
            }
        }
    }
    ev
}

/// One PTY read.
///
/// Order inside the loop is the stream order, and later PRs that touch this
/// function should keep it that way:
/// 1. `Bytes` are parsed first. OSC 133 A/D (inside `process_bytes`) reset
///    bidi. A CSI `?1049` / `?47` / `?1047` in the slice resets bidi even
///    when a second one in the same slice toggles the mode back, so a
///    start/end pair in one read is not invisible.
/// 2. `Bdsm` / `Scp` apply only after the bytes that preceded them, and only
///    while `running_cmd` is set or the alt screen is up. A sequence emitted
///    by `cat` therefore dies on OSC 133 D, and one that arrives at the
///    prompt is ignored.
/// 3. `Ris` resets bidi after the `ESC c` bytes, so alacritty's reset and
///    ours see the same event. `ESC ESC c`, `ESC <C0> c`, and `ESC` plus a
///    byte >= 0x80 then `c` are RIS too.
///
/// `merge_chunk` joins those `Bytes` items and extends `pty_replies`, so a
/// kitty `CSI ? u` answer is kept when the filter splits the read.
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
                if was_alt != t.mode().contains(TermMode::ALT_SCREEN)
                    || contains_alt_screen_csi(&bytes)
                {
                    meta.bidi_mut().reset();
                }
                merge_chunk(&mut ev, part);
            }
            crate::bidi_ctl::StreamItem::Bdsm(mode) => {
                if bidi_controls_live(t, meta) {
                    meta.bidi_mut().mode = mode;
                }
            }
            crate::bidi_ctl::StreamItem::Scp(dir) => {
                if bidi_controls_live(t, meta) {
                    let (abs, col) = crate::bidi_ctl::cursor_abs(t, meta.evicted);
                    meta.bidi_mut().note_scp(dir, abs, col);
                }
            }
            crate::bidi_ctl::StreamItem::Ris => {
                meta.saw_ris = true;
                meta.bidi_mut().reset();
            }
        }
    }
    ev
}

fn bidi_controls_live<T: EventListener>(t: &Term<T>, meta: &SessionMeta) -> bool {
    t.mode().contains(TermMode::ALT_SCREEN) || !meta.running_cmd.is_empty()
}

/// `CSI ?1049` / `?47` / `?1047` h or l anywhere in the slice.
pub(crate) fn contains_alt_screen_csi(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            if let Some((next, changed)) = scan_csi_alt(bytes, i) {
                if changed {
                    return true;
                }
                i = next;
                continue;
            }
        }
        i += 1;
    }
    false
}

/// `(index past the CSI, true when this CSI enters or leaves the alt screen)`.
fn scan_csi_alt(data: &[u8], i: usize) -> Option<(usize, bool)> {
    let mut j = i + 2;
    let private = data.get(j) == Some(&b'?');
    if private {
        j += 1;
    }
    let start = j;
    // `:` starts a subparameter. The mode number is the first one, so
    // `?1049:0h` is still 1049.
    while j < data.len() && matches!(data[j], b'0'..=b'9' | b';' | b':') {
        j += 1;
    }
    let final_b = *data.get(j)?;
    if !(0x40..=0x7e).contains(&final_b) {
        return None;
    }
    let next = j + 1;
    if !private || !matches!(final_b, b'h' | b'l') {
        return Some((next, false));
    }
    let hit = data[start..j].split(|b| *b == b';').any(|p| {
        let head = p.split(|b| *b == b':').next().unwrap_or(p);
        matches!(head, b"1049" | b"47" | b"1047")
    });
    Some((next, hit))
}

/// On Enter at a prompt, read the typed command straight off the screen
/// (prompt echo included, so history-recalled/edited lines are captured
/// correctly too — EasyTer's technique). Feeds cmd_is_claude.
pub(crate) fn capture_command_core<T: EventListener>(t: &Term<T>, meta: &mut SessionMeta) {
    if t.mode().contains(TermMode::ALT_SCREEN) || !meta.at_prompt {
        return;
    }
    // Every Enter at the prompt, before we know whether the command text
    // is readable. Paste and Enter in one write, Home then Enter, and a
    // wrapped line all leave the cursor at or before the prompt column, so
    // `running_cmd` stays empty. The nonce gate still has to close.
    meta.cmd_pending = true;
    let Some(pcol) = meta.prompt_col else {
        return;
    };
    meta.at_prompt = false;
    let cur = t.grid().cursor.point;
    let first = meta.prompt_abs.map_or(cur.line.0 as i64, |abs| {
        abs as i64 - meta.evicted as i64 - t.grid().history_size() as i64
    });
    if first < -(t.grid().history_size() as i64) || first > cur.line.0 as i64 {
        return;
    }
    let mut s = String::new();
    for line in first as i32..=cur.line.0 {
        let row = &t.grid()[alacritty_terminal::index::Line(line)];
        let start = if line == first as i32 { pcol } else { 0 };
        let end = if line == cur.line.0 {
            cur.column.0
        } else {
            t.columns()
        };
        for c in start..end {
            let cell = &row[Column(c)];
            if cell
                .flags
                .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER)
            {
                continue;
            }
            s.push(cell.c);
            if let Some(marks) = cell.zerowidth() {
                s.extend(marks);
            }
        }
    }
    let s = s.trim();
    if !s.is_empty() {
        meta.running_cmd = s.to_string();
        if crate::bidi::cmd_is_claude(s)
            && !s
                .split_whitespace()
                .any(|arg| matches!(arg, "-p" | "--print"))
        {
            meta.clean_claude_after =
                Some(meta.evicted + t.grid().history_size() as u64 + cur.line.0.max(0) as u64 + 1);
            meta.claude_from_abs = meta.clean_claude_after;
        }
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
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub meta: Arc<Mutex<SessionMeta>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// Command line this pane was spawned with. Drag-drop quoting follows it,
    /// not whatever the settings say now — the live shell is the one reading.
    shell: String,
    /// The reader thread's VTE parser. Shell exit resets keyboard modes
    /// through this one, so a sequence the child already started is the
    /// machine that sees the pop. A fresh `Processor` would leave this one
    /// mid-CSI and the next byte would not be text.
    parser: Arc<Mutex<Processor>>,
}

impl Session {
    pub fn spawn(
        size: TermSize,
        proxy: EventLoopProxy<UserEvent>,
        id: u64,
        start_cwd: Option<&str>,
        scrollback: usize,
        shell: &str,
        kitty_protocol: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let prompt_nonce = fresh_prompt_nonce();
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
        // what programs key their terminal features on: Claude Code, ripgrep,
        // delta and the rest read these before deciding what to emit
        cmd.env("TERM_PROGRAM", "Bayan");
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        cmd.env("COLORTERM", "truecolor");
        // requested dir (a restored tab, or "new tab inherits the cwd"),
        // else the launcher's directory, else home.
        // accept_local_cwd runs BEFORE any probe: a UNC path must not be
        // opened. usable_spawn_dir then rejects a remote drive and a reparse
        // point, and only then calls is_dir.
        let dir = start_cwd
            .and_then(accept_local_cwd)
            .filter(|d| usable_spawn_dir(d))
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
            setup.push_str(&shell_integration_ps(&prompt_nonce));
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
            // Off unless the user opted in. The flag gates every stack
            // mutation inside alacritty, including forms our scanner does
            // not recognize (`CSI > 1 NUL u`, a C0 before the intermediate,
            // a `:` sub-parameter). Queries while off are answered by
            // `process_chunk`, not by the parser.
            kitty_keyboard: kitty_protocol,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(
            config,
            &size,
            EventProxy(proxy.clone(), id),
        )));

        let meta = Arc::new(Mutex::new(SessionMeta {
            kitty_protocol,
            prompt_nonce,
            ..SessionMeta::default()
        }));
        let parser = Arc::new(Mutex::new(Processor::new()));
        let term2 = Arc::clone(&term);
        let meta2 = Arc::clone(&meta);
        let parser2 = Arc::clone(&parser);
        std::thread::spawn(move || {
            let mut carry: Vec<u8> = Vec::new();
            let mut buf = [0u8; PTY_READ];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let ev = {
                            // term, then meta, then parser. reset_keyboard
                            // on the UI thread takes the same order.
                            let mut t = term2.lock();
                            let mut m = meta2.lock().unwrap();
                            let mut p = parser2.lock().unwrap();
                            process_chunk(&mut p, &mut t, &mut m, &mut carry, &buf[..n])
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
                        if ev.clean_claude {
                            // Ask Claude to redraw its own screen (Ctrl+L),
                            // preserving its conversation and terminal state.
                            let _ = proxy.send_event(UserEvent::PtyWrite(id, "\x0c".into()));
                        }
                        let mut loop_gone = false;
                        for reply in ev.pty_replies {
                            if proxy.send_event(UserEvent::PtyWrite(id, reply)).is_err() {
                                loop_gone = true;
                                break;
                            }
                        }
                        if let Some((title, body)) = ev.notify {
                            let _ = proxy.send_event(UserEvent::Notify(id, title, body));
                        }
                        if let Some(p) = ev.progress {
                            let _ = proxy.send_event(UserEvent::Progress(id, p));
                        }
                        if loop_gone || proxy.send_event(UserEvent::Wakeup(id)).is_err() {
                            break; // the event loop is gone; still reset below
                        }
                    }
                }
            }
            // The shell is gone. Drop leaked kitty flags before the pane
            // closes, so a key event racing the exit cannot encode CSI u.
            // This is the session parser, not a new one: a CSI the child
            // already started has to be the machine that sees the pop.
            {
                let mut t = term2.lock();
                let mut m = meta2.lock().unwrap();
                let mut p = parser2.lock().unwrap();
                reset_keyboard_modes(&mut p, &mut t, &mut m);
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
            parser,
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

    /// Clear the current screen's keyboard-mode stack (Ctrl+Shift+K, a
    /// rebindable chord). Shell exit and `OSC 133;A` call
    /// [`reset_keyboard_modes`] on the reader thread, with this same parser.
    pub fn reset_keyboard(&mut self) {
        let mut t = self.term.lock();
        let mut m = self.meta.lock().unwrap();
        let mut parser = self.parser.lock().unwrap();
        reset_keyboard_modes(&mut parser, &mut t, &mut m);
    }

    pub fn write(&mut self, bytes: &[u8]) {
        // Enter at a prompt starts a command: read it off the screen now,
        // before the shell reacts (Claude mode auto-enable needs the name)
        if submits_a_command(bytes) {
            let t = self.term.lock();
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
        // Drop the term lock before meta. A resize reflows rows, so SCP
        // stamps no longer name the text they were written on.
        self.term.lock().resize(size);
        self.meta.lock().unwrap().bidi_mut().on_resize();
    }
}

/// Does this write submit the line at the prompt? A carriage return does —
/// unless it sits inside a bracketed paste, where the shell treats the whole
/// payload as typed text and runs nothing: capturing there read whatever was
/// on the prompt line as a "running command" (a spurious close-guard prompt,
/// or a stale name feeding Claude-mode detection).
pub(crate) fn submits_a_command(bytes: &[u8]) -> bool {
    bytes.contains(&b'\r') && !bytes.starts_with(b"\x1b[200~")
}

/// Why a paste or a drop is held for the confirm card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteRisk {
    /// CR, LF, CRLF, NEL, U+2028, or U+2029.
    Enter,
    /// A control, a C1 byte, or a bidi / zero-width spoof.
    Control,
    EnterAndControl,
    /// More than [`PASTE_GUARD_MAX_CHARS`] characters.
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

/// Character count above which a paste asks before it is sent. Unicode
/// scalars, not bytes. Plugin `send` strings are rejected at load past
/// this same limit.
pub const PASTE_GUARD_MAX_CHARS: usize = 2000;

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
/// Arabic is not "huge" because UTF-8 uses two bytes. The cap is
/// [`PASTE_GUARD_MAX_CHARS`].
///
/// Bracketed paste is intentionally not forced from our side. The guard runs
/// first either way; injecting `\x1b[?2004h` into a host that does not
/// understand the wrappers would type the wrappers into the prompt.
///
/// Plugin palette `send` strings are rejected at load when this returns
/// `Some`. A row that still carries one is armed through the same confirm
/// path as a clipboard paste.
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
    if saw_break || saw_control || chars > PASTE_GUARD_MAX_CHARS {
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
    fn a_panic_holding_the_term_lock_does_not_poison_it() {
        // Session::term is a FairMutex for this: a std Mutex would hand
        // every later lock a PoisonError, and the window would die with
        // the reader thread.
        let term = Arc::new(FairMutex::new(0u32));
        let held = Arc::clone(&term);
        let died = std::thread::spawn(move || {
            let mut t = held.lock();
            *t = 7;
            panic!("reader thread dies holding the lock");
        })
        .join();
        assert!(died.is_err());
        assert_eq!(*term.lock(), 7);
    }

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
    fn a_bracketed_paste_does_not_count_as_submitting_a_command() {
        assert!(submits_a_command(b"ls\r"));
        assert!(submits_a_command(b"\r"));
        assert!(!submits_a_command(b"abc"));
        // the wrapped payload of normalize_paste(_, true) carries \r inside
        let wrapped = normalize_paste("a\nb\n", true);
        assert!(wrapped.contains('\r'));
        assert!(!submits_a_command(wrapped.as_bytes()));
        // an UNbracketed multi-line paste really does run lines: still a submit
        assert!(submits_a_command(
            normalize_paste("a\nb\n", false).as_bytes()
        ));
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
        assert_eq!(
            needs_paste_guard(&huge).map(|g| g.risk),
            Some(PasteRisk::TooLong)
        );
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
        assert_eq!(
            needs_paste_guard("a\u{200D}b\u{200C}c"),
            None,
            "ZWJ and ZWNJ stay"
        );
        assert_eq!(
            normalize_paste("a\u{200D}b\u{200C}c", false),
            "a\u{200D}b\u{200C}c"
        );
    }

    /// Arabic copied from the web is full of LRM, RLM, and ALM. Those marks
    /// paste straight through. An override still stops for the card.
    #[test]
    fn arabic_with_lrm_and_rlm_pastes_without_a_card() {
        let text = "السلام\u{200E} عليكم\u{200F} و\u{061C}رحمة";
        assert_eq!(needs_paste_guard(text), None);
        assert_eq!(normalize_paste(text, false), text);
        for (c, label) in [
            ('\u{200E}', "<LRM>"),
            ('\u{200F}', "<RLM>"),
            ('\u{061C}', "<ALM>"),
        ] {
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
            dropped_path_arg(
                Path::new(r"C:\tmp\a`ncalc.txt"),
                "powershell.exe -NoProfile"
            )
            .as_deref(),
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
            dropped_path_arg(
                Path::new(r"C:\tmp\a&calc.exe"),
                r"C:\Windows\System32\cmd.exe"
            )
            .as_deref(),
            Some(r#""C:\tmp\a&calc.exe" "#)
        );
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\tmp\%COMSPEC%.txt"), "cmd.exe"),
            None
        );
        assert_eq!(
            dropped_path_arg(Path::new(r"C:\tmp\!PATH!.txt"), "cmd.exe"),
            None
        );
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
        assert_eq!(
            dropped_path_arg(Path::new("C:\\tmp\\a\nb.txt"), "powershell.exe"),
            None
        );
        assert_eq!(
            dropped_path_arg(Path::new("C:\\tmp\\a\rb.txt"), "cmd.exe"),
            None
        );
        assert_eq!(
            dropped_path_arg(Path::new("C:\\tmp\\a\rb.txt"), "wsl.exe -d Ubuntu"),
            None
        );
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
        assert_eq!(
            shell_family(r"C:\Program Files\Git\git-bash.exe"),
            ShellFamily::Posix
        );
        assert_eq!(shell_family("notbash.exe"), ShellFamily::PowerShell);
        assert_eq!(
            shell_family(r"C:\Windows\System32\cmd.exe"),
            ShellFamily::Cmd
        );
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
        assert!(
            drop_needs_confirm(true, true, "safe"),
            "alt screen, even at a prompt mark"
        );
        assert!(drop_needs_confirm(false, false, "safe"), "no OSC 133 B");
        assert!(
            !drop_needs_confirm(false, true, r"'C:\safe.txt' "),
            "cooked prompt types immediately"
        );
        // RLO / ZWSP in the name would be stripped on the way to the shell
        // with no card. A prompt does not make that silent.
        let rlo = dropped_path_arg(Path::new("C:\\d\\a\u{202E}b.txt"), "powershell.exe").unwrap();
        assert!(
            drop_needs_confirm(false, true, &rlo),
            "RLO at the prompt still confirms"
        );
        let zwsp = dropped_path_arg(Path::new("C:\\d\\a\u{200B}b.txt"), "cmd.exe").unwrap();
        assert!(drop_needs_confirm(false, true, &zwsp));
        let zwj = dropped_path_arg(Path::new("C:\\d\\a\u{200D}b.txt"), "powershell.exe").unwrap();
        assert!(
            !drop_needs_confirm(false, true, &zwj),
            "ZWJ is part of the name"
        );
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
        assert_eq!(
            c,
            crate::shells::detect()
                .into_iter()
                .map(|s| s.command)
                .collect::<Vec<_>>()
        );
        let mut sorted = c.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            c.len(),
            "cycling must not repeat a shell: {c:?}"
        );
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
            assert!(
                !out.contains("$env:") && !out.contains('%'),
                "leftover: {out}"
            );
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
        assert!(is_under(
            r"C:\WINDOWS\System32\WindowsPowerShell",
            r"c:\Windows"
        ));
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
            (
                r"C:\PROGRA~1\Git\usr\bin\bash.exe".into(),
                vec!["--login".to_string(), "-i".to_string()]
            )
        );
        assert_eq!(
            split_command("wsl.exe -d Ubuntu"),
            (
                "wsl.exe".into(),
                vec!["-d".to_string(), "Ubuntu".to_string()]
            )
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
            assert!(
                prog.to_lowercase().ends_with(".exe"),
                "bad program in {c:?}"
            );
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
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1bc\x1b[?1004h");
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

    #[test]
    fn command_capture_keeps_wrapped_claude_commands() {
        let (mut t, mut p, mut meta, mut carry) = test_term();
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"PS C:\\a-very-long-directory> \x1b]133;B\x07",
        );
        let command = format!("claude --safe-mode {}", "x".repeat(90));
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, command.as_bytes());
        capture_command_core(&t, &mut meta);
        assert_eq!(meta.running_cmd, command);
        assert!(crate::bidi::cmd_is_claude(&meta.running_cmd));
    }

    #[test]
    fn interactive_claude_cleans_shell_prefix_once_after_new_header() {
        let (mut t, mut p, mut meta, mut carry) = test_term();
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"Claude Code vOLD\r\n> \x1b]133;B\x07claude",
        );
        capture_command_core(&t, &mut meta);
        let ev = process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\r\n\x1b[?2004hloading",
        );
        assert!(!ev.clean_claude, "an old header must not trigger cleanup");
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\r\nClaude Co");
        assert!(!ev.clean_claude);
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"de v2.1.311");
        assert!(ev.clean_claude);
        let ev = process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\r\nClaude Code v2.1.311",
        );
        assert!(!ev.clean_claude, "cleanup must never loop on redraw");

        let (mut t, mut p, mut meta, mut carry) = test_term();
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"> \x1b]133;B\x07claude --print hello",
        );
        capture_command_core(&t, &mut meta);
        assert!(meta.clean_claude_after.is_none());
    }

    #[test]
    fn resumed_claude_cleans_shell_prefix_without_a_startup_header() {
        let (mut t, mut p, mut meta, mut carry) = test_term();
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"PS> \x1b]133;B\x07claude --resume test-session",
        );
        capture_command_core(&t, &mut meta);
        let ev = process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            "\r\n\x1b[?2004hResumed conversation\r\n──────────\r\n>\u{a0}".as_bytes(),
        );
        assert!(
            ev.clean_claude,
            "a resumed input frame must not require a version banner"
        );
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b" redraw");
        assert!(!ev.clean_claude, "redraw is requested only once");
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
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b[?1049l\x1b]133;D;0\x07",
        );
        assert!(!t.mode().contains(TermMode::ALT_SCREEN));
        assert!(meta.running_cmd.is_empty());
    }

    /// The eviction counter (EasyTer's dropped counter, alacritty edition):
    /// flood far past a tiny scrollback cap and verify marks stay anchored.
    #[test]
    fn marks_survive_scrollback_eviction() {
        let size = TermSize { cols: 20, rows: 4 };
        let config = Config {
            scrolling_history: 8,
            ..Config::default()
        };
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
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
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
        assert_eq!(
            scan_osc52(b"\x1b]52;c;aGVsbG8=\x07").as_deref(),
            Some("hello")
        );
        assert_eq!(
            scan_osc52(b"\x1b]52;c;aGVsbG8=\x1b\\").as_deref(),
            Some("hello")
        );
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
        big.extend(
            base64::engine::general_purpose::STANDARD
                .encode(vec![b'A'; OSC52_MAX_BYTES + 1])
                .as_bytes(),
        );
        big.push(0x07);
        assert_eq!(scan_osc52(&big), None);
        let mut at_cap = b"\x1b]52;c;".to_vec();
        at_cap.extend(
            base64::engine::general_purpose::STANDARD
                .encode(vec![b'B'; OSC52_MAX_BYTES])
                .as_bytes(),
        );
        at_cap.push(0x07);
        assert_eq!(scan_osc52(&at_cap).map(|s| s.len()), Some(OSC52_MAX_BYTES));
        // longer than the carry window, still unterminated: not a write
        let mut carried = b"\x1b]52;c;".to_vec();
        carried.extend(std::iter::repeat_n(b'A', 5000));
        assert_eq!(scan_osc52(&carried), None);

        assert!(!allow_osc52_write(false, true, true), "default is off");
        assert!(!allow_osc52_write(false, false, true));
        assert!(
            !allow_osc52_write(true, false, true),
            "unfocused opt-in still does not write"
        );
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
            r"C:\CON",
            r"C:\con.txt",
            r"C:\foo\NUL",
            r"C:\PRN.",
            r"C:\AUX ",
            r"C:\COM1",
            r"C:\foo\COM1.dat",
            r"C:\LPT9",
            r"C:\lpt1.txt",
            r"C:\foo:bar",
            r"C:\foo::$DATA",
            "C:\\a\u{202e}b",
            "C:\\a\u{200e}",
            "C:\\a\u{202b}b",
        ] {
            assert_eq!(accept_local_cwd(bad), None, "accepted {bad:?}");
        }
        assert_eq!(accept_local_cwd(r"C:\COM10").as_deref(), Some(r"C:\COM10"));
        assert_eq!(
            accept_local_cwd(r"C:\config").as_deref(),
            Some(r"C:\config")
        );
        assert_eq!(
            accept_local_cwd(r"C:\Users\Admin\Bayan\").as_deref(),
            Some(r"C:\Users\Admin\Bayan")
        );
        assert_eq!(
            accept_local_cwd("C:/Users/Admin").as_deref(),
            Some(r"C:\Users\Admin")
        );
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
        assert_eq!(scan_cwd(&osc(r"C:\CON")), None, "DOS device via 9;9");
        assert_eq!(scan_cwd(&osc(r"C:\foo::$DATA")), None, "ADS via 9;9");
        assert_eq!(scan_cwd(&osc("C:\\a\u{202e}b")), None, "bidi via 9;9");

        // a good report survives a later UNC one: nothing to probe, nothing to store
        let (mut t, mut p, mut meta, mut carry) = test_term();
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, &osc(r"C:\work"));
        assert_eq!(ev.cwd.as_deref(), Some(r"C:\work"));
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, &osc(r"\\evil\share"));
        assert!(ev.cwd.is_none());
        assert_eq!(meta.cwd.as_deref(), Some(r"C:\work"));
    }

    /// OSC 7 and OSC 9;9 from a running command, or from the alt screen, do
    /// not replace the cwd. The prompt wrapper's report follows OSC 133 D
    /// in the same read and is still trusted.
    #[test]
    fn cwd_reports_only_at_the_prompt() {
        let osc9 = |p: &str| {
            let mut v = b"\x1b]9;9;".to_vec();
            v.extend_from_slice(p.as_bytes());
            v.push(0x07);
            v
        };
        let osc7 = |url: &str| {
            let mut v = b"\x1b]7;".to_vec();
            v.extend_from_slice(url.as_bytes());
            v.push(0x07);
            v
        };

        let (mut t, mut p, mut meta, mut carry) = test_term();
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, &osc9(r"C:\work"));
        assert_eq!(ev.cwd.as_deref(), Some(r"C:\work"));

        meta.running_cmd = "cat".into();
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, &osc9(r"C:\evil"));
        assert!(ev.cwd.is_none());
        let ev = process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            &osc7("file:///C:/evil"),
        );
        assert!(ev.cwd.is_none());
        assert_eq!(meta.cwd.as_deref(), Some(r"C:\work"));

        // Prompt order: D, A, 9;9, 7. The command was running at the first
        // byte. The report after D is the one that sticks, and 7 wins over 9;9.
        let mut prompt = b"\x1b]133;D;0\x07\x1b]133;A\x07".to_vec();
        prompt.extend(osc9(r"C:\from9"));
        prompt.extend(osc7("file:///C:/from7"));
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, &prompt);
        assert_eq!(ev.cwd.as_deref(), Some(r"C:\from7"));
        assert_eq!(meta.cwd.as_deref(), Some(r"C:\from7"));
        assert!(meta.running_cmd.is_empty());

        // A report that sits before D in the same slice is not stored when
        // nothing trusted follows it.
        meta.running_cmd = "cat".into();
        let mut early = osc9(r"C:\early");
        early.extend_from_slice(b"\x1b]133;D;0\x07");
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, &early);
        assert!(ev.cwd.is_none());
        assert_eq!(meta.cwd.as_deref(), Some(r"C:\from7"));

        // Alt screen rejects both reports. Leaving it in the same slice
        // trusts a report that comes after the leave.
        let (mut t, mut p, mut meta, mut carry) = test_term();
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?1049h");
        assert!(t.mode().contains(TermMode::ALT_SCREEN));
        let ev = process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            &osc7("file:///C:/alt"),
        );
        assert!(ev.cwd.is_none());
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, &osc9(r"C:\alt"));
        assert!(ev.cwd.is_none());
        assert!(meta.cwd.is_none());

        let mut leave = b"\x1b[?1049l".to_vec();
        leave.extend(osc7("file:///D:/after"));
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, &leave);
        assert!(!t.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(ev.cwd.as_deref(), Some(r"D:\after"));
    }

    /// Comma, quote, a `\\` anywhere, an ADS colon, and `..` that escapes
    /// the drive are not a cwd and not an `explorer /select` argument.
    #[test]
    fn local_cwd_rejects_select_injection_and_ads() {
        for bad in [
            r"C:\x,/root,\\evil\share",
            r"C:\foo\\bar",
            "C:/foo//bar",
            "C:\\foo\"bar",
            r"C:\..\..\evil",
            r"C:\a.txt:evil.exe",
            r"C:\evil.exe::$DATA",
            r"C:\work\..\..\..\Windows",
        ] {
            assert_eq!(accept_local_cwd(bad), None, "accepted {bad:?}");
        }
        assert_eq!(accept_local_cwd(r"C:\a\..\b").as_deref(), Some(r"C:\b"));
        assert_eq!(accept_local_cwd(r"C:\a\.\b").as_deref(), Some(r"C:\a\b"));
        assert_eq!(accept_local_cwd(r"  C:\src  ").as_deref(), Some(r"C:\src"));
        assert_eq!(
            accept_local_cwd(r"C:\work\src\..\lib.rs").as_deref(),
            Some(r"C:\work\lib.rs")
        );
    }

    /// COM0/LPT0, superscript digits, the `$` devices, and a space before the
    /// extension. `COM10` stays a normal directory. A device component is
    /// rejected even when `..` would have popped it.
    #[test]
    fn local_cwd_rejects_spaced_superscript_and_dollar_devices() {
        let superscripts = [
            '\u{2070}', '\u{00B9}', '\u{00B2}', '\u{00B3}', '\u{2074}', '\u{2075}', '\u{2076}',
            '\u{2077}', '\u{2078}', '\u{2079}',
        ];
        for (i, digit) in superscripts.iter().enumerate() {
            let com = format!("C:\\COM{digit}");
            let lpt = format!("C:\\lpt{digit}.txt");
            assert_eq!(accept_local_cwd(&com), None, "COM superscript {i} in {com}");
            assert_eq!(accept_local_cwd(&lpt), None, "LPT superscript {i} in {lpt}");
        }
        for bad in [
            r"C:\COM0",
            r"C:\LPT0",
            r"C:\com0.txt",
            r"C:\CON .txt",
            r"C:\con .TXT",
            r"C:\CONIN$",
            r"C:\CONOUT$",
            r"C:\CLOCK$",
            r"C:\foo\CONIN$.txt",
            r"C:\CON\..\Users",
        ] {
            assert_eq!(accept_local_cwd(bad), None, "accepted {bad:?}");
        }
        assert_eq!(accept_local_cwd(r"C:\COM10").as_deref(), Some(r"C:\COM10"));
        assert_eq!(accept_local_cwd(r"C:\LPT10").as_deref(), Some(r"C:\LPT10"));
    }

    #[test]
    fn progress_reports_parse_every_state_and_the_last_one_wins() {
        assert_eq!(
            parse_progress(b"1;42"),
            Some(Some(Progress {
                state: ProgressState::Normal,
                percent: 42
            }))
        );
        assert_eq!(
            parse_progress(b"2;7"),
            Some(Some(Progress {
                state: ProgressState::Error,
                percent: 7
            }))
        );
        assert_eq!(
            parse_progress(b"3"),
            Some(Some(Progress {
                state: ProgressState::Indeterminate,
                percent: 0
            }))
        );
        assert_eq!(
            parse_progress(b"4;50"),
            Some(Some(Progress {
                state: ProgressState::Paused,
                percent: 50
            }))
        );
        assert_eq!(parse_progress(b"0"), Some(None), "state 0 clears");
        assert_eq!(
            parse_progress(b"1;250").unwrap().unwrap().percent,
            100,
            "clamped"
        );
        assert_eq!(parse_progress(b"9;1"), None, "unknown state ignored");
        assert_eq!(parse_progress(b"x"), None);
        // a burst collapses to its final value, through the real chunk path
        let (mut t, mut p, mut meta, mut carry) = test_term();
        let ev = process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]9;4;1;10\x07work\x1b]9;4;1;90\x07",
        );
        assert_eq!(ev.progress.unwrap().unwrap().percent, 90);
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]9;4;0;0\x07");
        assert_eq!(ev.progress, Some(None));
        // the cwd report shares the OSC 9 prefix and must not be mistaken
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]9;9;C:\\x\x07");
        assert_eq!(ev.progress, None);
        assert_eq!(ev.notify, None);
    }

    /// A program killed mid-report never sends its own 9;4;0, so the bar
    /// would sit on the tab for the rest of the session. Coming back to a
    /// prompt means nothing is running, which is the same information.
    #[test]
    fn a_returning_prompt_clears_a_stale_progress_bar() {
        let (mut t, mut p, mut meta, mut carry) = test_term();
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]9;4;1;40\x07");
        assert_eq!(ev.progress.unwrap().unwrap().percent, 40);
        // Ctrl+C: the shell prints a fresh prompt, the program said nothing
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]133;A\x07PS> ");
        assert_eq!(ev.progress, Some(None), "a returning prompt must clear it");
        // a finished command does the same
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]133;D;0\x07");
        assert_eq!(ev.progress, Some(None));
        // but a report in the SAME chunk is live, not stale: it wins
        let ev = process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]133;A\x07\x1b]9;4;1;70\x07",
        );
        assert_eq!(ev.progress.unwrap().unwrap().percent, 70);
        // and an ordinary chunk still reports nothing either way
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"just output\r\n");
        assert_eq!(ev.progress, None);
    }

    #[test]
    fn notification_sequences_in_three_dialects() {
        // iTerm2 / ConEmu message form
        assert_eq!(
            scan_notify(b"\x1b]9;Build done\x07"),
            Some(("Bayan".into(), "Build done".into()))
        );
        // ConEmu subcommands are not messages
        assert_eq!(scan_notify(b"\x1b]9;4;1;50\x07"), None);
        assert_eq!(scan_notify(b"\x1b]9;9;C:\\Users\x07"), None);
        // rxvt-unicode form (Ghostty/WezTerm), ESC-backslash terminated
        assert_eq!(
            scan_notify(b"\x1b]777;notify;Claude;needs your approval\x1b\\"),
            Some(("Claude".into(), "needs your approval".into()))
        );
        // kitty form: title chunk and body chunk
        assert_eq!(
            scan_notify(b"\x1b]99;i=1:p=title;Tests\x07"),
            Some(("Tests".into(), String::new()))
        );
        assert_eq!(
            scan_notify(b"\x1b]99;i=1:d=1;all green\x07"),
            Some(("Bayan".into(), "all green".into()))
        );
        // several in one read: the LAST one is what the user sees
        assert_eq!(
            scan_notify(b"\x1b]9;first\x07 x \x1b]777;notify;T;second\x07"),
            Some(("T".into(), "second".into()))
        );
        assert_eq!(scan_notify(b"plain text"), None);
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

    struct PtyLog(Arc<Mutex<Vec<String>>>);

    impl EventListener for PtyLog {
        fn send_event(&self, event: Event) {
            if let Event::PtyWrite(text) = event {
                self.0.lock().unwrap().push(text);
            }
        }
    }

    struct KittyFixture {
        term: Term<PtyLog>,
        parser: Processor,
        meta: SessionMeta,
        carry: Vec<u8>,
        log: Arc<Mutex<Vec<String>>>,
    }

    fn kitty_term(protocol: bool) -> KittyFixture {
        let log = Arc::new(Mutex::new(Vec::new()));
        let size = TermSize { cols: 40, rows: 6 };
        // Off means the parser never touches its keyboard stack. The probe
        // reply is synthesized by process_chunk, not by alacritty.
        let config = Config {
            kitty_keyboard: protocol,
            ..Config::default()
        };
        KittyFixture {
            term: Term::new(config, &size, PtyLog(Arc::clone(&log))),
            parser: Processor::new(),
            meta: SessionMeta {
                kitty_protocol: protocol,
                ..SessionMeta::default()
            },
            carry: Vec::new(),
            log,
        }
    }

    fn take_writes(log: &Mutex<Vec<String>>) -> Vec<String> {
        std::mem::take(&mut *log.lock().unwrap())
    }

    #[test]
    fn kitty_push_pop_set_and_query() {
        let KittyFixture {
            term: mut t,
            parser: mut p,
            mut meta,
            mut carry,
            log,
        } = kitty_term(true);
        let kitty = TermMode::KITTY_KEYBOARD_PROTOCOL;

        // a probe before any push reports 0 and must not invent a stack level
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?u");
        assert_eq!(take_writes(&log), vec!["\x1b[?0u".to_string()]);
        assert!(!t.mode().intersects(kitty));

        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[>1u");
        assert!(t.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
        let other_flags = kitty.difference(TermMode::DISAMBIGUATE_ESC_CODES);
        assert!(!t.mode().intersects(other_flags));

        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[>31u\x1b[?u");
        assert_eq!(take_writes(&log), vec!["\x1b[?31u".to_string()]);
        assert!(t.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
        assert!(t.mode().contains(TermMode::REPORT_ASSOCIATED_TEXT));

        // pop restores the pushed 1, not zero — and not a phantom from the probe
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[<u\x1b[?u");
        assert_eq!(take_writes(&log), vec!["\x1b[?1u".to_string()]);

        // CSI = updates flags in place: union alternate (4) onto 1, then
        // clear disambiguate (bit 1) with mode 3. The query must report the
        // mode bits, which stock 0.26 would not — it answers from the stack.
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[=4;2u\x1b[?u");
        assert_eq!(take_writes(&log), vec!["\x1b[?5u".to_string()]);
        assert!(t.mode().contains(TermMode::REPORT_ALTERNATE_KEYS));
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[=1;3u\x1b[?u");
        assert_eq!(take_writes(&log), vec!["\x1b[?4u".to_string()]);
        assert!(!t.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));

        // clearing the last bit with CSI = must answer 0, not the stale push
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[=0u\x1b[?u");
        assert_eq!(take_writes(&log), vec!["\x1b[?0u".to_string()]);
        assert!(!t.mode().intersects(kitty));

        // rebuild a primary level, then check the alternate screen's own stack
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[>4u");
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?1049h\x1b[?u");
        assert!(t.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(take_writes(&log), vec!["\x1b[?0u".to_string()]);
        assert!(!t.mode().intersects(kitty));
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b[>8u\x1b[?1049l\x1b[?u",
        );
        assert!(!t.mode().contains(TermMode::ALT_SCREEN));
        assert!(t.mode().contains(TermMode::REPORT_ALTERNATE_KEYS));
        assert_eq!(take_writes(&log), vec!["\x1b[?4u".to_string()]);
    }

    #[test]
    fn kitty_protocol_off_ignores_push_and_answers_zero() {
        let KittyFixture {
            term: mut t,
            parser: mut p,
            mut meta,
            mut carry,
            log,
        } = kitty_term(false);
        let ev = process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b[>31u\x1b[=1u\x1b[<u\x1b[?u",
        );
        // We answer. The parser's kitty_keyboard flag is false, so it does not.
        assert!(take_writes(&log).is_empty());
        assert_eq!(ev.pty_replies, vec!["\x1b[?0u".to_string()]);
        assert!(!t.mode().intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));
        // a second probe is still zero — nothing stuck from the dropped push
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?u");
        assert_eq!(ev.pty_replies, vec!["\x1b[?0u".to_string()]);
        assert!(take_writes(&log).is_empty());
    }

    /// The BiDi filter emits one `Bytes` item for the query and another for
    /// the text after the stripped BDSM. `merge_chunk` has to keep both
    /// replies, or the `CSI ?0u` answer never reaches the PTY.
    #[test]
    fn kitty_query_reply_survives_merged_chunks() {
        let KittyFixture {
            term: mut t,
            parser: mut p,
            mut meta,
            mut carry,
            log,
        } = kitty_term(false);
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?u\x1b[8lNEXT");
        assert_eq!(ev.pty_replies, vec!["\x1b[?0u".to_string()]);
        assert!(take_writes(&log).is_empty());
        let row = &t.grid()[alacritty_terminal::index::Line(0)];
        assert_eq!(row[Column(0)].c, 'N');
        assert_eq!(row[Column(1)].c, 'E');
        assert_eq!(row[Column(2)].c, 'X');
        assert_eq!(row[Column(3)].c, 'T');

        let ev = process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b[?u\x1b[8l\x1b[?u",
        );
        assert_eq!(
            ev.pty_replies,
            vec!["\x1b[?0u".to_string(), "\x1b[?0u".to_string()]
        );
    }

    /// A `133;D` that does not carry the session nonce must not authorize
    /// the OSC 7 after it. The real prompt (D and A, both with the nonce)
    /// still stores the directory. Idle OSC 7, the clink case, does too.
    #[test]
    fn spoofed_133_d_does_not_accept_osc7_without_the_nonce() {
        let osc7 = |url: &str| {
            let mut v = b"\x1b]7;".to_vec();
            v.extend_from_slice(url.as_bytes());
            v.push(0x07);
            v
        };
        let (mut t, mut p, mut meta, mut carry) = test_term();
        meta.prompt_nonce = "n0nce".into();
        let ev = process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            &osc7("file:///C:/work"),
        );
        assert_eq!(
            ev.cwd.as_deref(),
            Some(r"C:\work"),
            "idle OSC 7 needs no 133"
        );

        meta.running_cmd = "cat".into();
        meta.bidi_mut().mode = crate::bidi_ctl::BidiMode::Explicit;
        let mut spoof = b"\x1b]133;D;0\x07".to_vec();
        spoof.extend(osc7("file:///C:/evil"));
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, &spoof);
        assert!(ev.cwd.is_none());
        assert_eq!(meta.cwd.as_deref(), Some(r"C:\work"));
        assert_eq!(
            meta.running_cmd, "cat",
            "a spoofed D must not clear the command"
        );
        assert_eq!(
            meta.bidi.mode,
            crate::bidi_ctl::BidiMode::Implicit,
            "bidi still resets on any D"
        );

        let mut both = b"\x1b]133;D;0\x07\x1b]133;A\x07".to_vec();
        both.extend(osc7("file:///C:/evil"));
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, &both);
        assert!(ev.cwd.is_none());
        assert_eq!(meta.running_cmd, "cat");

        // D with the nonce ends the command but does not arm the report.
        let mut only_d = b"\x1b]133;D;0;n0nce\x07".to_vec();
        only_d.extend(osc7("file:///C:/afterd"));
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, &only_d);
        assert!(ev.cwd.is_none());
        assert!(meta.running_cmd.is_empty());
        assert_eq!(meta.cwd.as_deref(), Some(r"C:\work"));

        meta.running_cmd = "cat".into();
        let mut prompt = b"\x1b]133;D;0;n0nce\x07\x1b]133;A;n0nce\x07".to_vec();
        prompt.extend(osc7("file:///C:/from7"));
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, &prompt);
        assert_eq!(ev.cwd.as_deref(), Some(r"C:\from7"));
        assert!(meta.running_cmd.is_empty());
    }

    fn osc7_of(url: &str) -> Vec<u8> {
        let mut v = b"\x1b]7;".to_vec();
        v.extend_from_slice(url.as_bytes());
        v.push(0x07);
        v
    }

    /// Prompt showing, nonce set, cwd already `C:\work` from an idle OSC 7.
    fn prompt_ready() -> (Term<VoidListener>, Processor, SessionMeta, Vec<u8>) {
        let (mut t, mut p, mut meta, mut carry) = test_term();
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"PS> \x1b]133;B\x07");
        assert!(meta.at_prompt);
        assert_eq!(meta.prompt_col, Some(4));
        meta.prompt_nonce = "n0nce".into();
        let ev = process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            &osc7_of("file:///C:/work"),
        );
        assert_eq!(ev.cwd.as_deref(), Some(r"C:\work"));
        (t, p, meta, carry)
    }

    /// Enter already happened and `running_cmd` is empty. OSC 7 from the
    /// command is refused until a trusted 133 A. A spoofed D does not clear
    /// `cmd_pending`.
    fn assert_pending_blocks_osc7(
        t: &mut Term<VoidListener>,
        p: &mut Processor,
        meta: &mut SessionMeta,
        carry: &mut Vec<u8>,
        expected_command: &str,
    ) {
        assert_eq!(
            meta.running_cmd, expected_command,
            "command capture must match this scenario"
        );
        assert!(
            meta.cmd_pending,
            "Enter at the prompt must arm the nonce gate"
        );
        let ev = process_chunk(p, t, meta, carry, &osc7_of("file:///C:/evil"));
        assert!(ev.cwd.is_none());
        assert_eq!(meta.cwd.as_deref(), Some(r"C:\work"));

        let mut spoof = b"\x1b]133;D;0\x07".to_vec();
        spoof.extend(osc7_of("file:///C:/spoof"));
        let ev = process_chunk(p, t, meta, carry, &spoof);
        assert!(ev.cwd.is_none());
        assert!(
            meta.cmd_pending,
            "a spoofed D must not clear the pending command"
        );
        assert_eq!(meta.cwd.as_deref(), Some(r"C:\work"));

        let mut prompt = b"\x1b]133;D;0;n0nce\x07\x1b]133;A;n0nce\x07".to_vec();
        prompt.extend(osc7_of("file:///C:/from7"));
        let ev = process_chunk(p, t, meta, carry, &prompt);
        assert_eq!(ev.cwd.as_deref(), Some(r"C:\from7"));
        assert!(!meta.cmd_pending);
        assert!(meta.running_cmd.is_empty());
    }

    /// Paste and Enter arrive together. The shell has not echoed the paste,
    /// so the cursor is still on the prompt column and `running_cmd` stays
    /// empty.
    #[test]
    fn paste_and_enter_in_one_chunk_still_requires_the_osc7_nonce() {
        let (mut t, mut p, mut meta, mut carry) = prompt_ready();
        assert_eq!(t.grid().cursor.point.column.0, meta.prompt_col.unwrap());
        capture_command_core(&t, &mut meta);
        assert_pending_blocks_osc7(&mut t, &mut p, &mut meta, &mut carry, "");
    }

    /// Home, then Enter. The cursor is at column 0, which is not past the
    /// prompt, so the typed line is not read.
    #[test]
    fn home_then_enter_still_requires_the_osc7_nonce() {
        let (mut t, mut p, mut meta, mut carry) = prompt_ready();
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[H");
        assert_eq!(t.grid().cursor.point.column.0, 0);
        capture_command_core(&t, &mut meta);
        assert_pending_blocks_osc7(&mut t, &mut p, &mut meta, &mut carry, "");
    }

    /// The command wrapped onto the next line. The cursor column is back
    /// near 0, so it is not past the prompt column on the row above.
    #[test]
    fn wrapped_command_still_requires_the_osc7_nonce() {
        let (mut t, mut p, mut meta, mut carry) = prompt_ready();
        let pcol = meta.prompt_col.unwrap();
        let n = t.grid().columns() - pcol + 1;
        let line = "x".repeat(n);
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, line.as_bytes());
        assert!(
            t.grid().cursor.point.line.0 > 0,
            "the command should occupy the next row, cursor {:?}",
            t.grid().cursor.point
        );
        assert!(
            t.grid().cursor.point.column.0 <= pcol,
            "cursor {:?} should sit at or before the prompt column",
            t.grid().cursor.point
        );
        capture_command_core(&t, &mut meta);
        assert_pending_blocks_osc7(&mut t, &mut p, &mut meta, &mut carry, &line);
    }

    #[test]
    fn kitty_stack_cap_survives_five_thousand_pushes() {
        let KittyFixture {
            term: mut t,
            parser: mut p,
            mut meta,
            mut carry,
            log,
        } = kitty_term(true);
        let mut burst = Vec::with_capacity(5000 * 6);
        for _ in 0..5000 {
            burst.extend_from_slice(b"\x1b[>1u");
        }
        // Past the cap, a different flag must not land — those pushes are
        // dropped, not applied, and must not panic the parser.
        burst.extend_from_slice(b"\x1b[>31u");
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, &burst);
        assert!(t.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
        assert!(!t.mode().contains(TermMode::REPORT_ALL_KEYS_AS_ESC));
        assert_eq!(meta.kitty_depth, 16);

        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?uHi");
        assert_eq!(take_writes(&log), vec!["\x1b[?1u".to_string()]);
        {
            let row = &t.grid()[alacritty_terminal::index::Line(0)];
            assert_eq!(row[Column(0)].c, 'H');
            assert_eq!(row[Column(1)].c, 'i');
        }
        // still a terminal: a pop under the cap answers, and more text lands
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[<16u\x1b[?u!");
        assert_eq!(take_writes(&log), vec!["\x1b[?0u".to_string()]);
        assert!(!t.mode().intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));
        let row = &t.grid()[alacritty_terminal::index::Line(0)];
        assert_eq!(row[Column(2)].c, '!');
    }

    #[test]
    fn kitty_set_survives_alt_screen_without_a_query() {
        let KittyFixture {
            term: mut t,
            parser: mut p,
            mut meta,
            mut carry,
            log,
        } = kitty_term(true);
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[>1u\x1b[=4;2u");
        assert!(t.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
        assert!(t.mode().contains(TermMode::REPORT_ALTERNATE_KEYS));
        // No query in between. Leaving the alt screen reloads the stack;
        // the set must already be on that stack or the flags revert to 1.
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?1049h");
        assert!(t.mode().contains(TermMode::ALT_SCREEN));
        assert!(!t.mode().intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?1049l\x1b[?u");
        assert!(!t.mode().contains(TermMode::ALT_SCREEN));
        assert!(t.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
        assert!(t.mode().contains(TermMode::REPORT_ALTERNATE_KEYS));
        assert_eq!(take_writes(&log), vec!["\x1b[?5u".to_string()]);
    }

    #[test]
    fn osc133_prompt_resets_keyboard_mode() {
        let KittyFixture {
            term: mut t,
            parser: mut p,
            mut meta,
            mut carry,
            log,
        } = kitty_term(true);
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[>31uHi");
        assert!(t.mode().contains(TermMode::REPORT_ASSOCIATED_TEXT));
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]133;A\x07\x1b[?u",
        );
        assert_eq!(take_writes(&log), vec!["\x1b[?0u".to_string()]);
        assert!(!t.mode().intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));
        assert_eq!(meta.kitty_depth, 0);
        let row = &t.grid()[alacritty_terminal::index::Line(0)];
        assert_eq!(row[Column(0)].c, 'H', "reset must not wipe the prompt line");
        assert_eq!(row[Column(1)].c, 'i');
        assert!(!meta.marks.is_empty(), "the prompt mark is still recorded");
    }

    fn cell(t: &Term<PtyLog>, col: usize) -> char {
        t.grid()[alacritty_terminal::index::Line(0)][Column(col)].c
    }

    fn repeat(seq: &[u8], n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(seq.len() * n);
        for _ in 0..n {
            out.extend_from_slice(seq);
        }
        out
    }

    /// Forms vte still accepts as `CSI > u` that `kitty_seq_at` does not:
    /// a C0 inside the parameter, a C0 before the intermediate, a `:`.
    #[test]
    fn kitty_bypass_pushes_do_not_panic_on_or_off() {
        let variants: &[&[u8]] = &[b"\x1b[>1\x00u", b"\x1b[\x0b>1u", b"\x1b[>1:1u"];
        for protocol in [false, true] {
            for seq in variants {
                let KittyFixture {
                    term: mut t,
                    parser: mut p,
                    mut meta,
                    mut carry,
                    log,
                } = kitty_term(protocol);
                process_chunk(&mut p, &mut t, &mut meta, &mut carry, &repeat(seq, 5000));
                // VT inside `ESC [ VT > 1 u` is a real linefeed, so the proof
                // that the terminal is still usable is the cursor cell, not row 0.
                let at = t.grid().cursor.point;
                process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"Z");
                assert_eq!(
                    t.grid()[at.line][at.column].c,
                    'Z',
                    "protocol={protocol} seq={seq:?}"
                );
                let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?u");
                if protocol {
                    assert!(t.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
                    assert_eq!(take_writes(&log), vec!["\x1b[?1u".to_string()]);
                    assert!(ev.pty_replies.is_empty());
                } else {
                    assert!(!t.mode().intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));
                    assert!(take_writes(&log).is_empty(), "parser must stay silent");
                    assert_eq!(ev.pty_replies, vec!["\x1b[?0u".to_string()]);
                    assert_eq!(meta.kitty_depth, 0);
                }
            }
        }
    }

    #[test]
    fn ris_zeroes_keyboard_depth_on_both_screens() {
        let KittyFixture {
            term: mut t,
            parser: mut p,
            mut meta,
            mut carry,
            log,
        } = kitty_term(true);
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            &repeat(b"\x1b[>1u", 16),
        );
        assert_eq!(meta.kitty_depth, 16);
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b[?1049h\x1b[>4u",
        );
        assert!(t.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(meta.kitty_depth, 1);
        assert_eq!(meta.kitty_alt_depth, 16);
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1bc");
        assert!(meta.saw_ris, "the focus-tracking flag is the RIS signal");
        assert_eq!(
            meta.kitty_depth, 0,
            "RIS clears the counter, not only the parser"
        );
        assert_eq!(meta.kitty_alt_depth, 0);
        assert!(!t.mode().intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));
        // the cap is not stuck at 16: a push after RIS is accepted
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[>1u\x1b[?uZ");
        assert_eq!(take_writes(&log), vec!["\x1b[?1u".to_string()]);
        assert_eq!(meta.kitty_depth, 1);
        assert_eq!(cell(&t, 0), 'Z');
    }

    #[test]
    fn prompt_keeps_the_selection_and_only_resets_the_current_stack() {
        use alacritty_terminal::index::{Point, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        let KittyFixture {
            term: mut t,
            parser: mut p,
            mut meta,
            mut carry,
            log: _,
        } = kitty_term(true);
        let point = Point::new(alacritty_terminal::index::Line(0), Column(0));
        // Empty stack: a prompt must not feed anything, so DECSC survives.
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"Hi\x1b7\x1b[2;4H");
        let saved = t.grid().saved_cursor.point;
        t.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b]133;A\x07");
        assert!(t.selection.is_some(), "empty stack is a no-op");
        assert_eq!(t.grid().saved_cursor.point, saved);
        assert_eq!(cell(&t, 0), 'H');

        // A real level still must not visit the other screen.
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[>1u");
        assert_eq!(meta.kitty_depth, 1);
        t.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]133;A\x07\x1b[?u",
        );
        assert!(
            t.selection.is_some(),
            "prompt reset must not clear the selection"
        );
        assert_eq!(
            t.grid().saved_cursor.point,
            saved,
            "DECSC must survive the prompt"
        );
        assert!(!t.mode().contains(TermMode::ALT_SCREEN));
        assert!(!t.mode().intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));
        assert_eq!(meta.kitty_depth, 0);
        assert_eq!(cell(&t, 0), 'H');
        assert_eq!(cell(&t, 1), 'i');
    }

    #[test]
    fn alt_screen_reset_keeps_the_tui_and_the_other_stack() {
        use alacritty_terminal::index::{Point, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        let KittyFixture {
            term: mut t,
            parser: mut p,
            mut meta,
            mut carry,
            log,
        } = kitty_term(true);
        // Primary keeps disambiguate. Alt has report-all and the word TUI.
        // Ctrl+Shift+K (and shell exit) call the same helper.
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b[>1u\x1b[?1049hTUI\x1b[>8u",
        );
        assert!(t.mode().contains(TermMode::ALT_SCREEN));
        assert!(t.mode().contains(TermMode::REPORT_ALL_KEYS_AS_ESC));
        assert_eq!(meta.kitty_alt_depth, 1, "primary level is parked");
        let point = Point::new(alacritty_terminal::index::Line(0), Column(0));
        t.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
        reset_keyboard_modes(&mut p, &mut t, &mut meta);
        assert!(
            t.mode().contains(TermMode::ALT_SCREEN),
            "reset must not leave the alt screen"
        );
        assert_eq!(cell(&t, 0), 'T');
        assert_eq!(cell(&t, 1), 'U');
        assert_eq!(cell(&t, 2), 'I');
        assert!(t.selection.is_some());
        assert!(!t.mode().intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));
        assert_eq!(meta.kitty_depth, 0);
        assert_eq!(meta.kitty_alt_depth, 1, "only the current screen's stack");
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?u");
        assert_eq!(take_writes(&log), vec!["\x1b[?0u".to_string()]);
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?1049l\x1b[?u");
        assert!(!t.mode().contains(TermMode::ALT_SCREEN));
        assert!(t.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
        assert_eq!(take_writes(&log), vec!["\x1b[?1u".to_string()]);
    }

    #[test]
    fn reset_is_a_noop_when_the_protocol_is_off() {
        use alacritty_terminal::index::{Point, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        let KittyFixture {
            term: mut t,
            parser: mut p,
            mut meta,
            mut carry,
            log,
        } = kitty_term(false);
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"Hi\x1b7\x1b[2;4H");
        let saved = t.grid().saved_cursor.point;
        let cursor = t.grid().cursor.point;
        let point = Point::new(alacritty_terminal::index::Line(0), Column(0));
        t.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            &repeat(b"\x1b[>1\x00u", 5000),
        );
        reset_keyboard_modes(&mut p, &mut t, &mut meta);
        let ev = process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            b"\x1b]133;A\x07\x1b[?u",
        );
        assert!(t.selection.is_some());
        assert_eq!(t.grid().saved_cursor.point, saved);
        assert_eq!(
            t.grid().cursor.point,
            cursor,
            "the prompt must not move the cursor"
        );
        assert_eq!(cell(&t, 0), 'H');
        assert!(!t.mode().contains(TermMode::ALT_SCREEN));
        assert!(!t.mode().intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));
        assert!(take_writes(&log).is_empty());
        assert_eq!(ev.pty_replies, vec!["\x1b[?0u".to_string()]);
    }

    #[test]
    fn reset_pops_zero_value_layers_by_depth() {
        let KittyFixture {
            term: mut t,
            parser: mut p,
            mut meta,
            mut carry,
            log,
        } = kitty_term(true);
        // Two counted levels. The top mode is 0, so no kitty bits are set.
        // A flags check would skip the reset and a later pop would reveal 1.
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[>1u\x1b[>0u");
        assert_eq!(meta.kitty_depth, 2);
        assert!(!t.mode().intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));
        reset_keyboard_modes(&mut p, &mut t, &mut meta);
        assert_eq!(meta.kitty_depth, 0);
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?u");
        assert_eq!(take_writes(&log), vec!["\x1b[?0u".to_string()]);
        // The extra pop must not uncover the buried 1.
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[<u\x1b[?u");
        assert_eq!(take_writes(&log), vec!["\x1b[?0u".to_string()]);
        assert!(!t.mode().intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));
    }

    #[test]
    fn reset_ignores_flag_bits_when_the_counted_depth_is_zero() {
        let KittyFixture {
            term: mut t,
            parser: mut p,
            mut meta,
            mut carry,
            log,
        } = kitty_term(true);
        // A `:` sub-parameter is not counted, so kitty_depth stays 0 while
        // the real stack is 20 deep and disambiguate is on. Popping from
        // the flag bits would drain it. The depth is what we pop.
        process_chunk(
            &mut p,
            &mut t,
            &mut meta,
            &mut carry,
            &repeat(b"\x1b[>1:1u", 20),
        );
        assert_eq!(meta.kitty_depth, 0);
        assert!(t.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
        reset_keyboard_modes(&mut p, &mut t, &mut meta);
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?u!");
        assert_eq!(take_writes(&log), vec!["\x1b[?1u".to_string()]);
        assert!(t.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
        assert_eq!(cell(&t, 0), '!');
    }

    #[test]
    fn protocol_off_answers_a_query_with_a_c0_inside_the_csi() {
        let KittyFixture {
            term: mut t,
            parser: mut p,
            mut meta,
            mut carry,
            log,
        } = kitty_term(false);
        // NUL does not end the CSI. vte would still execute the query.
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[?\x00u");
        assert!(take_writes(&log).is_empty(), "parser must stay silent");
        assert_eq!(ev.pty_replies, vec!["\x1b[?0u".to_string()]);
        // C0 before the intermediate is the same sequence.
        let ev = process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[\x00?u");
        assert!(take_writes(&log).is_empty());
        assert_eq!(ev.pty_replies, vec!["\x1b[?0u".to_string()]);
    }

    #[test]
    fn exit_reset_returns_the_live_parser_to_ground() {
        let KittyFixture {
            term: mut t,
            parser: mut p,
            mut meta,
            mut carry,
            log: _,
        } = kitty_term(true);
        process_chunk(&mut p, &mut t, &mut meta, &mut carry, b"\x1b[>1u");
        assert!(t.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
        // A read that ended on ESC [ leaves the session parser in CSI.
        // Shell exit feeds the pop through that parser. A fresh one would
        // leave this machine inside the CSI, and 'Z' would not be text.
        p.advance(&mut t, b"\x1b[");
        reset_keyboard_modes(&mut p, &mut t, &mut meta);
        assert!(!t.mode().intersects(TermMode::KITTY_KEYBOARD_PROTOCOL));
        p.advance(&mut t, b"Z");
        assert_eq!(cell(&t, 0), 'Z');
    }
}
