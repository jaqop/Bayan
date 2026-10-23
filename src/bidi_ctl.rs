//! BDSM and SCP, from ECMA-48 and Egmont Koblinger's "BiDi in Terminal
//! Emulators" (terminal-wg).
//!
//! `alacritty_terminal` 0.26 (vte 0.15) does not act on either sequence:
//! mode 8 is an unknown SM/RM parameter, and `CSI Ps SP k` is not in its
//! CSI dispatch. A small filter pulls them out of the PTY byte stream
//! before `Processor::advance`, including when a sequence is split across
//! reads, and the parser never sees them.
//!
//! Defaults follow the recommendation, not a cold reading of ECMA-48's
//! reset-initial table: BDSM starts SET (implicit — Bayan applies UAX#9),
//! because a shell that has never heard of BDSM still needs Arabic. SCP
//! starts at default (auto paragraph direction). RIS and an alternate-screen
//! switch both return to that initial state; the recommendation's "generic
//! reset sets BDSM" is why RIS does not leave the pane in explicit mode.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::Term;

/// `CSI 8 h` — the terminal applies the BiDi algorithm.
/// `CSI 8 l` — the application already ordered the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BidiMode {
    /// Terminal reorders (BDSM set). The initial state.
    #[default]
    Implicit,
    /// Application reordered (BDSM reset). Bayan must not reorder this pane.
    Explicit,
}

/// SCP first parameter: the paragraph's base direction.
///
/// `CSI 1 SP k` LTR, `CSI 2 SP k` RTL, `CSI 0 SP k` / `CSI SP k` default.
/// The value sticks to the current line when the cursor is in column 0, and
/// to subsequent lines either way — SCP's scope in the recommendation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ParagraphDir {
    /// First strong character decides (SCP 0, the initial state).
    #[default]
    Default,
    /// Strong LTR base (SCP 1).
    Ltr,
    /// Strong RTL base (SCP 2).
    Rtl,
}

/// Per-pane BiDi mode. SCP additionally stamps a direction on lines.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BidiState {
    pub mode: BidiMode,
    /// Direction that applies to lines written from now on.
    pub direction: ParagraphDir,
    /// `(absolute line, direction)` in ascending line order. A line uses
    /// the latest stamp at or before it; lines before the first stamp stay
    /// at the default.
    stamps: Vec<(u64, ParagraphDir)>,
}

const MAX_STAMPS: usize = 2048;

impl BidiState {
    /// Whether this pane's rows should be reordered.
    ///
    /// Explicit BDSM means the application already did the ordering, so
    /// Bayan must not run UAX#9 and must not apply Claude-mode restoration.
    /// `honor_bdsm` (config, default true) is the gate on that override.
    /// Set it false and Bayan's own detection keeps reordering anyway.
    pub fn reorders(&self, honor_bdsm: bool) -> bool {
        !(honor_bdsm && self.mode == BidiMode::Explicit)
    }

    pub fn direction_at(&self, abs_line: u64) -> ParagraphDir {
        // Stamps are in ascending line order. The last one at or before
        // this line wins; lines before the first stamp stay at the default.
        let i = self.stamps.partition_point(|&(line, _)| line <= abs_line);
        if i == 0 {
            ParagraphDir::Default
        } else {
            self.stamps[i - 1].1
        }
    }

    /// Explicit mode and SCP stamps affect the screen only while a command
    /// is running or the pane is on the alternate screen. Output from `cat`
    /// must not leave the prompt in explicit RTL.
    pub fn for_display(
        &self,
        honor_bdsm: bool,
        command_running: bool,
        alt_screen: bool,
    ) -> (bool, bool) {
        if command_running || alt_screen {
            (self.reorders(honor_bdsm), true)
        } else {
            (true, false)
        }
    }

    /// Record an SCP. Column 0 updates the line the cursor is on; anywhere
    /// else, the new direction starts on the next line.
    pub fn note_scp(&mut self, dir: ParagraphDir, abs_line: u64, column: usize) {
        self.direction = dir;
        let at = if column == 0 {
            abs_line
        } else {
            abs_line.saturating_add(1)
        };
        if let Some(i) = self.stamps.iter().position(|&(line, _)| line >= at) {
            self.stamps.truncate(i);
        }
        self.stamps.push((at, dir));
        if self.stamps.len() > MAX_STAMPS {
            let drop_n = self.stamps.len() - MAX_STAMPS / 2;
            self.stamps.drain(..drop_n);
        }
    }

    /// Back to the initial state: implicit mode, default direction, no stamps.
    /// The byte-stream filter is separate and is not cleared — a reset is a
    /// finished sequence, not a reason to drop a half-read CSI.
    pub fn reset(&mut self) {
        *self = BidiState::default();
    }

    /// Drop SCP stamps. A resize reflows rows, so an absolute line no longer
    /// names the text it was stamped on. The mode stays: a full-screen
    /// program does not resend BDSM just because the window changed size.
    /// `Session::resize` is the caller.
    pub fn on_resize(&mut self) {
        self.stamps.clear();
        self.direction = ParagraphDir::Default;
    }
}

/// Absolute line of the cursor, in the same units as OSC 133 command marks:
/// evicted lines + history + the cursor's screen row.
pub fn cursor_line_abs(evicted: u64, history: u64, cursor_line: i32) -> u64 {
    evicted + history + cursor_line.max(0) as u64
}

/// Absolute line of a viewport row. `display_offset` is how far the view
/// has scrolled into history; row 0 is the top of the screen.
pub fn viewport_line_abs(evicted: u64, history: u64, display_offset: u64, vrow: u64) -> u64 {
    evicted
        .saturating_add(history)
        .saturating_add(vrow)
        .saturating_sub(display_offset)
}

/// UAX#9 base-direction mark. cosmic-text chooses the paragraph level from
/// the first strong character and has no knob for it; LRM/RLM are that
/// character and occupy no columns. `None` leaves auto-detection alone,
/// which is what an unstamped line must keep doing — prepending a mark on
/// every Arabic row would change the shaped-run cache key for no reason.
pub fn base_direction_mark(dir: ParagraphDir) -> Option<char> {
    match dir {
        ParagraphDir::Ltr => Some('\u{200E}'),
        ParagraphDir::Rtl => Some('\u{200F}'),
        ParagraphDir::Default => None,
    }
}

/// What the filter found, in stream order. `Bytes` is what the VT parser
/// should see; the other variants were stripped.
#[derive(Debug, PartialEq, Eq)]
pub enum StreamItem {
    Bytes(Vec<u8>),
    Bdsm(BidiMode),
    Scp(ParagraphDir),
    /// `ESC c`. The bytes themselves are also emitted, so alacritty performs
    /// its own RIS; the caller resets BiDi state after feeding them.
    Ris,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Ground,
    Esc,
    Csi,
}

/// Holds back a CSI (or a bare ESC) that might still turn into BDSM, SCP,
/// or RIS. Everything else is emitted immediately, so OSC 133 / OSC 7 keep
/// the carry behaviour they already have.
#[derive(Default)]
pub struct StreamFilter {
    phase: Phase,
    /// The sequence under inspection, starting at its ESC.
    held: Vec<u8>,
}

/// Longer than any real BDSM/SCP. Past this the bytes cannot be one of ours.
const CSI_CAP: usize = 64;

impl StreamFilter {
    pub fn pull(&mut self, input: &[u8]) -> Vec<StreamItem> {
        // Most PTY reads are plain text. Skip the state machine when we are
        // not holding a sequence and the chunk has no ESC.
        if self.phase == Phase::Ground && !input.contains(&0x1b) {
            if input.is_empty() {
                return Vec::new();
            }
            return vec![StreamItem::Bytes(input.to_vec())];
        }
        let mut items = Vec::new();
        let mut out = Vec::new();
        for &b in input {
            self.step(b, &mut items, &mut out);
        }
        if !out.is_empty() {
            items.push(StreamItem::Bytes(out));
        }
        items
    }

    fn step(&mut self, b: u8, items: &mut Vec<StreamItem>, out: &mut Vec<u8>) {
        match self.phase {
            Phase::Ground => {
                if b == 0x1b {
                    self.phase = Phase::Esc;
                    self.held.clear();
                    self.held.push(b);
                } else {
                    out.push(b);
                }
            }
            Phase::Esc => {
                // vte 0.15 `advance_esc`: a second ESC is ignored and the
                // state stays Escape, so `ESC ESC c` is RIS. C0 other than
                // CAN/SUB is executed and does not leave Escape, so
                // `ESC <C0> c` is RIS too. CAN (0x18) and SUB (0x1A) cancel.
                match b {
                    0x1b => {
                        self.held.clear();
                        self.held.push(b);
                    }
                    0x00..=0x17 | 0x19 | 0x1c..=0x1f => out.push(b),
                    0x18 | 0x1a => {
                        self.held.clear();
                        out.push(b);
                        self.phase = Phase::Ground;
                    }
                    0x7f => {}
                    b'[' => {
                        self.held.push(b);
                        self.phase = Phase::Csi;
                    }
                    b'c' => {
                        self.held.push(b);
                        out.append(&mut self.held);
                        self.phase = Phase::Ground;
                        if !out.is_empty() {
                            items.push(StreamItem::Bytes(std::mem::take(out)));
                        }
                        items.push(StreamItem::Ris);
                    }
                    _ => {
                        // OSC, ESC D, charset designation, … not ours.
                        self.held.push(b);
                        out.append(&mut self.held);
                        self.phase = Phase::Ground;
                    }
                }
            }
            Phase::Csi => {
                // CAN/SUB cancel the sequence. A fresh ESC aborts it and
                // starts a new one. Either way the parser, not us, decides
                // what the aborted bytes mean.
                if b == 0x18 || b == 0x1a {
                    self.held.push(b);
                    out.append(&mut self.held);
                    self.phase = Phase::Ground;
                    return;
                }
                if b == 0x1b {
                    out.append(&mut self.held);
                    self.held.push(b);
                    self.phase = Phase::Esc;
                    return;
                }
                // vte executes other C0 and stays in CSI. DEL and bytes
                // above 0x7E are ignored and do not leave the sequence, so
                // `CSI ?1049 <C0> h` is still an alt-screen toggle. The C0
                // is emitted before the CSI bytes; vte runs it, then the CSI.
                if b < 0x20 {
                    out.push(b);
                    return;
                }
                if b == 0x7f || b > 0x7e {
                    return;
                }
                self.held.push(b);
                if self.held.len() > CSI_CAP {
                    out.append(&mut self.held);
                    self.phase = Phase::Ground;
                    return;
                }
                if (0x40..=0x7e).contains(&b) {
                    self.finish_csi(b, items, out);
                }
            }
        }
    }

    fn finish_csi(&mut self, final_byte: u8, items: &mut Vec<StreamItem>, out: &mut Vec<u8>) {
        let raw = std::mem::take(&mut self.held);
        self.phase = Phase::Ground;
        // raw = ESC [ body final
        let body = &raw[2..raw.len() - 1];
        let inter_at = body
            .iter()
            .position(|b| (0x20..=0x2f).contains(b))
            .unwrap_or(body.len());
        let params = &body[..inter_at];
        let inter = &body[inter_at..];

        if (final_byte == b'h' || final_byte == b'l') && inter.is_empty() {
            if let Some(rest) = strip_mode_8(params) {
                if !rest.is_empty() {
                    out.push(0x1b);
                    out.push(b'[');
                    out.extend_from_slice(rest.as_bytes());
                    out.push(final_byte);
                }
                if !out.is_empty() {
                    items.push(StreamItem::Bytes(std::mem::take(out)));
                }
                let mode = if final_byte == b'h' {
                    BidiMode::Implicit
                } else {
                    BidiMode::Explicit
                };
                items.push(StreamItem::Bdsm(mode));
                return;
            }
        }

        if final_byte == b'k' && inter == *b" " {
            match parse_scp(params) {
                Some(dir) => {
                    if !out.is_empty() {
                        items.push(StreamItem::Bytes(std::mem::take(out)));
                    }
                    items.push(StreamItem::Scp(dir));
                }
                // A numeric parameter we don't know: still SCP-shaped, so it
                // must not fall through into the grid as text. Direction stays.
                None if scp_params_are_numeric(params) => {}
                None => out.extend_from_slice(&raw),
            }
            return;
        }

        out.extend_from_slice(&raw);
    }
}

/// `Some(remaining)` when parameter 8 is present. The remaining string is
/// the other modes, still in order, ready to be wrapped as `CSI <rest> h`.
/// An empty string means 8 was the only parameter — emit no CSI at all.
/// `None` means this is not a BDSM sequence (no mode 8, or not plain digits).
fn strip_mode_8(params: &[u8]) -> Option<String> {
    if params.iter().any(|b| !matches!(b, b'0'..=b'9' | b';')) {
        return None;
    }
    let s = std::str::from_utf8(params).ok()?;
    let mut found = false;
    let mut kept: Vec<&str> = Vec::new();
    for p in s.split(';') {
        let n = if p.is_empty() {
            0
        } else {
            p.parse::<u32>().ok()?
        };
        if n == 8 {
            found = true;
        } else {
            kept.push(if p.is_empty() { "0" } else { p });
        }
    }
    found.then(|| kept.join(";"))
}

fn scp_params_are_numeric(params: &[u8]) -> bool {
    params.iter().all(|b| matches!(b, b'0'..=b'9' | b';'))
}

/// First SCP parameter. `None` for a non-numeric body or a value other than
/// 0, 1, 2. Empty parameters are 0, per ECMA-48's default.
fn parse_scp(params: &[u8]) -> Option<ParagraphDir> {
    if !scp_params_are_numeric(params) {
        return None;
    }
    let s = std::str::from_utf8(params).ok()?;
    let first = s.split(';').next().unwrap_or("");
    let n = if first.is_empty() {
        0
    } else {
        first.parse::<u32>().ok()?
    };
    match n {
        0 => Some(ParagraphDir::Default),
        1 => Some(ParagraphDir::Ltr),
        2 => Some(ParagraphDir::Rtl),
        _ => None,
    }
}

/// Cursor coordinates the SCP stamp needs. Shared so the feed loop and the
/// tests use one definition.
pub fn cursor_abs<T: alacritty_terminal::event::EventListener>(
    term: &Term<T>,
    evicted: u64,
) -> (u64, usize) {
    let cur = term.grid().cursor.point;
    let abs = cursor_line_abs(evicted, term.grid().history_size() as u64, cur.line.0);
    (abs, cur.column.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::Column;
    use alacritty_terminal::term::{Config, Term, TermMode};
    use alacritty_terminal::vte::ansi::Processor;

    use crate::term::{process_chunk, SessionMeta, TermSize};

    fn fresh() -> (Term<VoidListener>, Processor, SessionMeta, Vec<u8>) {
        let size = TermSize { cols: 40, rows: 8 };
        (
            Term::new(Config::default(), &size, VoidListener),
            Processor::new(),
            SessionMeta::default(),
            Vec::new(),
        )
    }

    fn feed(
        t: &mut Term<VoidListener>,
        p: &mut Processor,
        meta: &mut SessionMeta,
        carry: &mut Vec<u8>,
        data: &[u8],
    ) {
        process_chunk(p, t, meta, carry, data);
    }

    fn cell(t: &Term<VoidListener>, col: usize) -> char {
        let line = t.grid().cursor.point.line;
        t.grid()[line][Column(col)].c
    }

    /// BDSM and SCP are recorded only while a command is running, or on the
    /// alt screen. Tests that check the sequence itself set this first.
    fn running(meta: &mut SessionMeta) {
        meta.running_cmd = "cat".into();
    }

    #[test]
    fn bdsm_scp_split_across_reads_and_mixed_with_other_csi() {
        let (mut t, mut p, mut meta, mut carry) = fresh();
        running(&mut meta);

        // Split BDSM: the 'l' arrives in the next read. The letter on each
        // side is still a cell — the sequence was stripped, not printed.
        feed(&mut t, &mut p, &mut meta, &mut carry, b"A\x1b[8");
        assert_eq!(
            meta.bidi.mode,
            BidiMode::Implicit,
            "half a CSI is not a mode yet"
        );
        assert_eq!(cell(&t, 0), 'A');
        feed(&mut t, &mut p, &mut meta, &mut carry, b"lB");
        assert_eq!(meta.bidi.mode, BidiMode::Explicit);
        assert_eq!(cell(&t, 0), 'A');
        assert_eq!(cell(&t, 1), 'B');

        // A private mode 8 is not BDSM, and a CSI with no space is not SCP.
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[?8h\x1b[1k");
        assert_eq!(meta.bidi.mode, BidiMode::Explicit);
        assert_eq!(meta.bidi.direction, ParagraphDir::Default);

        // Mode 4 (insert) shares the CSI with mode 8 and must still reach
        // the parser. `CSI 4;8 h` sets insert and returns to implicit.
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[4;8h");
        assert!(t.mode().contains(TermMode::INSERT));
        assert_eq!(meta.bidi.mode, BidiMode::Implicit);
        // The reverse mix clears insert and selects explicit.
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[8;4l");
        assert!(!t.mode().contains(TermMode::INSERT));
        assert_eq!(meta.bidi.mode, BidiMode::Explicit);

        // CHA (cursor to column 5) beside a split SCP. Column is 0-based.
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[5G\x1b[2 ");
        assert_eq!(t.grid().cursor.point.column.0, 4);
        assert_eq!(
            meta.bidi.direction,
            ParagraphDir::Default,
            "SCP missing its final"
        );
        feed(&mut t, &mut p, &mut meta, &mut carry, b"k");
        assert_eq!(meta.bidi.direction, ParagraphDir::Rtl);
        // An unknown SCP parameter is consumed and does not move the direction.
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[9 k");
        assert_eq!(meta.bidi.direction, ParagraphDir::Rtl);
    }

    #[test]
    fn explicit_mode_disables_reordering_for_that_pane() {
        let (_t, _p, meta, _carry) = fresh();
        assert!(meta.bidi.reorders(true), "implicit is the initial state");
        assert_eq!(base_direction_mark(ParagraphDir::Default), None);

        let mut meta = meta;
        meta.bidi_mut().mode = BidiMode::Explicit;
        assert!(
            !meta.bidi.reorders(true),
            "explicit BDSM overrides auto-detection when the flag is on"
        );
        assert!(
            meta.bidi.reorders(false),
            "bidi.honor_bdsm = false keeps Bayan's own reordering"
        );
        // A leaked explicit mode does not restyle the prompt. Stamps are
        // ignored too. While the command is still running, both apply.
        assert_eq!(meta.bidi.for_display(true, false, false), (true, false));
        assert_eq!(meta.bidi.for_display(true, true, false), (false, true));
        assert_eq!(meta.bidi.for_display(true, false, true), (false, true));

        // The same answer after a real sequence, not a hand-poked field.
        let (mut t, mut p, mut meta, mut carry) = fresh();
        running(&mut meta);
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[8l");
        assert!(!meta.bidi.reorders(true));
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[8h");
        assert!(meta.bidi.reorders(true));
    }

    #[test]
    fn scp_rtl_ltr_and_default() {
        let (mut t, mut p, mut meta, mut carry) = fresh();
        running(&mut meta);
        // line 0 is written before any SCP, so it stays at the default.
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            b"aaa\r\n\x1b[2 kbbb\r\n\x1b[1 kccc\r\n\x1b[ kddd",
        );
        assert_eq!(meta.bidi.direction_at(0), ParagraphDir::Default);
        assert_eq!(meta.bidi.direction_at(1), ParagraphDir::Rtl);
        assert_eq!(meta.bidi.direction_at(2), ParagraphDir::Ltr);
        assert_eq!(meta.bidi.direction_at(3), ParagraphDir::Default);
        // `CSI 0 SP k` is the same default as a bare `CSI SP k`.
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\r\n\x1b[0 keee");
        assert_eq!(meta.bidi.direction_at(4), ParagraphDir::Default);
        // A later line with no new SCP inherits the stamp above it.
        assert_eq!(meta.bidi.direction_at(2), ParagraphDir::Ltr);
        assert_eq!(base_direction_mark(ParagraphDir::Rtl), Some('\u{200F}'));
        assert_eq!(base_direction_mark(ParagraphDir::Ltr), Some('\u{200E}'));

        // Mid-line SCP does not restamp the line already in progress.
        let (mut t, mut p, mut meta, mut carry) = fresh();
        running(&mut meta);
        feed(&mut t, &mut p, &mut meta, &mut carry, b"ab\x1b[2 k");
        assert_eq!(t.grid().cursor.point.column.0, 2);
        assert_eq!(meta.bidi.direction_at(0), ParagraphDir::Default);
        assert_eq!(meta.bidi.direction_at(1), ParagraphDir::Rtl);
    }

    #[test]
    fn ris_resets_bidi_state() {
        let (mut t, mut p, mut meta, mut carry) = fresh();
        running(&mut meta);
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[8l\x1b[2 kZ");
        assert_eq!(meta.bidi.mode, BidiMode::Explicit);
        assert_eq!(meta.bidi.direction, ParagraphDir::Rtl);
        assert_eq!(cell(&t, 0), 'Z');
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1bc");
        assert_eq!(meta.bidi.mode, BidiMode::Implicit);
        assert_eq!(meta.bidi.direction, ParagraphDir::Default);
        assert_eq!(meta.bidi.direction_at(0), ParagraphDir::Default);
        assert!(meta.bidi.reorders(true));
        // ESC c was passed through: the screen actually reset.
        assert_eq!(t.grid().cursor.point.line.0, 0);
        assert_eq!(t.grid().cursor.point.column.0, 0);
        assert_eq!(cell(&t, 0), ' ');
    }

    #[test]
    fn alt_screen_switch_resets_bidi_state() {
        let (mut t, mut p, mut meta, mut carry) = fresh();
        running(&mut meta);
        // Entering the alternate screen clears a mode set on the primary.
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            b"\x1b[8l\x1b[2 k\x1b[?1049h",
        );
        assert!(t.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(meta.bidi.mode, BidiMode::Implicit);
        assert_eq!(meta.bidi.direction, ParagraphDir::Default);
        assert_eq!(meta.bidi.direction_at(1), ParagraphDir::Default);
        // The command name is gone. Alt screen alone still honors BDSM,
        // until the switch back.
        meta.running_cmd.clear();
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[8l\x1b[1 k");
        assert_eq!(meta.bidi.mode, BidiMode::Explicit);
        assert_eq!(meta.bidi.direction, ParagraphDir::Ltr);
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[?1049l");
        assert!(!t.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(meta.bidi.mode, BidiMode::Implicit);
        assert_eq!(meta.bidi.direction, ParagraphDir::Default);
    }

    /// `cat` of a file that contains `CSI 8 l` / `CSI 2 SP k` must not leave
    /// the prompt in explicit RTL. OSC 133 D and A both clear the state, and
    /// a sequence that arrives with no command running is ignored.
    #[test]
    fn osc133_a_and_d_reset_bidi_and_idle_ignores_it() {
        let (mut t, mut p, mut meta, mut carry) = fresh();
        running(&mut meta);
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            b"\x1b[8l\x1b[2 k\x1b]133;D;0\x07",
        );
        assert!(meta.running_cmd.is_empty());
        assert_eq!(meta.bidi.mode, BidiMode::Implicit);
        assert_eq!(meta.bidi.direction, ParagraphDir::Default);
        assert_eq!(meta.bidi.direction_at(1), ParagraphDir::Default);
        assert_eq!(meta.bidi.for_display(true, false, false), (true, false));

        running(&mut meta);
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            b"\x1b[8l\x1b[2 k\x1b]133;A\x07",
        );
        assert!(meta.running_cmd.is_empty());
        assert_eq!(meta.bidi.mode, BidiMode::Implicit);
        assert_eq!(meta.bidi.direction_at(0), ParagraphDir::Default);

        // The same bytes at the prompt do nothing.
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[8l\x1b[2 k");
        assert_eq!(meta.bidi.mode, BidiMode::Implicit);
        assert_eq!(meta.bidi.direction, ParagraphDir::Default);
        assert!(meta.bidi.reorders(true));
    }

    /// vte stays in Escape across a second ESC and across C0 other than
    /// CAN/SUB, so both of these are RIS. CAN (0x18) cancels, and the `c`
    /// is then an ordinary character.
    #[test]
    fn esc_esc_c_and_esc_c0_c_are_ris() {
        let (mut t, mut p, mut meta, mut carry) = fresh();
        running(&mut meta);
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[8l\x1b[2 k");
        assert_eq!(meta.bidi.mode, BidiMode::Explicit);
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b\x1bc");
        assert_eq!(meta.bidi.mode, BidiMode::Implicit);
        assert_eq!(meta.bidi.direction, ParagraphDir::Default);
        assert!(meta.saw_ris);

        running(&mut meta);
        meta.saw_ris = false;
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[8l\x1b[1 k");
        assert_eq!(meta.bidi.mode, BidiMode::Explicit);
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b\x00c");
        assert_eq!(meta.bidi.mode, BidiMode::Implicit);
        assert_eq!(meta.bidi.direction, ParagraphDir::Default);
        assert!(meta.saw_ris);

        running(&mut meta);
        meta.saw_ris = false;
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[8l");
        assert_eq!(meta.bidi.mode, BidiMode::Explicit);
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b\x18c");
        assert_eq!(meta.bidi.mode, BidiMode::Explicit);
        assert!(!meta.saw_ris);
        assert_eq!(cell(&t, 0), 'c');
    }

    /// Two alt-screen sequences in one read can leave the mode bit where it
    /// started. Bidi still resets, because each CSI is seen, not the net.
    #[test]
    fn double_alt_screen_toggle_in_one_chunk_resets() {
        let (mut t, mut p, mut meta, mut carry) = fresh();
        running(&mut meta);
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            b"\x1b[8l\x1b[2 k\x1b[?1049h\x1b[?1049l",
        );
        assert!(!t.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(meta.bidi.mode, BidiMode::Implicit);
        assert_eq!(meta.bidi.direction, ParagraphDir::Default);
        assert_eq!(meta.bidi.direction_at(1), ParagraphDir::Default);

        // 47 and 1047 are the other alt sequences. alacritty ignores them
        // as unknown private modes; we still reset on the CSI itself.
        running(&mut meta);
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            b"\x1b[8l\x1b[2 k\x1b[?47h\x1b[?47l\x1b[?1047h\x1b[?1047l",
        );
        assert!(!t.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(meta.bidi.mode, BidiMode::Implicit);
        assert_eq!(meta.bidi.direction, ParagraphDir::Default);
    }

    /// A resize reflows rows. Stamps are dropped; the mode stays, because a
    /// full-screen program does not resend BDSM just because the window
    /// changed size. `Session::resize` is the caller.
    #[test]
    fn resize_clears_scp_stamps_and_keeps_mode() {
        let mut state = BidiState {
            mode: BidiMode::Explicit,
            direction: ParagraphDir::Default,
            stamps: Vec::new(),
        };
        state.note_scp(ParagraphDir::Rtl, 3, 0);
        state.note_scp(ParagraphDir::Ltr, 8, 0);
        assert_eq!(state.direction_at(3), ParagraphDir::Rtl);
        assert_eq!(state.direction_at(9), ParagraphDir::Ltr);
        state.on_resize();
        assert_eq!(state.mode, BidiMode::Explicit);
        assert_eq!(state.direction, ParagraphDir::Default);
        assert_eq!(state.direction_at(3), ParagraphDir::Default);
        assert_eq!(state.direction_at(9), ParagraphDir::Default);
    }

    /// Random chunks: the filter's RIS count, and the alt-screen CSIs it
    /// passes through, match a vte parser on the same bytes. BDSM and SCP
    /// are stripped; they are not RIS and not an alt-screen mode.
    #[test]
    fn filter_matches_vte_on_random_streams() {
        use alacritty_terminal::vte::{Params, Parser, Perform};

        struct Watch {
            ris: usize,
            alt: usize,
        }
        impl Perform for Watch {
            fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
                if !ignore && intermediates.is_empty() && byte == b'c' {
                    self.ris += 1;
                }
            }
            fn csi_dispatch(
                &mut self,
                params: &Params,
                intermediates: &[u8],
                ignore: bool,
                action: char,
            ) {
                if ignore || intermediates != b"?" || !matches!(action, 'h' | 'l') {
                    return;
                }
                let hit = params.iter().any(|p| {
                    matches!(p.first().copied(), Some(1049 | 47 | 1047))
                });
                if hit {
                    self.alt += 1;
                }
            }
        }

        let mut rng = 0xC0FFEE_u64;
        let mut next = || {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            (rng >> 33) as u8
        };

        for case in 0..32 {
            let mut filter = StreamFilter::default();
            let mut vte_raw = Parser::new();
            let mut raw_w = Watch { ris: 0, alt: 0 };
            let mut vte_filt = Parser::new();
            let mut filt_w = Watch { ris: 0, alt: 0 };
            let mut ris_items = 0usize;
            let nchunks = 2 + (next() as usize % 3);
            for _ in 0..nchunks {
                let len = next() as usize % 40;
                let chunk: Vec<u8> = (0..len).map(|_| next()).collect();
                vte_raw.advance(&mut raw_w, &chunk);
                for item in filter.pull(&chunk) {
                    match item {
                        StreamItem::Ris => ris_items += 1,
                        StreamItem::Bytes(b) => vte_filt.advance(&mut filt_w, &b),
                        StreamItem::Bdsm(_) | StreamItem::Scp(_) => {}
                    }
                }
            }
            assert_eq!(ris_items, raw_w.ris, "RIS items, case {case}");
            assert_eq!(filt_w.ris, raw_w.ris, "RIS bytes passed through, case {case}");
            assert_eq!(filt_w.alt, raw_w.alt, "alt-screen CSI, case {case}");
        }
    }
}
