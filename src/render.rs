//! Software renderer: cosmic-text does the heavy lifting (HarfBuzz-class
//! shaping, BiDi, font fallback — Arabic joins correctly from day one) and
//! softbuffer presents the pixels. GPU (wgpu) lands in a later milestone;
//! the renderer boundary is designed so only this file changes.

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

fn pick_family(db: &cosmic_text::fontdb::Database) -> String {
    for cand in FAMILY_CANDIDATES {
        if db
            .faces()
            .any(|f| f.families.iter().any(|(name, _)| name == cand))
        {
            return (*cand).to_string();
        }
    }
    "Consolas".to_string()
}

fn base_attrs<'a>() -> Attrs<'a> {
    // used by tests; the renderer itself uses the picked family
    Attrs::new().family(Family::Name("Consolas"))
}

pub struct Renderer {
    font_system: FontSystem,
    cache: SwashCache,
    buffer: Buffer,
    family: String,
    pub cell_w: f32,
    pub cell_h: f32,
}

impl Renderer {
    pub fn new(scale: f32) -> Self {
        let mut font_system = FontSystem::new();
        font_system.db_mut().load_font_data(AMIRI.to_vec());
        let family = pick_family(font_system.db());
        let size = FONT_SIZE * scale;
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

    pub fn draw(&mut self, frame: &mut [u32], width: usize, height: usize, term: &Term<EventProxy>) {
        frame.fill(pack(BG));
        let rows = term.screen_lines();
        let content = term.renderable_content();
        let cursor = content.cursor;

        // collect the visible grid: per-row char+color, and non-default
        // cell backgrounds as pixel-snapped rects (EasyTer lesson: snap both
        // edges so adjacent cells share a boundary — no 1px seams)
        let mut lines: Vec<Vec<(char, (u8, u8, u8))>> = vec![Vec::new(); rows];
        for cell in content.display_iter {
            let line = cell.point.line.0;
            if line < 0 || line as usize >= rows {
                continue;
            }
            let line = line as usize;
            let col = cell.point.column.0;
            let mut fg = ansi_rgb(cell.fg);
            let mut bg = ansi_rgb(cell.bg);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if bg != BG {
                let x0 = (col as f32 * self.cell_w).round() as i32;
                let x1 = ((col + 1) as f32 * self.cell_w).round() as i32;
                let y0 = (line as f32 * self.cell_h).round() as i32;
                let y1 = ((line + 1) as f32 * self.cell_h).round() as i32;
                fill_rect(frame, width, height, x0, y0, x1 - x0, y1 - y0, pack(bg));
            }
            let row = &mut lines[line];
            while row.len() < col {
                row.push((' ', FG));
            }
            row.push((cell.c, fg));
        }

        // one shaped buffer for the whole screen: rows joined by '\n', colors
        // as rich-text spans. cosmic-text applies BiDi per line, so Arabic
        // from PowerShell (logical order) comes out right — already beyond
        // what Alacritty does.
        let mut segs: Vec<(String, (u8, u8, u8))> = Vec::new();
        for (i, row) in lines.iter().enumerate() {
            let mut end = row.len();
            while end > 0 && row[end - 1].0 == ' ' {
                end -= 1; // trailing blanks add shaping cost, render nothing
            }
            for &(ch, fg) in &row[..end] {
                match segs.last_mut() {
                    Some((s, c)) if *c == fg => s.push(ch),
                    _ => segs.push((String::from(ch), fg)),
                }
            }
            if i + 1 < rows {
                match segs.last_mut() {
                    Some((s, c)) if *c == FG => s.push('\n'),
                    _ => segs.push((String::from('\n'), FG)),
                }
            }
        }
        let base = Attrs::new().family(Family::Name(self.family.as_str()));
        let rich: Vec<(&str, Attrs)> = segs
            .iter()
            .map(|(s, c)| (s.as_str(), base.color(Color::rgb(c.0, c.1, c.2))))
            .collect();
        self.buffer
            .set_size(&mut self.font_system, Some(width as f32), Some(height as f32));
        self.buffer
            .set_rich_text(&mut self.font_system, rich, base, Shaping::Advanced);
        // pin every line to the LEFT: cosmic-text right-aligns RTL-base lines
        // to the buffer width, but the terminal grid owns horizontal placement
        // (conhost put the Arabic in columns 0..n). BiDi still reorders the
        // glyphs correctly inside the line.
        for line in self.buffer.lines.iter_mut() {
            line.set_align(Some(Align::Left));
        }
        self.buffer
            .shape_until_scroll(&mut self.font_system, false);
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
                for dy in 0..h as i32 {
                    let py = y + dy;
                    if py < 0 || py as usize >= height {
                        continue;
                    }
                    let row = py as usize * width;
                    for dx in 0..w as i32 {
                        let px = x + dx;
                        if px < 0 || px as usize >= width {
                            continue;
                        }
                        let i = row + px as usize;
                        frame[i] = blend(frame[i], rgb, a);
                    }
                }
            },
        );

        // cursor: translucent block over the text so the glyph stays legible.
        // (Arabic-shaped rows can run wider than col*cell_w — the exact grid
        // fit is milestone M2, same solution EasyTer uses.)
        let cl = cursor.point.line.0;
        if cl >= 0 && (cl as usize) < rows {
            let x0 = (cursor.point.column.0 as f32 * self.cell_w).round() as i32;
            let y0 = (cl as f32 * self.cell_h).round() as i32;
            blend_rect(frame, width, height,
                       x0, y0, self.cell_w.round() as i32, self.cell_h.round() as i32,
                       FG, 170);
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
