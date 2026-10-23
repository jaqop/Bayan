//! Mouse reporting and focus events for the program inside the pane.
//!
//! xterm's tracking modes are mutually exclusive in alacritty_terminal
//! (setting one clears the others): 1000 is clicks, 1002 is clicks plus
//! motion while a button is held, 1003 is clicks plus every motion.
//! 1006 selects SGR encoding. Without it the bytes are the legacy X10
//! form, which only fits coordinates and button codes 1..=95: the wire
//! byte is that value plus 32, and ConPTY's UTF-8 pipe turns 0x80 and
//! above into U+FFFD, so the historical 223 cap lands on the wrong cell.
//! Mode 1005 (UTF-8 coordinates) is not encoded — silence, never a
//! fallback to X10. 1004 is independent: the focused pane hears CSI I,
//! the one that lost focus hears CSI O.
//!
//! Shift bypasses all of it, including the wheel on the alt screen: the
//! terminal keeps the gesture for its own selection and its own
//! scrollback. Buttons past the three primaries (codes >= 128) and the
//! horizontal wheel are not reported.

use alacritty_terminal::term::TermMode;

/// Which button the report is about. `None` is motion with nothing held
/// (1003) and the X10 release code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Btn {
    Left,
    Middle,
    Right,
    None,
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Press,
    Release,
    Move,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// What the program asked to hear. Built from `TermMode` so the flag
/// names stay the emulator's, not a second copy of the bit numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Proto {
    /// 1000 SET_VT200_MOUSE — press and release.
    pub clicks: bool,
    /// 1002 SET_BTN_EVENT_MOUSE — motion while a button is held.
    pub drag: bool,
    /// 1003 SET_ANY_EVENT_MOUSE — motion with no button too.
    pub motion: bool,
    /// 1006 SGR encoding.
    pub sgr: bool,
    /// 1005 UTF-8 coordinates. Not encoded; must not fall through to X10.
    pub utf8: bool,
    /// 1004 focus in/out.
    pub focus: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    Click,
    Drag,
    Motion,
    Wheel,
}

impl Proto {
    pub fn from_mode(mode: TermMode) -> Self {
        Self {
            clicks: mode.contains(TermMode::MOUSE_REPORT_CLICK),
            drag: mode.contains(TermMode::MOUSE_DRAG),
            motion: mode.contains(TermMode::MOUSE_MOTION),
            sgr: mode.contains(TermMode::SGR_MOUSE),
            utf8: mode.contains(TermMode::UTF8_MOUSE),
            focus: mode.contains(TermMode::FOCUS_IN_OUT),
        }
    }

    /// Which bytes `encode` may emit. SGR wins if both encoding flags are
    /// set; 1005 is its own case so a caller cannot treat it as X10.
    pub fn wire(&self) -> Wire {
        if self.sgr {
            Wire::Sgr
        } else if self.utf8 {
            Wire::Utf8
        } else {
            Wire::X10
        }
    }

    /// Any of 1000/1002/1003. 1006 alone encodes nothing.
    pub fn tracking(&self) -> bool {
        self.clicks || self.drag || self.motion
    }

    /// `shift` is the bypass: the terminal handles the gesture itself.
    /// 1002 and 1003 include clicks even though alacritty stores them as
    /// a single flag (the modes are mutually exclusive there).
    pub fn wants(&self, what: What, shift: bool) -> bool {
        if shift || !self.tracking() {
            return false;
        }
        match what {
            What::Click | What::Wheel => true,
            What::Drag => self.drag || self.motion,
            What::Motion => self.motion,
        }
    }
}

/// CSI I on focus, CSI O on blur. The caller checks mode 1004.
pub fn focus_seq(focused: bool) -> &'static [u8] {
    if focused {
        b"\x1b[I"
    } else {
        b"\x1b[O"
    }
}

fn base_code(button: Btn, phase: Phase) -> Option<u16> {
    // Wheel buttons have a press and no release (xterm ctlseqs).
    match (button, phase) {
        (Btn::WheelUp | Btn::WheelDown, Phase::Release | Phase::Move) => return None,
        (Btn::WheelUp, Phase::Press) => return Some(64),
        (Btn::WheelDown, Phase::Press) => return Some(65),
        _ => {}
    }
    let b = match button {
        Btn::Left => 0,
        Btn::Middle => 1,
        Btn::Right => 2,
        Btn::None => 3,
        Btn::WheelUp | Btn::WheelDown => return None,
    };
    Some(match phase {
        Phase::Press | Phase::Release => b,
        Phase::Move => b + 32,
    })
}

fn with_mods(code: u16, mods: Mods) -> u16 {
    let mut c = code;
    if mods.shift {
        c += 4;
    }
    if mods.alt {
        c += 8;
    }
    if mods.ctrl {
        c += 16;
    }
    c
}

/// How a report is framed. `Utf8` is mode 1005: [`encode_wire`] returns
/// `None` rather than inventing an X10 report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    Sgr,
    X10,
    Utf8,
}

/// Largest X10 coordinate or button code. The wire byte is this plus 32,
/// which keeps it in 7-bit ASCII. Past that, ConPTY's UTF-8 input turns
/// the byte into U+FFFD and the program reads a different cell.
const X10_MAX: u16 = 95;

/// One report for `proto`'s encoding. Mode 1005 yields `None`.
pub fn encode_wire(
    wire: Wire,
    phase: Phase,
    button: Btn,
    col: usize,
    row: usize,
    mods: Mods,
) -> Option<Vec<u8>> {
    match wire {
        Wire::Sgr => encode(true, phase, button, col, row, mods),
        Wire::X10 => encode(false, phase, button, col, row, mods),
        Wire::Utf8 => None,
    }
}

/// One report. `col` and `row` are 1-based, matching what the program
/// counts. `None` when the event has no encoding (a wheel release, or an
/// X10 value past [`X10_MAX`] — clamping would land the click on the
/// wrong cell). This is the SGR/X10 encoder; mode 1005 goes through
/// [`encode_wire`] and does not arrive here.
pub fn encode(
    sgr: bool,
    phase: Phase,
    button: Btn,
    col: usize,
    row: usize,
    mods: Mods,
) -> Option<Vec<u8>> {
    let col = col.max(1);
    let row = row.max(1);
    let code = with_mods(base_code(button, phase)?, mods);
    if sgr {
        let tail = if phase == Phase::Release { 'm' } else { 'M' };
        Some(format!("\x1b[<{code};{col};{row}{tail}").into_bytes())
    } else {
        // X10 release is always button 3; modifiers survive, the motion
        // bit does not. The pressed button is not in the byte.
        let x10 = if phase == Phase::Release {
            with_mods(3, mods)
        } else {
            code
        };
        if col > X10_MAX as usize || row > X10_MAX as usize || x10 > X10_MAX {
            return None;
        }
        Some(vec![
            0x1b,
            b'[',
            b'M',
            x10 as u8 + 32,
            col as u8 + 32,
            row as u8 + 32,
        ])
    }
}

/// A window pixel that landed in a pane's content rect.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellHit {
    pub pane: usize,
    /// 0-based viewport row.
    pub row: usize,
    /// 0-based column from the pane's left edge (`x / cell_w`), before
    /// any bidi remap. Grid rows use this as the logical column.
    pub col: usize,
    /// Left half of the cell. Selection anchors care; reports don't.
    pub left: bool,
}

/// Pixel → cell, in window coordinates.
///
/// `rects` are pane content rects already inset by the padding and split
/// across the content area, so a click in the padding or in a divider gap
/// is not a cell. The column is relative to that pane's origin: the second
/// pane's first cell is column 0, not a column of the first pane.
pub fn cell_at(
    x: f64,
    y: f64,
    rects: &[(i32, i32, i32, i32)],
    cell_w: f64,
    cell_h: f64,
) -> Option<CellHit> {
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return None;
    }
    let (pane, &(rx, ry, rw, rh)) = rects.iter().enumerate().find(|(_, (rx, ry, rw, rh))| {
        x >= *rx as f64 && x < (*rx + *rw) as f64 && y >= *ry as f64 && y < (*ry + *rh) as f64
    })?;
    if rw <= 0 || rh <= 0 {
        return None;
    }
    let lx = x - rx as f64;
    let ly = y - ry as f64;
    // floor matches the PTY's column count (`width / cell` truncated).
    // A partial cell at the edge clamps onto the last real column.
    let cols = ((rw as f64 / cell_w).floor() as usize).max(1);
    let rows = ((rh as f64 / cell_h).floor() as usize).max(1);
    let colf = lx / cell_w;
    let rowf = ly / cell_h;
    let col = (colf.floor().max(0.0) as usize).min(cols - 1);
    let row = (rowf.floor().max(0.0) as usize).min(rows - 1);
    let left = colf - col as f64 <= 0.5;
    Some(CellHit {
        pane,
        row,
        col,
        left,
    })
}

/// One shaped glyph, in pane-local pixels before `scale` (the compression
/// `draw_line_bidi` applies when a line is wider than its pane).
#[derive(Clone, Copy, Debug)]
pub struct GlyphSpan {
    pub x: f32,
    pub w: f32,
    pub start: usize,
    pub end: usize,
    pub rtl: bool,
}

/// A glyph's pixel box and the grid column it belongs to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlyphBox {
    pub x0: f32,
    pub x1: f32,
    pub col: usize,
}

/// Map a byte offset in the shaped string to a grid column.
pub fn col_for_byte(spans: &[(usize, usize, usize)], byte: usize) -> Option<usize> {
    spans
        .iter()
        .find(|&&(a, b, _)| byte >= a && byte < b)
        .map(|&(_, _, c)| c)
}

/// Glyph spans → boxes in pane-local pixels. A glyph that covers N
/// columns is split into N equal slices; an RTL glyph's leftmost slice
/// is the highest logical column (the end of its byte range).
pub fn boxes_for_glyphs(
    glyphs: &[GlyphSpan],
    scale: f32,
    mut col_for_byte: impl FnMut(usize) -> Option<usize>,
) -> Vec<GlyphBox> {
    let mut out = Vec::new();
    if scale <= 0.0 {
        return out;
    }
    for g in glyphs {
        if g.w <= 0.0 {
            continue;
        }
        let x0 = g.x * scale;
        let x1 = (g.x + g.w) * scale;
        if x1 <= x0 {
            continue;
        }
        let Some(c_start) = col_for_byte(g.start) else {
            continue;
        };
        let c_end = col_for_byte(g.end.saturating_sub(1)).unwrap_or(c_start);
        let (lo, hi) = if c_start <= c_end {
            (c_start, c_end)
        } else {
            (c_end, c_start)
        };
        let n = hi - lo + 1;
        let width = x1 - x0;
        for i in 0..n {
            let col = if g.rtl { hi - i } else { lo + i };
            let a = x0 + width * (i as f32) / (n as f32);
            let b = x0 + width * ((i + 1) as f32) / (n as f32);
            out.push(GlyphBox { x0: a, x1: b, col });
        }
    }
    out
}

fn nearest_col(boxes: &[GlyphBox], x: f32) -> Option<usize> {
    boxes
        .iter()
        .min_by(|a, b| {
            let da = (x - (a.x0 + a.x1) * 0.5).abs();
            let db = (x - (b.x0 + b.x1) * 0.5).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|b| b.col)
}

/// Logical grid column under pane-local `x`.
///
/// Boxes come from the shaped line (the same buffer the bidi path draws),
/// so an RTL row reports the column of the glyph under the pointer rather
/// than `x / cell_w`. Past the shaped run, trailing blanks — which the
/// bidi path does not shape — step by `cell_w` from `first_blank`.
pub fn column_for_click(
    x: f32,
    boxes: &[GlyphBox],
    cell_w: f32,
    first_blank: usize,
    last_col: usize,
    fallback: usize,
) -> usize {
    let clamp = |c: usize| c.min(last_col);
    if boxes.is_empty() {
        return clamp(fallback);
    }
    if let Some(b) = boxes.iter().find(|b| x >= b.x0 && x < b.x1) {
        return clamp(b.col);
    }
    let min_x = boxes.iter().map(|b| b.x0).fold(f32::MAX, f32::min);
    let max_x = boxes.iter().map(|b| b.x1).fold(f32::MIN, f32::max);
    if x >= max_x {
        if first_blank <= last_col {
            if cell_w <= 0.0 {
                return clamp(first_blank);
            }
            let extra = ((x - max_x) / cell_w).floor().max(0.0) as usize;
            return clamp(first_blank.saturating_add(extra));
        }
        // The line filled the grid; the click is just past the last glyph.
        return clamp(
            boxes
                .iter()
                .max_by(|a, b| a.x1.partial_cmp(&b.x1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|b| b.col)
                .unwrap_or(fallback),
        );
    }
    if x < min_x {
        return clamp(
            boxes
                .iter()
                .min_by(|a, b| a.x0.partial_cmp(&b.x0).unwrap_or(std::cmp::Ordering::Equal))
                .map(|b| b.col)
                .unwrap_or(fallback),
        );
    }
    clamp(nearest_col(boxes, x).unwrap_or(fallback))
}

/// One pane's mouse-relevant mode, for wheel routing.
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub id: u64,
    pub proto: Proto,
    pub alt_screen: bool,
    pub app_cursor: bool,
}

/// A grid cell the pointer is actually over. Absent means the tab bar,
/// the padding, or a split gap — not cell 1;1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub pane: u64,
    pub row: usize,
    pub col: usize,
}

/// Bytes the router wants written to one pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Out {
    pub pane: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Grab {
    pane_id: u64,
    /// bit 0 left, bit 1 middle, bit 2 right
    down: u8,
}

fn button_bit(button: Btn) -> u8 {
    match button {
        Btn::Left => 1,
        Btn::Middle => 2,
        Btn::Right => 4,
        _ => 0,
    }
}

fn grab_button(down: u8) -> Btn {
    if down & 1 != 0 {
        Btn::Left
    } else if down & 2 != 0 {
        Btn::Middle
    } else {
        Btn::Right
    }
}

/// What a button-up did with a grab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Take {
    /// This button was not one we reported.
    Ignore,
    /// We owned it. `None` when tracking is already off, or the wire
    /// cannot carry the release (mode 1005, X10 past 95): the grab is
    /// still dropped, and no release byte is written.
    Consumed(Option<Out>),
}

/// Where a wheel notch goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wheel {
    Report { pane: u64, bytes: Vec<u8> },
    Arrows { pane: u64, bytes: Vec<u8> },
    Scroll { pane: u64, lines: i32 },
}

/// Grab, motion dedupe, per-pane wheel choice, and which pane was told
/// it has focus. The window asks this instead of keeping the rules in
/// the event match, so a focus loss can be tested without a window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Route {
    grab: Option<Grab>,
    last: Option<(u64, usize, usize)>,
    focus_sent: Option<u64>,
    /// Local selection drag. Cleared with the grab so Alt+Tab cannot
    /// keep painting a selection in a pane the pointer has left.
    pub selecting: bool,
}

impl Route {
    pub fn grabbed_pane(&self) -> Option<u64> {
        self.grab.map(|g| g.pane_id)
    }

    pub fn focus_sent(&self) -> Option<u64> {
        self.focus_sent
    }

    /// A press the program should hear. `Pass`-equivalent is `None`:
    /// Shift, or tracking off, and nothing is grabbed. `Some(None)` is
    /// mode 1005: the gesture is ours, but there is no wire form.
    pub fn press(
        &mut self,
        cell: Cell,
        button: Btn,
        proto: Proto,
        mods: Mods,
    ) -> Option<Option<Out>> {
        if !proto.wants(What::Click, mods.shift) {
            return None;
        }
        let bit = button_bit(button);
        if self.grab.as_ref().is_some_and(|g| g.pane_id == cell.pane) {
            if let Some(g) = self.grab.as_mut() {
                g.down |= bit;
            }
        } else {
            self.grab = Some(Grab {
                pane_id: cell.pane,
                down: bit,
            });
        }
        self.last = Some((cell.pane, cell.row, cell.col));
        Some(self.report(
            cell.pane,
            Phase::Press,
            button,
            (cell.row, cell.col),
            proto,
            mods,
        ))
    }

    /// Button up for a press we reported. Tracking that has since turned
    /// off consumes the grab and writes nothing.
    pub fn release(&mut self, button: Btn, cell: Option<Cell>, proto: Proto, mods: Mods) -> Take {
        let bit = button_bit(button);
        let Some(grab) = self.grab else {
            return Take::Ignore;
        };
        if bit == 0 || grab.down & bit == 0 {
            return Take::Ignore;
        }
        let pane = grab.pane_id;
        let down = grab.down & !bit;
        if down == 0 {
            self.grab = None;
            self.last = None;
        } else if let Some(g) = self.grab.as_mut() {
            g.down = down;
        }
        if !proto.tracking() {
            return Take::Consumed(None);
        }
        let Some(cell) = cell else {
            return Take::Consumed(None);
        };
        Take::Consumed(self.report(
            pane,
            Phase::Release,
            button,
            (cell.row, cell.col),
            proto,
            mods,
        ))
    }

    /// Motion while a reported button is held (1002/1003). Same cell
    /// twice produces one report.
    pub fn drag(&mut self, cell: Cell, proto: Proto, mods: Mods) -> Option<Out> {
        let button = grab_button(self.grab?.down);
        self.moved(cell, button, What::Drag, proto, mods)
    }

    /// Motion with no button (1003). Same cell twice produces one report.
    pub fn motion(&mut self, cell: Cell, proto: Proto, mods: Mods) -> Option<Out> {
        self.moved(cell, Btn::None, What::Motion, proto, mods)
    }

    fn moved(
        &mut self,
        cell: Cell,
        button: Btn,
        what: What,
        proto: Proto,
        mods: Mods,
    ) -> Option<Out> {
        if !proto.wants(what, mods.shift) {
            return None;
        }
        if self.last == Some((cell.pane, cell.row, cell.col)) {
            return None;
        }
        self.last = Some((cell.pane, cell.row, cell.col));
        self.report(
            cell.pane,
            Phase::Move,
            button,
            (cell.row, cell.col),
            proto,
            mods,
        )
    }

    /// Focus loss, tab switch, or pane close. A still-tracking program
    /// hears a release at the last cell; either way the grab, the last
    /// cell, and the local selection drag are cleared.
    pub fn abandon(&mut self, proto: Proto, mods: Mods) -> Vec<Out> {
        let grab = self.grab.take();
        let last = self.last.take();
        self.selecting = false;
        let Some(grab) = grab else {
            return Vec::new();
        };
        if !proto.tracking() {
            return Vec::new();
        }
        let Some((_, row, col)) = last else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for button in [Btn::Left, Btn::Middle, Btn::Right] {
            if grab.down & button_bit(button) == 0 {
                continue;
            }
            if let Some(bytes) = self.report(
                grab.pane_id,
                Phase::Release,
                button,
                (row, col),
                proto,
                mods,
            ) {
                out.push(bytes);
            }
        }
        out
    }

    /// RIS forgot the program's idea of focus. The next [`Self::apply_focus`]
    /// sends the current state again, including CSI I when 1004 comes back
    /// in the same chunk.
    pub fn note_ris(&mut self, pane: u64) {
        if self.focus_sent == Some(pane) {
            self.focus_sent = None;
        }
    }

    /// Test entry. The window calls [`Self::apply_focus`] after it has
    /// read each pane's mode, so this predicate form stays out of the
    /// binary.
    #[cfg(test)]
    pub fn sync_focus(
        &mut self,
        window_focused: bool,
        focused_pane: Option<u64>,
        mut wants: impl FnMut(u64) -> bool,
    ) -> Vec<Out> {
        let new_reports = focused_pane.is_some_and(&mut wants);
        let old_reports = self.focus_sent.is_some_and(wants);
        self.apply_focus(window_focused, focused_pane, new_reports, old_reports)
    }

    pub fn apply_focus(
        &mut self,
        window_focused: bool,
        focused_pane: Option<u64>,
        new_reports: bool,
        old_reports: bool,
    ) -> Vec<Out> {
        let want = if window_focused && new_reports {
            focused_pane
        } else {
            None
        };
        if want == self.focus_sent {
            return Vec::new();
        }
        let old = self.focus_sent;
        self.focus_sent = want;
        let mut out = Vec::new();
        if let Some(old) = old {
            if old_reports {
                out.push(Out {
                    pane: old,
                    bytes: focus_seq(false).to_vec(),
                });
            }
        }
        if let Some(id) = want {
            out.push(Out {
                pane: id,
                bytes: focus_seq(true).to_vec(),
            });
        }
        out
    }

    /// Wheel notch. `at` is the cell under the pointer; `None` is outside
    /// every pane grid, and a tracking program does not hear cell 1;1.
    /// Shift scrolls that pane's history even on the alt screen.
    /// `view` describes the pane that owns the gesture: the one under the
    /// pointer when `at` is `Some`, otherwise the focused pane.
    pub fn wheel(at: Option<Cell>, view: View, n: i32, mods: Mods) -> Option<Wheel> {
        if n == 0 {
            return None;
        }
        let pane = at.map(|c| c.pane).unwrap_or(view.id);
        if mods.shift {
            return Some(Wheel::Scroll { pane, lines: n });
        }
        if view.proto.wants(What::Wheel, false) {
            let cell = at?;
            let button = if n > 0 { Btn::WheelUp } else { Btn::WheelDown };
            let report_mods = Mods {
                shift: false,
                alt: mods.alt,
                ctrl: false,
            };
            let seq = encode_wire(
                view.proto.wire(),
                Phase::Press,
                button,
                cell.col + 1,
                cell.row + 1,
                report_mods,
            )?;
            let mut bytes = Vec::with_capacity(seq.len() * n.unsigned_abs() as usize);
            for _ in 0..n.unsigned_abs() {
                bytes.extend_from_slice(&seq);
            }
            return Some(Wheel::Report {
                pane: cell.pane,
                bytes,
            });
        }
        if view.alt_screen {
            let ch = if n > 0 { b'A' } else { b'B' };
            let prefix: &[u8] = if view.app_cursor { b"\x1bO" } else { b"\x1b[" };
            let mut bytes = Vec::new();
            for _ in 0..n.unsigned_abs() {
                bytes.extend_from_slice(prefix);
                bytes.push(ch);
            }
            return Some(Wheel::Arrows { pane, bytes });
        }
        Some(Wheel::Scroll { pane, lines: n })
    }

    fn report(
        &self,
        pane: u64,
        phase: Phase,
        button: Btn,
        at: (usize, usize),
        proto: Proto,
        mods: Mods,
    ) -> Option<Out> {
        let (row, col) = at;
        encode_wire(proto.wire(), phase, button, col + 1, row + 1, mods)
            .map(|bytes| Out { pane, bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(shift: bool, alt: bool, ctrl: bool) -> Mods {
        Mods { shift, alt, ctrl }
    }

    #[test]
    fn sgr_press_release_motion_and_wheel() {
        let m = Mods::default();
        // left press, 1-based cell the program counts
        assert_eq!(
            encode(true, Phase::Press, Btn::Left, 1, 1, m).unwrap(),
            b"\x1b[<0;1;1M"
        );
        // release keeps the button number and switches the final byte
        assert_eq!(
            encode(true, Phase::Release, Btn::Left, 4, 2, m).unwrap(),
            b"\x1b[<0;4;2m"
        );
        assert_eq!(
            encode(true, Phase::Press, Btn::Right, 8, 3, m).unwrap(),
            b"\x1b[<2;8;3M"
        );
        // drag motion: button + 32
        assert_eq!(
            encode(true, Phase::Move, Btn::Left, 3, 4, m).unwrap(),
            b"\x1b[<32;3;4M"
        );
        // 1003 motion with nothing held is button 3 + 32
        assert_eq!(
            encode(true, Phase::Move, Btn::None, 3, 4, m).unwrap(),
            b"\x1b[<35;3;4M"
        );
        assert_eq!(
            encode(true, Phase::Press, Btn::WheelUp, 5, 6, m).unwrap(),
            b"\x1b[<64;5;6M"
        );
        assert_eq!(
            encode(true, Phase::Press, Btn::WheelDown, 5, 6, m).unwrap(),
            b"\x1b[<65;5;6M"
        );
        // no wheel release in the protocol
        assert!(encode(true, Phase::Release, Btn::WheelUp, 1, 1, m).is_none());
        // coordinates are unbounded in SGR, and a 0 clamps up to 1
        assert_eq!(
            encode(true, Phase::Press, Btn::Left, 0, 0, m).unwrap(),
            b"\x1b[<0;1;1M"
        );
        assert_eq!(
            encode(true, Phase::Press, Btn::Middle, 1000, 200, m).unwrap(),
            b"\x1b[<1;1000;200M"
        );
    }

    #[test]
    fn modifier_bits_are_shift_alt_ctrl() {
        // 2 (right) + 4 (shift) + 8 (alt) + 16 (ctrl) = 30
        let seq = encode(true, Phase::Press, Btn::Right, 1, 1, mods(true, true, true)).unwrap();
        assert_eq!(seq, b"\x1b[<30;1;1M");
        // wheel up + ctrl = 80
        assert_eq!(
            encode(
                true,
                Phase::Press,
                Btn::WheelUp,
                2,
                3,
                mods(false, false, true)
            )
            .unwrap(),
            b"\x1b[<80;2;3M"
        );
    }

    #[test]
    fn x10_is_the_legacy_fallback_and_stops_at_95() {
        let m = Mods::default();
        // Cb/Cx/Cy = value + 32. Column 1 row 1 left press → bytes 32, 33, 33.
        assert_eq!(
            encode(false, Phase::Press, Btn::Left, 1, 1, m).unwrap(),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
        // release is button 3 (byte 35), not the button that went up
        assert_eq!(
            encode(false, Phase::Release, Btn::Right, 1, 1, m).unwrap(),
            vec![0x1b, b'[', b'M', 35, 33, 33]
        );
        // ctrl survives on the release (3 + 16 = 19, byte 51)
        assert_eq!(
            encode(
                false,
                Phase::Release,
                Btn::Left,
                2,
                3,
                mods(false, false, true)
            )
            .unwrap(),
            vec![0x1b, b'[', b'M', 51, 34, 35]
        );
        // motion with no button: code 35, byte 67
        assert_eq!(
            encode(false, Phase::Move, Btn::None, 1, 1, m).unwrap(),
            vec![0x1b, b'[', b'M', 67, 33, 33]
        );
        // 95 is byte 127, the last value ConPTY's UTF-8 pipe will not
        // replace. 96 would be 0x80 and must be dropped, not clamped.
        assert!(encode(false, Phase::Press, Btn::Left, 95, 1, m).is_some());
        assert!(encode(false, Phase::Press, Btn::Left, 96, 1, m).is_none());
        assert!(encode(false, Phase::Press, Btn::Left, 1, 96, m).is_none());
        assert!(encode(false, Phase::Press, Btn::Left, 223, 1, m).is_none());
    }

    #[test]
    fn utf8_mouse_is_not_silently_x10() {
        let utf8 = Proto::from_mode(TermMode::UTF8_MOUSE | TermMode::MOUSE_REPORT_CLICK);
        assert!(utf8.utf8 && !utf8.sgr);
        assert_eq!(utf8.wire(), Wire::Utf8);
        assert!(encode_wire(Wire::Utf8, Phase::Press, Btn::Left, 1, 1, Mods::default()).is_none());
        // a tracking press is still ours, it just has no bytes
        let mut route = Route::default();
        let cell = Cell {
            pane: 1,
            row: 0,
            col: 0,
        };
        assert_eq!(
            route.press(cell, Btn::Left, utf8, Mods::default()),
            Some(None)
        );
        assert_eq!(route.grabbed_pane(), Some(1));
    }

    #[test]
    fn focus_sequences_are_csi_i_and_o() {
        assert_eq!(focus_seq(true), b"\x1b[I");
        assert_eq!(focus_seq(false), b"\x1b[O");
    }

    #[test]
    fn tracking_modes_and_the_shift_bypass() {
        let click = Proto::from_mode(TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE);
        assert!(click.sgr && click.clicks && !click.drag && !click.motion);
        assert!(click.wants(What::Click, false));
        assert!(click.wants(What::Wheel, false));
        assert!(!click.wants(What::Drag, false));
        assert!(!click.wants(What::Motion, false));
        // shift is the terminal's gesture, even in a tracking mode
        assert!(!click.wants(What::Click, true));
        assert!(!click.wants(What::Wheel, true));

        // 1002 is stored alone (alacritty clears the other mouse flags)
        // but it still reports clicks and drags, not free motion
        let drag = Proto::from_mode(TermMode::MOUSE_DRAG);
        assert!(drag.wants(What::Click, false));
        assert!(drag.wants(What::Drag, false));
        assert!(!drag.wants(What::Motion, false));
        assert!(!drag.sgr, "no 1006 → X10");

        let any = Proto::from_mode(TermMode::MOUSE_MOTION | TermMode::SGR_MOUSE);
        assert!(any.wants(What::Motion, false));
        assert!(any.wants(What::Drag, false));

        // 1006 or 1004 alone do not turn tracking on
        let enc = Proto::from_mode(TermMode::SGR_MOUSE | TermMode::FOCUS_IN_OUT);
        assert!(enc.focus && enc.sgr);
        assert!(!enc.tracking());
        assert!(!enc.wants(What::Click, false));
        assert!(!Proto::from_mode(TermMode::ALT_SCREEN).wants(What::Wheel, false));
    }

    #[test]
    fn padding_and_splits_change_which_cell_a_pixel_is() {
        // tab bar 30px, padding 8: the content rect does not start at the
        // window origin. cell is 10×20.
        let one = [(8, 38, 184, 54)];
        assert!(cell_at(20.0, 10.0, &one, 10.0, 20.0).is_none(), "tab bar");
        assert!(
            cell_at(3.0, 50.0, &one, 10.0, 20.0).is_none(),
            "left padding"
        );
        assert!(
            cell_at(8.0, 37.0, &one, 10.0, 20.0).is_none(),
            "top padding"
        );
        let origin = cell_at(8.0, 38.0, &one, 10.0, 20.0).unwrap();
        assert_eq!((origin.pane, origin.row, origin.col), (0, 0, 0));
        assert!(origin.left);
        // one cell right and one row down. 16px is the right half of
        // column 1 (the midpoint, 15px, still counts as the left side).
        let inner = cell_at(8.0 + 16.0, 38.0 + 25.0, &one, 10.0, 20.0).unwrap();
        assert_eq!((inner.row, inner.col), (1, 1));
        assert!(!inner.left);
        // the partial cell past the last full column clamps, it doesn't invent one
        let edge = cell_at(8.0 + 183.0, 38.0 + 53.0, &one, 10.0, 20.0).unwrap();
        assert_eq!(edge.col, 17); // floor(184/10) - 1
        assert_eq!(edge.row, 1); // floor(54/20) - 1

        // two panes, 1px gap. A click in the second pane is its column 0.
        let split = [(8, 38, 90, 54), (99, 38, 93, 54)];
        assert!(
            cell_at(98.5, 50.0, &split, 10.0, 20.0).is_none(),
            "divider gap"
        );
        let second = cell_at(99.0 + 5.0, 40.0, &split, 10.0, 20.0).unwrap();
        assert_eq!((second.pane, second.col, second.row), (1, 0, 0));
        let first = cell_at(8.0 + 85.0, 40.0, &split, 10.0, 20.0).unwrap();
        assert_eq!(first.pane, 0);
        assert_ne!(
            first.col,
            second.col + 9,
            "pane 1 is not a continuation of pane 0"
        );
    }

    #[test]
    fn rtl_boxes_map_the_visual_left_to_the_last_logical_column() {
        // logical 0 drawn on the right, logical 4 on the left — what
        // cosmic-text does with an RTL word anchored at x = 0.
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
        assert_eq!(column_for_click(2.0, &boxes, 10.0, 5, 80, 0), 4);
        assert_eq!(column_for_click(45.0, &boxes, 10.0, 5, 80, 0), 0);
        assert_eq!(column_for_click(15.0, &boxes, 10.0, 5, 80, 0), 3);
        // past the shaped run, trailing blanks step by cell width
        assert_eq!(column_for_click(50.0, &boxes, 10.0, 5, 80, 0), 5);
        assert_eq!(column_for_click(72.0, &boxes, 10.0, 5, 80, 0), 7);
        // and they don't run off the grid
        assert_eq!(column_for_click(500.0, &boxes, 10.0, 5, 9, 0), 9);
    }

    #[test]
    fn ltr_digit_boxes_are_not_mirrored() {
        // ١٧٥ laid out left to right: a mirror would report the last digit.
        let boxes = [
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
            GlyphBox {
                x0: 20.0,
                x1: 30.0,
                col: 2,
            },
        ];
        assert_eq!(column_for_click(1.0, &boxes, 10.0, 3, 80, 0), 0);
        assert_eq!(column_for_click(25.0, &boxes, 10.0, 3, 80, 0), 2);
    }

    #[test]
    fn glyph_spans_split_wide_clusters_and_honor_scale() {
        let spans = [(0, 2, 0), (2, 4, 1)];
        let glyphs = [GlyphSpan {
            x: 0.0,
            w: 20.0,
            start: 0,
            end: 4,
            rtl: false,
        }];
        let boxes = boxes_for_glyphs(&glyphs, 0.5, |b| col_for_byte(&spans, b));
        // scale 0.5 → the 20px glyph is 10px, split into the two columns
        assert_eq!(
            boxes,
            vec![
                GlyphBox {
                    x0: 0.0,
                    x1: 5.0,
                    col: 0
                },
                GlyphBox {
                    x0: 5.0,
                    x1: 10.0,
                    col: 1
                },
            ]
        );
        // RTL: the left half is the end of the byte range
        let rtl = [GlyphSpan {
            x: 0.0,
            w: 20.0,
            start: 0,
            end: 4,
            rtl: true,
        }];
        let boxes = boxes_for_glyphs(&rtl, 1.0, |b| col_for_byte(&spans, b));
        assert_eq!(boxes[0].col, 1);
        assert_eq!(boxes[1].col, 0);
        // three columns, equal slices, not a pair of halves
        let wide = [(0usize, 1, 0), (1, 2, 1), (2, 3, 2)];
        let glyph = [GlyphSpan {
            x: 0.0,
            w: 30.0,
            start: 0,
            end: 3,
            rtl: false,
        }];
        let boxes = boxes_for_glyphs(&glyph, 1.0, |b| col_for_byte(&wide, b));
        assert_eq!(
            boxes,
            vec![
                GlyphBox {
                    x0: 0.0,
                    x1: 10.0,
                    col: 0
                },
                GlyphBox {
                    x0: 10.0,
                    x1: 20.0,
                    col: 1
                },
                GlyphBox {
                    x0: 20.0,
                    x1: 30.0,
                    col: 2
                },
            ]
        );
    }

    fn click_proto() -> Proto {
        Proto::from_mode(TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE)
    }

    #[test]
    fn focus_loss_releases_a_held_button() {
        let mut route = Route::default();
        let cell = Cell {
            pane: 7,
            row: 2,
            col: 3,
        };
        let proto = click_proto();
        let sent = route
            .press(cell, Btn::Left, proto, Mods::default())
            .unwrap();
        assert_eq!(sent.unwrap().bytes, b"\x1b[<0;4;3M");
        route.selecting = true;
        let out = route.abandon(proto, Mods::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pane, 7);
        assert_eq!(out[0].bytes, b"\x1b[<0;4;3m");
        assert_eq!(route.grabbed_pane(), None);
        assert!(!route.selecting);
        // a second loss has nothing left to release
        assert!(route.abandon(proto, Mods::default()).is_empty());
    }

    #[test]
    fn release_after_tracking_off_writes_nothing() {
        let mut route = Route::default();
        let cell = Cell {
            pane: 3,
            row: 0,
            col: 1,
        };
        let proto = click_proto();
        assert!(route
            .press(cell, Btn::Left, proto, Mods::default())
            .unwrap()
            .is_some());
        let off = Proto::default();
        assert!(!off.tracking());
        assert_eq!(
            route.release(Btn::Left, Some(cell), off, Mods::default()),
            Take::Consumed(None)
        );
        assert_eq!(route.grabbed_pane(), None);
        // the same press, then focus loss after the program left mouse mode
        assert!(route
            .press(cell, Btn::Left, proto, Mods::default())
            .unwrap()
            .is_some());
        assert!(route.abandon(off, Mods::default()).is_empty());
        assert_eq!(route.grabbed_pane(), None);
    }

    #[test]
    fn motion_to_the_same_cell_is_reported_once() {
        let mut route = Route::default();
        let proto = Proto::from_mode(TermMode::MOUSE_MOTION | TermMode::SGR_MOUSE);
        let a = Cell {
            pane: 1,
            row: 0,
            col: 0,
        };
        let b = Cell {
            pane: 1,
            row: 0,
            col: 2,
        };
        assert!(route.motion(a, proto, Mods::default()).is_some());
        assert!(route.motion(a, proto, Mods::default()).is_none());
        assert!(route.motion(b, proto, Mods::default()).is_some());
        // 1002 drag: the second visit to a cell is quiet too
        let drag = Proto::from_mode(TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE);
        route.press(a, Btn::Left, drag, Mods::default());
        assert!(route.drag(b, drag, Mods::default()).is_some());
        assert!(route.drag(b, drag, Mods::default()).is_none());
    }

    #[test]
    fn wheel_outside_the_grid_is_not_a_cell() {
        let tracking = View {
            id: 4,
            proto: click_proto(),
            alt_screen: true,
            app_cursor: false,
        };
        // tab bar / padding: do not invent column 1 row 1
        assert!(Route::wheel(None, tracking, 1, Mods::default()).is_none());
        let under = Cell {
            pane: 9,
            row: 3,
            col: 5,
        };
        let hit = View { id: 9, ..tracking };
        match Route::wheel(Some(under), hit, 2, Mods::default()) {
            Some(Wheel::Report { pane, bytes }) => {
                assert_eq!(pane, 9);
                assert_eq!(bytes, b"\x1b[<64;6;4M\x1b[<64;6;4M");
            }
            other => panic!("expected a report to pane 9, got {other:?}"),
        }
    }

    #[test]
    fn shift_wheel_on_the_alt_screen_scrolls_locally() {
        let view = View {
            id: 2,
            proto: click_proto(),
            alt_screen: true,
            app_cursor: true,
        };
        let cell = Cell {
            pane: 2,
            row: 1,
            col: 1,
        };
        assert_eq!(
            Route::wheel(
                Some(cell),
                view,
                -3,
                Mods {
                    shift: true,
                    ..Mods::default()
                }
            ),
            Some(Wheel::Scroll { pane: 2, lines: -3 })
        );
        // without Shift, and without tracking, the alt screen still gets arrows
        let plain = View {
            proto: Proto::from_mode(TermMode::ALT_SCREEN),
            ..view
        };
        match Route::wheel(Some(cell), plain, 1, Mods::default()) {
            Some(Wheel::Arrows { pane, bytes }) => {
                assert_eq!(pane, 2);
                assert_eq!(bytes, b"\x1bOA");
            }
            other => panic!("expected arrows, got {other:?}"),
        }
    }

    #[test]
    fn opening_a_tab_releases_the_grab_on_the_old_pane() {
        // spawn_tab changes the active tab without switch_tab, and calls
        // the same abandon the focus-loss path uses.
        let mut route = Route::default();
        let old = Cell {
            pane: 4,
            row: 1,
            col: 2,
        };
        let proto = click_proto();
        route.press(old, Btn::Left, proto, Mods::default()).unwrap();
        route.selecting = true;
        let out = route.abandon(proto, Mods::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pane, 4);
        assert_eq!(out[0].bytes, b"\x1b[<0;3;2m");
        assert_eq!(route.grabbed_pane(), None);
        assert!(!route.selecting);
        // the new tab's press is its own grab, not a chord on the old pane
        let fresh = Cell {
            pane: 9,
            row: 0,
            col: 0,
        };
        assert!(route
            .press(fresh, Btn::Left, proto, Mods::default())
            .unwrap()
            .is_some());
        assert_eq!(route.grabbed_pane(), Some(9));
    }

    #[test]
    fn sync_focus_uses_the_window_and_forgets_ris() {
        let mut route = Route::default();
        let wants = |id: u64| id == 1;
        // startup has no Focused event yet: an unfocused window
        // (has_focus false) does not announce
        assert!(route.sync_focus(false, Some(1), wants).is_empty());
        let on = route.sync_focus(true, Some(1), wants);
        assert_eq!(on[0].bytes, b"\x1b[I");
        assert!(route.sync_focus(true, Some(1), wants).is_empty());
        // RIS clears the memory, so the same focused pane hears CSI I again
        route.note_ris(1);
        assert_eq!(route.focus_sent(), None);
        let again = route.sync_focus(true, Some(1), wants);
        assert_eq!(again[0].bytes, b"\x1b[I");
        // Focused(false) passes that bit. A lagging has_focus() that is
        // still true must not be consulted here, or CSI O is lost.
        let off = route.sync_focus(false, Some(1), wants);
        assert_eq!(off[0].bytes, b"\x1b[O");
        assert_eq!(route.focus_sent(), None);
    }
}
