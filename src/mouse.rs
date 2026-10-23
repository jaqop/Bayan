//! Mouse reporting and focus events for the program inside the pane.
//!
//! xterm's tracking modes are mutually exclusive in alacritty_terminal
//! (setting one clears the others): 1000 is clicks, 1002 is clicks plus
//! motion while a button is held, 1003 is clicks plus every motion.
//! 1006 selects SGR encoding; without it the bytes are the legacy X10
//! form, which only fits coordinates 1..=223. 1004 is independent: the
//! focused pane hears CSI I, the one that lost focus hears CSI O.
//!
//! Shift bypasses all of it. The terminal keeps the gesture for its own
//! selection (and its own scroll), which is how a mouse-aware TUI stays
//! copyable.
//!
//! Ctrl+click is not a report either: it opens a hyperlink
//! (`link::click_owner`). Shift still keeps local selection when the cell
//! is a link, and it wins if Ctrl is held too. A plain click while
//! tracking is on belongs to the program, link or not. The column is the
//! one `column_for_click` already computed, so a link on an Arabic row is
//! the glyph under the pointer.

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
    /// 1006 SGR encoding. Off means X10.
    pub sgr: bool,
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
            focus: mode.contains(TermMode::FOCUS_IN_OUT),
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

/// One report. `col` and `row` are 1-based, matching what the program
/// counts. `None` when the event has no encoding (a wheel release, or an
/// X10 coordinate past 223 — a byte can't hold it, and clamping would
/// land the click on the wrong cell).
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
        if col > 223 || row > 223 || x10 > 223 {
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

/// Glyph spans → boxes in pane-local pixels. A glyph that covers two
/// columns is split at its midpoint; an RTL glyph's left half is the
/// higher logical column (the end of its byte range).
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
        if c_start == c_end {
            out.push(GlyphBox {
                x0,
                x1,
                col: c_start,
            });
            continue;
        }
        let (left, right) = if g.rtl {
            (c_end, c_start)
        } else {
            (c_start, c_end)
        };
        let mid = (x0 + x1) * 0.5;
        out.push(GlyphBox {
            x0,
            x1: mid,
            col: left,
        });
        out.push(GlyphBox {
            x0: mid,
            x1,
            col: right,
        });
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
    fn x10_is_the_legacy_fallback_and_stops_at_223() {
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
        // 223 fits (byte 255); 224 does not, and we refuse rather than lie
        assert!(encode(false, Phase::Press, Btn::Left, 223, 1, m).is_some());
        assert!(encode(false, Phase::Press, Btn::Left, 224, 1, m).is_none());
        assert!(encode(false, Phase::Press, Btn::Left, 1, 224, m).is_none());
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
    }
}
