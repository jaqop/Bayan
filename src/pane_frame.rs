//! One pane's display state, owned: the photograph a frame is drawn from.
//!
//! [`PaneFrame`] is filled by [`crate::term::Session::fill_frame`] under the
//! session's locks (term, then meta — the reader's order) and consumed
//! without locks by the draw path, the screen reader, the cockpit, and the
//! scrollbar. One photograph per pane per frame: every consumer in a frame
//! reads the same value, so viewport facts are derived once, not twice.
//!
//! The cell buffer is retained across frames and refilled in place; a frame
//! is a refill, not a construction. Raw cell attributes are stored here;
//! theme mapping (`cell_paint`) stays in the renderer, which owns the theme.

use std::sync::Arc;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionRange};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{RenderableCursor, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape};

use crate::bidi_ctl::{BidiReorder, BidiState};
use crate::images::ImageSnap;

/// One visible cell, raw attributes. Wide spacers are skipped at capture
/// (the draw path's filter); each consumer keeps its own additional filters
/// (the screen reader also skips leading wide spacers).
pub struct FrameCell {
    pub col: usize,
    pub c: char,
    /// Combining marks (Arabic tashkeel); empty when none. Capacity is
    /// retained across frames.
    pub zw: Vec<char>,
    pub fg: AnsiColor,
    pub bg: AnsiColor,
    pub flags: Flags,
    /// SGR 58 underline color, when the program set one.
    pub ul: Option<AnsiColor>,
}

impl FrameCell {
    /// 1, or 2 for wide chars (CJK, some emoji).
    pub fn w(&self) -> usize {
        if self.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        }
    }
}

/// The photograph. Grid facts come from `capture_grid`; meta facts from
/// `Session::fill_frame` / `fill_facts`. A facts-only fill (background tabs,
/// cockpit) leaves the grid fields stale — only a full fill feeds the draw
/// path and the screen reader.
pub struct PaneFrame {
    // grid facts
    pub rows: usize,
    pub columns: usize,
    pub history: u64,
    pub display_offset: u64,
    pub alt_screen: bool,
    pub vi_mode: bool,
    /// The renderable cursor (shape `Hidden` when the mode hides it).
    pub cursor: RenderableCursor,
    /// The grid cursor, even while hidden — the screen-reader stamp uses it.
    pub grid_cursor: Point,
    /// The native selection, for visual-range validation.
    pub selection: Option<Selection>,
    /// The resolved range, for draw fallback and the stamp.
    pub selection_range: Option<SelectionRange>,
    /// Visible cells in grid order, row-major, wide spacers skipped.
    pub cells: Vec<FrameCell>,
    /// Each viewport row's span into `cells`; empty rows are empty spans.
    pub row_spans: Vec<std::ops::Range<usize>>,
    /// `wrapped[r]` = grid row `r - display_offset - 1` ends in a soft wrap.
    /// `rows + 1` entries: the draw path reads `r` and `r + 1`; the screen
    /// reader's own-row flag for viewport row `r` is `wrapped[r + 1]`.
    pub wrapped: Vec<bool>,
    /// The history row above the viewport (`-(offset + 1)`), raw cells.
    /// Empty when the offset is at the top of history.
    pub peek: Vec<FrameCell>,
    // meta facts
    /// Command blocks in grid space: (prompt line, exit code), evictions
    /// subtracted behind the seam.
    pub marks: Vec<(usize, Option<i32>)>,
    /// Last mark with an exit code, in global space — the screen reader's
    /// finish announcement, which must survive eviction.
    pub last_finished: Option<(u64, Option<i32>)>,
    pub running_cmd: String,
    pub cwd: Option<String>,
    /// Arc clone: the stamp list is not copied; the reader copy-on-writes.
    pub bidi: Arc<BidiState>,
    pub evicted: u64,
    /// First inline-Claude row, converted to grid space. The alt-screen and
    /// auto-follow gates stay with the caller.
    pub claude_from_grid: Option<i64>,
    pub bidi_reorder: Option<BidiReorder>,
    pub tracked_alt: bool,
    pub images: Vec<ImageSnap>,
}

impl Default for PaneFrame {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneFrame {
    pub fn new() -> Self {
        Self {
            rows: 0,
            columns: 0,
            history: 0,
            display_offset: 0,
            alt_screen: false,
            vi_mode: false,
            cursor: RenderableCursor {
                point: Point::new(Line(0), Column(0)),
                shape: CursorShape::Block,
            },
            grid_cursor: Point::new(Line(0), Column(0)),
            selection: None,
            selection_range: None,
            cells: Vec::new(),
            row_spans: Vec::new(),
            wrapped: Vec::new(),
            peek: Vec::new(),
            marks: Vec::new(),
            last_finished: None,
            running_cmd: String::new(),
            cwd: None,
            bidi: Arc::new(BidiState::default()),
            evicted: 0,
            claude_from_grid: None,
            bidi_reorder: None,
            tracked_alt: false,
            images: Vec::new(),
        }
    }

    /// Capture the grid facts and cells. No locks here: the caller holds the
    /// term lock (or owns a bare `Term`, as the render tests do).
    pub(crate) fn capture_grid<T: EventListener>(&mut self, term: &Term<T>) {
        let rows = term.screen_lines();
        let columns = term.columns();
        let content = term.renderable_content();
        let off = content.display_offset;
        self.rows = rows;
        self.columns = columns;
        self.history = term.grid().history_size() as u64;
        self.display_offset = off as u64;
        self.alt_screen = term.mode().contains(TermMode::ALT_SCREEN);
        self.vi_mode = term.mode().contains(TermMode::VI);
        self.cursor = content.cursor;
        self.grid_cursor = term.grid().cursor.point;
        self.selection = term.selection.clone();
        self.selection_range = content.selection;

        // Visible cells, refilled in place so the zero-width capacity lives
        // across frames. display_iter yields row-major; spans close when the
        // row changes, empty rows keep their empty span.
        self.row_spans.clear();
        self.row_spans.resize(rows, 0..0);
        let mut n = 0usize;
        let mut last_vrow: Option<usize> = None;
        let mut start = 0usize;
        for cell in content.display_iter {
            let vrow = cell.point.line.0 + off as i32;
            if vrow < 0 || vrow as usize >= rows {
                continue;
            }
            let li = vrow as usize;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            if last_vrow != Some(li) {
                if let Some(prev) = last_vrow {
                    self.row_spans[prev] = start..n;
                }
                start = n;
                last_vrow = Some(li);
            }
            put_cell(&mut self.cells, &mut n, cell.point.column.0, &cell);
        }
        if let Some(prev) = last_vrow {
            self.row_spans[prev] = start..n;
        }
        self.cells.truncate(n);

        // Wrap flags: rows + 1, the previous grid row's last-column flag.
        self.wrapped.resize(rows + 1, false);
        for row in 0..=rows {
            self.wrapped[row] = crate::render::line_wrapped_into(term, row, off as i32);
        }

        // The peek row: one history line above the viewport, same raw walk
        // the draw path's cells_of_line does, minus the theme mapping.
        let mut pn = 0usize;
        if (off as usize) < self.history as usize {
            let line = Line(-(off as i32 + 1));
            let row = &term.grid()[line];
            for col in 0..columns {
                let cell = &row[Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                put_cell(&mut self.peek, &mut pn, col, cell);
            }
        }
        self.peek.truncate(pn);
    }
}

/// Refill cell `n` in place (keeping its zero-width capacity), or push.
fn put_cell(cells: &mut Vec<FrameCell>, n: &mut usize, col: usize, cell: &Cell) {
    if *n < cells.len() {
        let fc = &mut cells[*n];
        fc.col = col;
        fc.c = cell.c;
        fc.fg = cell.fg;
        fc.bg = cell.bg;
        fc.flags = cell.flags;
        fc.ul = cell.underline_color();
        fc.zw.clear();
        if let Some(z) = cell.zerowidth() {
            fc.zw.extend_from_slice(z);
        }
    } else {
        cells.push(FrameCell {
            col,
            c: cell.c,
            zw: cell.zerowidth().map(|z| z.to_vec()).unwrap_or_default(),
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags,
            ul: cell.underline_color(),
        });
    }
    *n += 1;
}
