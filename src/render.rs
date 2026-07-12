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
//! Since M7 the unit of drawing is a PANE (a rect + its own Term): tabs can
//! split into side-by-side or stacked panes, each clipped to its rect.
//! Window-level chrome (tab bar, search bar, Claude badge) draws separately.
//!
//! cosmic-text does shaping/BiDi/fallback; softbuffer presents the pixels.
//! GPU (wgpu) lands in a later milestone behind this same boundary.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use cosmic_text::{
    Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache,
    SwashContent, Weight, Wrap,
};

use crate::gpu::Vertex;
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

// (color resolution lives on Renderer: bg/fg/palette are config-themable)

/// Straight-alpha color for the quad batch.
fn c4((r, g, b): (u8, u8, u8), a: u8) -> [f32; 4] {
    [
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        a as f32 / 255.0,
    ]
}

pub const ATLAS_SIZE: u32 = 2048;
/// uv of the white texel block at the atlas origin: solid quads sample it.
const WHITE_UV: f32 = 1.0 / ATLAS_SIZE as f32;

/// Emit one quad clipped to `clip` (uv adjusted proportionally): a pane's
/// content can never bleed into a neighbouring pane. `layer` is the atlas page.
#[allow(clippy::too_many_arguments)]
fn push_quad(out: &mut Vec<Vertex>, clip: Rect, x: f32, y: f32, w: f32, h: f32,
             layer: u32, u0: f32, v0: f32, u1: f32, v1: f32, color: [f32; 4]) {
    if w <= 0.0 || h <= 0.0 || color[3] <= 0.0 {
        return;
    }
    let (cx, cy, cw, ch) = clip;
    let (cx0, cy0) = (cx as f32, cy as f32);
    let (cx1, cy1) = ((cx + cw) as f32, (cy + ch) as f32);
    let (x1, y1) = (x + w, y + h);
    let (nx0, ny0) = (x.max(cx0), y.max(cy0));
    let (nx1, ny1) = (x1.min(cx1), y1.min(cy1));
    if nx0 >= nx1 || ny0 >= ny1 {
        return;
    }
    let du = (u1 - u0) / w;
    let dv = (v1 - v0) / h;
    let (mu0, mv0) = (u0 + (nx0 - x) * du, v0 + (ny0 - y) * dv);
    let (mu1, mv1) = (u1 - (x1 - nx1) * du, v1 - (y1 - ny1) * dv);
    let lf = layer as f32;
    let v = |px: f32, py: f32, u: f32, vv: f32| Vertex {
        pos: [px, py],
        uv: [u, vv],
        layer: lf,
        color,
    };
    out.extend_from_slice(&[
        v(nx0, ny0, mu0, mv0),
        v(nx1, ny0, mu1, mv0),
        v(nx0, ny1, mu0, mv1),
        v(nx1, ny0, mu1, mv0),
        v(nx1, ny1, mu1, mv1),
        v(nx0, ny1, mu0, mv1),
    ]);
}

/// Solid rect (samples the white texel on page 0).
fn push_rect(out: &mut Vec<Vertex>, clip: Rect, x: i32, y: i32, w: i32, h: i32,
             color: [f32; 4]) {
    push_quad(out, clip, x as f32, y as f32, w as f32, h as f32,
              0, WHITE_UV, WHITE_UV, WHITE_UV, WHITE_UV, color);
}

/// Hard ceiling on atlas pages (each is ATLAS_SIZE² RGBA = 16MB). Reached
/// only after ~tens of thousands of distinct glyphs — then, and only then,
/// the oldest content resets. In practice a session never exceeds 1-2 pages.
pub const MAX_PAGES: u32 = 8;

/// One rasterized glyph's slot in the atlas: which page, and its uv rect.
#[derive(Clone, Copy)]
struct GlyphEntry {
    layer: u32,
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
    w: u32,
    h: u32,
    left: i32,
    top: i32,
    is_color: bool,
}

/// CPU-side glyph atlas: a stack of shelf-packed RGBA pages that GROWS a new
/// page when the current one fills, instead of wiping everything (M10's
/// reset-loses-all became a visible flash with big fonts + emoji + Arabic).
pub struct Atlas {
    pub pages: Vec<Vec<u8>>,
    /// pixels changed since the last GPU sync (per-page dirty flag)
    pub dirty: bool,
    /// bumps when a NEW page is added — the GPU must recreate its texture
    /// array with more layers (rare: a handful of times per session at most)
    pub layer_gen: u32,
    /// bumps on the wholesale reset at MAX_PAGES — one healing redraw
    pub generation: u32,
    map: std::collections::HashMap<cosmic_text::CacheKey, Option<GlyphEntry>>,
    cur: u32, // page currently being filled
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
}

impl Atlas {
    fn new() -> Self {
        let mut a = Atlas {
            pages: vec![vec![0; (ATLAS_SIZE * ATLAS_SIZE * 4) as usize]],
            dirty: true,
            layer_gen: 0,
            generation: 0,
            map: std::collections::HashMap::new(),
            cur: 0,
            shelf_x: 4,
            shelf_y: 0,
            shelf_h: 4,
        };
        a.write_white_texel();
        a
    }

    /// The white texel lives at (0,0) of page 0 — solid quads sample it.
    fn write_white_texel(&mut self) {
        for y in 0..3u32 {
            for x in 0..3u32 {
                let i = ((y * ATLAS_SIZE + x) * 4) as usize;
                self.pages[0][i..i + 4].copy_from_slice(&[255; 4]);
            }
        }
    }

    /// Reserve a w×h slot, opening a new page (or resetting at the cap).
    /// Returns (layer, x, y).
    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32, u32)> {
        let pad = 1;
        if w + pad > ATLAS_SIZE || h + pad > ATLAS_SIZE {
            return None; // a single glyph larger than a whole page
        }
        if self.shelf_x + w + pad > ATLAS_SIZE {
            self.shelf_y += self.shelf_h + pad;
            self.shelf_x = 0;
            self.shelf_h = 0;
        }
        if self.shelf_y + h + pad > ATLAS_SIZE {
            // this page is full: grow a new one (no loss), or reset at the cap
            if (self.pages.len() as u32) < MAX_PAGES {
                self.pages.push(vec![0; (ATLAS_SIZE * ATLAS_SIZE * 4) as usize]);
                self.cur = self.pages.len() as u32 - 1;
                self.layer_gen = self.layer_gen.wrapping_add(1);
            } else {
                self.map.clear();
                for p in &mut self.pages {
                    p.fill(0);
                }
                self.cur = 0;
                self.write_white_texel();
                self.generation = self.generation.wrapping_add(1);
            }
            self.shelf_x = 0;
            self.shelf_y = 0;
            self.shelf_h = 0;
        }
        let pos = (self.cur, self.shelf_x, self.shelf_y);
        self.shelf_x += w + pad;
        self.shelf_h = self.shelf_h.max(h);
        Some(pos)
    }

    /// Get-or-rasterize a glyph (None = zero-size, e.g. spaces).
    fn entry(&mut self, fs: &mut FontSystem, swash: &mut SwashCache,
             key: cosmic_text::CacheKey) -> Option<GlyphEntry> {
        if let Some(e) = self.map.get(&key) {
            return *e;
        }
        let entry = swash.get_image_uncached(fs, key).and_then(|img| {
            let (w, h) = (img.placement.width, img.placement.height);
            if w == 0 || h == 0 {
                return None;
            }
            let (layer, ax, ay) = self.alloc(w, h)?;
            let is_color = matches!(img.content, SwashContent::Color);
            let page = &mut self.pages[layer as usize];
            for row in 0..h {
                for col in 0..w {
                    let dst = (((ay + row) * ATLAS_SIZE + ax + col) * 4) as usize;
                    let px: [u8; 4] = match img.content {
                        SwashContent::Mask => {
                            let a = img.data[(row * w + col) as usize];
                            [255, 255, 255, a]
                        }
                        SwashContent::Color => {
                            let s = ((row * w + col) * 4) as usize;
                            [img.data[s], img.data[s + 1], img.data[s + 2], img.data[s + 3]]
                        }
                        SwashContent::SubpixelMask => {
                            let s = ((row * w + col) * 4) as usize;
                            [255, 255, 255, img.data[s]]
                        }
                    };
                    page[dst..dst + 4].copy_from_slice(&px);
                }
            }
            self.dirty = true;
            let s = ATLAS_SIZE as f32;
            Some(GlyphEntry {
                layer,
                u0: ax as f32 / s,
                v0: ay as f32 / s,
                u1: (ax + w) as f32 / s,
                v1: (ay + h) as f32 / s,
                w,
                h,
                left: img.placement.left,
                top: img.placement.top,
                is_color,
            })
        });
        self.map.insert(key, entry);
        entry
    }
}

/// Emit a shaped buffer's glyphs as atlas quads at (x_off, y_off), with the
/// horizontal compression `scale` (EasyTer's fit — now a free GPU transform).
#[allow(clippy::too_many_arguments)]
fn push_shaped(out: &mut Vec<Vertex>, fs: &mut FontSystem, swash: &mut SwashCache,
               atlas: &mut Atlas, buf: &Buffer, clip: Rect,
               x_off: f32, y_off: i32, scale: f32, default_fg: (u8, u8, u8)) {
    for run in buf.layout_runs() {
        for glyph in run.glyphs.iter() {
            let phys = glyph.physical((0.0, 0.0), 1.0);
            let Some(e) = atlas.entry(fs, swash, phys.cache_key) else {
                continue;
            };
            let color = if e.is_color {
                [1.0, 1.0, 1.0, 1.0]
            } else {
                glyph
                    .color_opt
                    .map(|c| c4((c.r(), c.g(), c.b()), c.a()))
                    .unwrap_or_else(|| c4(default_fg, 255))
            };
            let gx = x_off + (phys.x as f32 + e.left as f32) * scale;
            let gy = y_off as f32 + run.line_y + phys.y as f32 - e.top as f32;
            push_quad(out, clip, gx, gy, e.w as f32 * scale, e.h as f32,
                      e.layer, e.u0, e.v0, e.u1, e.v1, color);
        }
    }
}

/// One tab as the bar renders it.
pub struct TabInfo {
    pub title: String,
    pub busy: bool,
    /// a pane in this background tab rang the bell (Claude asking for an
    /// approval): amber beats the green busy dot
    pub attention: bool,
    pub active: bool,
}

/// Fixed tab width in cells — the app's click hit-test relies on this.
pub const TAB_CELLS: f32 = 24.0;

/// A rect in frame pixels: (x, y, w, h).
pub type Rect = (i32, i32, i32, i32);

/// One row of the agent cockpit (Ctrl+Shift+D): a tab at a glance.
pub struct CockpitEntry {
    pub title: String,
    /// running command, or the idle directory
    pub status: String,
    pub busy: bool,
    pub attention: bool,
    pub active: bool,
}

/// One pane's draw parameters.
pub struct PaneView<'a> {
    pub rect: Rect,
    pub focused: bool,
    /// draw a border (only when the tab actually has multiple panes)
    pub bordered: bool,
    /// Claude mode for THIS pane's content.
    pub claude: bool,
    /// The current search hit (focused pane only).
    pub search_match: Option<&'a std::ops::RangeInclusive<alacritty_terminal::index::Point>>,
    /// Command blocks: (absolute prompt line, exit code) — gutter lights.
    pub marks: &'a [(usize, Option<i32>)],
}

// translucent overlays, EasyTer's colors: selection blue, search amber
const SELECTION_RGBA: ((u8, u8, u8), u32) = ((80, 140, 255), 90);
const SEARCH_RGBA: ((u8, u8, u8), u32) = ((240, 180, 40), 120);

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
    style: u8,
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
// bundled so connected Arabic works from a fresh clone (same fonts EasyTer
// ships) — bold included, so Arabic bold is a real weight, not a fake
const AMIRI: &[u8] = include_bytes!("../fonts/Amiri-Regular.ttf");
const AMIRI_BOLD: &[u8] = include_bytes!("../fonts/Amiri-Bold.ttf");

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

// text style bits carried per segment (baked into the shaped run)
pub(crate) const ST_BOLD: u8 = 1;
pub(crate) const ST_ITALIC: u8 = 2;
pub(crate) const ST_UNDERLINE: u8 = 4;
pub(crate) const ST_STRIKE: u8 = 8;

/// One rich-text segment: text + color + style.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct Seg {
    text: String,
    fg: (u8, u8, u8),
    style: u8,
}

impl Seg {
    fn plain(text: impl Into<String>, fg: (u8, u8, u8)) -> Self {
        Seg { text: text.into(), fg, style: 0 }
    }
}

/// One shaped run, cached: shaping is the paint loop's dominant cost, and a
/// terminal redraws the same lines almost every frame.
struct ShapedRun {
    buffer: Buffer,
    natw: f32,
}

/// Cache key: the exact rich-text content (text + color + style) + alignment.
type RunKey = (Vec<Seg>, bool);

/// Generational cap: on overflow the hot map becomes the cold one and a
/// fresh hot map starts — the working set survives, stale runs age out
/// without a clear-everything stall (EasyTer's LRU lesson, generational).
const CACHE_CAP: usize = 4096;

pub struct Renderer {
    font_system: FontSystem,
    cache: SwashCache,
    metrics: Metrics,
    cache_hot: std::collections::HashMap<RunKey, ShapedRun>,
    cache_cold: std::collections::HashMap<RunKey, ShapedRun>,
    pub atlas: Atlas,
    family: String,
    // theme (config-overridable; defaults are EasyTer's heritage colors)
    pub bg: (u8, u8, u8),
    fg: (u8, u8, u8),
    palette: [(u8, u8, u8); 16],
    pub cell_w: f32,
    pub cell_h: f32,
}

impl Renderer {
    /// `extra_pts` is the live Ctrl+wheel zoom delta on top of the
    /// configured base size.
    pub fn new(scale: f32, cfg: &crate::config::UserConfig, extra_pts: f32) -> Self {
        let mut font_system = FontSystem::new();
        font_system.db_mut().load_font_data(AMIRI.to_vec());
        font_system.db_mut().load_font_data(AMIRI_BOLD.to_vec());
        let family = pick_family(font_system.db(), cfg.font_family.as_deref());
        let size = (cfg.font_size.unwrap_or(FONT_SIZE) + extra_pts).clamp(8.0, 40.0) * scale;
        let metrics = Metrics::new(size, (size * 1.4).ceil());
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
        let mut palette = PALETTE;
        if let Some(p) = &cfg.palette {
            for (slot, hex) in palette.iter_mut().zip(p.iter()) {
                if let Some(c) = crate::config::parse_hex(hex) {
                    *slot = c;
                }
            }
        }
        Self {
            font_system,
            cache: SwashCache::new(),
            metrics,
            cache_hot: std::collections::HashMap::new(),
            cache_cold: std::collections::HashMap::new(),
            atlas: Atlas::new(),
            family,
            bg: cfg.bg.as_deref().and_then(crate::config::parse_hex).unwrap_or(BG),
            fg: cfg.fg.as_deref().and_then(crate::config::parse_hex).unwrap_or(FG),
            palette,
            cell_w,
            cell_h: metrics.line_height,
        }
    }

    fn named_rgb(&self, name: NamedColor) -> (u8, u8, u8) {
        use NamedColor::*;
        match name {
            Black | DimBlack => self.palette[0],
            Red | DimRed => self.palette[1],
            Green | DimGreen => self.palette[2],
            Yellow | DimYellow => self.palette[3],
            Blue | DimBlue => self.palette[4],
            Magenta | DimMagenta => self.palette[5],
            Cyan | DimCyan => self.palette[6],
            White | DimWhite => self.palette[7],
            BrightBlack => self.palette[8],
            BrightRed => self.palette[9],
            BrightGreen => self.palette[10],
            BrightYellow => self.palette[11],
            BrightBlue => self.palette[12],
            BrightMagenta => self.palette[13],
            BrightCyan => self.palette[14],
            BrightWhite => self.palette[15],
            Background => self.bg,
            _ => self.fg, // Foreground / BrightForeground / DimForeground
        }
    }

    fn ansi_rgb(&self, color: AnsiColor) -> (u8, u8, u8) {
        match color {
            AnsiColor::Named(n) => self.named_rgb(n),
            AnsiColor::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
            AnsiColor::Indexed(i) => match i {
                0..=15 => self.palette[i as usize],
                16..=231 => {
                    let n = i - 16;
                    let f = |c: u8| if c == 0 { 0 } else { 55 + 40 * c };
                    (f(n / 36), f((n % 36) / 6), f(n % 6))
                }
                _ => {
                    let v = 8 + 10 * (i - 232);
                    (v, v, v)
                }
            },
        }
    }

    /// Get-or-shape a run. Hits are the common case: a terminal repaints
    /// the same content nearly every frame, and shaping is the expensive
    /// part of the CPU renderer (the honest 80% of what wgpu would buy).
    fn ensure_shaped(&mut self, segs: &[Seg], align_left: bool) -> RunKey {
        let key: RunKey = (segs.to_vec(), align_left);
        if self.cache_hot.contains_key(&key) {
            return key;
        }
        if let Some(run) = self.cache_cold.remove(&key) {
            self.cache_hot.insert(key.clone(), run);
            return key;
        }
        let base = Attrs::new().family(Family::Name(self.family.as_str()));
        let rich: Vec<(&str, Attrs)> = segs
            .iter()
            .map(|s| {
                let mut a = base.color(Color::rgb(s.fg.0, s.fg.1, s.fg.2));
                if s.style & ST_BOLD != 0 {
                    a = a.weight(Weight::BOLD);
                }
                if s.style & ST_ITALIC != 0 {
                    a = a.style(Style::Italic);
                }
                (s.text.as_str(), a)
            })
            .collect();
        let mut buffer = Buffer::new(&mut self.font_system, self.metrics);
        buffer.set_wrap(&mut self.font_system, Wrap::None);
        buffer.set_size(&mut self.font_system, Some(1_000_000.0), Some(self.cell_h));
        buffer.set_rich_text(&mut self.font_system, rich, base, Shaping::Advanced);
        if align_left {
            for line in buffer.lines.iter_mut() {
                line.set_align(Some(Align::Left));
            }
        }
        buffer.shape_until_scroll(&mut self.font_system, false);
        let natw = buffer
            .layout_runs()
            .map(|r| r.line_w)
            .fold(0.0_f32, f32::max);
        if self.cache_hot.len() >= CACHE_CAP {
            self.cache_cold = std::mem::take(&mut self.cache_hot);
        }
        self.cache_hot.insert(key.clone(), ShapedRun { buffer, natw });
        key
    }

    /// Height of the tab bar in pixels — pane content starts below it.
    pub fn tab_bar_h(&self) -> f32 {
        self.cell_h + 10.0
    }

    /// Natural width of a run (shaping it into the cache if new).
    fn measure(&mut self, segs: &[Seg], align_left: bool) -> f32 {
        let key = self.ensure_shaped(segs, align_left);
        self.cache_hot[&key].natw
    }

    /// Shape (cached) and emit a run's glyph quads at (x, y), optionally
    /// compressed horizontally, clipped to `clip`. Returns the natural width.
    #[allow(clippy::too_many_arguments)]
    fn draw_run(&mut self, segs: &[Seg], align_left: bool,
                out: &mut Vec<Vertex>, clip: Rect,
                x: f32, y: i32, scale: f32) -> f32 {
        let key = self.ensure_shaped(segs, align_left);
        let Renderer { cache_hot, font_system, cache, atlas, fg, .. } = self;
        let run = &cache_hot[&key];
        push_shaped(out, font_system, cache, atlas, &run.buffer, clip, x, y, scale, *fg);
        run.natw
    }

    /// A row containing Arabic: shape the WHOLE line so cosmic-text applies
    /// UAX#9 BiDi (mixed directions, LTR islands). Compressed to the pane
    /// when overflowing; the cursor resolves THROUGH the shaped layout.
    #[allow(clippy::too_many_arguments)]
    fn draw_line_bidi(&mut self, out: &mut Vec<Vertex>, clip: Rect,
                      x0: i32, y: i32, pane_w: i32, cells: &[CellInfo], claude: bool,
                      cursor_col: Option<usize>) -> Option<(i32, i32)> {
        let mut end = cells.len();
        while end > 0 && cells[end - 1].c == ' ' {
            end -= 1;
        }
        let mut segs: Vec<Seg> = Vec::new();
        let mut cur_bytes: Option<(usize, usize)> = None;
        let mut nbytes = 0usize;
        for ci in &cells[..end] {
            if cursor_col == Some(ci.col) {
                cur_bytes = Some((nbytes, nbytes + ci.c.len_utf8()));
            }
            nbytes += ci.text_len();
            match segs.last_mut() {
                Some(s) if s.fg == ci.fg && s.style == ci.style => ci.push_text(&mut s.text),
                _ => {
                    let mut s = String::new();
                    ci.push_text(&mut s);
                    segs.push(Seg { text: s, fg: ci.fg, style: ci.style });
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
            let full: String = segs.iter().map(|s| s.text.as_str()).collect();
            if let Some(fixed) = crate::bidi::restore_bidi_line(&full) {
                segs = vec![Seg::plain(fixed, self.fg)];
                cur_bytes = None;
            }
        }
        // shape unconstrained (cached), then compress to the pane on overflow
        let natw = self.measure(&segs, true);
        let scale = if natw > pane_w as f32 { pane_w as f32 / natw } else { 1.0 };
        self.draw_run(&segs, true, out, clip, x0 as f32, y, scale);
        if let Some((b0, b1)) = cur_bytes {
            let c0 = cosmic_text::Cursor::new(0, b0);
            let c1 = cosmic_text::Cursor::new(0, b1);
            let key = self.ensure_shaped(&segs, true);
            for run in self.cache_hot[&key].buffer.layout_runs() {
                if let Some((x, w)) = run.highlight(c0, c1) {
                    return Some((
                        x0 + (x * scale).round() as i32,
                        ((w * scale).ceil() as i32).max(2),
                    ));
                }
            }
        }
        None
    }

    /// A row without Arabic (prompts, code, TUIs): strict grid placement.
    /// ASCII batches into runs pinned at col*cell_w; every other glyph draws
    /// per cell, compressed into its box when wider.
    fn draw_line_grid(&mut self, out: &mut Vec<Vertex>, clip: Rect,
                      x0: i32, y: i32, cells: &[CellInfo]) {
        let n = cells.len();
        let mut i = 0;
        // underline/strikethrough decorations: (col_start, col_end, style, fg)
        let mut decos: Vec<(usize, usize, u8, (u8, u8, u8))> = Vec::new();
        let mut note_deco = |ci: &CellInfo| {
            if ci.style & (ST_UNDERLINE | ST_STRIKE) == 0 {
                return;
            }
            match decos.last_mut() {
                Some((_, end, st, fg)) if *end == ci.col && *st == ci.style && *fg == ci.fg => {
                    *end = ci.col + ci.w;
                }
                _ => decos.push((ci.col, ci.col + ci.w, ci.style, ci.fg)),
            }
        };
        while i < n {
            let ci = &cells[i];
            if ci.c.is_ascii() {
                let col0 = ci.col;
                let mut segs: Vec<Seg> = Vec::new();
                let mut expect = ci.col;
                let mut j = i;
                while j < n {
                    let cj = &cells[j];
                    if !cj.c.is_ascii() || cj.col != expect {
                        break;
                    }
                    note_deco(cj);
                    match segs.last_mut() {
                        Some(s) if s.fg == cj.fg && s.style == cj.style => {
                            cj.push_text(&mut s.text)
                        }
                        _ => {
                            let mut s = String::new();
                            cj.push_text(&mut s);
                            segs.push(Seg { text: s, fg: cj.fg, style: cj.style });
                        }
                    }
                    expect = cj.col + cj.w;
                    j += 1;
                }
                // trailing blanks paint nothing (bg rects are separate)
                while let Some(s) = segs.last_mut() {
                    while s.text.ends_with(' ') {
                        s.text.pop();
                    }
                    if s.text.is_empty() {
                        segs.pop();
                    } else {
                        break;
                    }
                }
                if !segs.is_empty() {
                    self.draw_run(&segs, false, out, clip,
                                  x0 as f32 + col0 as f32 * self.cell_w, y, 1.0);
                }
                i = j;
            } else {
                // exotic glyph: pin to its own cell box, flush left so
                // powerline separators stay seamless; compress if wider
                note_deco(ci);
                let mut s = String::new();
                ci.push_text(&mut s);
                let seg = [Seg { text: s, fg: ci.fg, style: ci.style }];
                let natw = self.measure(&seg, false);
                let boxw = ci.w as f32 * self.cell_w;
                let scale = if natw > boxw + 0.5 { boxw / natw } else { 1.0 };
                self.draw_run(&seg, false, out, clip,
                              x0 as f32 + ci.col as f32 * self.cell_w, y, scale);
                i += 1;
            }
        }
        // decoration lines over the text (underline hugs the cell bottom)
        let th = (self.cell_h / 14.0).max(1.0) as i32;
        for (c0, c1, style, fg) in decos {
            let dx0 = x0 + (c0 as f32 * self.cell_w).round() as i32;
            let dx1 = x0 + (c1 as f32 * self.cell_w).round() as i32;
            if style & ST_UNDERLINE != 0 {
                let uy = y + self.cell_h.round() as i32 - th - 1;
                push_rect(out, clip, dx0, uy, dx1 - dx0, th, c4(fg, 255));
            }
            if style & ST_STRIKE != 0 {
                let sy = y + (self.cell_h * 0.55).round() as i32;
                push_rect(out, clip, dx0, sy, dx1 - dx0, th, c4(fg, 255));
            }
        }
    }

    /// Draw one pane's terminal into its rect.
    pub fn draw_pane(&mut self, out: &mut Vec<Vertex>,
                     view: &PaneView, term: &Term<EventProxy>) {
        let (px, py, pw, ph) = view.rect;
        let clip = view.rect;
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
            let mut fg = self.ansi_rgb(cell.fg);
            let mut bg = self.ansi_rgb(cell.bg);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            // text attributes -> style bits (weight/slant shape into the run;
            // under/strike draw as rects; DIM fades the color; HIDDEN blanks)
            let mut style = 0u8;
            if cell.flags.intersects(Flags::BOLD) {
                style |= ST_BOLD;
            }
            if cell.flags.intersects(Flags::ITALIC) {
                style |= ST_ITALIC;
            }
            if cell.flags.intersects(
                Flags::UNDERLINE
                    | Flags::DOUBLE_UNDERLINE
                    | Flags::UNDERCURL
                    | Flags::DOTTED_UNDERLINE
                    | Flags::DASHED_UNDERLINE,
            ) {
                style |= ST_UNDERLINE;
            }
            if cell.flags.intersects(Flags::STRIKEOUT) {
                style |= ST_STRIKE;
            }
            if cell.flags.intersects(Flags::DIM) {
                fg = (fg.0 * 2 / 3, fg.1 * 2 / 3, fg.2 * 2 / 3);
            }
            let ch = if cell.flags.intersects(Flags::HIDDEN) { ' ' } else { cell.c };
            if bg != self.bg {
                let x0 = px + (col as f32 * self.cell_w).round() as i32;
                let x1 = px + ((col + w) as f32 * self.cell_w).round() as i32;
                let y0 = py + (li as f32 * self.cell_h).round() as i32;
                let y1 = py + ((li + 1) as f32 * self.cell_h).round() as i32;
                push_rect(out, clip, x0, y0, x1 - x0, y1 - y0, c4(bg, 255));
            }
            if selection.is_some_and(|s| s.contains(cell.point)) {
                sel_cells.push((li, col, w));
            }
            if view
                .search_match
                .is_some_and(|m| *m.start() <= cell.point && cell.point <= *m.end())
            {
                hit_cells.push((li, col, w));
            }
            let zw = cell.zerowidth().map(|z| z.to_vec());
            lines[li].push(CellInfo { col, w, c: ch, zw, fg, style });
        }

        let cursor_vrow = cursor.point.line.0 + off;
        let ccol = cursor.point.column.0;
        let mut cursor_rect: Option<(i32, i32)> = None; // (x, w) via shaped layout
        for (li, cells) in lines.iter().enumerate() {
            if cells.is_empty() {
                continue;
            }
            let y = py + (li as f32 * self.cell_h).round() as i32;
            if cells.iter().any(|ci| is_arabic(ci.c)) {
                let on_row = cursor_vrow >= 0 && cursor_vrow as usize == li;
                let r = self.draw_line_bidi(out, clip, px, y, pw, cells,
                                            view.claude,
                                            if on_row { Some(ccol) } else { None });
                if r.is_some() {
                    cursor_rect = r;
                }
            } else {
                self.draw_line_grid(out, clip, px, y, cells);
            }
        }

        // translucent overlays above the text (EasyTer's stacking): selection
        // blue, current search hit amber
        for (cells, ((r, g, b), a)) in [(&sel_cells, SELECTION_RGBA), (&hit_cells, SEARCH_RGBA)] {
            for &(li, col, w) in cells.iter() {
                let x0 = px + (col as f32 * self.cell_w).round() as i32;
                let x1 = px + ((col + w) as f32 * self.cell_w).round() as i32;
                let y0 = py + (li as f32 * self.cell_h).round() as i32;
                let y1 = py + ((li + 1) as f32 * self.cell_h).round() as i32;
                push_rect(out, clip, x0, y0, x1 - x0, y1 - y0, c4((r, g, b), a as u8));
            }
        }

        // cursor: only the focused pane shows the block (translucent, so the
        // glyph stays legible); grid rows are column-exact, Arabic rows map
        // through the shaped layout
        if view.focused && cursor_vrow >= 0 && (cursor_vrow as usize) < rows {
            let (x0, wpx) = cursor_rect.unwrap_or((
                px + (ccol as f32 * self.cell_w).round() as i32,
                self.cell_w.round() as i32,
            ));
            let y0 = py + (cursor_vrow as f32 * self.cell_h).round() as i32;
            push_rect(out, clip, x0, y0, wpx, self.cell_h.round() as i32,
                      c4(self.fg, 170));
        }

        // command-block lights in the pane's left gutter (EasyTer's bars)
        for &(abs, exit) in view.marks {
            let vrow = abs as i64 - history as i64 + off as i64;
            if vrow < 0 || vrow >= rows as i64 {
                continue;
            }
            let color = match exit {
                Some(0) => (0x2e, 0xa0, 0x43),
                Some(_) => (0xcf, 0x22, 0x2e),
                None => (0x6e, 0x76, 0x81),
            };
            let y0 = py + (vrow as f32 * self.cell_h).round() as i32;
            push_rect(out, clip, px, y0, 3, self.cell_h.round() as i32,
                      c4(color, 255));
        }

        // scroll position indicator while in history (EasyTer's slim bar)
        if off > 0 && history > 0 {
            let th = ((ph as f32 * rows as f32 / (history + rows) as f32) as i32).max(24);
            let ty = py + ((ph - th) as f32 * (1.0 - off as f32 / history as f32)) as i32;
            push_rect(out, clip, px + pw - 7, ty, 4, th, c4(self.fg, 70));
        }

        // pane border when split: green marks the focused pane (EasyTer)
        if view.bordered {
            let c = if view.focused {
                c4((0x2e, 0xa0, 0x43), 255)
            } else {
                c4((0x30, 0x36, 0x3d), 255)
            };
            push_rect(out, clip, px, py, pw, 1, c);
            push_rect(out, clip, px, py + ph - 1, pw, 1, c);
            push_rect(out, clip, px, py, 1, ph, c);
            push_rect(out, clip, px + pw - 1, py, 1, ph, c);
        }
    }

    /// Cockpit panel geometry (shared with the app's click hit-test).
    pub fn cockpit_rect(&self, fw: usize, fh: usize, n: usize) -> Rect {
        let w = ((self.cell_w * 64.0) as i32).min(fw as i32 - 60);
        let row_h = self.cockpit_row_h();
        let header = row_h + 6;
        let h = header + row_h * n.max(1) as i32 + 14;
        let x = (fw as i32 - w) / 2;
        let y = (self.tab_bar_h() as i32 + (fh as i32 - h) / 4).max(self.tab_bar_h() as i32 + 8);
        (x, y, w, h)
    }

    pub fn cockpit_row_h(&self) -> i32 {
        (self.cell_h + 12.0) as i32
    }

    /// Which cockpit row a pixel position lands on.
    pub fn cockpit_row_at(&self, fw: usize, fh: usize, n: usize, px: f64, py: f64) -> Option<usize> {
        let (x, y, w, _h) = self.cockpit_rect(fw, fh, n);
        let row_h = self.cockpit_row_h();
        let rows_top = y + row_h + 6;
        if px < x as f64 || px >= (x + w) as f64 || py < rows_top as f64 {
            return None;
        }
        let row = ((py - rows_top as f64) / row_h as f64) as usize;
        (row < n).then_some(row)
    }

    /// The agent cockpit: every tab's state on one card. Amber = waiting for
    /// you (bell), green = working, dim = idle. Enter/click jumps.
    pub fn draw_cockpit(&mut self, out: &mut Vec<Vertex>, fw: usize, fh: usize,
                        entries: &[CockpitEntry], sel: usize) {
        let whole: Rect = (0, 0, fw as i32, fh as i32);
        let (x, y, w, h) = self.cockpit_rect(fw, fh, entries.len());
        // dim the world behind the card
        push_rect(out, whole, 0, 0, fw as i32, fh as i32, c4((0, 0, 0), 110));
        push_rect(out, whole, x, y, w, h, c4((0x1c, 0x21, 0x28), 255));
        push_rect(out, whole, x, y, w, 2, c4((0x2e, 0xa0, 0x43), 255)); // accent
        let row_h = self.cockpit_row_h();
        // header (bold — and a live proof the style pipeline works)
        let head = [Seg { text: "مقصورة الوكلاء".into(), fg: self.fg, style: ST_BOLD }];
        self.draw_run(&head, true, out, whole, (x + 14) as f32, y + 8, 1.0);
        let rows_top = y + row_h + 6;
        for (i, e) in entries.iter().enumerate() {
            let ry = rows_top + i as i32 * row_h;
            if i == sel {
                push_rect(out, whole, x + 4, ry, w - 8, row_h, c4((0x24, 0x2b, 0x36), 255));
            }
            // state dot: attention (amber) beats busy (green) beats idle (dim)
            let dot = if e.attention {
                (0xf2, 0xcc, 0x60)
            } else if e.busy {
                (0x56, 0xd3, 0x64)
            } else {
                (0x6e, 0x76, 0x81)
            };
            let d = 8;
            push_rect(out, whole, x + 14, ry + row_h / 2 - d / 2, d, d, c4(dot, 255));
            let fg = if e.active { self.fg } else { (0xb0, 0xb8, 0xc4) };
            let mark = if e.active { "● " } else { "" };
            let title: String = e.title.chars().take(22).collect();
            let status: String = e.status.chars().take(48).collect();
            let segs = [
                Seg::plain(format!("{mark}{title}"), fg),
                Seg::plain("   —   ", (0x6e, 0x76, 0x81)),
                Seg::plain(status, (0x9a, 0xa4, 0xb2)),
            ];
            self.draw_run(&segs, true, out, whole, (x + 32) as f32, ry + 6, 1.0);
        }
    }

    /// Debug (BAYAN_ATLAS_STRESS): draw a big grid of large distinct glyphs
    /// to force the atlas past one page, then a line of ASCII whose glyphs
    /// were rasterized into page 0 first — if paging is correct both render.
    pub fn stress_atlas(&mut self, out: &mut Vec<Vertex>, fw: usize, fh: usize) {
        let whole: Rect = (0, 0, fw as i32, fh as i32);
        let mut y = self.tab_bar_h() as i32 + 4;
        // many CJK glyphs at a big scale chew through page space fast
        for base in 0x4E00u32..0x4E00 + 900 {
            if y > fh as i32 - 40 {
                break;
            }
            let mut line = String::new();
            for k in 0..40 {
                if let Some(c) = char::from_u32(base + k * 20) {
                    line.push(c);
                }
            }
            let seg = [Seg::plain(line, self.fg)];
            self.draw_run(&seg, false, out, whole, 4.0, y, 1.0);
            y += self.cell_h as i32;
        }
        // page count now > 1; this ASCII line's glyphs live on page 0
        let tag = [Seg { text: "ATLAS PAGE OK — الصفحة تعمل".into(), fg: (0x7e, 0xe7, 0x87), style: ST_BOLD }];
        self.draw_run(&tag, false, out, whole, 4.0, fh as i32 - 30, 1.0);
    }

    /// The paste guard (EasyTer's protection): a multi-line/huge paste can
    /// execute commands on arrival — confirm before sending it to the shell.
    pub fn draw_paste_guard(&mut self, out: &mut Vec<Vertex>, fw: usize, fh: usize,
                            lines: usize, chars: usize) {
        let whole: Rect = (0, 0, fw as i32, fh as i32);
        push_rect(out, whole, 0, 0, fw as i32, fh as i32, c4((0, 0, 0), 110));
        let w = ((self.cell_w * 56.0) as i32).min(fw as i32 - 60);
        let h = (self.cell_h * 2.0 + 24.0) as i32;
        let x = (fw as i32 - w) / 2;
        let y = self.tab_bar_h() as i32 + (fh as i32 - h) / 4;
        push_rect(out, whole, x, y, w, h, c4((0x1c, 0x21, 0x28), 255));
        push_rect(out, whole, x, y, w, 2, c4((0xf2, 0xcc, 0x60), 255)); // warn accent
        let l1 = [Seg {
            text: format!("لصق {lines} سطراً ({chars} حرفاً)؟ اللصق متعدد الأسطر قد ينفّذ أوامر فوراً."),
            fg: self.fg,
            style: ST_BOLD,
        }];
        self.draw_run(&l1, true, out, whole, (x + 14) as f32, y + 8, 1.0);
        let l2 = [Seg::plain("Enter تأكيد   ·   Esc إلغاء", (0x9a, 0xa4, 0xb2))];
        self.draw_run(&l2, true, out, whole, (x + 14) as f32,
                      y + 12 + self.cell_h as i32, 1.0);
    }

    /// Window-level chrome: tab bar, search bar, Claude badge.
    pub fn draw_chrome(&mut self, out: &mut Vec<Vertex>, width: usize, height: usize,
                       tabs: &[TabInfo], search_query: Option<&str>, claude: bool) {
        let oy = self.tab_bar_h().round() as i32;
        let whole: Rect = (0, 0, width as i32, height as i32);

        // Claude-mode badge, top-right below the tab bar (EasyTer's green badge)
        if claude {
            let segs = [Seg::plain("● وضع كلود", (0xff, 0xff, 0xff))];
            let tw = self.measure(&segs, true);
            let bw = tw as i32 + 16;
            let bh = (self.cell_h + 8.0) as i32;
            let bx = width as i32 - bw - 10;
            let by = oy + if search_query.is_some() {
                (self.cell_h + 24.0) as i32
            } else {
                6
            };
            push_rect(out, whole, bx, by, bw, bh, c4((0x2e, 0xa0, 0x43), 255));
            self.draw_run(&segs, true, out, whole, (bx + 8) as f32, by + 4, 1.0);
        }

        // search bar, top-right below the tab bar
        if let Some(q) = search_query {
            let bar_w = (self.cell_w * 34.0) as i32;
            let bar_h = (self.cell_h + 10.0) as i32;
            let bx = width as i32 - bar_w - 10;
            let by = oy + 6;
            push_rect(out, whole, bx, by, bar_w, bar_h, c4((0x1c, 0x21, 0x28), 255));
            push_rect(out, whole, bx, by + bar_h - 2, bar_w, 2,
                      c4(PALETTE[11], 200)); // amber underline = search accent
            let segs = [Seg::plain(format!("بحث: {q}_"), self.fg)];
            self.draw_run(&segs, true, out, whole, (bx + 8) as f32, by + 5, 1.0);
        }

        // the tab bar, drawn last (nothing may bleed into it)
        push_rect(out, whole, 0, 0, width as i32, oy, c4((0x16, 0x1b, 0x22), 255));
        let tw = (self.cell_w * TAB_CELLS) as i32;
        for (i, tab) in tabs.iter().enumerate() {
            let x0 = i as i32 * tw;
            let (bg, fg): ((u8, u8, u8), (u8, u8, u8)) = if tab.active {
                ((0x24, 0x2b, 0x36), self.fg)
            } else {
                ((0x16, 0x1b, 0x22), (0x8a, 0x94, 0xa3))
            };
            push_rect(out, whole, x0, 0, tw - 2, oy, c4(bg, 255));
            if tab.active {
                // accent line on top: the focused tab is unmistakable
                push_rect(out, whole, x0, 0, tw - 2, 2, c4((0x2e, 0xa0, 0x43), 255));
            }
            let max_chars = TAB_CELLS as usize - 5;
            let title: String = tab.title.chars().take(max_chars).collect();
            let segs = [Seg::plain(title, fg)];
            self.draw_run(&segs, true, out, whole, (x0 + 10) as f32, 5, 1.0);
            // dots: amber attention (a Claude waiting on you) beats green busy
            let dot = if tab.attention {
                Some((0xf2, 0xcc, 0x60))
            } else if tab.busy {
                Some((0x56, 0xd3, 0x64))
            } else {
                None
            };
            if let Some(c) = dot {
                let d = 6;
                push_rect(out, whole, x0 + tw - 16, oy / 2 - d / 2, d, d, c4(c, 255));
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
mod cache_tests {
    use super::*;
    use std::time::Instant;

    fn test_renderer() -> Renderer {
        Renderer::new(1.0, &crate::config::UserConfig::default(), 0.0)
    }

    /// Same content -> same cached run (no re-shape); different colors or
    /// styles are different runs (both bake into the shaped buffer).
    #[test]
    fn shaped_runs_are_cached_by_content_color_and_style() {
        let mut r = test_renderer();
        let a = [Seg::plain("hello بيان", FG)];
        let w1 = r.measure(&a, false);
        assert_eq!(r.cache_hot.len(), 1);
        let w2 = r.measure(&a, false);
        assert_eq!(r.cache_hot.len(), 1, "second measure must hit the cache");
        assert_eq!(w1, w2);
        let b = [Seg::plain("hello بيان", (0xff, 0, 0))];
        r.measure(&b, false);
        assert_eq!(r.cache_hot.len(), 2, "a different color is a new run");
        // bold is a different run too (and usually a different width)
        let bold = [Seg { text: "hello بيان".into(), fg: FG, style: ST_BOLD }];
        r.measure(&bold, false);
        assert_eq!(r.cache_hot.len(), 3, "a different style is a new run");
        // alignment is part of the key too
        r.measure(&a, true);
        assert_eq!(r.cache_hot.len(), 4);
    }

    /// The generational cap keeps the hot set: overflow demotes, a hit on a
    /// demoted run promotes it back instead of re-shaping.
    #[test]
    fn cache_overflow_keeps_the_working_set() {
        let mut r = test_renderer();
        let hot = [Seg::plain("keep me", FG)];
        r.measure(&hot, false);
        for i in 0..CACHE_CAP {
            r.measure(&[Seg::plain(format!("filler {i}"), FG)], false);
        }
        assert!(r.cache_hot.len() <= CACHE_CAP);
        // "keep me" was demoted to cold; hitting it promotes without growth
        let before = r.cache_hot.len();
        r.measure(&hot, false);
        assert_eq!(r.cache_hot.len(), before + 1);
    }

    /// The atlas grows a new page when one fills, keeping earlier glyphs —
    /// instead of M10's wipe-everything reset.
    #[test]
    fn atlas_grows_pages_without_losing_earlier_glyphs() {
        let mut a = Atlas::new();
        assert_eq!(a.pages.len(), 1);
        // pack rows of tall slots until the first page must overflow. A
        // near-full-width slot per shelf forces a new shelf each alloc.
        let (w, h) = (ATLAS_SIZE - 8, 64);
        let rows_per_page = (ATLAS_SIZE / (h + 1)) as usize;
        let first = a.alloc(10, 10).unwrap();
        assert_eq!(first.0, 0, "first glyph on page 0");
        for _ in 0..rows_per_page + 1 {
            a.alloc(w, h);
        }
        assert_eq!(a.pages.len(), 2, "a second page was grown");
        assert!(a.layer_gen >= 1, "layer generation bumped for the GPU");
        assert_eq!(a.generation, 0, "no wholesale reset while under the cap");
        // page 0's white texel survived the growth (still opaque white)
        assert_eq!(&a.pages[0][0..4], &[255, 255, 255, 255]);
    }

    /// Solid rects always reference page 0 (the white texel lives there).
    #[test]
    fn solid_rects_target_page_zero() {
        let mut out = Vec::new();
        push_rect(&mut out, (0, 0, 100, 100), 10, 10, 20, 20, [1.0, 0.0, 0.0, 1.0]);
        assert!(out.iter().all(|v| v.layer == 0.0));
    }

    /// Not a strict assert (CI timing is flaky) — prints the win.
    #[test]
    fn measure_cache_speedup_probe() {
        let mut r = test_renderer();
        let rows: Vec<[Seg; 1]> = (0..40)
            .map(|i| [Seg::plain(format!("line {i} of some shell output مع عربية"), FG)])
            .collect();
        let t0 = Instant::now();
        for row in &rows {
            r.measure(row, false);
        }
        let cold = t0.elapsed();
        let t1 = Instant::now();
        for _frame in 0..100 {
            for row in &rows {
                r.measure(row, false);
            }
        }
        let warm_100 = t1.elapsed();
        eprintln!("PROBE shape 40 rows cold  = {cold:?}");
        eprintln!("PROBE 100 cached frames   = {warm_100:?} (per frame: {:?})",
                  warm_100 / 100);
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
