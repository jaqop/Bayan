//! Software renderer with a dual text engine — EasyTer's proven design,
//! ported:
//!
//! - rows WITHOUT Arabic (prompts, code, TUIs — most of a terminal's life)
//!   render on a strict grid: ASCII batches into runs pinned at col*cell_w,
//!   and every other glyph (Nerd Font icons, powerline separators, box
//!   drawing, CJK) draws per cell, compressed into its box when wider. Cell
//!   backgrounds and glyphs can never drift apart.
//! - rows WITH Arabic shape as whole lines so cosmic-text applies UAX#9
//!   BiDi (mixed directions, LTR islands): correct text outranks column
//!   fidelity for prose.
//!
//! cosmic-text does shaping/BiDi/fallback; softbuffer presents the pixels.
//! GPU (wgpu) lands in a later milestone behind this same boundary.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use cosmic_text::{
    Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Wrap,
};

use crate::term::EventProxy;

// EasyTer heritage colors: the palette that proved itself for Arabic text
pub const BG: (u8, u8, u8) = (0x0d, 0x11, 0x17);
pub const FG: (u8, u8, u8) = (0xe6, 0xed, 0xf3);

const PALETTE: [(u8, u8, u8); 16] = [
    (0x0d, 0x11, 0x17), // black
    (0xff, 0x6b, 0x6b), // red
    (0x7e, 0xe7, 0x87), // green
    (0xe3, 0xb3, 0x41), // yellow
    (0x6c, 0xa0, 0xf6), // blue
    (0xd2, 0xa8, 0xff), // magenta
    (0x56, 0xd4, 0xdd), // cyan
    (0xe6, 0xed, 0xf3), // white
    (0x6e, 0x76, 0x81), // bright black
    (0xff, 0x8a, 0x8a), // bright red
    (0xa2, 0xf5, 0xb0), // bright green
    (0xf2, 0xcc, 0x60), // bright yellow
    (0x8d, 0xb4, 0xf8), // bright blue
    (0xe0, 0xc1, 0xff), // bright magenta
    (0x7e, 0xe0, 0xe6), // bright cyan
    (0xff, 0xff, 0xff), // bright white
];

fn named_rgb(name: NamedColor) -> (u8, u8, u8) {
    use NamedColor::*;
    match name {
        Black | DimBlack => PALETTE[0],
        Red | DimRed => PALETTE[1],
        Green | DimGreen => PALETTE[2],
        Yellow | DimYellow => PALETTE[3],
        Blue | DimBlue => PALETTE[4],
        Magenta | DimMagenta => PALETTE[5],
        Cyan | DimCyan => PALETTE[6],
        White | DimWhite => PALETTE[7],
        BrightBlack => PALETTE[8],
        BrightRed => PALETTE[9],
        BrightGreen => PALETTE[10],
        BrightYellow => PALETTE[11],
        BrightBlue => PALETTE[12],
        BrightMagenta => PALETTE[13],
        BrightCyan => PALETTE[14],
        BrightWhite => PALETTE[15],
        Background => BG,
        _ => FG, // Foreground / BrightForeground / DimForeground / Cursor
    }
}

fn indexed_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        0..=15 => PALETTE[i as usize],
        16..=231 => {
            let n = i - 16;
            let f = |c: u8| if c == 0 { 0 } else { 55 + 40 * c };
            (f(n / 36), f((n % 36) / 6), f(n % 6))
        }
        _ => {
            let v = 8 + 10 * (i - 232);
            (v, v, v)
        }
    }
}

pub fn ansi_rgb(color: AnsiColor) -> (u8, u8, u8) {
    match color {
        AnsiColor::Named(n) => named_rgb(n),
        AnsiColor::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(i) => indexed_rgb(i),
    }
}

fn pack((r, g, b): (u8, u8, u8)) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// The background as a packed pixel — for the instant first frame drawn
/// before the font system finishes loading.
pub fn bg_packed() -> u32 {
    pack(BG)
}

/// One tab as the bar renders it.
pub struct TabInfo {
    pub title: String,
    pub busy: bool,
    pub active: bool,
}

/// Fixed tab width in cells — the app's click hit-test relies on this.
pub const TAB_CELLS: f32 = 24.0;

/// UI state rendered on top of the grid (owned by the app, drawn here).
pub struct Overlay<'a> {
    /// Some(query) = the search bar is open with this text.
    pub search_query: Option<&'a str>,
    /// The current search hit, highlighted stronger than a selection.
    pub search_match: Option<&'a std::ops::RangeInclusive<alacritty_terminal::index::Point>>,
    /// Claude mode: cells hold VISUAL-order Arabic that must be restored
    /// to logical before shaping (and a badge is drawn).
    pub claude: bool,
    /// The tab bar (always visible: titles, busy dots).
    pub tabs: &'a [TabInfo],
    /// Command blocks: (absolute prompt line, exit code) — gutter lights.
    pub marks: &'a [(usize, Option<i32>)],
}

// translucent overlays, EasyTer's colors: selection blue, search amber
const SELECTION_RGBA: ((u8, u8, u8), u32) = ((80, 140, 255), 90);
const SEARCH_RGBA: ((u8, u8, u8), u32) = ((240, 180, 40), 120);

fn blend(dst: u32, (sr, sg, sb): (u8, u8, u8), a: u32) -> u32 {
    let (dr, dg, db) = ((dst >> 16) & 0xff, (dst >> 8) & 0xff, dst & 0xff);
    let r = (sr as u32 * a + dr * (255 - a)) / 255;
    let g = (sg as u32 * a + dg * (255 - a)) / 255;
    let b = (sb as u32 * a + db * (255 - a)) / 255;
    (r << 16) | (g << 8) | b
}

fn fill_rect(frame: &mut [u32], fw: usize, fh: usize, x0: i32, y0: i32, w: i32, h: i32, c: u32) {
    for y in y0.max(0)..(y0 + h).min(fh as i32) {
        let row = y as usize * fw;
        for x in x0.max(0)..(x0 + w).min(fw as i32) {
            frame[row + x as usize] = c;
        }
    }
}

fn blend_rect(frame: &mut [u32], fw: usize, fh: usize, x0: i32, y0: i32, w: i32, h: i32,
              c: (u8, u8, u8), a: u32) {
    for y in y0.max(0)..(y0 + h).min(fh as i32) {
        let row = y as usize * fw;
        for x in x0.max(0)..(x0 + w).min(fw as i32) {
            let i = row + x as usize;
            frame[i] = blend(frame[i], c, a);
        }
    }
}

fn is_arabic(c: char) -> bool {
    matches!(c as u32,
        0x0600..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF
        | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF)
}

/// One visible grid cell (wide chars own their spacer's width).
struct CellInfo {
    col: usize,
    w: usize, // 1, or 2 for wide chars (CJK, some emoji)
    c: char,
    // combining marks stored zero-width in the cell (Arabic tashkeel:
    // tanwin, shadda ... ) — dropping them loses the diacritics
    zw: Option<Vec<char>>,
    fg: (u8, u8, u8),
}

impl CellInfo {
    fn push_text(&self, s: &mut String) {
        s.push(self.c);
        if let Some(z) = &self.zw {
            s.extend(z.iter());
        }
    }

    /// UTF-8 length this cell contributes to the shaped text.
    fn text_len(&self) -> usize {
        self.c.len_utf8()
            + self
                .zw
                .as_ref()
                .map_or(0, |z| z.iter().map(|c| c.len_utf8()).sum())
    }
}

const FONT_SIZE: f32 = 15.0;
// bundled so connected Arabic works from a fresh clone (same font EasyTer ships)
const AMIRI: &[u8] = include_bytes!("../fonts/Amiri-Regular.ttf");

/// Primary-font preference order. Nerd Font variants first: oh-my-posh
/// prompts are built from their private-use icons and powerline separators,
/// and a non-NF primary renders those as ugly boxes/blocks.
const FAMILY_CANDIDATES: &[&str] = &[
    "Cascadia Mono NF",
    "Cascadia Code NF",
    "CaskaydiaCove Nerd Font Mono",
    "JetBrainsMono Nerd Font Mono",
    "Cascadia Mono",
    "Consolas",
];

fn pick_family(db: &cosmic_text::fontdb::Database, preferred: Option<&str>) -> String {
    let has = |name: &str| {
        db.faces()
            .any(|f| f.families.iter().any(|(n, _)| n == name))
    };
    if let Some(p) = preferred {
        if has(p) {
            return p.to_string();
        }
    }
    for cand in FAMILY_CANDIDATES {
        if has(cand) {
            return (*cand).to_string();
        }
    }
    "Consolas".to_string()
}

#[cfg(test)]
fn base_attrs<'a>() -> Attrs<'a> {
    Attrs::new().family(Family::Name("Consolas"))
}

pub struct Renderer {
    font_system: FontSystem,
    cache: SwashCache,
    buffer: Buffer, // scratch: one shaped run/line at a time
    family: String,
    pub cell_w: f32,
    pub cell_h: f32,
}

impl Renderer {
    /// `extra_pts` is the live Ctrl+wheel zoom delta on top of the
    /// configured base size.
    pub fn new(scale: f32, cfg: &crate::config::UserConfig, extra_pts: f32) -> Self {
        let mut font_system = FontSystem::new();
        font_system.db_mut().load_font_data(AMIRI.to_vec());
        let family = pick_family(font_system.db(), cfg.font_family.as_deref());
        let size = (cfg.font_size.unwrap_or(FONT_SIZE) + extra_pts).clamp(8.0, 40.0) * scale;
        let metrics = Metrics::new(size, (size * 1.4).ceil());
        let mut buffer = Buffer::new(&mut font_system, metrics);
        buffer.set_wrap(&mut font_system, Wrap::None);
        // cell width = the primary monospace font's advance for an ASCII probe
        let mut probe = Buffer::new(&mut font_system, metrics);
        probe.set_wrap(&mut font_system, Wrap::None);
        probe.set_size(&mut font_system, Some(1000.0), Some(metrics.line_height));
        probe.set_text(
            &mut font_system,
            "M",
            Attrs::new().family(Family::Name(&family)),
            Shaping::Advanced,
        );
        probe.shape_until_scroll(&mut font_system, false);
        let cell_w = probe
            .layout_runs()
            .next()
            .map(|r| r.line_w)
            .filter(|w| *w > 1.0)
            .unwrap_or(size * 0.6);
        Self {
            font_system,
            cache: SwashCache::new(),
            buffer,
            family,
            cell_w,
            cell_h: metrics.line_height,
        }
    }

    /// Height of the tab bar in pixels — the grid starts below it.
    pub fn tab_bar_h(&self) -> f32 {
        self.cell_h + 10.0
    }

    /// Shape `segs` (text + color spans) into the scratch buffer; returns the
    /// natural width. `align_left` pins RTL-base lines to the left edge
    /// (cosmic-text otherwise pushes them to the buffer's right edge).
    fn shape_scratch(&mut self, segs: &[(String, (u8, u8, u8))], buf_w: f32, align_left: bool) -> f32 {
        let base = Attrs::new().family(Family::Name(self.family.as_str()));
        let rich: Vec<(&str, Attrs)> = segs
            .iter()
            .map(|(s, c)| (s.as_str(), base.color(Color::rgb(c.0, c.1, c.2))))
            .collect();
        self.buffer
            .set_size(&mut self.font_system, Some(buf_w), Some(self.cell_h));
        self.buffer
            .set_rich_text(&mut self.font_system, rich, base, Shaping::Advanced);
        if align_left {
            for line in self.buffer.lines.iter_mut() {
                line.set_align(Some(Align::Left));
            }
        }
        self.buffer
            .shape_until_scroll(&mut self.font_system, false);
        self.buffer
            .layout_runs()
            .map(|r| r.line_w)
            .fold(0.0_f32, f32::max)
    }

    /// Blit the scratch buffer at (x_off, y_off), optionally compressed
    /// horizontally (EasyTer's fit for glyphs wider than their grid box).
    fn blit_scratch(&mut self, frame: &mut [u32], fw: usize, fh: usize,
                    x_off: f32, y_off: i32, scale: f32) {
        self.buffer.draw(
            &mut self.font_system,
            &mut self.cache,
            Color::rgb(FG.0, FG.1, FG.2),
            |x, y, w, h, color| {
                let a = color.a() as u32;
                if a == 0 {
                    return;
                }
                let rgb = (color.r(), color.g(), color.b());
                let xs = (x_off + x as f32 * scale).round() as i32;
                let ws = ((w as f32 * scale).ceil() as i32).max(1);
                for dy in 0..h as i32 {
                    let py = y_off + y + dy;
                    if py < 0 || py as usize >= fh {
                        continue;
                    }
                    let row = py as usize * fw;
                    for dx in 0..ws {
                        let px = xs + dx;
                        if px < 0 || px as usize >= fw {
                            continue;
                        }
                        let i = row + px as usize;
                        frame[i] = blend(frame[i], rgb, a);
                    }
                }
            },
        );
    }

    /// A row containing Arabic: shape the WHOLE line so cosmic-text applies
    /// UAX#9 BiDi (mixed directions, LTR islands). Correct text outranks
    /// column fidelity for prose output.
    ///
    /// Grid fit (M2b): a shaped Arabic line wider than the window compresses
    /// horizontally to fit (EasyTer's fix, applied at line level). When the
    /// terminal cursor sits on this row, its pixel position is resolved
    /// THROUGH the shaped layout — in RTL, logical column N is not at
    /// N*cell_w — and returned as (x, w).
    fn draw_line_bidi(&mut self, frame: &mut [u32], fw: usize, fh: usize,
                      y: i32, cells: &[CellInfo], claude: bool,
                      cursor_col: Option<usize>) -> Option<(i32, i32)> {
        let mut end = cells.len();
        while end > 0 && cells[end - 1].c == ' ' {
            end -= 1;
        }
        let mut segs: Vec<(String, (u8, u8, u8))> = Vec::new();
        let mut cur_bytes: Option<(usize, usize)> = None;
        let mut nbytes = 0usize;
        for ci in &cells[..end] {
            if cursor_col == Some(ci.col) {
                cur_bytes = Some((nbytes, nbytes + ci.c.len_utf8()));
            }
            nbytes += ci.text_len();
            match segs.last_mut() {
                Some((s, c)) if *c == ci.fg => ci.push_text(s),
                _ => {
                    let mut s = String::new();
                    ci.push_text(&mut s);
                    segs.push((s, ci.fg));
                }
            }
        }
        if segs.is_empty() {
            return None;
        }
        // Claude mode: the cells hold Claude's pre-reversed VISUAL order.
        // Restore logical so cosmic-text's BiDi shows it right. Per-cell
        // colors can't survive the reordering (EasyTer draws these lines in
        // the default FG too), and the cursor byte-mapping no longer holds.
        if claude {
            let full: String = segs.iter().map(|(s, _)| s.as_str()).collect();
            if let Some(fixed) = crate::bidi::restore_bidi_line(&full) {
                segs = vec![(fixed, FG)];
                cur_bytes = None;
            }
        }
        // shape unconstrained, then compress to the window if it overflows
        let natw = self.shape_scratch(&segs, 1_000_000.0, true);
        let scale = if natw > fw as f32 { fw as f32 / natw } else { 1.0 };
        self.blit_scratch(frame, fw, fh, 0.0, y, scale);
        if let Some((b0, b1)) = cur_bytes {
            let c0 = cosmic_text::Cursor::new(0, b0);
            let c1 = cosmic_text::Cursor::new(0, b1);
            for run in self.buffer.layout_runs() {
                if let Some((x, w)) = run.highlight(c0, c1) {
                    return Some((
                        (x * scale).round() as i32,
                        ((w * scale).ceil() as i32).max(2),
                    ));
                }
            }
        }
        None
    }

    /// A row without Arabic (prompts, code, TUIs): strict grid placement.
    /// ASCII batches into runs pinned at col*cell_w (uniform advance in a
    /// mono font, so columns stay exact); every other glyph — Nerd Font
    /// icons, powerline separators, box drawing, CJK — draws per cell,
    /// compressed into its box when wider. Backgrounds and glyphs can't drift.
    fn draw_line_grid(&mut self, frame: &mut [u32], fw: usize, fh: usize,
                      y: i32, cells: &[CellInfo]) {
        let n = cells.len();
        let mut i = 0;
        while i < n {
            let ci = &cells[i];
            if ci.c.is_ascii() {
                let col0 = ci.col;
                let mut segs: Vec<(String, (u8, u8, u8))> = Vec::new();
                let mut expect = ci.col;
                let mut j = i;
                while j < n {
                    let cj = &cells[j];
                    if !cj.c.is_ascii() || cj.col != expect {
                        break;
                    }
                    match segs.last_mut() {
                        Some((s, c)) if *c == cj.fg => cj.push_text(s),
                        _ => {
                            let mut s = String::new();
                            cj.push_text(&mut s);
                            segs.push((s, cj.fg));
                        }
                    }
                    expect = cj.col + cj.w;
                    j += 1;
                }
                // trailing blanks paint nothing (bg rects are separate)
                while let Some((s, _)) = segs.last_mut() {
                    while s.ends_with(' ') {
                        s.pop();
                    }
                    if s.is_empty() {
                        segs.pop();
                    } else {
                        break;
                    }
                }
                if !segs.is_empty() {
                    self.shape_scratch(&segs, 1_000_000.0, false);
                    self.blit_scratch(frame, fw, fh, col0 as f32 * self.cell_w, y, 1.0);
                }
                i = j;
            } else {
                // exotic glyph: pin to its own cell box, flush left so
                // powerline separators stay seamless; compress if wider
                let mut s = String::new();
                ci.push_text(&mut s);
                let seg = [(s, ci.fg)];
                let natw = self.shape_scratch(&seg, 1_000_000.0, false);
                let boxw = ci.w as f32 * self.cell_w;
                let scale = if natw > boxw + 0.5 { boxw / natw } else { 1.0 };
                self.blit_scratch(frame, fw, fh, ci.col as f32 * self.cell_w, y, scale);
                i += 1;
            }
        }
    }

    pub fn draw(&mut self, frame: &mut [u32], width: usize, height: usize,
                term: &Term<EventProxy>, overlay: &Overlay) {
        frame.fill(pack(BG));
        // everything below the tab bar
        let oy = self.tab_bar_h().round() as i32;
        let rows = term.screen_lines();
        let history = term.grid().history_size();
        let content = term.renderable_content();
        let cursor = content.cursor;
        let selection = content.selection;
        // scrolled into history: viewport row = grid line + display offset
        let off = content.display_offset as i32;

        // collect the visible grid; paint non-default cell backgrounds as
        // pixel-snapped rects (EasyTer lesson: snap both edges so adjacent
        // cells share a boundary — no 1px seams)
        let mut lines: Vec<Vec<CellInfo>> = Vec::with_capacity(rows);
        lines.resize_with(rows, Vec::new);
        let mut sel_cells: Vec<(usize, usize, usize)> = Vec::new(); // (vrow, col, w)
        let mut hit_cells: Vec<(usize, usize, usize)> = Vec::new();
        for cell in content.display_iter {
            let vrow = cell.point.line.0 + off;
            if vrow < 0 || vrow as usize >= rows {
                continue;
            }
            let li = vrow as usize;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let col = cell.point.column.0;
            let w = if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
            let mut fg = ansi_rgb(cell.fg);
            let mut bg = ansi_rgb(cell.bg);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if bg != BG {
                let x0 = (col as f32 * self.cell_w).round() as i32;
                let x1 = ((col + w) as f32 * self.cell_w).round() as i32;
                let y0 = oy + (li as f32 * self.cell_h).round() as i32;
                let y1 = oy + ((li + 1) as f32 * self.cell_h).round() as i32;
                fill_rect(frame, width, height, x0, y0, x1 - x0, y1 - y0, pack(bg));
            }
            if selection.is_some_and(|s| s.contains(cell.point)) {
                sel_cells.push((li, col, w));
            }
            if overlay
                .search_match
                .is_some_and(|m| *m.start() <= cell.point && cell.point <= *m.end())
            {
                hit_cells.push((li, col, w));
            }
            let zw = cell.zerowidth().map(|z| z.to_vec());
            lines[li].push(CellInfo { col, w, c: cell.c, zw, fg });
        }

        let cursor_vrow = cursor.point.line.0 + off;
        let ccol = cursor.point.column.0;
        let mut cursor_rect: Option<(i32, i32)> = None; // (x, w) via shaped layout
        for (li, cells) in lines.iter().enumerate() {
            if cells.is_empty() {
                continue;
            }
            let y = oy + (li as f32 * self.cell_h).round() as i32;
            if cells.iter().any(|ci| is_arabic(ci.c)) {
                let on_row = cursor_vrow >= 0 && cursor_vrow as usize == li;
                let r = self.draw_line_bidi(frame, width, height, y, cells,
                                            overlay.claude,
                                            if on_row { Some(ccol) } else { None });
                if r.is_some() {
                    cursor_rect = r;
                }
            } else {
                self.draw_line_grid(frame, width, height, y, cells);
            }
        }

        // translucent overlays above the text (EasyTer's stacking): selection
        // blue, current search hit amber
        for (cells, ((r, g, b), a)) in [(&sel_cells, SELECTION_RGBA), (&hit_cells, SEARCH_RGBA)] {
            for &(li, col, w) in cells.iter() {
                let x0 = (col as f32 * self.cell_w).round() as i32;
                let x1 = ((col + w) as f32 * self.cell_w).round() as i32;
                let y0 = oy + (li as f32 * self.cell_h).round() as i32;
                let y1 = oy + ((li + 1) as f32 * self.cell_h).round() as i32;
                blend_rect(frame, width, height, x0, y0, x1 - x0, y1 - y0, (r, g, b), a);
            }
        }

        // cursor: translucent block over the text so the glyph stays legible.
        // Grid rows are column-exact; on Arabic rows the position comes from
        // the shaped layout (logical col != visual x under RTL).
        if cursor_vrow >= 0 && (cursor_vrow as usize) < rows {
            let (x0, wpx) = cursor_rect.unwrap_or((
                (ccol as f32 * self.cell_w).round() as i32,
                self.cell_w.round() as i32,
            ));
            let y0 = oy + (cursor_vrow as f32 * self.cell_h).round() as i32;
            blend_rect(frame, width, height,
                       x0, y0, wpx, self.cell_h.round() as i32,
                       FG, 170);
        }

        // command-block lights in the left gutter (EasyTer's bars):
        // green = succeeded, red = failed, grey = still running/unknown
        for &(abs, exit) in overlay.marks {
            let vrow = abs as i64 - history as i64 + off as i64;
            if vrow < 0 || vrow >= rows as i64 {
                continue;
            }
            let color = match exit {
                Some(0) => (0x2e, 0xa0, 0x43),
                Some(_) => (0xcf, 0x22, 0x2e),
                None => (0x6e, 0x76, 0x81),
            };
            let y0 = oy + (vrow as f32 * self.cell_h).round() as i32;
            fill_rect(frame, width, height, 0, y0, 3,
                      self.cell_h.round() as i32, pack(color));
        }

        // scroll position indicator while in history (EasyTer's slim bar)
        if off > 0 && history > 0 {
            let vh = height as i32 - oy;
            let th = ((vh as f32 * rows as f32 / (history + rows) as f32) as i32).max(24);
            let ty = oy + ((vh - th) as f32 * (1.0 - off as f32 / history as f32)) as i32;
            blend_rect(frame, width, height, width as i32 - 7, ty, 4, th, FG, 70);
        }

        // Claude-mode badge, top-right below the tab bar (EasyTer's green badge)
        if overlay.claude {
            let label = "● وضع كلود".to_string();
            let segs = [(label, (0xff, 0xff, 0xff))];
            let tw = self.shape_scratch(&segs, 1_000_000.0, true);
            let bw = tw as i32 + 16;
            let bh = (self.cell_h + 8.0) as i32;
            let bx = width as i32 - bw - 10;
            let by = oy + if overlay.search_query.is_some() {
                (self.cell_h + 24.0) as i32
            } else {
                6
            };
            fill_rect(frame, width, height, bx, by, bw, bh, pack((0x2e, 0xa0, 0x43)));
            self.blit_scratch(frame, width, height, (bx + 8) as f32, by + 4, 1.0);
        }

        // search bar, top-right below the tab bar
        if let Some(q) = overlay.search_query {
            let bar_w = (self.cell_w * 34.0) as i32;
            let bar_h = (self.cell_h + 10.0) as i32;
            let bx = width as i32 - bar_w - 10;
            let by = oy + 6;
            fill_rect(frame, width, height, bx, by, bar_w, bar_h, pack((0x1c, 0x21, 0x28)));
            blend_rect(frame, width, height, bx, by + bar_h - 2, bar_w, 2,
                       PALETTE[11], 200); // amber underline = search accent
            let label = format!("بحث: {q}_");
            let segs = [(label, FG)];
            self.shape_scratch(&segs, 1_000_000.0, true);
            self.blit_scratch(frame, width, height, (bx + 8) as f32, by + 5, 1.0);
        }

        // the tab bar itself, drawn last (nothing may bleed into it)
        self.draw_tab_bar(frame, width, height, overlay.tabs);
    }

    fn draw_tab_bar(&mut self, frame: &mut [u32], width: usize, height: usize,
                    tabs: &[TabInfo]) {
        let oy = self.tab_bar_h().round() as i32;
        fill_rect(frame, width, height, 0, 0, width as i32, oy, pack((0x16, 0x1b, 0x22)));
        let tw = (self.cell_w * TAB_CELLS) as i32;
        for (i, tab) in tabs.iter().enumerate() {
            let x0 = i as i32 * tw;
            let (bg, fg): ((u8, u8, u8), (u8, u8, u8)) = if tab.active {
                ((0x24, 0x2b, 0x36), FG)
            } else {
                ((0x16, 0x1b, 0x22), (0x8a, 0x94, 0xa3))
            };
            fill_rect(frame, width, height, x0, 0, tw - 2, oy, pack(bg));
            if tab.active {
                // accent line on top: the focused tab is unmistakable
                fill_rect(frame, width, height, x0, 0, tw - 2, 2, pack((0x2e, 0xa0, 0x43)));
            }
            let max_chars = TAB_CELLS as usize - 5;
            let title: String = tab.title.chars().take(max_chars).collect();
            let segs = [(title, fg)];
            self.shape_scratch(&segs, 1_000_000.0, true);
            self.blit_scratch(frame, width, height, (x0 + 10) as f32, 5, 1.0);
            if tab.busy {
                // green dot: this background tab is producing output —
                // the agent-cockpit seed (a Claude finishing while you look away)
                let d = 6;
                blend_rect(frame, width, height, x0 + tw - 16, oy / 2 - d / 2, d, d,
                           (0x56, 0xd3, 0x64), 255);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Arabic must shape to real glyphs (no .notdef) through the fallback
    /// chain — this is Bayan's reason to exist. If this fails, the renderer
    /// is drawing blank lines for Arabic output.
    #[test]
    fn arabic_shapes_to_real_glyphs() {
        let mut fs = FontSystem::new();
        fs.db_mut().load_font_data(AMIRI.to_vec());
        let metrics = Metrics::new(15.0, 21.0);
        let mut b = Buffer::new(&mut fs, metrics);
        b.set_wrap(&mut fs, Wrap::None);
        b.set_size(&mut fs, Some(500.0), Some(21.0));
        b.set_text(&mut fs, "مرحبا بالعالم", base_attrs(), Shaping::Advanced);
        b.shape_until_scroll(&mut fs, false);
        let run = b.layout_runs().next().expect("one layout run");
        assert!(!run.glyphs.is_empty(), "no glyphs laid out at all");
        let notdef = run.glyphs.iter().filter(|g| g.glyph_id == 0).count();
        assert_eq!(notdef, 0, ".notdef glyphs: Arabic fell through the fallback");
        assert!(run.line_w > 10.0, "line width {} suspiciously small", run.line_w);
    }

    /// Latin text must keep working through the same path.
    #[test]
    fn latin_shapes_to_real_glyphs() {
        let mut fs = FontSystem::new();
        fs.db_mut().load_font_data(AMIRI.to_vec());
        let metrics = Metrics::new(15.0, 21.0);
        let mut b = Buffer::new(&mut fs, metrics);
        b.set_wrap(&mut fs, Wrap::None);
        b.set_size(&mut fs, Some(500.0), Some(21.0));
        b.set_text(&mut fs, "Hello Bayan 123", base_attrs(), Shaping::Advanced);
        b.shape_until_scroll(&mut fs, false);
        let run = b.layout_runs().next().expect("one layout run");
        assert_eq!(run.glyphs.iter().filter(|g| g.glyph_id == 0).count(), 0);
    }

    /// The cursor on an RTL row must map through the layout: logical char 0
    /// (rightmost visually) sits at a HIGHER x than the last logical char.
    /// This is what makes the block cursor land on the letter it edits.
    #[test]
    fn rtl_cursor_maps_through_the_layout() {
        let mut fs = FontSystem::new();
        fs.db_mut().load_font_data(AMIRI.to_vec());
        let mut b = Buffer::new(&mut fs, Metrics::new(15.0, 21.0));
        b.set_wrap(&mut fs, Wrap::None);
        b.set_size(&mut fs, Some(1_000_000.0), Some(21.0));
        let text = "مرحبا"; // 5 chars x 2 bytes
        b.set_text(&mut fs, text, base_attrs(), Shaping::Advanced);
        for line in b.lines.iter_mut() {
            line.set_align(Some(Align::Left));
        }
        b.shape_until_scroll(&mut fs, false);
        let hl = |b: &Buffer, b0: usize, b1: usize| -> f32 {
            for run in b.layout_runs() {
                if let Some((x, _)) = run.highlight(
                    cosmic_text::Cursor::new(0, b0),
                    cosmic_text::Cursor::new(0, b1),
                ) {
                    return x;
                }
            }
            panic!("no highlight for byte range {b0}..{b1}");
        };
        let first = hl(&b, 0, 2); // م — visually rightmost
        let last = hl(&b, 8, 10); // ا — visually leftmost
        assert!(
            first > last,
            "RTL mapping inverted: first logical char at x={first}, last at x={last}"
        );
    }

    /// The line classifier: Arabic rows take the BiDi path, others the grid.
    #[test]
    fn arabic_detection_ranges() {
        assert!(is_arabic('م'));
        assert!(is_arabic('ﻻ'));  // presentation form
        assert!(!is_arabic('a'));
        assert!(!is_arabic('\u{e0b0}')); // powerline triangle -> grid path
        assert!(!is_arabic('─'));        // box drawing -> grid path
    }
}

#[cfg(test)]
mod align_tests {
    use super::*;

    /// RTL-base lines must pin to the LEFT once aligned, matching the grid.
    /// (Unaligned, cosmic-text pushes them to the buffer's right edge — the
    /// bug that made Arabic output invisible in M1's first run.)
    #[test]
    fn rtl_lines_pin_left_when_aligned() {
        let mut fs = FontSystem::new();
        fs.db_mut().load_font_data(AMIRI.to_vec());
        let mut b = Buffer::new(&mut fs, Metrics::new(15.0, 21.0));
        b.set_wrap(&mut fs, Wrap::None);
        b.set_size(&mut fs, Some(800.0), Some(21.0));
        b.set_text(&mut fs, "مرحبا", base_attrs(), Shaping::Advanced);
        for line in b.lines.iter_mut() {
            line.set_align(Some(Align::Left));
        }
        b.shape_until_scroll(&mut fs, false);
        let run = b.layout_runs().next().unwrap();
        let first_x = run.glyphs.iter().map(|g| g.x as i32).min().unwrap();
        assert!(
            first_x < 5,
            "RTL line starts at x={first_x}, expected pinned to the left edge"
        );
    }
}

#[cfg(test)]
mod startup_probe {
    use super::*;
    use std::time::Instant;

    #[test]
    fn measure_startup_costs() {
        let t0 = Instant::now();
        let mut fs = FontSystem::new();
        let t_fontsystem = t0.elapsed();
        let t1 = Instant::now();
        fs.db_mut().load_font_data(AMIRI.to_vec());
        let t_amiri = t1.elapsed();
        let t2 = Instant::now();
        let fam = pick_family(fs.db(), None);
        let t_pick = t2.elapsed();
        let t3 = Instant::now();
        let mut b = Buffer::new(&mut fs, Metrics::new(15.0, 21.0));
        b.set_size(&mut fs, Some(100.0), Some(21.0));
        b.set_text(&mut fs, "M", Attrs::new().family(Family::Name(&fam)), Shaping::Advanced);
        b.shape_until_scroll(&mut fs, false);
        let t_first_shape = t3.elapsed();
        eprintln!("PROBE FontSystem::new = {:?}", t_fontsystem);
        eprintln!("PROBE load amiri      = {:?}", t_amiri);
        eprintln!("PROBE pick_family     = {:?} (faces: {})", t_pick, fs.db().faces().count());
        eprintln!("PROBE first shape     = {:?}", t_first_shape);
    }
}
