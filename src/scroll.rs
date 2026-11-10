//! Smooth scrollback. A touchpad reports pixels and a mouse wheel reports
//! lines; both used to wait for a whole line before the viewport moved
//! (`input::take_whole_lines`). With `scroll.smooth` on, the leftover is a
//! pixel offset: the pane draws one extra row and `push_quad` clips it to
//! the pane. A wheel notch eases that offset in over [`WHEEL_MS`]. Off, the
//! wheel is the old whole-line step and the offset stays zero.
//!
//! The offset is lines toward older history, matching `Scroll::Delta`'s
//! sign. It lives in `[0, 1)` on top of `display_offset`. Alt-screen wheels
//! and mouse reports never come through here: they stay on the integer path.

use alacritty_terminal::grid::Scroll;

/// Geometry key for the history line peeking in above the viewport.
pub(crate) const PEEK_ROW: usize = usize::MAX;

/// A mouse-wheel notch eases in over this many milliseconds.
pub(crate) const WHEEL_MS: f32 = 120.0;

/// One pane's fractional scroll. `subline` is the visible remainder;
/// `pending` is a wheel notch still easing in.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PaneScroll {
    pub subline: f32,
    pub pending: f32,
    per_ms: f32,
}

/// Where a wheel delta goes. Classic is the pre-smooth path: whole lines
/// out of an accumulator, nothing drawn between them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum WheelPlan {
    Classic {
        lines: i32,
        accum: f32,
    },
    /// `pixel`: apply now. A line delta eases through [`PaneScroll::pending`].
    Smooth {
        pixel: bool,
    },
}

/// True when this wheel must take the whole-line path. Smooth scroll is the
/// primary screen's own scrollback only. The alt screen (arrows, and
/// Shift-scroll) and a program that asked for wheel reports stay as they were.
pub(crate) fn use_classic(
    smooth: bool,
    alt_screen: bool,
    reports_wheel: bool,
    shift: bool,
) -> bool {
    if !smooth || alt_screen {
        return true;
    }
    if shift {
        return false;
    }
    reports_wheel
}

/// The plan for one wheel event. Classic's `(lines, accum)` is
/// [`crate::input::take_whole_lines`], so `scroll.smooth: false` cannot
/// drift from that function.
pub(crate) fn wheel_plan(
    smooth: bool,
    alt_screen: bool,
    reports_wheel: bool,
    shift: bool,
    accum: f32,
    delta_lines: f32,
    pixel: bool,
) -> WheelPlan {
    if use_classic(smooth, alt_screen, reports_wheel, shift) {
        let (lines, accum) = crate::input::take_whole_lines(accum, delta_lines);
        WheelPlan::Classic { lines, accum }
    } else {
        WheelPlan::Smooth { pixel }
    }
}

impl PaneScroll {
    /// Add `lines` (signed, fractional) to the position and carry whole
    /// lines into `offset`. The fraction stays in `subline`. Past either end
    /// of the scrollback the extra is dropped: the position clamps.
    pub(crate) fn apply_pixels(&mut self, offset: i32, history: i32, lines: f32) -> i32 {
        let (offset, sub) = advance(offset, history, self.subline, lines);
        self.subline = sub;
        offset
    }

    /// A mouse-wheel notch. It does not jump; [`Self::tick`] spends it.
    pub(crate) fn queue_lines(&mut self, lines: f32) {
        if lines == 0.0 {
            return;
        }
        self.pending += lines;
        if self.pending.abs() < 1e-6 {
            self.pending = 0.0;
            self.per_ms = 0.0;
            return;
        }
        self.per_ms = self.pending.abs() / WHEEL_MS;
    }

    /// Ease `pending` by `dt_ms` and fold what was spent into the position.
    /// Returns the display offset after the carry.
    pub(crate) fn tick(&mut self, offset: i32, history: i32, dt_ms: f32) -> i32 {
        if self.pending == 0.0 || dt_ms <= 0.0 {
            return offset;
        }
        if self.per_ms <= 0.0 {
            let step = self.pending;
            self.pending = 0.0;
            return self.apply_pixels(offset, history, step);
        }
        let mag = (self.per_ms * dt_ms).min(self.pending.abs());
        let mut step = mag.copysign(self.pending);
        self.pending -= step;
        if self.pending.abs() < 1e-4 {
            step += self.pending;
            self.pending = 0.0;
            self.per_ms = 0.0;
        }
        self.apply_pixels(offset, history, step)
    }

    /// A copy-mode key, a page, or a jump wants a whole line. Spend any
    /// eased notch, then round onto a line so the vi cursor is not left
    /// between rows. The returned offset is what `scroll_display` should
    /// show; `subline` and `pending` are zero.
    pub(crate) fn commit_for_navigation(mut self, offset: i32, history: i32) -> (i32, Self) {
        let offset = self.apply_pixels(offset, history, self.pending);
        self.pending = 0.0;
        self.per_ms = 0.0;
        let history = history.max(0);
        let pos = (offset as f32 + self.subline).clamp(0.0, history as f32);
        let next = pos.round().clamp(0.0, history as f32) as i32;
        self.subline = 0.0;
        (next, self)
    }

    pub(crate) fn idle(&self) -> bool {
        self.subline == 0.0 && self.pending == 0.0
    }
}

/// `offset + subline + delta`, clamped to `0..=history`, split back into a
/// whole display offset and a fraction in `[0, 1)`.
pub(crate) fn advance(offset: i32, history: i32, subline: f32, delta: f32) -> (i32, f32) {
    let history = history.max(0);
    let offset = offset.clamp(0, history);
    let subline = if (0.0..1.0).contains(&subline) {
        subline
    } else {
        0.0
    };
    let pos = ((offset as f64) + f64::from(subline) + f64::from(delta)).clamp(0.0, history as f64);
    if pos >= history as f64 {
        return (history, 0.0);
    }
    let whole = pos.floor();
    let mut sub = (pos - whole) as f32;
    let mut off = whole as i32;
    if sub >= 1.0 {
        off += 1;
        sub = 0.0;
    }
    if sub < 0.0 {
        sub = 0.0;
    }
    if off >= history {
        return (history, 0.0);
    }
    (off, sub)
}

/// Pixels the grid shifts down inside the pane. Zero when there is no
/// fraction, so a disabled or settled pane draws on the same rows as before.
/// Never a full line: that line was already carried into `display_offset`.
pub(crate) fn offset_px(subline: f32, cell_h: f32) -> i32 {
    if subline <= 0.0 || cell_h <= 1.0 {
        return 0;
    }
    let px = (subline * cell_h).round() as i32;
    let max = cell_h.floor() as i32 - 1;
    px.clamp(0, max.max(0))
}

/// Top of viewport `row` after the shift. `row` may be `-1`, the history
/// line drawn above the pane and clipped by the pane rect.
pub(crate) fn content_y(pane_y: i32, row: i32, cell_h: f32, offset_px: i32) -> i32 {
    pane_y + (row as f32 * cell_h).round() as i32 + offset_px.max(0)
}

/// Viewport row under a pane-local y. `offset_px` is the downward shift.
/// Zero matches `mouse::cell_at` (floor, then clamp into `0..rows`). A
/// positive shift can return `-1`, the line peeking in at the top.
pub(crate) fn selection_row(local_y: f64, cell_h: f64, offset_px: f64, rows: usize) -> i32 {
    if cell_h <= 0.0 || rows == 0 {
        return 0;
    }
    let rows = rows as i32;
    if offset_px <= 0.0 {
        let row = (local_y / cell_h).floor().max(0.0) as i32;
        return row.min(rows - 1);
    }
    let row = (local_y - offset_px).div_euclid(cell_h) as i32;
    row.clamp(-1, rows - 1)
}

/// Grid line of a viewport row. Viewport row 0 is `Line(-display_offset)`,
/// the same subtraction the selection anchor uses.
pub(crate) fn grid_line(viewport_row: i32, display_offset: i32) -> i32 {
    viewport_row - display_offset
}

/// Whole lines `scroll_display` needs so the viewport matches `next`.
pub(crate) fn display_delta(current: i32, next: i32) -> Scroll {
    Scroll::Delta(next - current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::Line;
    use alacritty_terminal::selection::SelectionType;
    use alacritty_terminal::term::TermMode;
    use alacritty_terminal::vi_mode::ViMotion;

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-4, "{a} != {b}");
    }

    #[test]
    fn sub_line_pixels_carry_into_lines_without_losing_motion() {
        let mut st = PaneScroll::default();
        let mut offset = 0;
        // 1/8 is exact in f32. Twenty of them are 2.5 lines.
        for _ in 0..20 {
            offset = st.apply_pixels(offset, 100, 0.125);
        }
        assert_eq!(offset, 2);
        approx(st.subline, 0.5);
        assert_eq!(offset as f32 + st.subline, 2.5);

        // Smaller steps, summed the same way advance sums them.
        let mut st = PaneScroll::default();
        let mut offset = 0;
        let mut expect = 0.0f64;
        let step = 0.01f32;
        for _ in 0..250 {
            expect += f64::from(step);
            offset = st.apply_pixels(offset, 1_000, step);
        }
        let pos = offset as f64 + f64::from(st.subline);
        assert!(
            (pos - expect).abs() < 1e-3,
            "position {pos} dropped motion from {expect}"
        );
    }

    #[test]
    fn the_offset_clamps_at_both_ends_of_the_scrollback() {
        let mut st = PaneScroll::default();
        let offset = st.apply_pixels(0, 10, -3.5);
        assert_eq!(offset, 0);
        assert_eq!(st.subline, 0.0);

        st.subline = 0.25;
        let offset = st.apply_pixels(0, 10, -0.25);
        assert_eq!(offset, 0);
        assert_eq!(st.subline, 0.0);

        let offset = st.apply_pixels(10, 10, 4.0);
        assert_eq!(offset, 10);
        assert_eq!(st.subline, 0.0);

        // 9.8 + 0.5 would be 10.3. The 0.3 past the top is not kept.
        st.subline = 0.8;
        let offset = st.apply_pixels(9, 10, 0.5);
        assert_eq!(offset, 10);
        assert_eq!(st.subline, 0.0);

        // A fraction that still fits is kept.
        st.subline = 0.2;
        let offset = st.apply_pixels(8, 10, 0.5);
        assert_eq!(offset, 8);
        approx(st.subline, 0.7);
    }

    #[test]
    fn a_wheel_notch_eases_in_and_the_lines_still_add_up() {
        let mut st = PaneScroll::default();
        st.queue_lines(3.0);
        let mut offset = 0;
        let mut guard = 0;
        while st.pending != 0.0 {
            offset = st.tick(offset, 50, 16.0);
            guard += 1;
            assert!(guard < 30, "the notch never finished");
        }
        assert_eq!(offset, 3);
        assert_eq!(st.subline, 0.0);
        // one long tick spends the whole notch
        st.queue_lines(-1.5);
        offset = st.tick(offset, 50, WHEEL_MS);
        assert_eq!(st.pending, 0.0);
        assert_eq!(offset, 1);
        approx(st.subline, 0.5);

        // queued past the bottom drains, and the position stays put
        let mut st = PaneScroll::default();
        st.queue_lines(-4.0);
        let mut offset = 0;
        for _ in 0..20 {
            offset = st.tick(offset, 10, 16.0);
        }
        assert_eq!(offset, 0);
        assert_eq!(st.pending, 0.0);
        assert_eq!(st.subline, 0.0);
    }

    #[test]
    fn smooth_off_is_the_whole_line_accumulator() {
        let samples = [0.0, 0.4, 0.7, -0.3, -2.5, 3.0, 1.0, -1.0, 0.125];
        for &accum in &samples {
            for &delta in &samples {
                for pixel in [false, true] {
                    let plan = wheel_plan(false, false, false, false, accum, delta, pixel);
                    let (lines, rest) = crate::input::take_whole_lines(accum, delta);
                    assert_eq!(plan, WheelPlan::Classic { lines, accum: rest });
                }
            }
        }
        // and every destination is classic when the setting is off
        for alt in [false, true] {
            for reports in [false, true] {
                for shift in [false, true] {
                    assert!(use_classic(false, alt, reports, shift));
                }
            }
        }
        assert_eq!(offset_px(0.0, 20.0), 0);
    }

    #[test]
    fn the_alt_screen_and_mouse_reports_stay_on_the_classic_path() {
        assert!(use_classic(true, true, false, false), "arrows");
        assert!(use_classic(true, true, false, true), "shift-scroll on alt");
        assert!(use_classic(true, true, true, false), "report on alt");
        assert!(use_classic(true, false, true, false), "report");
        assert!(
            !use_classic(true, false, true, true),
            "shift scrolls locally"
        );
        assert!(
            !use_classic(true, false, false, false),
            "primary scrollback"
        );

        // The integer count the arrow / report encoder sees is the old one.
        let decided = wheel_plan(true, true, false, false, 0.2, 1.4, true);
        let (n, rest) = crate::input::take_whole_lines(0.2, 1.4);
        assert_eq!(
            decided,
            WheelPlan::Classic {
                lines: n,
                accum: rest
            }
        );
        assert_eq!(n, 1);
        approx(rest, 0.6);

        let view = crate::mouse::View {
            id: 3,
            proto: crate::mouse::Proto::default(),
            alt_screen: true,
            app_cursor: false,
        };
        let at = Some(crate::mouse::Cell {
            pane: 3,
            row: 1,
            col: 4,
        });
        match crate::mouse::Route::wheel(at, view, n, crate::mouse::Mods::default()) {
            Some(crate::mouse::Wheel::Arrows { pane, bytes }) => {
                assert_eq!(pane, 3);
                assert_eq!(bytes, b"\x1b[A");
            }
            other => panic!("alt-screen wheel changed: {other:?}"),
        }

        let tracking = crate::mouse::Proto {
            clicks: true,
            ..crate::mouse::Proto::default()
        };
        assert!(tracking.wants(crate::mouse::What::Wheel, false));
        let view = crate::mouse::View {
            id: 3,
            proto: tracking,
            alt_screen: false,
            app_cursor: false,
        };
        let decided = wheel_plan(true, false, true, false, 0.0, 3.0, false);
        assert_eq!(
            decided,
            WheelPlan::Classic {
                lines: 3,
                accum: 0.0
            }
        );
        match crate::mouse::Route::wheel(at, view, 3, crate::mouse::Mods::default()) {
            Some(crate::mouse::Wheel::Report { pane, bytes }) => {
                assert_eq!(pane, 3);
                assert!(!bytes.is_empty());
            }
            other => panic!("wheel report changed: {other:?}"),
        }
    }

    #[test]
    fn a_click_with_a_pixel_offset_lands_on_the_shifted_row() {
        let cell = 20.0f64;
        let shift = offset_px(0.4, cell as f32);
        assert_eq!(shift, 8);
        // offset 0 matches the unshifted floor-and-clamp
        assert_eq!(selection_row(0.0, cell, 0.0, 10), 0);
        assert_eq!(selection_row(19.9, cell, 0.0, 10), 0);
        assert_eq!(selection_row(20.0, cell, 0.0, 10), 1);
        assert_eq!(selection_row(1_000.0, cell, 0.0, 10), 9);
        let rects = [(0, 0, 80, 200)];
        for y in [0.0, 19.9, 20.0, 199.0] {
            let hit = crate::mouse::cell_at(4.0, y, &rects, 8.0, cell).unwrap();
            assert_eq!(selection_row(y, cell, 0.0, 10), hit.row as i32);
        }

        let shift = shift as f64;
        // the top of the pane is the history line peeking in
        assert_eq!(selection_row(0.0, cell, shift, 10), -1);
        assert_eq!(selection_row(shift, cell, shift, 10), 0);
        assert_eq!(selection_row(shift + 2.0 * cell + 5.0, cell, shift, 10), 2);
        // that row's grid line follows the display offset
        assert_eq!(grid_line(-1, 5), -6);
        assert_eq!(grid_line(2, 5), -3);

        // the copy-mode cursor sits on viewport row 2; a click in its
        // shifted box is still that row, not the row above it
        let y = content_y(100, 2, 20.0, shift as i32);
        let row = selection_row((y - 100) as f64 + 1.0, cell, shift, 8);
        assert_eq!(row, 2);
        assert_eq!(grid_line(row, 4), -2);
    }

    #[test]
    fn the_extra_row_shifts_by_the_offset_and_the_cursor_row_does_not() {
        let cell = 20.0;
        let shift = 8;
        // a zero offset is the y the renderer already used
        for row in 0..5 {
            let y = content_y(40, row, 18.5, 0);
            let old = 40 + (row as f32 * 18.5).round() as i32;
            assert_eq!(y, old);
        }
        let peek = content_y(100, -1, cell, shift);
        assert_eq!(peek, 88);
        // eight pixels of that row sit inside the pane; the rest is clipped
        assert_eq!(100 - peek, 12);
        assert_eq!(peek + cell as i32 - 100, shift);
        let cursor_row = 3;
        let y0 = content_y(100, cursor_row, cell, 0);
        let y1 = content_y(100, cursor_row, cell, shift);
        assert_eq!(y1 - y0, shift);
        assert_eq!(
            selection_row((y1 - 100) as f64 + 1.0, cell as f64, shift as f64, 10),
            cursor_row
        );
    }

    #[test]
    fn split_panes_keep_their_own_offsets() {
        let mut left = PaneScroll::default();
        let mut right = PaneScroll::default();
        let l = left.apply_pixels(0, 40, 1.25);
        let r = right.apply_pixels(0, 40, 0.0);
        assert_eq!(l, 1);
        approx(left.subline, 0.25);
        assert_eq!(r, 0);
        assert_eq!(right.subline, 0.0);
        right.queue_lines(-0.5);
        assert_eq!(left.pending, 0.0);
        approx(right.pending, -0.5);
    }

    fn vi_term(
        rows: usize,
        lines: usize,
    ) -> alacritty_terminal::term::Term<alacritty_terminal::event::VoidListener> {
        use alacritty_terminal::term::{Config, Term};
        let size = crate::term::TermSize { cols: 20, rows };
        let mut t = Term::new(
            Config::default(),
            &size,
            alacritty_terminal::event::VoidListener,
        );
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        let text: String = (0..lines).map(|i| format!("line{i}\r\n")).collect();
        parser.advance(&mut t, text.as_bytes());
        t.toggle_vi_mode();
        assert!(t.mode().contains(TermMode::VI));
        t
    }

    fn cursor_in_view(t: &alacritty_terminal::term::Term<alacritty_terminal::event::VoidListener>) {
        let line = t.vi_mode_cursor.point.line.0;
        let start = -(t.grid().display_offset() as i32);
        let end = start + t.screen_lines() as i32 - 1;
        assert!(
            (start..=end).contains(&line),
            "vi cursor {line} left the viewport {start}..={end}"
        );
    }

    #[test]
    fn a_smooth_wheel_keeps_the_copy_cursor_in_the_viewport() {
        let mut t = vi_term(4, 14);
        let history = t.grid().history_size() as i32;
        assert!(history >= 8, "history {history}");
        let mut st = PaneScroll::default();
        st.queue_lines(3.0);
        let mut offset = t.grid().display_offset() as i32;
        for _ in 0..20 {
            let next = st.tick(offset, history, 16.0);
            if next != offset {
                t.scroll_display(display_delta(offset, next));
                offset = t.grid().display_offset() as i32;
            }
        }
        assert_eq!(st.pending, 0.0);
        assert_eq!(offset, 3);
        cursor_in_view(&t);
        // the cursor moved with the viewport, so a line selection is the
        // line it landed on, not a line the fraction skipped
        let at = t.vi_mode_cursor.point;
        crate::copymode::apply(&mut t, crate::copymode::Cmd::Select(SelectionType::Lines));
        assert!(t.selection.is_some(), "the cursor line was not selected");
        assert_eq!(
            t.vi_mode_cursor.point, at,
            "selecting must not move the cursor"
        );
        cursor_in_view(&t);
    }

    #[test]
    fn copy_mode_navigation_snaps_the_fraction_then_moves_by_lines() {
        let mut t = vi_term(4, 12);
        t.scroll_display(Scroll::Delta(2));
        let history = t.grid().history_size() as i32;
        let offset = t.grid().display_offset() as i32;
        let st = PaneScroll {
            subline: 0.7,
            pending: 0.0,
            per_ms: 0.0,
        };
        let (next, st) = st.commit_for_navigation(offset, history);
        assert_eq!(st.subline, 0.0);
        assert_eq!(st.pending, 0.0);
        assert_eq!(next, offset + 1, "0.7 rounds onto the next line");
        if next != t.grid().display_offset() as i32 {
            t.scroll_display(display_delta(t.grid().display_offset() as i32, next));
        }
        cursor_in_view(&t);

        crate::copymode::apply(&mut t, crate::copymode::Cmd::Top);
        cursor_in_view(&t);
        assert_eq!(t.vi_mode_cursor.point.line, t.topmost_line());
        crate::copymode::apply(&mut t, crate::copymode::Cmd::Select(SelectionType::Lines));
        assert_eq!(t.selection_to_string().as_deref(), Some("line0\n"));

        let before = t.grid().display_offset();
        crate::copymode::apply(&mut t, crate::copymode::Cmd::PageDown);
        assert!(t.grid().display_offset() < before);
        cursor_in_view(&t);
        crate::copymode::apply(&mut t, crate::copymode::Cmd::Motion(ViMotion::Down));
        cursor_in_view(&t);
        assert_eq!(t.topmost_line(), Line(-(t.grid().history_size() as i32)));
    }
}
