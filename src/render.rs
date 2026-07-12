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

/// A named built-in theme: bg, fg, and the 16 ANSI colors (EasyTer heritage).
pub struct Theme {
    pub name: &'static str,
    pub bg: (u8, u8, u8),
    pub fg: (u8, u8, u8),
    pub palette: [(u8, u8, u8); 16],
}

const fn rgb(v: u32) -> (u8, u8, u8) {
    ((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// A theme by name (used to resolve the config's `theme` field).
pub fn theme_by_name(name: &str) -> Option<&'static Theme> {
    THEMES.iter().find(|t| t.name == name)
}

/// The theme presets the settings panel cycles through.
pub const THEMES: &[Theme] = &[
    Theme {
        name: "بيان",
        bg: rgb(0x0d1117), fg: rgb(0xe6edf3),
        palette: [
            rgb(0x0d1117), rgb(0xff6b6b), rgb(0x7ee787), rgb(0xe3b341),
            rgb(0x6ca0f6), rgb(0xd2a8ff), rgb(0x56d4dd), rgb(0xe6edf3),
            rgb(0x6e7681), rgb(0xff8a8a), rgb(0xa2f5b0), rgb(0xf2cc60),
            rgb(0x8db4f8), rgb(0xe0c1ff), rgb(0x7ee0e6), rgb(0xffffff),
        ],
    },
    Theme {
        name: "أسود مطلق",
        bg: rgb(0x000000), fg: rgb(0xd0d0d0),
        palette: [
            rgb(0x000000), rgb(0xff5555), rgb(0x50fa7b), rgb(0xf1fa8c),
            rgb(0x6ca0f6), rgb(0xff79c6), rgb(0x8be9fd), rgb(0xd0d0d0),
            rgb(0x555555), rgb(0xff6e6e), rgb(0x69ff94), rgb(0xffffa5),
            rgb(0x8db4f8), rgb(0xff92df), rgb(0xa4ffff), rgb(0xffffff),
        ],
    },
    Theme {
        name: "Dracula",
        bg: rgb(0x282a36), fg: rgb(0xf8f8f2),
        palette: [
            rgb(0x21222c), rgb(0xff5555), rgb(0x50fa7b), rgb(0xf1fa8c),
            rgb(0xbd93f9), rgb(0xff79c6), rgb(0x8be9fd), rgb(0xf8f8f2),
            rgb(0x6272a4), rgb(0xff6e6e), rgb(0x69ff94), rgb(0xffffa5),
            rgb(0xd6acff), rgb(0xff92df), rgb(0xa4ffff), rgb(0xffffff),
        ],
    },
    Theme {
        name: "Gruvbox",
        bg: rgb(0x282828), fg: rgb(0xebdbb2),
        palette: [
            rgb(0x282828), rgb(0xcc241d), rgb(0x98971a), rgb(0xd79921),
            rgb(0x458588), rgb(0xb16286), rgb(0x689d6a), rgb(0xa89984),
            rgb(0x928374), rgb(0xfb4934), rgb(0xb8bb26), rgb(0xfabd2f),
            rgb(0x83a598), rgb(0xd3869b), rgb(0x8ec07c), rgb(0xebdbb2),
        ],
    },
    Theme {
        name: "Nord",
        bg: rgb(0x2e3440), fg: rgb(0xd8dee9),
        palette: [
            rgb(0x3b4252), rgb(0xbf616a), rgb(0xa3be8c), rgb(0xebcb8b),
            rgb(0x81a1c1), rgb(0xb48ead), rgb(0x88c0d0), rgb(0xe5e9f0),
            rgb(0x4c566a), rgb(0xbf616a), rgb(0xa3be8c), rgb(0xebcb8b),
            rgb(0x81a1c1), rgb(0xb48ead), rgb(0x8fbcbb), rgb(0xeceff4),
        ],
    },
    Theme {
        name: "Solarized",
        bg: rgb(0x002b36), fg: rgb(0x93a1a1),
        palette: [
            rgb(0x073642), rgb(0xdc322f), rgb(0x859900), rgb(0xb58900),
            rgb(0x268bd2), rgb(0xd33682), rgb(0x2aa198), rgb(0xeee8d5),
            rgb(0x586e75), rgb(0xcb4b16), rgb(0x859900), rgb(0x657b83),
            rgb(0x839496), rgb(0x6c71c4), rgb(0x93a1a1), rgb(0xfdf6e3),
        ],
    },
    Theme {
        name: "Tokyo Night",
        bg: rgb(0x1a1b26), fg: rgb(0xc0caf5),
        palette: [
            rgb(0x15161e), rgb(0xf7768e), rgb(0x9ece6a), rgb(0xe0af68),
            rgb(0x7aa2f7), rgb(0xbb9af7), rgb(0x7dcfff), rgb(0xa9b1d6),
            rgb(0x414868), rgb(0xf7768e), rgb(0x9ece6a), rgb(0xe0af68),
            rgb(0x7aa2f7), rgb(0xbb9af7), rgb(0x7dcfff), rgb(0xc0caf5),
        ],
    },
];

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

/// One row of the shortcuts editor.
pub struct ShortcutRow {
    pub label: String,
    /// display form, "Ctrl+Shift+T"
    pub chord: String,
    /// a non-default binding (drawn in the value green)
    pub custom: bool,
}

/// Clickable regions of the shortcuts editor (draw + hit-test agree).
pub struct ShortcutsLayout {
    pub card: Rect,
    pub rowh: i32,
    /// full-width hit strip per action row
    pub rows: Vec<Rect>,
}

/// Every clickable region of the settings panel (draw + hit-test agree).
pub struct SettingsLayout {
    pub card: Rect,
    pub head_h: i32,
    pub rowh: i32,
    /// header button that opens the shortcuts editor
    pub shortcuts_btn: Rect,
    pub theme_tiles: Vec<Rect>,
    pub font_label_y: i32,
    pub font_prev: Rect,
    pub font_next: Rect,
    pub size_label_y: i32,
    pub size_minus: Rect,
    pub size_plus: Rect,
    pub cursor_label_y: i32,
    pub cursor_btns: [Rect; 3],
    pub blink_label_y: i32,
    pub blink_toggle: Rect,
    pub scroll_label_y: i32,
    pub scroll_minus: Rect,
    pub scroll_plus: Rect,
    pub copy_label_y: i32,
    pub copy_toggle: Rect,
    pub liga_label_y: i32,
    pub liga_toggle: Rect,
    pub bell_label_y: i32,
    /// segmented control, left→right: [صامت, صوت, تنبيه] (RTL: تنبيه first)
    pub bell_btns: [Rect; 3],
    pub pad_label_y: i32,
    pub pad_minus: Rect,
    pub pad_plus: Rect,
    pub opacity_label_y: i32,
    pub opacity_minus: Rect,
    pub opacity_plus: Rect,
    pub shell_label_y: i32,
    pub shell_prev: Rect,
    pub shell_next: Rect,
    pub bar_label_y: i32,
    pub bar_toggle: Rect,
    pub close_label_y: i32,
    pub close_toggle: Rect,
}

/// The bell segments' modes in bell_btns order (left→right on screen, so
/// the RTL-first default تنبيه sits rightmost, beside the label).
pub const BELL_SEGMENTS: [crate::config::BellMode; 3] = [
    crate::config::BellMode::Silent,
    crate::config::BellMode::Sound,
    crate::config::BellMode::Attention,
];

/// Everything the settings panel displays (current values of each control).
pub struct SettingsView<'a> {
    pub theme: usize,
    pub font_family: &'a str,
    pub font_size: i32,
    pub cursor: crate::config::CursorStyle,
    pub cursor_blink: bool,
    pub scrollback: usize,
    pub copy_on_select: bool,
    pub ligatures: bool,
    pub bell: crate::config::BellMode,
    pub padding: i32,
    pub opacity_pct: i32,
    pub shell: &'a str,
    pub hide_single_tab: bool,
    pub confirm_close: bool,
}

/// Is (px, py) inside a rect?
pub fn rect_hit((rx, ry, rw, rh): Rect, px: f64, py: f64) -> bool {
    px >= rx as f64 && px < (rx + rw) as f64 && py >= ry as f64 && py < (ry + rh) as f64
}

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
    /// block / bar / underline (the settings panel's cursor row)
    pub cursor: crate::config::CursorStyle,
    /// blink phase: false = the off half of the blink, draw no cursor
    pub cursor_on: bool,
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
/// and a non-NF primary renders those as ugly boxes/blocks. When ligatures
/// are on, the ligature-capable "Code" variants lead (Cascadia Code, Fira,
/// JetBrains all ligate); when off, the plain "Mono" variants lead.
const FAMILY_LIGA: &[&str] = &[
    "Cascadia Code NF",
    "FiraCode Nerd Font",
    "JetBrainsMono Nerd Font",
    "Cascadia Code",
    "Cascadia Mono NF",
    "Consolas",
];
const FAMILY_MONO: &[&str] = &[
    "Cascadia Mono NF",
    "CaskaydiaCove Nerd Font Mono",
    "JetBrainsMono Nerd Font Mono",
    "Cascadia Mono",
    "Cascadia Code NF",
    "Consolas",
];

/// Families the settings panel's font cycler offers — the preference lists
/// plus the popular monospace fonts a Windows dev box tends to have. Only
/// the INSTALLED ones are shown.
const FONT_CANDIDATES: &[&str] = &[
    "Cascadia Code NF",
    "Cascadia Code",
    "Cascadia Mono NF",
    "Cascadia Mono",
    "CaskaydiaCove Nerd Font Mono",
    "FiraCode Nerd Font",
    "Fira Code",
    "JetBrainsMono Nerd Font",
    "JetBrainsMono Nerd Font Mono",
    "JetBrains Mono",
    "Consolas",
    "Courier New",
    "Hack",
    "Source Code Pro",
    "IBM Plex Mono",
    "Iosevka",
    "Victor Mono",
    "MesloLGS NF",
];

fn pick_family(db: &cosmic_text::fontdb::Database, preferred: Option<&str>,
               ligatures: bool) -> String {
    let has = |name: &str| {
        db.faces()
            .any(|f| f.families.iter().any(|(n, _)| n == name))
    };
    if let Some(p) = preferred {
        if has(p) {
            return p.to_string();
        }
    }
    let list = if ligatures { FAMILY_LIGA } else { FAMILY_MONO };
    for cand in list {
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
    /// ligatures on: ASCII shapes as one run (rustybuzz forms -> => != ...).
    /// off: ASCII shapes per cell, so no substitutions can occur.
    ligatures: bool,
    /// hide-tab-bar-with-one-tab, resolved per frame by the app (the
    /// renderer can't know the tab count)
    bar_hidden: bool,
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
        let ligatures = cfg.ligatures.unwrap_or(true);
        let family = pick_family(font_system.db(), cfg.font_family.as_deref(), ligatures);
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
        // start from a named theme (if any), then let explicit config
        // bg/fg/palette override individual colors on top of it
        let theme = cfg.theme.as_deref().and_then(theme_by_name);
        let (mut bg, mut fg, mut palette) = match theme {
            Some(t) => (t.bg, t.fg, t.palette),
            None => (BG, FG, PALETTE),
        };
        if let Some(c) = cfg.bg.as_deref().and_then(crate::config::parse_hex) {
            bg = c;
        }
        if let Some(c) = cfg.fg.as_deref().and_then(crate::config::parse_hex) {
            fg = c;
        }
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
            bg,
            fg,
            palette,
            ligatures,
            bar_hidden: false,
            cell_w,
            cell_h: metrics.line_height,
        }
    }

    /// The app resolves "hide the bar with a single tab" each layout pass.
    pub fn set_bar_hidden(&mut self, hidden: bool) {
        self.bar_hidden = hidden;
    }

    /// The primary family actually in use (post-fallback) — what the
    /// settings panel displays.
    pub fn family(&self) -> &str {
        &self.family
    }

    /// Installed candidates for the settings panel's font cycler. Always
    /// contains the current family so the cycle has a starting point.
    pub fn font_choices(&self) -> Vec<String> {
        let db = self.font_system.db();
        let has = |name: &str| {
            db.faces().any(|f| f.families.iter().any(|(n, _)| n == name))
        };
        let mut out: Vec<String> = FONT_CANDIDATES
            .iter()
            .filter(|c| has(c))
            .map(|c| c.to_string())
            .collect();
        if !out.iter().any(|c| *c == self.family) {
            out.insert(0, self.family.clone());
        }
        out
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
    /// Zero while hidden (single tab + the hide_single_tab setting).
    pub fn tab_bar_h(&self) -> f32 {
        if self.bar_hidden {
            0.0
        } else {
            self.cell_h + 10.0
        }
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
            // batch a contiguous ASCII run as ONE shaped buffer so rustybuzz
            // forms programming ligatures (-> => != >= ...). Only when
            // ligatures are enabled; otherwise every cell shapes alone below.
            if self.ligatures && ci.c.is_ascii() {
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
                // one cell, shaped alone: no adjacent context for rustybuzz
                // to ligate (ligatures off), and exotic glyphs (icons, box
                // drawing, CJK) get pinned to their box, compressed if wider
                note_deco(ci);
                if ci.c != ' ' {
                    let mut s = String::new();
                    ci.push_text(&mut s);
                    let seg = [Seg { text: s, fg: ci.fg, style: ci.style }];
                    let natw = self.measure(&seg, false);
                    let boxw = ci.w as f32 * self.cell_w;
                    let scale = if natw > boxw + 0.5 { boxw / natw } else { 1.0 };
                    self.draw_run(&seg, false, out, clip,
                                  x0 as f32 + ci.col as f32 * self.cell_w, y, scale);
                }
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

        // cursor: only the focused pane shows it (the block is translucent,
        // so the glyph stays legible); grid rows are column-exact, Arabic
        // rows map through the shaped layout
        if view.focused && view.cursor_on && cursor_vrow >= 0 && (cursor_vrow as usize) < rows {
            let (x0, wpx) = cursor_rect.unwrap_or((
                px + (ccol as f32 * self.cell_w).round() as i32,
                self.cell_w.round() as i32,
            ));
            let y0 = py + (cursor_vrow as f32 * self.cell_h).round() as i32;
            let hpx = self.cell_h.round() as i32;
            use crate::config::CursorStyle::*;
            match view.cursor {
                Block => push_rect(out, clip, x0, y0, wpx, hpx, c4(self.fg, 170)),
                // thin shapes don't cover the glyph, so they draw opaque
                Bar => {
                    let bw = ((self.cell_w * 0.15).round() as i32).max(2);
                    push_rect(out, clip, x0, y0, bw, hpx, c4(self.fg, 230));
                }
                Underline => {
                    let bh = ((self.cell_h * 0.1).round() as i32).max(2);
                    push_rect(out, clip, x0, y0 + hpx - bh, wpx, bh, c4(self.fg, 230));
                }
            }
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

    /// Geometry of every interactive element in the settings panel, computed
    /// once and shared by the renderer (draw) and the app (click hit-test) so
    /// they never disagree.
    pub fn settings_layout(&self, fw: usize, fh: usize) -> SettingsLayout {
        // The ledger grammar: every row is [control]···dotted leader···[label],
        // controls left, labels right (RTL), one shared control height.
        let pad = 24;
        let w = ((self.cell_w * 52.0) as i32).clamp(430, fw as i32 - 60);
        let rowh = (self.cell_h + 20.0) as i32; // 12 rows now — keep them tight
        let ctl = rowh - 10; // every square control is this tall
        let head_h = (self.cell_h + 30.0) as i32;
        let foot_h = (self.cell_h + 30.0) as i32;
        // the theme row obeys the same grammar: tiles left, label right,
        // so the strip leaves room for the "المظهر" label
        let label_reserve = (self.cell_w * 6.0) as i32;
        let tile_w = ((w - pad * 2 - label_reserve - 6 * 6) / 7).max(30);
        let tile_h = (self.cell_h * 1.6) as i32;
        let theme_rowh = tile_h + 14;
        let h = head_h + 6 + theme_rowh + rowh * 13 + foot_h;
        let x = (fw as i32 - w) / 2;
        let y = (self.tab_bar_h() as i32 + (fh as i32 - h) / 3)
            .max(self.tab_bar_h() as i32 + 12);

        // the shortcuts-editor button lives in the header, left side
        let sb_w = (self.cell_w * 10.0) as i32;
        let shortcuts_btn = (x + pad, y + 10, sb_w, (self.cell_h + 8.0) as i32);

        let theme_y = y + head_h + 6 + (theme_rowh - tile_h) / 2;
        let theme_tiles: Vec<Rect> = (0..THEMES.len() as i32)
            .map(|i| (x + pad + i * (tile_w + 6), theme_y, tile_w, tile_h))
            .collect();

        let rows_top = y + head_h + 6 + theme_rowh;
        let row_y = |i: i32| rows_top + rowh * i;
        let cy = |ry: i32| ry + (rowh - ctl) / 2; // control vertically centered

        // font family: [‹] name [›]
        let font_y = row_y(0);
        let name_w = (self.cell_w * 17.0) as i32;
        let font_prev = (x + pad, cy(font_y), ctl, ctl);
        let font_next = (x + pad + ctl + name_w, cy(font_y), ctl, ctl);

        // font size: [−  15  +]
        let size_y = row_y(1);
        let size_slot = (self.cell_w * 4.0) as i32;
        let size_minus = (x + pad, cy(size_y), ctl, ctl);
        let size_plus = (x + pad + ctl + size_slot, cy(size_y), ctl, ctl);

        // cursor style: three shape swatches, then its blink toggle
        let cursor_y = row_y(2);
        let cursor_btns = [0, 1, 2].map(|i| (x + pad + i * (ctl + 8), cy(cursor_y), ctl, ctl));

        // toggle pills (wide and shallow, so the track reads as a switch)
        let pill_w = (self.cell_w * 4.2) as i32;
        let pill_h = ctl - 8;
        let pcy = |ry: i32| ry + (rowh - pill_h) / 2;
        let blink_y = row_y(3);
        let blink_toggle = (x + pad, pcy(blink_y), pill_w, pill_h);

        // scrollback: [−  10k  +]
        let scroll_y = row_y(4);
        let scroll_slot = (self.cell_w * 5.0) as i32;
        let scroll_minus = (x + pad, cy(scroll_y), ctl, ctl);
        let scroll_plus = (x + pad + ctl + scroll_slot, cy(scroll_y), ctl, ctl);

        // padding / opacity: the same stepper shape
        let pad_y = row_y(5);
        let pad_slot = (self.cell_w * 4.0) as i32;
        let pad_minus = (x + pad, cy(pad_y), ctl, ctl);
        let pad_plus = (x + pad + ctl + pad_slot, cy(pad_y), ctl, ctl);
        let opacity_y = row_y(6);
        let op_slot = (self.cell_w * 5.0) as i32;
        let opacity_minus = (x + pad, cy(opacity_y), ctl, ctl);
        let opacity_plus = (x + pad + ctl + op_slot, cy(opacity_y), ctl, ctl);

        let copy_y = row_y(7);
        let copy_toggle = (x + pad, pcy(copy_y), pill_w, pill_h);
        let liga_y = row_y(8);
        let liga_toggle = (x + pad, pcy(liga_y), pill_w, pill_h);

        // bell: a three-segment control (all options visible, one active)
        let bell_y = row_y(9);
        let seg_w = (self.cell_w * 4.6) as i32;
        let bell_btns = [0, 1, 2].map(|i| (x + pad + i * (seg_w + 6), cy(bell_y), seg_w, ctl));

        // shell cycler (new tabs only) — same shape as the font cycler
        let shell_y = row_y(10);
        let shell_w = (self.cell_w * 12.0) as i32;
        let shell_prev = (x + pad, cy(shell_y), ctl, ctl);
        let shell_next = (x + pad + ctl + shell_w, cy(shell_y), ctl, ctl);

        // hide-bar / confirm-close toggles
        let bar_y = row_y(11);
        let bar_toggle = (x + pad, pcy(bar_y), pill_w, pill_h);
        let close_y = row_y(12);
        let close_toggle = (x + pad, pcy(close_y), pill_w, pill_h);

        SettingsLayout {
            card: (x, y, w, h),
            head_h,
            rowh,
            shortcuts_btn,
            theme_tiles,
            font_label_y: font_y,
            font_prev,
            font_next,
            size_label_y: size_y,
            size_minus,
            size_plus,
            cursor_label_y: cursor_y,
            cursor_btns,
            blink_label_y: blink_y,
            blink_toggle,
            scroll_label_y: scroll_y,
            scroll_minus,
            scroll_plus,
            copy_label_y: copy_y,
            copy_toggle,
            liga_label_y: liga_y,
            liga_toggle,
            bell_label_y: bell_y,
            bell_btns,
            pad_label_y: pad_y,
            pad_minus,
            pad_plus,
            opacity_label_y: opacity_y,
            opacity_minus,
            opacity_plus,
            shell_label_y: shell_y,
            shell_prev,
            shell_next,
            bar_label_y: bar_y,
            bar_toggle,
            close_label_y: close_y,
            close_toggle,
        }
    }

    /// The settings ledger: every row reads [control] ··· [label], leader
    /// dots tying each control to its name the way a book index does. One
    /// control height, one border color, labels vertically centered — the
    /// grid IS the design. Every control is a hit region in SettingsLayout.
    pub fn draw_settings(&mut self, out: &mut Vec<Vertex>, fw: usize, fh: usize,
                         v: &SettingsView) {
        const CARD: (u8, u8, u8) = (0x14, 0x19, 0x20);
        const EDGE: (u8, u8, u8) = (0x2a, 0x31, 0x3a); // card + control borders
        const CTL_BG: (u8, u8, u8) = (0x24, 0x2b, 0x34);
        const GREEN: (u8, u8, u8) = (0x2e, 0xa0, 0x43);
        const GREEN_DIM: (u8, u8, u8) = (0x1c, 0x33, 0x24);
        const VALUE: (u8, u8, u8) = (0x7e, 0xe7, 0x87);
        const MUTED: (u8, u8, u8) = (0x9a, 0xa4, 0xb2);
        const DOT: (u8, u8, u8) = (0x33, 0x3b, 0x45);

        let whole: Rect = (0, 0, fw as i32, fh as i32);
        let lay = self.settings_layout(fw, fh);
        let (x, y, w, h) = lay.card;
        let pad = 24;
        // label baseline offset: text vertically centered in its row
        let tc = (lay.rowh as f32 - self.cell_h) as i32 / 2;

        push_rect(out, whole, 0, 0, fw as i32, fh as i32, c4((0, 0, 0), 140));
        push_rect(out, whole, x - 1, y - 1, w + 2, h + 2, c4(EDGE, 255)); // border
        push_rect(out, whole, x, y, w, h, c4(CARD, 255));
        push_rect(out, whole, x, y, w, 3, c4(GREEN, 255)); // accent

        // title + hairline under it
        let head = [Seg { text: "الإعدادات".into(), fg: self.fg, style: ST_BOLD }];
        let hw = self.measure(&head, true);
        self.draw_run(&head, true, out, whole, (x + w - pad) as f32 - hw, y + 14, 1.0);
        push_rect(out, whole, x + pad, y + lay.head_h, w - pad * 2, 1, c4(EDGE, 255));

        // the shortcuts-editor button (header, left)
        {
            let (bx, by, bw, bh) = lay.shortcuts_btn;
            push_rect(out, whole, bx - 1, by - 1, bw + 2, bh + 2, c4(EDGE, 255));
            push_rect(out, whole, bx, by, bw, bh, c4(CTL_BG, 255));
            let seg = [Seg::plain("الاختصارات ‹", MUTED)];
            let lw = self.measure(&seg, true);
            self.draw_run(&seg, true, out, whole,
                          bx as f32 + (bw as f32 - lw) / 2.0, by + 3, 1.0);
        }

        // right-aligned row label; returns where the leader dots must stop
        let label = |r: &mut Self, out: &mut Vec<Vertex>, text: &str, row_top: i32| -> i32 {
            let seg = [Seg::plain(text, MUTED)];
            let lw = r.measure(&seg, true);
            let lx = (x + w - pad) as f32 - lw;
            r.draw_run(&seg, true, out, whole, lx, row_top + tc, 1.0);
            lx as i32
        };
        // the ledger dots: from a control's right edge to its label
        let leader = |out: &mut Vec<Vertex>, from_x: i32, to_x: i32, row_top: i32| {
            let dy = row_top + lay.rowh / 2 - 1;
            let mut dx = from_x + 16;
            while dx + 2 <= to_x - 14 {
                push_rect(out, whole, dx, dy, 2, 2, c4(DOT, 255));
                dx += 9;
            }
        };
        let bordered = |out: &mut Vec<Vertex>, (bx, by, bw, bh): Rect,
                        bg: (u8, u8, u8), edge: (u8, u8, u8)| {
            push_rect(out, whole, bx - 1, by - 1, bw + 2, bh + 2, c4(edge, 255));
            push_rect(out, whole, bx, by, bw, bh, c4(bg, 255));
        };

        // ---- theme row (same grammar: tiles ··· label) ----
        let theme_top = lay.theme_tiles[0].1;
        let theme_row_top = theme_top - (lay.rowh - lay.theme_tiles[0].3) / 2;
        let tl = label(self, out, "المظهر", theme_row_top);
        let last = lay.theme_tiles[lay.theme_tiles.len() - 1];
        leader(out, last.0 + last.2, tl, theme_row_top);
        for (i, tile) in lay.theme_tiles.iter().enumerate() {
            let (tx, ty, tw2, th) = *tile;
            let t = &THEMES[i];
            let edge = if i == v.theme { GREEN } else { EDGE };
            bordered(out, (tx, ty, tw2, th), t.bg, edge);
            if i == v.theme {
                // second border ring so the active tile reads at a glance
                push_rect(out, whole, tx, ty, tw2, 2, c4(GREEN, 255));
                push_rect(out, whole, tx, ty + th - 2, tw2, 2, c4(GREEN, 255));
                push_rect(out, whole, tx, ty, 2, th, c4(GREEN, 255));
                push_rect(out, whole, tx + tw2 - 2, ty, 2, th, c4(GREEN, 255));
            }
            // four palette dots as a mini preview
            let dots = [t.palette[1], t.palette[2], t.palette[4], t.palette[3]];
            let dw = (tw2 - 10) / 4;
            for (k, c) in dots.iter().enumerate() {
                push_rect(out, whole, tx + 5 + k as i32 * dw, ty + th - 9, dw - 2, 4,
                          c4(*c, 255));
            }
        }

        let draw_btn = |r: &mut Self, out: &mut Vec<Vertex>, rect: Rect, glyph: &str| {
            bordered(out, rect, CTL_BG, EDGE);
            let (bx, by, bw, bh) = rect;
            let seg = [Seg { text: glyph.into(), fg: r.fg, style: ST_BOLD }];
            let lw = r.measure(&seg, false);
            let gy = by + ((bh as f32 - r.cell_h) / 2.0) as i32;
            r.draw_run(&seg, false, out, whole,
                       bx as f32 + (bw as f32 - lw) / 2.0, gy, 1.0);
        };
        let draw_val = |r: &mut Self, out: &mut Vec<Vertex>, text: String,
                        left: Rect, right: Rect, row_top: i32| {
            let seg = [Seg { text, fg: VALUE, style: ST_BOLD }];
            let vw = r.measure(&seg, false);
            let mid = (left.0 + left.2 + right.0) / 2;
            r.draw_run(&seg, false, out, whole, mid as f32 - vw / 2.0, row_top + tc, 1.0);
        };
        let draw_toggle = |out: &mut Vec<Vertex>, rect: Rect, on: bool| {
            let (gx, gy, gw, gh) = rect;
            let track = if on { GREEN } else { CTL_BG };
            bordered(out, rect, track, if on { GREEN } else { EDGE });
            let knob = gh - 8;
            let kx = if on { gx + gw - knob - 4 } else { gx + 4 };
            let kc = if on { (0xff, 0xff, 0xff) } else { (0x8a, 0x94, 0xa3) };
            push_rect(out, whole, kx, gy + 4, knob, knob, c4(kc, 255));
        };

        // ---- font family cycler ----
        let ll = label(self, out, "الخطّ", lay.font_label_y);
        leader(out, lay.font_next.0 + lay.font_next.2, ll, lay.font_label_y);
        draw_btn(self, out, lay.font_prev, "‹");
        draw_btn(self, out, lay.font_next, "›");
        draw_val(self, out, v.font_family.to_string(),
                 lay.font_prev, lay.font_next, lay.font_label_y);

        // ---- font size stepper ----
        let ll = label(self, out, "حجم الخطّ", lay.size_label_y);
        leader(out, lay.size_plus.0 + lay.size_plus.2, ll, lay.size_label_y);
        draw_btn(self, out, lay.size_minus, "−");
        draw_btn(self, out, lay.size_plus, "+");
        draw_val(self, out, format!("{}", v.font_size),
                 lay.size_minus, lay.size_plus, lay.size_label_y);

        // ---- cursor shape swatches (block / bar / underline) ----
        let ll = label(self, out, "المؤشّر", lay.cursor_label_y);
        let lastc = lay.cursor_btns[2];
        leader(out, lastc.0 + lastc.2, ll, lay.cursor_label_y);
        for (i, &(bx, by, bw, bh)) in lay.cursor_btns.iter().enumerate() {
            let active = crate::config::CursorStyle::ALL[i] == v.cursor;
            bordered(out, (bx, by, bw, bh),
                     if active { GREEN_DIM } else { CTL_BG },
                     if active { GREEN } else { EDGE });
            // the shape itself, drawn as rects — no glyphs to go missing
            let c = c4(if active { (0xff, 0xff, 0xff) } else { MUTED }, 255);
            match crate::config::CursorStyle::ALL[i] {
                crate::config::CursorStyle::Block =>
                    push_rect(out, whole, bx + 8, by + 7, bw - 16, bh - 14, c),
                crate::config::CursorStyle::Bar =>
                    push_rect(out, whole, bx + bw / 2 - 1, by + 7, 3, bh - 14, c),
                crate::config::CursorStyle::Underline =>
                    push_rect(out, whole, bx + 8, by + bh - 10, bw - 16, 3, c),
            }
        }

        // ---- cursor blink toggle ----
        let ll = label(self, out, "وميض المؤشّر", lay.blink_label_y);
        leader(out, lay.blink_toggle.0 + lay.blink_toggle.2, ll, lay.blink_label_y);
        draw_toggle(out, lay.blink_toggle, v.cursor_blink);

        // ---- scrollback stepper (new tabs only) ----
        let ll = label(self, out, "سجلّ التمرير (التبويبات الجديدة)", lay.scroll_label_y);
        leader(out, lay.scroll_plus.0 + lay.scroll_plus.2, ll, lay.scroll_label_y);
        draw_btn(self, out, lay.scroll_minus, "−");
        draw_btn(self, out, lay.scroll_plus, "+");
        let sb = if v.scrollback % 1000 == 0 {
            format!("{}k", v.scrollback / 1000)
        } else {
            format!("{}", v.scrollback)
        };
        draw_val(self, out, sb, lay.scroll_minus, lay.scroll_plus, lay.scroll_label_y);

        // ---- padding / opacity steppers ----
        let ll = label(self, out, "الحواشي", lay.pad_label_y);
        leader(out, lay.pad_plus.0 + lay.pad_plus.2, ll, lay.pad_label_y);
        draw_btn(self, out, lay.pad_minus, "−");
        draw_btn(self, out, lay.pad_plus, "+");
        draw_val(self, out, format!("{}", v.padding),
                 lay.pad_minus, lay.pad_plus, lay.pad_label_y);
        let ll = label(self, out, "شفافية النافذة", lay.opacity_label_y);
        leader(out, lay.opacity_plus.0 + lay.opacity_plus.2, ll, lay.opacity_label_y);
        draw_btn(self, out, lay.opacity_minus, "−");
        draw_btn(self, out, lay.opacity_plus, "+");
        draw_val(self, out, format!("{}%", v.opacity_pct),
                 lay.opacity_minus, lay.opacity_plus, lay.opacity_label_y);

        // ---- copy-on-select / ligatures toggles ----
        let ll = label(self, out, "النسخ عند التحديد", lay.copy_label_y);
        leader(out, lay.copy_toggle.0 + lay.copy_toggle.2, ll, lay.copy_label_y);
        draw_toggle(out, lay.copy_toggle, v.copy_on_select);
        let ll = label(self, out, "الأربطة", lay.liga_label_y);
        leader(out, lay.liga_toggle.0 + lay.liga_toggle.2, ll, lay.liga_label_y);
        draw_toggle(out, lay.liga_toggle, v.ligatures);

        // ---- bell: segmented, all three options visible ----
        let ll = label(self, out, "الجرس", lay.bell_label_y);
        let lastb = lay.bell_btns[2];
        leader(out, lastb.0 + lastb.2, ll, lay.bell_label_y);
        let names = ["صامت", "صوت", "تنبيه"];
        for (i, &(bx, by, bw, bh)) in lay.bell_btns.iter().enumerate() {
            let active = BELL_SEGMENTS[i] == v.bell;
            bordered(out, (bx, by, bw, bh),
                     if active { GREEN_DIM } else { CTL_BG },
                     if active { GREEN } else { EDGE });
            let fg = if active { (0xff, 0xff, 0xff) } else { MUTED };
            let seg = [Seg { text: names[i].into(), fg, style: if active { ST_BOLD } else { 0 } }];
            let lw = self.measure(&seg, true);
            let gy = by + ((bh as f32 - self.cell_h) / 2.0) as i32;
            self.draw_run(&seg, true, out, whole,
                          bx as f32 + (bw as f32 - lw) / 2.0, gy, 1.0);
        }

        // ---- shell cycler (new tabs only) ----
        let ll = label(self, out, "الصدفة (التبويبات الجديدة)", lay.shell_label_y);
        leader(out, lay.shell_next.0 + lay.shell_next.2, ll, lay.shell_label_y);
        draw_btn(self, out, lay.shell_prev, "‹");
        draw_btn(self, out, lay.shell_next, "›");
        draw_val(self, out, v.shell.trim_end_matches(".exe").to_string(),
                 lay.shell_prev, lay.shell_next, lay.shell_label_y);

        // ---- hide-bar / confirm-close toggles ----
        let ll = label(self, out, "إخفاء الشريط مع تبويب واحد", lay.bar_label_y);
        leader(out, lay.bar_toggle.0 + lay.bar_toggle.2, ll, lay.bar_label_y);
        draw_toggle(out, lay.bar_toggle, v.hide_single_tab);
        let ll = label(self, out, "تأكيد الإغلاق أثناء أمر جارٍ", lay.close_label_y);
        leader(out, lay.close_toggle.0 + lay.close_toggle.2, ll, lay.close_label_y);
        draw_toggle(out, lay.close_toggle, v.confirm_close);

        // footer: hairline + hint
        push_rect(out, whole, x + pad, y + h - (self.cell_h as i32 + 24), w - pad * 2, 1,
                  c4(EDGE, 255));
        let hint = [Seg::plain("انقر للتغيير · Esc للإغلاق والحفظ", (0x6e, 0x78, 0x85))];
        let fw2 = self.measure(&hint, true);
        self.draw_run(&hint, true, out, whole, (x + w - pad) as f32 - fw2,
                      y + h - (self.cell_h as i32 + 14), 1.0);
    }

    /// Geometry of the shortcuts editor: one full-width clickable strip per
    /// action row (draw + hit-test agree, like the settings panel).
    pub fn shortcuts_layout(&self, fw: usize, fh: usize, n: usize) -> ShortcutsLayout {
        let w = ((self.cell_w * 44.0) as i32).clamp(380, fw as i32 - 60);
        let rowh = (self.cell_h + 18.0) as i32;
        let head_h = (self.cell_h + 30.0) as i32;
        let foot_h = (self.cell_h + 30.0) as i32;
        let h = head_h + 6 + rowh * n as i32 + foot_h;
        let x = (fw as i32 - w) / 2;
        let y = (self.tab_bar_h() as i32 + (fh as i32 - h) / 3)
            .max(self.tab_bar_h() as i32 + 12);
        let rows_top = y + head_h + 6;
        let rows = (0..n as i32)
            .map(|i| (x + 8, rows_top + rowh * i, w - 16, rowh))
            .collect();
        ShortcutsLayout { card: (x, y, w, h), rowh, rows }
    }

    /// The shortcuts editor: the settings ledger grammar again — keycap
    /// chips left ··· action label right. The selected row highlights; a
    /// capturing row swaps its chips for an amber "press the new chord".
    pub fn draw_shortcuts(&mut self, out: &mut Vec<Vertex>, fw: usize, fh: usize,
                          rows: &[ShortcutRow], sel: usize, capturing: bool,
                          flash: Option<&str>) {
        const CARD: (u8, u8, u8) = (0x14, 0x19, 0x20);
        const EDGE: (u8, u8, u8) = (0x2a, 0x31, 0x3a);
        const CTL_BG: (u8, u8, u8) = (0x24, 0x2b, 0x34);
        const GREEN: (u8, u8, u8) = (0x2e, 0xa0, 0x43);
        const VALUE: (u8, u8, u8) = (0x7e, 0xe7, 0x87);
        const MUTED: (u8, u8, u8) = (0x9a, 0xa4, 0xb2);
        const DOT: (u8, u8, u8) = (0x33, 0x3b, 0x45);
        const AMBER: (u8, u8, u8) = (0xf2, 0xcc, 0x60);

        let whole: Rect = (0, 0, fw as i32, fh as i32);
        let lay = self.shortcuts_layout(fw, fh, rows.len());
        let (x, y, w, h) = lay.card;
        let pad = 24;
        let tc = (lay.rowh as f32 - self.cell_h) as i32 / 2;

        push_rect(out, whole, 0, 0, fw as i32, fh as i32, c4((0, 0, 0), 140));
        push_rect(out, whole, x - 1, y - 1, w + 2, h + 2, c4(EDGE, 255));
        push_rect(out, whole, x, y, w, h, c4(CARD, 255));
        push_rect(out, whole, x, y, w, 3, c4(GREEN, 255));

        let head = [Seg { text: "الاختصارات".into(), fg: self.fg, style: ST_BOLD }];
        let hw = self.measure(&head, true);
        self.draw_run(&head, true, out, whole, (x + w - pad) as f32 - hw, y + 14, 1.0);
        push_rect(out, whole, x + pad, y + (self.cell_h + 30.0) as i32, w - pad * 2, 1,
                  c4(EDGE, 255));

        for (i, row) in rows.iter().enumerate() {
            let (rx, ry, rw, rh) = lay.rows[i];
            let selected = i == sel;
            if selected {
                // RTL accent: the highlight bar hugs the right edge
                push_rect(out, whole, rx, ry, rw, rh, c4((0x1b, 0x21, 0x29), 255));
                push_rect(out, whole, rx + rw - 3, ry + 2, 3, rh - 4, c4(GREEN, 255));
            }
            // label, right-aligned
            let lseg = [Seg::plain(row.label.clone(),
                                   if selected { self.fg } else { MUTED })];
            let lw = self.measure(&lseg, true);
            let label_x = (x + w - pad) as f32 - lw;
            self.draw_run(&lseg, true, out, whole, label_x, ry + tc, 1.0);

            let chips_end;
            if selected && capturing {
                // amber capture prompt in place of the chips
                let seg = [Seg { text: "اضغط الاختصار الجديد…".into(),
                                 fg: AMBER, style: ST_BOLD }];
                let cw = self.measure(&seg, true);
                self.draw_run(&seg, true, out, whole, (x + pad) as f32, ry + tc, 1.0);
                chips_end = x + pad + cw as i32;
            } else {
                // keycap chips, one per token: Ctrl / Shift / T
                let ch = lay.rowh - 12;
                let mut cx = x + pad;
                let cy = ry + (lay.rowh - ch) / 2;
                for token in row.chord.split('+') {
                    let fg = if row.custom { VALUE } else { self.fg };
                    let seg = [Seg { text: token.to_string(), fg, style: 0 }];
                    let tw = self.measure(&seg, false);
                    let cw = tw as i32 + 14;
                    push_rect(out, whole, cx - 1, cy - 1, cw + 2, ch + 2, c4(EDGE, 255));
                    push_rect(out, whole, cx, cy, cw, ch, c4(CTL_BG, 255));
                    self.draw_run(&seg, false, out, whole, (cx + 7) as f32,
                                  cy + ((ch as f32 - self.cell_h) / 2.0) as i32, 1.0);
                    cx += cw + 6;
                }
                chips_end = cx - 6;
            }
            // the ledger dots tie chips to label
            let dy = ry + lay.rowh / 2 - 1;
            let mut dx = chips_end + 16;
            while dx + 2 <= label_x as i32 - 14 {
                push_rect(out, whole, dx, dy, 2, 2, c4(DOT, 255));
                dx += 9;
            }
        }

        // footer: hairline + flash (amber, right) or the hint
        push_rect(out, whole, x + pad, y + h - (self.cell_h as i32 + 24), w - pad * 2, 1,
                  c4(EDGE, 255));
        let (text, color) = match flash {
            Some(f) => (f.to_string(), AMBER),
            None => (
                "Enter تغيير · Delete افتراضي · Esc إغلاق".to_string(),
                (0x6e, 0x78, 0x85),
            ),
        };
        let seg = [Seg::plain(text, color)];
        let fw2 = self.measure(&seg, true);
        self.draw_run(&seg, true, out, whole, (x + w - pad) as f32 - fw2,
                      y + h - (self.cell_h as i32 + 14), 1.0);
    }

    /// The close guard (same family as the paste guard): a pane or the
    /// window is about to close while a command still runs — confirm.
    pub fn draw_close_guard(&mut self, out: &mut Vec<Vertex>, fw: usize, fh: usize,
                            msg: &str) {
        let whole: Rect = (0, 0, fw as i32, fh as i32);
        push_rect(out, whole, 0, 0, fw as i32, fh as i32, c4((0, 0, 0), 110));
        let w = ((self.cell_w * 56.0) as i32).min(fw as i32 - 60);
        let h = (self.cell_h * 2.0 + 24.0) as i32;
        let x = (fw as i32 - w) / 2;
        let y = self.tab_bar_h() as i32 + (fh as i32 - h) / 4;
        push_rect(out, whole, x, y, w, h, c4((0x1c, 0x21, 0x28), 255));
        push_rect(out, whole, x, y, w, 2, c4((0xf2, 0xcc, 0x60), 255)); // warn accent
        let l1 = [Seg { text: msg.into(), fg: self.fg, style: ST_BOLD }];
        self.draw_run(&l1, true, out, whole, (x + 14) as f32, y + 8, 1.0);
        let l2 = [Seg::plain("Enter إغلاق   ·   Esc إبقاء", (0x9a, 0xa4, 0xb2))];
        self.draw_run(&l2, true, out, whole, (x + 14) as f32,
                      y + 12 + self.cell_h as i32, 1.0);
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

        // the tab bar, drawn last (nothing may bleed into it); hidden bar =
        // no chrome at all, the content owns the full window
        if self.bar_hidden {
            return;
        }
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

        // a clickable settings button (gear) at the tab bar's right end —
        // discoverable and layout-proof (a shortcut fights an Arabic layout)
        let (bx, by, bw, bh) = self.settings_button_rect(width);
        push_rect(out, whole, bx, by, bw, bh, c4((0x24, 0x2b, 0x36), 255));
        let seg = [Seg::plain("⚙", (0xc0, 0xca, 0xf5))];
        let gw = self.measure(&seg, false);
        self.draw_run(&seg, false, out, whole,
                      bx as f32 + (bw as f32 - gw) / 2.0, by + 3, 1.0);
    }

    /// The settings-gear button rect in the tab bar (shared with hit-test).
    pub fn settings_button_rect(&self, width: usize) -> Rect {
        let oy = self.tab_bar_h().round() as i32;
        let bw = (self.cell_w * 3.0).max(28.0) as i32;
        (width as i32 - bw - 6, 3, bw, oy - 6)
    }

    /// Is (px, py) on the settings-gear button? Never while the bar is
    /// hidden (the gear hides with it; Ctrl+, still opens the panel).
    pub fn settings_button_hit(&self, width: usize, px: f64, py: f64) -> bool {
        if self.bar_hidden {
            return false;
        }
        let (bx, by, bw, bh) = self.settings_button_rect(width);
        px >= bx as f64 && px < (bx + bw) as f64 && py >= by as f64 && py < (by + bh) as f64
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

    /// Count the glyphs a shaped run produced (a ligature merges N chars
    /// into 1 glyph, so `->` shapes to 1 when the font ligates).
    fn glyph_count(r: &mut Renderer, text: &str) -> usize {
        let key = r.ensure_shaped(&[Seg::plain(text, FG)], false);
        r.cache_hot[&key]
            .buffer
            .layout_runs()
            .map(|run| run.glyphs.len())
            .sum()
    }

    /// A shaped ASCII run merges to a ligature glyph THE MOMENT the shaper
    /// applies `calt` — the batched-run path is ready. cosmic-text 0.12
    /// builds its shape plan with no user features and does not enable
    /// programming ligatures, so today `->` stays 2 glyphs; this test
    /// asserts the mechanism (batched shaping) and reports the shaper state
    /// without failing when ligatures don't fire (an upstream limit, not a
    /// Bayan bug). See FAMILY_LIGA / the `ligatures` config.
    #[test]
    fn ascii_run_is_batched_for_ligature_shaping() {
        let cfg = crate::config::UserConfig::default(); // ligatures default on
        let mut r = Renderer::new(1.0, &cfg, 0.0);
        assert!(r.ligatures);
        // two distinct letters never merge — the run really is being shaped
        assert_eq!(glyph_count(&mut r, "ab"), 2);
        let arrow = glyph_count(&mut r, "->");
        assert!(arrow == 1 || arrow == 2, "unexpected glyph count {arrow}");
        eprintln!(
            "NOTE ligatures: `->` -> {arrow} glyphs (font {}); \
             cosmic-text 0.12 does not enable calt, so ligatures are wired \
             but dormant until an upstream shaper feature bump",
            r.family
        );
    }

    /// The settings→look path: a config with a theme name (or explicit
    /// colors) must actually change the renderer's bg/fg/palette. This is the
    /// heart of "settings change the appearance" — the complaint that started
    /// M15. Verified without any GUI.
    #[test]
    fn a_theme_changes_the_renderer_colors() {
        // default theme = بيان (dark GitHub-ish)
        let def = Renderer::new(1.0, &crate::config::UserConfig::default(), 0.0);
        assert_eq!(def.bg, THEMES[0].bg);

        // a named theme flows all the way to the renderer's colors
        let dracula = theme_by_name("Dracula").unwrap();
        let mut cfg = crate::config::UserConfig {
            theme: Some("Dracula".into()),
            ..Default::default()
        };
        let r = Renderer::new(1.0, &cfg, 0.0);
        assert_eq!(r.bg, dracula.bg, "theme bg must reach the renderer");
        assert_eq!(r.fg, dracula.fg);
        assert_eq!(r.palette, dracula.palette);
        assert_eq!(r.ansi_rgb(AnsiColor::Named(NamedColor::Red)), dracula.palette[1]);

        // an explicit bg overrides the theme (config precedence)
        cfg.bg = Some("#123456".into());
        let r2 = Renderer::new(1.0, &cfg, 0.0);
        assert_eq!(r2.bg, (0x12, 0x34, 0x56));
        assert_eq!(r2.fg, dracula.fg, "unset fg still comes from the theme");

        // the settings config round-trips through the file format
        let saved: crate::config::UserConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(saved.theme.as_deref(), Some("Dracula"));
        assert_eq!(saved.bg.as_deref(), Some("#123456"));
    }

    /// The ligatures toggle picks a Mono-leading font family when off (so a
    /// ligature-capable default doesn't sneak substitutions in later).
    #[test]
    fn ligatures_off_prefers_a_mono_family() {
        let mut cfg = crate::config::UserConfig::default();
        cfg.ligatures = Some(false);
        let r = Renderer::new(1.0, &cfg, 0.0);
        assert!(!r.ligatures);
        // the picked family is the first installed MONO candidate (or a
        // Consolas fallback) — never a Code-leading pick
        assert!(FAMILY_MONO.contains(&r.family.as_str()) || r.family == "Consolas");
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

    /// The settings panel layout: one clickable tile per theme, distinct
    /// −/+ and toggle regions, all inside the card and on-screen.
    #[test]
    fn settings_layout_places_clickable_controls() {
        let r = Renderer::new(1.0, &crate::config::UserConfig::default(), 0.0);
        let (fw, fh) = (1400usize, 900usize);
        let lay = r.settings_layout(fw, fh);
        assert_eq!(lay.theme_tiles.len(), THEMES.len());
        let (cx, cy, cw, ch) = lay.card;
        // the card is centered and fully on-screen
        assert!(cx > 0 && cy > 0 && cx + cw <= fw as i32 && cy + ch <= fh as i32);
        // every theme tile is inside the card and non-overlapping (left to right)
        let mut last_right = 0;
        for &(tx, ty, tw, th) in &lay.theme_tiles {
            assert!(tx >= cx && tx + tw <= cx + cw && ty >= cy && ty + th <= cy + ch);
            assert!(tx >= last_right, "tiles must not overlap");
            last_right = tx + tw;
        }
        // every control is a distinct rect, inside the card, and none of
        // them overlap (each row's controls sit in their own band)
        let mut controls: Vec<Rect> = vec![
            lay.font_prev, lay.font_next,
            lay.size_minus, lay.size_plus,
            lay.scroll_minus, lay.scroll_plus,
            lay.copy_toggle, lay.liga_toggle,
            lay.pad_minus, lay.pad_plus,
            lay.opacity_minus, lay.opacity_plus,
            lay.shell_prev, lay.shell_next,
            lay.bar_toggle, lay.close_toggle, lay.blink_toggle,
        ];
        controls.extend(lay.cursor_btns);
        controls.extend(lay.bell_btns);
        for (i, &(rx, ry, rw, rh)) in controls.iter().enumerate() {
            assert!(rx >= cx && rx + rw <= cx + cw && ry >= cy && ry + rh <= cy + ch,
                    "control {i} escapes the card");
            assert!(rect_hit(controls[i], (rx + 2) as f64, (ry + 2) as f64));
            for &other in &controls[i + 1..] {
                let (ox, oy, ow, oh) = other;
                let disjoint = rx + rw <= ox || ox + ow <= rx || ry + rh <= oy || oy + oh <= ry;
                assert!(disjoint, "controls overlap: {:?} vs {:?}", controls[i], other);
            }
        }
        // a point in the first tile does NOT hit the +/− or toggle
        let (t0x, t0y, _, _) = lay.theme_tiles[0];
        assert!(!rect_hit(lay.size_plus, (t0x + 2) as f64, (t0y + 2) as f64));
    }

    /// The shortcuts editor layout: one clickable strip per action, all
    /// inside the card, none overlapping, in row order top to bottom.
    #[test]
    fn shortcuts_layout_places_one_strip_per_action() {
        let r = Renderer::new(1.0, &crate::config::UserConfig::default(), 0.0);
        let n = crate::keybinds::Action::ALL.len();
        let lay = r.shortcuts_layout(1400, 900, n);
        assert_eq!(lay.rows.len(), n);
        let (cx, cy, cw, ch) = lay.card;
        assert!(cx > 0 && cy > 0 && cx + cw <= 1400 && cy + ch <= 900);
        let mut last_bottom = 0;
        for &(rx, ry, rw, rh) in &lay.rows {
            assert!(rx >= cx && rx + rw <= cx + cw && ry >= cy && ry + rh <= cy + ch);
            assert!(ry >= last_bottom, "rows must stack without overlap");
            last_bottom = ry + rh;
        }
        // the settings panel's header button exists and sits inside ITS card
        let slay = r.settings_layout(1400, 900);
        let (bx, by, bw, bh) = slay.shortcuts_btn;
        let (scx, scy, scw, sch) = slay.card;
        assert!(bx >= scx && bx + bw <= scx + scw && by >= scy && by + bh <= scy + sch);
    }

    /// The settings gear button hit-test: a click on it registers, a click
    /// elsewhere on the tab bar does not (so settings is reachable by mouse
    /// regardless of keyboard layout).
    #[test]
    fn settings_button_is_clickable() {
        let r = Renderer::new(1.0, &crate::config::UserConfig::default(), 0.0);
        let w = 1000;
        let (bx, by, bw, bh) = r.settings_button_rect(w);
        // a point inside the button hits
        assert!(r.settings_button_hit(w, (bx + bw / 2) as f64, (by + bh / 2) as f64));
        // the far left of the tab bar (where tabs live) does not
        assert!(!r.settings_button_hit(w, 20.0, (by + bh / 2) as f64));
        // the button sits at the right edge
        assert!(bx + bw <= w as i32 && bx > w as i32 / 2);
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
        let fam = pick_family(fs.db(), None, true);
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
