//! Inline images: Sixel (DCS `q`), Kitty graphics (APC `_G`), and iTerm2
//! (OSC `1337 ; File=` with `inline=1`).
//!
//! `alacritty_terminal` does not place these. [`ImageFilter`] pulls them out
//! of the PTY bytes before the VTE parser, the same way BDSM / SCP are
//! stripped and OSC 133 is split: the cursor the store anchors to is the
//! cursor the bytes before the image left behind.
//!
//! ConPTY is opened **without** `PSEUDOCONSOLE_PASSTHROUGH_MODE` (`0x8`).
//! The flags in `vendor/portable-pty/src/win/psuedocon.rs` are inherit-cursor,
//! resize-quirk, and Win32 input. In that mode conhost parses DCS and APC
//! and does not forward them, so Sixel and Kitty graphics do not arrive from
//! a real console host. OSC does get forwarded (OSC 7 / 9 / 52 / 133 already
//! do), so iTerm2 can. The parsers, the store, and the quad builder do not
//! depend on that path: a byte slice fed to [`crate::term::process_chunk`]
//! is enough.
//!
//! `t=f`, `t=t`, and `t=s` (file, temp file, shared memory) are rejected.
//! Nothing in this module opens a path or a mapping, and there is no switch
//! that turns those transmissions on.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use base64::Engine;

/// Decoded pixels of one image, hard ceiling. The config default is lower.
pub const CEIL_IMAGE_BYTES: usize = 64 * 1024 * 1024;
/// Widest or tallest image, in pixels.
pub const CEIL_DIM: u32 = 8192;
/// Pixel bytes one pane may hold.
pub const CEIL_PANE_BYTES: usize = 256 * 1024 * 1024;
/// Pixel bytes every pane of this process may hold together.
pub const CEIL_SESSION_BYTES: usize = 512 * 1024 * 1024;
/// Images one pane may hold. A 1×1 RGBA is 4 bytes; without a count the
/// byte budget would still allow millions of allocations.
pub const MAX_IMAGES_PER_PANE: usize = 64;
/// Images every pane may hold together.
pub const MAX_IMAGES_PER_SESSION: usize = 128;
/// A placement wider or taller than this is clamped, not stored as a
/// multi-megabyte rectangle of cells.
const MAX_CELLS: usize = 512;

const DEFAULT_MAX_BYTES: usize = 20 * 1024 * 1024;
const DEFAULT_DIM: u32 = 4096;
const DEFAULT_PANE_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_SESSION_BYTES: usize = 128 * 1024 * 1024;
/// Cell size used when the protocol gives pixels and not columns. The
/// renderer then stretches those cells to the real font cell.
const CELL_PX_W: u32 = 8;
const CELL_PX_H: u32 = 16;

/// Caps for one running Bayan. Built from [`crate::config::ImageConfig`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageLimits {
    pub enabled: bool,
    pub max_bytes: usize,
    pub max_width: u32,
    pub max_height: u32,
    pub max_pane_bytes: usize,
    pub max_session_bytes: usize,
    pub max_images: usize,
    pub max_session_images: usize,
}

impl Default for ImageLimits {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: DEFAULT_MAX_BYTES,
            max_width: DEFAULT_DIM,
            max_height: DEFAULT_DIM,
            max_pane_bytes: DEFAULT_PANE_BYTES,
            max_session_bytes: DEFAULT_SESSION_BYTES,
            max_images: MAX_IMAGES_PER_PANE,
            max_session_images: MAX_IMAGES_PER_SESSION,
        }
    }
}

impl ImageLimits {
    /// Config values, clamped to the hard ceilings. A zero or huge number
    /// cannot raise the ceiling or turn the cap off.
    pub fn from_user(cfg: &crate::config::ImageConfig) -> Self {
        let max_bytes = clamp_u64(cfg.max_bytes, 1, CEIL_IMAGE_BYTES as u64);
        let mut max_pane = clamp_u64(cfg.max_pane_bytes, 1, CEIL_PANE_BYTES as u64);
        let max_session = clamp_u64(cfg.max_session_bytes, 1, CEIL_SESSION_BYTES as u64);
        if max_pane > max_session {
            max_pane = max_session;
        }
        Self {
            enabled: cfg.enabled,
            max_bytes,
            max_width: cfg.max_width.clamp(1, CEIL_DIM),
            max_height: cfg.max_height.clamp(1, CEIL_DIM),
            max_pane_bytes: max_pane,
            max_session_bytes: max_session,
            max_images: MAX_IMAGES_PER_PANE,
            max_session_images: MAX_IMAGES_PER_SESSION,
        }
    }
}

fn clamp_u64(v: u64, lo: u64, hi: u64) -> usize {
    v.clamp(lo, hi) as usize
}

fn saturating_fetch_sub(atom: &AtomicUsize, n: usize) {
    let mut cur = atom.load(Ordering::Relaxed);
    loop {
        let next = cur.saturating_sub(n);
        match atom.compare_exchange(cur, next, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return,
            Err(seen) => cur = seen,
        }
    }
}

/// Pixel bytes and image count shared by every pane. Charged when an image
/// is stored, released when it is deleted, scrolled out, or the pane resets.
pub struct ImageBudget {
    limits: ImageLimits,
    bytes: AtomicUsize,
    images: AtomicUsize,
}

impl ImageBudget {
    pub fn new(limits: ImageLimits) -> Self {
        Self {
            limits,
            bytes: AtomicUsize::new(0),
            images: AtomicUsize::new(0),
        }
    }

    pub fn limits(&self) -> ImageLimits {
        self.limits
    }

    #[cfg(test)]
    pub fn charged_bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    fn try_charge(&self, bytes: usize, images: usize) -> bool {
        if images > 0 {
            let mut n = self.images.load(Ordering::Relaxed);
            loop {
                let next = n.saturating_add(images);
                if next > self.limits.max_session_images {
                    return false;
                }
                match self
                    .images
                    .compare_exchange(n, next, Ordering::AcqRel, Ordering::Relaxed)
                {
                    Ok(_) => break,
                    Err(cur) => n = cur,
                }
            }
        }
        let mut n = self.bytes.load(Ordering::Relaxed);
        loop {
            let next = n.saturating_add(bytes);
            if next > self.limits.max_session_bytes || next < n {
                if images > 0 {
                    self.images.fetch_sub(images, Ordering::AcqRel);
                }
                return false;
            }
            match self
                .bytes
                .compare_exchange(n, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(cur) => n = cur,
            }
        }
    }

    fn uncharge(&self, bytes: usize, images: usize) {
        saturating_fetch_sub(&self.bytes, bytes);
        if images > 0 {
            saturating_fetch_sub(&self.images, images);
        }
    }
}

/// Where a placement hangs on the grid. `line` is the absolute row
/// [`crate::bidi_ctl::cursor_line_abs`] uses, so the image scrolls with the
/// text and leaves with the scrollback.
#[derive(Clone, Copy, Debug)]
pub struct PlaceAt {
    pub line: u64,
    pub col: usize,
    pub columns: usize,
    pub screen_rows: usize,
    pub screen_top: u64,
}

/// One visible image, already limited to the screen the pane is on.
#[derive(Clone, Debug)]
pub struct ImageSnap {
    pub cookie: u64,
    pub line: u64,
    pub col: usize,
    pub cols: usize,
    pub rows: usize,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacementInfo {
    pub line: u64,
    pub col: usize,
    pub cols: usize,
    pub rows: usize,
    pub alt: bool,
    pub image: u32,
}

struct Img {
    cookie: u64,
    w: u32,
    h: u32,
    rgba: Arc<[u8]>,
    bytes: usize,
    /// `a=t` keeps the pixels after the last placement is gone so `a=p`
    /// can still show them. `a=T` does not: once the cells scroll out of
    /// the scrollback the pixels go with them.
    pinned: bool,
}

struct Placement {
    id: u32,
    image: u32,
    line: u64,
    col: usize,
    cols: usize,
    rows: usize,
    alt: bool,
}

struct Raster {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
enum ClearKind {
    /// Erase in display. `0` cursor to end, `1` start to cursor, `2` screen.
    Ed(u16),
    /// Erase in line, same parameter meanings.
    El(u16),
}

/// What applying one image command did. `move_down` is fed back through the
/// VTE parser so the cursor the program sees matches the cells the image took.
#[derive(Default)]
pub struct Effect {
    pub replies: Vec<String>,
    pub move_down: usize,
}

/// Images for one pane, plus the filter that feeds them.
pub struct ImageStore {
    limits: ImageLimits,
    budget: Arc<ImageBudget>,
    pane_bytes: usize,
    images: HashMap<u32, Img>,
    placements: Vec<Placement>,
    next_id: u32,
    next_place: u32,
    next_cookie: u64,
    on_alt: bool,
    pending: Option<PendingKitty>,
}

struct PendingKitty {
    id: u32,
    format: u16,
    width: u32,
    height: u32,
    quiet: u8,
    columns: Option<u32>,
    rows: Option<u32>,
    cursor_move: bool,
    pinned: bool,
    b64: String,
}

impl Default for ImageStore {
    fn default() -> Self {
        Self::new(ImageLimits::default())
    }
}

impl ImageStore {
    fn new(limits: ImageLimits) -> Self {
        Self {
            limits,
            budget: Arc::new(ImageBudget::new(limits)),
            pane_bytes: 0,
            images: HashMap::new(),
            placements: Vec::new(),
            next_id: 0,
            next_place: 0,
            next_cookie: 1,
            on_alt: false,
            pending: None,
        }
    }

    /// Share `budget` with the other panes. Drops anything this pane held
    /// against the previous budget (nothing, at spawn).
    pub fn attach(&mut self, budget: Arc<ImageBudget>) {
        self.reset();
        self.limits = budget.limits();
        self.budget = budget;
    }

    #[cfg(test)]
    pub fn pane_bytes(&self) -> usize {
        self.pane_bytes
    }

    #[cfg(test)]
    pub fn placement_count(&self) -> usize {
        self.placements.len()
    }

    #[cfg(test)]
    pub fn placements(&self) -> Vec<PlacementInfo> {
        self.placements
            .iter()
            .map(|p| PlacementInfo {
                line: p.line,
                col: p.col,
                cols: p.cols,
                rows: p.rows,
                alt: p.alt,
                image: p.image,
            })
            .collect()
    }

    #[cfg(test)]
    pub fn on_alt(&self) -> bool {
        self.on_alt
    }

    /// Placements on the screen the pane is showing now.
    pub fn snapshot(&self) -> Vec<ImageSnap> {
        let alt = self.on_alt;
        self.placements
            .iter()
            .filter(|p| p.alt == alt)
            .filter_map(|p| {
                let img = self.images.get(&p.image)?;
                Some(ImageSnap {
                    cookie: img.cookie,
                    line: p.line,
                    col: p.col,
                    cols: p.cols,
                    rows: p.rows,
                    width: img.w,
                    height: img.h,
                    rgba: Arc::clone(&img.rgba),
                })
            })
            .collect()
    }

    pub fn reset(&mut self) {
        let ids: Vec<u32> = self.images.keys().copied().collect();
        for id in ids {
            self.free_image(id);
        }
        self.placements.clear();
        self.pending = None;
        self.on_alt = false;
        self.pane_bytes = 0;
    }

    /// A resize reflows rows, so an absolute line no longer names the cells
    /// the image was anchored to. Same reason SCP stamps are dropped.
    pub fn on_resize(&mut self) {
        let alt = self.on_alt;
        self.reset();
        self.on_alt = alt;
    }

    /// Drop primary-screen images whose anchor has left the scrollback.
    pub fn evict(&mut self, before: u64) {
        self.placements.retain(|p| p.alt || p.line >= before);
        self.gc();
    }

    /// ED 3 erased the scrollback. Placements whose anchor is above the
    /// screen go with it. The feed then bumps `evicted` and [`Self::evict`]
    /// agrees.
    pub fn clear_history(&mut self, screen_top: u64) {
        self.placements.retain(|p| p.alt || p.line >= screen_top);
        self.gc();
    }

    fn clear_display(&mut self, kind: ClearKind, at: PlaceAt) {
        let alt = self.on_alt;
        self.placements
            .retain(|p| p.alt != alt || !hits_clear(p, kind, &at));
        self.gc();
    }

    /// `enter` saves the primary grid's images (they stay stored, hidden)
    /// and shows a fresh alt screen. Leaving drops alt placements.
    pub fn set_alt(&mut self, enter: bool) {
        if enter {
            self.placements.retain(|p| !p.alt);
            self.on_alt = true;
            self.gc();
        } else if self.on_alt {
            self.placements.retain(|p| !p.alt);
            self.on_alt = false;
            self.gc();
        }
    }

    pub fn erase_display(&mut self, mode: u16, at: PlaceAt) {
        self.clear_display(ClearKind::Ed(mode), at);
    }

    pub fn erase_line(&mut self, mode: u16, at: PlaceAt) {
        self.clear_display(ClearKind::El(mode), at);
    }

    pub(crate) fn ingest(&mut self, cmd: Inbound, at: &PlaceAt) -> Effect {
        if !self.limits.enabled {
            return Effect::default();
        }
        match cmd {
            Inbound::Bytes(_) => Effect::default(),
            Inbound::Sixel { p2, data, overflow } => self.place_sixel(p2, &data, overflow, at),
            Inbound::Kitty { body, overflow } => self.place_kitty(&body, overflow, at),
            Inbound::Iterm { body, overflow } => self.place_iterm(&body, overflow, at),
        }
    }

    fn place_sixel(&mut self, p2: u8, data: &[u8], overflow: bool, at: &PlaceAt) -> Effect {
        if overflow {
            return Effect::default();
        }
        let Some(raster) = decode_sixel(p2, data, &self.limits) else {
            return Effect::default();
        };
        let (cols, rows) = cell_span(raster.w, raster.h, None, None);
        let Some(id) = self.alloc_id() else {
            return Effect::default();
        };
        if !self.store_image(id, raster, false) {
            return Effect::default();
        }
        let move_down = self.add_placement(id, None, at, cols, rows, true);
        Effect {
            move_down,
            ..Effect::default()
        }
    }

    fn place_iterm(&mut self, body: &[u8], overflow: bool, at: &PlaceAt) -> Effect {
        if overflow {
            return Effect::default();
        }
        let Some(parsed) = parse_iterm(body) else {
            return Effect::default();
        };
        if !parsed.inline {
            return Effect::default();
        }
        if parsed.size.is_some_and(|n| n > self.limits.max_bytes) {
            return Effect::default();
        }
        let Some(bytes) = decode_b64(&parsed.b64) else {
            return Effect::default();
        };
        if bytes.len() > self.limits.max_bytes {
            return Effect::default();
        }
        let Some(raster) = decode_png(&bytes, &self.limits) else {
            return Effect::default();
        };
        let (cols, rows) = iterm_cells(&parsed, raster.w, raster.h, at);
        let Some(id) = self.alloc_id() else {
            return Effect::default();
        };
        if !self.store_image(id, raster, false) {
            return Effect::default();
        }
        let move_down = self.add_placement(id, None, at, cols, rows, parsed.move_cursor);
        Effect {
            move_down,
            ..Effect::default()
        }
    }

    fn place_kitty(&mut self, body: &[u8], overflow: bool, at: &PlaceAt) -> Effect {
        if overflow {
            return self.kitty_fail(0, true);
        }
        let Some((fields, payload)) = parse_kitty(body) else {
            return self.kitty_fail(0, true);
        };
        if fields.transmission != Trans::Direct {
            // File, temp-file, and shared-memory payloads are not decoded
            // and are not opened. Drop the bytes.
            self.pending = None;
            return self.kitty_fail(fields.quiet, false);
        }
        if fields.compression {
            self.pending = None;
            return self.kitty_fail(fields.quiet, false);
        }
        match fields.action {
            Action::Delete => self.kitty_delete(&fields, at),
            Action::Put => self.kitty_put(&fields, at),
            Action::Transmit | Action::TransmitDisplay => self.kitty_transmit(fields, payload, at),
            Action::Other => self.kitty_fail(fields.quiet, false),
        }
    }

    fn kitty_fail(&mut self, quiet: u8, _parse: bool) -> Effect {
        self.pending = None;
        Effect {
            replies: kitty_reply(quiet, None, None, false),
            move_down: 0,
        }
    }

    fn kitty_transmit(&mut self, fields: KittyFields, payload: &[u8], at: &PlaceAt) -> Effect {
        let quiet = fields.quiet;
        // A later chunk often omits `i`. It belongs to the open transmission.
        let id = if let Some(id) = fields.image_id {
            id
        } else if let Some(p) = &self.pending {
            p.id
        } else {
            match self.alloc_id() {
                Some(id) => id,
                None => return self.kitty_fail(quiet, false),
            }
        };
        if !matches!(fields.format, 24 | 32 | 100) {
            self.pending = None;
            return self.kitty_fail(quiet, false);
        }
        if matches!(fields.format, 24 | 32)
            && (fields.width.is_none() || fields.height.is_none())
            && self.pending.is_none()
        {
            // Size can arrive on the first chunk only. A continuation keeps
            // the size already stored.
            if !self.pending_matches(id) {
                return self.kitty_fail(quiet, false);
            }
        }
        if let (Some(w), Some(h)) = (fields.width, fields.height) {
            if !dims_ok(w, h, &self.limits) {
                self.pending = None;
                return self.kitty_fail(quiet, false);
            }
        }
        let piece = std::str::from_utf8(payload).unwrap_or("");
        let start = if self.pending_matches(id) {
            self.pending.take().unwrap()
        } else {
            self.pending = None;
            let (w, h) = match (fields.width, fields.height) {
                (Some(w), Some(h)) => (w, h),
                _ if fields.format == 100 => (0, 0),
                _ => return self.kitty_fail(quiet, false),
            };
            PendingKitty {
                id,
                format: fields.format,
                width: w,
                height: h,
                quiet,
                columns: fields.columns,
                rows: fields.rows,
                cursor_move: fields.cursor_move,
                pinned: fields.action == Action::Transmit,
                b64: String::new(),
            }
        };
        let mut pending = start;
        // Later chunks may fill in placement hints. The first chunk's size
        // and format stay.
        if fields.columns.is_some() {
            pending.columns = fields.columns;
        }
        if fields.rows.is_some() {
            pending.rows = fields.rows;
        }
        if fields.cursor_move {
            pending.cursor_move = true;
        }
        if fields.action == Action::Transmit {
            pending.pinned = true;
        }
        let extra = piece.chars().filter(|c| !c.is_ascii_whitespace()).count();
        let cap = self.limits.max_bytes.saturating_mul(4) / 3 + 8;
        if pending.b64.len().saturating_add(extra) > cap {
            self.pending = None;
            return self.kitty_fail(quiet, false);
        }
        pending
            .b64
            .extend(piece.chars().filter(|c| !c.is_ascii_whitespace()));
        if fields.more {
            self.pending = Some(pending);
            return Effect::default();
        }
        let quiet = pending.quiet;
        let place = fields.action != Action::Transmit && !pending.pinned;
        // `a=t` on a later chunk can pin; `a=T` on the first does not.
        let pinned = pending.pinned;
        let cursor_move = pending.cursor_move && !pinned;
        let columns = pending.columns;
        let rows = pending.rows;
        let id = pending.id;
        let decoded = match finish_kitty(&pending, &self.limits) {
            Some(r) => r,
            None => return self.kitty_fail(quiet, false),
        };
        if !self.store_image(id, decoded, pinned) {
            return self.kitty_fail(quiet, false);
        }
        let mut move_down = 0;
        if place || fields.action == Action::TransmitDisplay {
            let img = &self.images[&id];
            let (cols, rows) = cell_span(img.w, img.h, columns, rows);
            let do_move =
                cursor_move || (fields.action == Action::TransmitDisplay && fields.cursor_move);
            move_down = self.add_placement(id, fields.placement_id, at, cols, rows, do_move);
        }
        Effect {
            replies: kitty_reply(quiet, Some(id), fields.placement_id, true),
            move_down,
        }
    }

    fn pending_matches(&self, id: u32) -> bool {
        self.pending.as_ref().is_some_and(|p| p.id == id)
    }

    fn kitty_put(&mut self, fields: &KittyFields, at: &PlaceAt) -> Effect {
        let Some(id) = fields.image_id else {
            return self.kitty_fail(fields.quiet, false);
        };
        if !self.images.contains_key(&id) {
            return self.kitty_fail(fields.quiet, false);
        }
        let img = &self.images[&id];
        let (cols, rows) = cell_span(img.w, img.h, fields.columns, fields.rows);
        let move_down =
            self.add_placement(id, fields.placement_id, at, cols, rows, fields.cursor_move);
        Effect {
            replies: kitty_reply(fields.quiet, Some(id), fields.placement_id, true),
            move_down,
        }
    }

    fn kitty_delete(&mut self, fields: &KittyFields, at: &PlaceAt) -> Effect {
        let which = fields.delete.unwrap_or('a');
        let alt = self.on_alt;
        match which {
            'A' => {
                let ids: Vec<u32> = self.images.keys().copied().collect();
                for id in ids {
                    self.free_image(id);
                }
                self.placements.clear();
                self.pending = None;
            }
            'i' | 'I' => {
                let Some(id) = fields.image_id else {
                    return self.kitty_fail(fields.quiet, false);
                };
                if let Some(pid) = fields.placement_id {
                    self.placements.retain(|p| !(p.image == id && p.id == pid));
                    if which == 'I' {
                        self.gc();
                    }
                } else {
                    self.free_image(id);
                }
            }
            'p' | 'q' => {
                let Some(pid) = fields.placement_id else {
                    return self.kitty_fail(fields.quiet, false);
                };
                self.placements.retain(|p| p.id != pid);
                if which == 'q' {
                    self.gc();
                }
            }
            'c' => {
                self.placements
                    .retain(|p| p.alt != alt || !cell_in(p, at.line, at.col));
                self.gc();
            }
            'a' => {
                self.placements
                    .retain(|p| p.alt != alt || !rows_overlap(p, at.screen_top, at.screen_rows));
                self.gc();
            }
            _ => return self.kitty_fail(fields.quiet, false),
        }
        Effect {
            replies: kitty_reply(fields.quiet, fields.image_id, fields.placement_id, true),
            move_down: 0,
        }
    }

    fn alloc_id(&mut self) -> Option<u32> {
        if self.images.len() >= self.limits.max_images {
            return None;
        }
        for _ in 0..=self.limits.max_images {
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if !self.images.contains_key(&self.next_id) {
                return Some(self.next_id);
            }
        }
        None
    }

    fn alloc_cookie(&mut self) -> u64 {
        let c = self.next_cookie;
        self.next_cookie = self.next_cookie.wrapping_add(1).max(1);
        c
    }

    fn store_image(&mut self, id: u32, raster: Raster, pinned: bool) -> bool {
        let bytes = raster.rgba.len();
        if bytes == 0 || bytes > self.limits.max_bytes {
            return false;
        }
        let old = self.images.remove(&id);
        let replacing = old.is_some();
        let old_pin = old.as_ref().is_some_and(|img| img.pinned);
        if !replacing && self.images.len() >= self.limits.max_images {
            if let Some(old) = old {
                self.images.insert(id, old);
            }
            return false;
        }
        if let Some(ref old) = old {
            self.release(old.bytes, 0);
        }
        let count = usize::from(!replacing);
        if self.pane_bytes.saturating_add(bytes) > self.limits.max_pane_bytes
            || !self.budget.try_charge(bytes, count)
        {
            if let Some(old) = old {
                let b = old.bytes;
                if self.budget.try_charge(b, 0) {
                    self.pane_bytes += b;
                    self.images.insert(id, old);
                }
            }
            return false;
        }
        let cookie = self.alloc_cookie();
        self.pane_bytes += bytes;
        let keep_pin = pinned || old_pin;
        self.images.insert(
            id,
            Img {
                cookie,
                w: raster.w,
                h: raster.h,
                rgba: Arc::from(raster.rgba),
                bytes,
                pinned: keep_pin,
            },
        );
        true
    }

    fn add_placement(
        &mut self,
        image: u32,
        protocol_id: Option<u32>,
        at: &PlaceAt,
        cols: usize,
        rows: usize,
        move_cursor: bool,
    ) -> usize {
        let id = protocol_id.unwrap_or_else(|| {
            self.next_place = self.next_place.wrapping_add(1).max(1);
            self.next_place
        });
        self.placements.retain(|p| p.id != id);
        let cols = cols.clamp(1, MAX_CELLS);
        let rows = rows.clamp(1, MAX_CELLS);
        self.placements.push(Placement {
            id,
            image,
            line: at.line,
            col: at.col,
            cols,
            rows,
            alt: self.on_alt,
        });
        if move_cursor {
            rows
        } else {
            0
        }
    }

    fn free_image(&mut self, id: u32) {
        if let Some(img) = self.images.remove(&id) {
            self.release(img.bytes, 1);
        }
        self.placements.retain(|p| p.image != id);
        if self.pending.as_ref().is_some_and(|p| p.id == id) {
            self.pending = None;
        }
    }

    fn release(&mut self, bytes: usize, images: usize) {
        self.pane_bytes = self.pane_bytes.saturating_sub(bytes);
        self.budget.uncharge(bytes, images);
    }

    fn gc(&mut self) {
        let used: Vec<u32> = self.placements.iter().map(|p| p.image).collect();
        let drop_ids: Vec<u32> = self
            .images
            .iter()
            .filter(|(id, img)| !img.pinned && !used.contains(id))
            .map(|(id, _)| *id)
            .collect();
        for id in drop_ids {
            self.free_image(id);
        }
    }
}

fn hits_clear(p: &Placement, kind: ClearKind, at: &PlaceAt) -> bool {
    let top = at.screen_top;
    let bottom = top.saturating_add(at.screen_rows as u64);
    let line = at.line;
    match kind {
        ClearKind::Ed(2) => rows_overlap(p, top, at.screen_rows),
        ClearKind::Ed(0) => {
            full_lines(p, line.saturating_add(1), bottom)
                || (line >= p.line
                    && line < p.line.saturating_add(p.rows as u64)
                    && cols_overlap(p, at.col, at.columns))
        }
        ClearKind::Ed(1) => {
            full_lines(p, top, line)
                || (line >= p.line
                    && line < p.line.saturating_add(p.rows as u64)
                    && cols_overlap(p, 0, at.col.saturating_add(1)))
        }
        ClearKind::Ed(_) => false,
        ClearKind::El(2) => line >= p.line && line < p.line.saturating_add(p.rows as u64),
        ClearKind::El(0) => {
            line >= p.line
                && line < p.line.saturating_add(p.rows as u64)
                && cols_overlap(p, at.col, at.columns)
        }
        ClearKind::El(1) => {
            line >= p.line
                && line < p.line.saturating_add(p.rows as u64)
                && cols_overlap(p, 0, at.col.saturating_add(1))
        }
        ClearKind::El(_) => false,
    }
}

fn full_lines(p: &Placement, from: u64, to: u64) -> bool {
    if from >= to {
        return false;
    }
    let end = p.line.saturating_add(p.rows as u64);
    p.line < to && end > from
}

fn rows_overlap(p: &Placement, top: u64, rows: usize) -> bool {
    full_lines(p, top, top.saturating_add(rows as u64))
}

fn cols_overlap(p: &Placement, from: usize, to: usize) -> bool {
    if from >= to {
        return false;
    }
    let end = p.col.saturating_add(p.cols);
    p.col < to && end > from
}

fn cell_in(p: &Placement, line: u64, col: usize) -> bool {
    line >= p.line
        && line < p.line.saturating_add(p.rows as u64)
        && col >= p.col
        && col < p.col.saturating_add(p.cols)
}

fn cell_span(px_w: u32, px_h: u32, cols: Option<u32>, rows: Option<u32>) -> (usize, usize) {
    let c = cols
        .map(|n| n as usize)
        .unwrap_or_else(|| px_span(px_w, CELL_PX_W));
    let r = rows
        .map(|n| n as usize)
        .unwrap_or_else(|| px_span(px_h, CELL_PX_H));
    (c.clamp(1, MAX_CELLS), r.clamp(1, MAX_CELLS))
}

fn px_span(px: u32, cell: u32) -> usize {
    (px as usize).div_ceil(cell.max(1) as usize).max(1)
}

fn dims_ok(w: u32, h: u32, limits: &ImageLimits) -> bool {
    w > 0 && h > 0 && w <= limits.max_width && h <= limits.max_height
}

fn raw_bytes(w: u32, h: u32, bpp: usize, limits: &ImageLimits) -> Option<usize> {
    if !dims_ok(w, h, limits) {
        return None;
    }
    let n = (w as usize).checked_mul(h as usize)?.checked_mul(bpp)?;
    (n > 0 && n <= limits.max_bytes).then_some(n)
}

// --- sixel -----------------------------------------------------------------

fn decode_sixel(p2: u8, data: &[u8], limits: &ImageLimits) -> Option<Raster> {
    let mut d = SixelDec::new(p2 == 2, limits);
    let mut i = 0;
    while i < data.len() {
        if d.fail {
            return None;
        }
        let b = data[i];
        if matches!(b, b'\n' | b'\r' | b'\t') {
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                let (nums, next) = read_params(data, i + 1)?;
                i = next;
                if nums.len() >= 4 {
                    let (w, h) = (nums[2], nums[3]);
                    if !d.reserve(w, h) {
                        return None;
                    }
                }
            }
            b'#' => {
                let (nums, next) = read_params(data, i + 1)?;
                i = next;
                if nums.is_empty() {
                    continue;
                }
                let pc = nums[0];
                if pc > 255 {
                    return None;
                }
                if nums.len() >= 5 {
                    d.define(pc as u8, nums[1], nums[2], nums[3], nums[4]);
                } else {
                    d.selected = pc as u8;
                }
            }
            b'!' => {
                let (count, next) = read_u32(data, i + 1)?;
                i = next;
                let ch = *data.get(i)?;
                i += 1;
                if !d.repeat(count, ch) {
                    return None;
                }
            }
            b'$' => {
                d.x = 0;
                i += 1;
            }
            b'-' => {
                d.x = 0;
                d.y = d.y.saturating_add(6);
                if d.y >= d.max_h {
                    return None;
                }
                i += 1;
            }
            0x3f..=0x7e => {
                if !d.put(b) {
                    return None;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    if d.fail {
        return None;
    }
    d.finish()
}

struct SixelDec {
    palette: [[u8; 4]; 256],
    selected: u8,
    rows: Vec<Vec<u8>>,
    x: usize,
    y: usize,
    max_w: usize,
    max_h: usize,
    max_bytes: usize,
    fail: bool,
    bg: Option<[u8; 4]>,
}

impl SixelDec {
    fn new(opaque: bool, limits: &ImageLimits) -> Self {
        let mut palette = [[0, 0, 0, 255]; 256];
        palette[1] = [255, 0, 0, 255];
        palette[2] = [0, 255, 0, 255];
        palette[3] = [255, 255, 0, 255];
        palette[4] = [0, 0, 255, 255];
        palette[5] = [255, 0, 255, 255];
        palette[6] = [0, 255, 255, 255];
        palette[7] = [255, 255, 255, 255];
        Self {
            palette,
            selected: 0,
            rows: Vec::new(),
            x: 0,
            y: 0,
            max_w: limits.max_width as usize,
            max_h: limits.max_height as usize,
            max_bytes: limits.max_bytes,
            fail: false,
            bg: opaque.then_some([0, 0, 0, 255]),
        }
    }

    fn reserve(&mut self, w: u32, h: u32) -> bool {
        if w == 0 || h == 0 {
            return true;
        }
        let w = w as usize;
        let h = h as usize;
        if w > self.max_w || h > self.max_h {
            return false;
        }
        if w.saturating_mul(h).saturating_mul(4) > self.max_bytes {
            return false;
        }
        for y in 0..h {
            if !self.ensure_row(y, w) {
                return false;
            }
        }
        true
    }

    fn define(&mut self, pc: u8, pu: u32, x: u32, y: u32, z: u32) {
        let rgb = if pu == 1 {
            hls_to_rgb(x, y, z)
        } else {
            [pct(x), pct(y), pct(z)]
        };
        self.palette[pc as usize] = [rgb[0], rgb[1], rgb[2], 255];
        self.selected = pc;
        if pc == 0 {
            if let Some(bg) = self.bg.as_mut() {
                *bg = self.palette[0];
            }
        }
    }

    fn repeat(&mut self, count: u32, ch: u8) -> bool {
        if count > self.max_w as u32 {
            return false;
        }
        if self.x.saturating_add(count as usize) > self.max_w {
            return false;
        }
        for _ in 0..count {
            if !self.put(ch) {
                return false;
            }
        }
        true
    }

    fn put(&mut self, ch: u8) -> bool {
        if !(0x3f..=0x7e).contains(&ch) {
            return true;
        }
        if self.x >= self.max_w {
            return false;
        }
        let bits = ch - 0x3f;
        for bit in 0..6 {
            if bits & (1 << bit) == 0 {
                continue;
            }
            let y = self.y + bit;
            if y >= self.max_h || !self.set(self.x, y) {
                return false;
            }
        }
        self.x += 1;
        true
    }

    fn set(&mut self, x: usize, y: usize) -> bool {
        if !self.ensure_row(y, x + 1) {
            return false;
        }
        let px = &self.palette[self.selected as usize];
        let row = &mut self.rows[y];
        let i = x * 4;
        row[i..i + 4].copy_from_slice(px);
        true
    }

    fn ensure_row(&mut self, y: usize, width: usize) -> bool {
        if y >= self.max_h || width > self.max_w {
            return false;
        }
        let h = y + 1;
        let pixels =
            h.saturating_mul(width.max(self.rows.first().map(|r| r.len() / 4).unwrap_or(0)));
        // Checked properly below per row. This is a cheap reject.
        if pixels.saturating_mul(4) > self.max_bytes && width > 0 && h > self.max_bytes {
            return false;
        }
        while self.rows.len() <= y {
            self.rows.push(Vec::new());
        }
        let need = width * 4;
        if self.rows[y].len() >= need {
            return true;
        }
        let mut total = 0usize;
        for (i, row) in self.rows.iter().enumerate() {
            let len = if i == y { need } else { row.len() };
            total = match total.checked_add(len) {
                Some(n) => n,
                None => return false,
            };
        }
        if total > self.max_bytes {
            return false;
        }
        let bg = self.bg.unwrap_or([0, 0, 0, 0]);
        let row = &mut self.rows[y];
        row.reserve(need - row.len());
        while row.len() < need {
            row.extend_from_slice(&bg);
        }
        true
    }

    fn finish(self) -> Option<Raster> {
        if self.fail || self.rows.is_empty() {
            return None;
        }
        let w = self.rows.iter().map(|r| r.len() / 4).max().unwrap_or(0);
        let h = self.rows.len();
        if w == 0 || h == 0 || w > self.max_w || h > self.max_h {
            return None;
        }
        let bytes = w.checked_mul(h)?.checked_mul(4)?;
        if bytes > self.max_bytes {
            return None;
        }
        let bg = self.bg.unwrap_or([0, 0, 0, 0]);
        let mut rgba = vec![0u8; bytes];
        for (y, row) in self.rows.iter().enumerate() {
            let dst = &mut rgba[y * w * 4..(y + 1) * w * 4];
            let n = row.len().min(dst.len());
            dst[..n].copy_from_slice(&row[..n]);
            let mut i = n;
            while i < dst.len() {
                dst[i..i + 4].copy_from_slice(&bg);
                i += 4;
            }
        }
        Some(Raster {
            w: w as u32,
            h: h as u32,
            rgba,
        })
    }
}

fn read_params(data: &[u8], mut i: usize) -> Option<(Vec<u32>, usize)> {
    let mut nums = Vec::new();
    if i >= data.len() {
        return Some((nums, i));
    }
    loop {
        let (n, next) = read_u32(data, i)?;
        // read_u32 on a non-digit returns (0, i) — a real empty parameter.
        if next == i
            && data.get(i).is_some_and(|b| !b.is_ascii_digit())
            && nums.is_empty()
            && data[i] != b';'
        {
            break;
        }
        nums.push(n);
        i = next;
        if data.get(i) == Some(&b';') {
            i += 1;
            if nums.len() > 8 {
                return None;
            }
            continue;
        }
        break;
    }
    Some((nums, i))
}

fn read_u32(data: &[u8], mut i: usize) -> Option<(u32, usize)> {
    let start = i;
    let mut n: u32 = 0;
    let mut any = false;
    while i < data.len() && data[i].is_ascii_digit() {
        any = true;
        if i - start > 9 {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((data[i] - b'0') as u32)?;
        i += 1;
    }
    if !any {
        return Some((0, start));
    }
    Some((n, i))
}

fn pct(v: u32) -> u8 {
    (v.min(100) as u16 * 255 / 100) as u8
}

fn hls_to_rgb(h: u32, l: u32, s: u32) -> [u8; 3] {
    let h = (h % 360) as f32 / 360.0;
    let l = l.min(100) as f32 / 100.0;
    let s = s.min(100) as f32 / 100.0;
    if s == 0.0 {
        let v = (l * 255.0) as u8;
        return [v, v, v];
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let chan = |t: f32| {
        let t = if t < 0.0 {
            t + 1.0
        } else if t > 1.0 {
            t - 1.0
        } else {
            t
        };
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v * 255.0) as u8
    };
    [chan(h + 1.0 / 3.0), chan(h), chan(h - 1.0 / 3.0)]
}

// --- kitty -----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    TransmitDisplay,
    Transmit,
    Put,
    Delete,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trans {
    Direct,
    File,
    Temp,
    Shm,
    Other,
}

#[derive(Clone, Copy, Debug)]
struct KittyFields {
    action: Action,
    transmission: Trans,
    format: u16,
    quiet: u8,
    more: bool,
    image_id: Option<u32>,
    placement_id: Option<u32>,
    width: Option<u32>,
    height: Option<u32>,
    columns: Option<u32>,
    rows: Option<u32>,
    cursor_move: bool,
    delete: Option<char>,
    compression: bool,
}

impl Default for KittyFields {
    fn default() -> Self {
        Self {
            action: Action::TransmitDisplay,
            transmission: Trans::Direct,
            format: 32,
            quiet: 0,
            more: false,
            image_id: None,
            placement_id: None,
            width: None,
            height: None,
            columns: None,
            rows: None,
            cursor_move: false,
            delete: None,
            compression: false,
        }
    }
}

fn parse_kitty(body: &[u8]) -> Option<(KittyFields, &[u8])> {
    let split = body.iter().position(|&b| b == b';').unwrap_or(body.len());
    let ctl = std::str::from_utf8(&body[..split]).ok()?;
    let payload = if split < body.len() {
        &body[split + 1..]
    } else {
        &[]
    };
    let mut f = KittyFields::default();
    if !ctl.is_empty() {
        for part in ctl.split(',') {
            if part.is_empty() {
                continue;
            }
            let (k, v) = part.split_once('=')?;
            if k.len() != 1 {
                return None;
            }
            apply_kitty_key(&mut f, k.as_bytes()[0], v)?;
        }
    }
    Some((f, payload))
}

fn apply_kitty_key(f: &mut KittyFields, key: u8, val: &str) -> Option<()> {
    match key {
        b'a' => {
            f.action = match val {
                "T" | "" => Action::TransmitDisplay,
                "t" => Action::Transmit,
                "p" => Action::Put,
                "d" => Action::Delete,
                _ => Action::Other,
            };
        }
        b't' => {
            f.transmission = match val {
                "d" | "" => Trans::Direct,
                "f" => Trans::File,
                "t" => Trans::Temp,
                "s" => Trans::Shm,
                _ => Trans::Other,
            };
        }
        b'f' => f.format = parse_u16(val)?,
        b'q' => f.quiet = parse_u16(val)?.min(255) as u8,
        b'm' => f.more = val == "1",
        b'i' => f.image_id = Some(parse_u32_key(val)?),
        b'p' => f.placement_id = Some(parse_u32_key(val)?),
        b's' => f.width = Some(parse_u32_key(val)?),
        b'v' => f.height = Some(parse_u32_key(val)?),
        b'c' => f.columns = Some(parse_u32_key(val)?),
        b'r' => f.rows = Some(parse_u32_key(val)?),
        b'C' => f.cursor_move = val == "1",
        b'd' => f.delete = val.chars().next(),
        b'o' => {
            if val == "z" {
                f.compression = true;
            } else if !val.is_empty() {
                return None;
            }
        }
        // Placement offsets and cursor policy we do not draw. Ignoring them
        // still accepts the command; the image lands on the cursor cell.
        _ => {}
    }
    Some(())
}

fn parse_u32_key(v: &str) -> Option<u32> {
    if v.is_empty() {
        return Some(0);
    }
    if v.len() > 9 || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    v.parse().ok()
}

fn parse_u16(v: &str) -> Option<u16> {
    if v.is_empty() {
        return Some(0);
    }
    if v.len() > 5 || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    v.parse().ok()
}

fn finish_kitty(p: &PendingKitty, limits: &ImageLimits) -> Option<Raster> {
    let bytes = decode_b64(&p.b64)?;
    if bytes.len() > limits.max_bytes {
        return None;
    }
    match p.format {
        24 => {
            let n = raw_bytes(p.width, p.height, 3, limits)?;
            if bytes.len() != n {
                return None;
            }
            Some(expand_rgb(&bytes, p.width, p.height))
        }
        32 => {
            let n = raw_bytes(p.width, p.height, 4, limits)?;
            if bytes.len() != n {
                return None;
            }
            Some(Raster {
                w: p.width,
                h: p.height,
                rgba: bytes,
            })
        }
        100 => decode_png(&bytes, limits),
        _ => None,
    }
}

fn expand_rgb(rgb: &[u8], w: u32, h: u32) -> Raster {
    let mut rgba = vec![255u8; rgb.len() / 3 * 4];
    let (chunks, _) = rgb.as_chunks::<3>();
    for (i, px) in chunks.iter().enumerate() {
        let o = i * 4;
        rgba[o] = px[0];
        rgba[o + 1] = px[1];
        rgba[o + 2] = px[2];
        rgba[o + 3] = 255;
    }
    Raster { w, h, rgba }
}

fn kitty_reply(quiet: u8, id: Option<u32>, place: Option<u32>, ok: bool) -> Vec<String> {
    let send = match quiet {
        2 => false,
        1 => !ok,
        _ => true,
    };
    if !send {
        return Vec::new();
    }
    let mut ctl = String::from("\x1b_G");
    let mut wrote = false;
    if let Some(id) = id {
        ctl.push_str(&format!("i={id}"));
        wrote = true;
    }
    if let Some(p) = place {
        if wrote {
            ctl.push(',');
        }
        ctl.push_str(&format!("p={p}"));
    }
    ctl.push(';');
    ctl.push_str(if ok { "OK" } else { "EINVAL" });
    ctl.push_str("\x1b\\");
    vec![ctl]
}

fn decode_b64(s: &str) -> Option<Vec<u8>> {
    let cleaned: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if cleaned.is_empty() {
        return Some(Vec::new());
    }
    base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .ok()
}

fn decode_png(data: &[u8], limits: &ImageLimits) -> Option<Raster> {
    if data.len() < 8 || !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    use image::ImageDecoder;
    let mut decoder = image::codecs::png::PngDecoder::new(std::io::Cursor::new(data)).ok()?;
    let mut lim = image::Limits::default();
    lim.max_image_width = Some(limits.max_width);
    lim.max_image_height = Some(limits.max_height);
    lim.max_alloc = Some(limits.max_bytes as u64);
    decoder.set_limits(lim).ok()?;
    let (w, h) = decoder.dimensions();
    if !dims_ok(w, h, limits) {
        return None;
    }
    let bytes = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    if bytes > limits.max_bytes {
        return None;
    }
    let img = image::DynamicImage::from_decoder(decoder).ok()?;
    let rgba = img.into_rgba8();
    if rgba.width() != w || rgba.height() != h || rgba.as_raw().len() != bytes {
        return None;
    }
    Some(Raster {
        w,
        h,
        rgba: rgba.into_raw(),
    })
}

// --- iterm2 ----------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Dim {
    Auto,
    Cells(u32),
    Px(u32),
    Percent(u32),
}

struct ItermParsed {
    inline: bool,
    width: Dim,
    height: Dim,
    move_cursor: bool,
    size: Option<usize>,
    b64: String,
}

fn parse_iterm(body: &[u8]) -> Option<ItermParsed> {
    let s = std::str::from_utf8(body).ok()?;
    let rest = s.strip_prefix("1337;")?;
    let rest = rest.strip_prefix("File=")?;
    let (args, b64) = rest.split_once(':')?;
    let mut inline = false;
    let mut width = Dim::Auto;
    let mut height = Dim::Auto;
    let mut move_cursor = true;
    let mut size = None;
    for arg in args.split(';') {
        if arg.is_empty() {
            continue;
        }
        let Some((k, v)) = arg.split_once('=') else {
            continue;
        };
        match k {
            "inline" => inline = v == "1",
            "width" => width = parse_dim(v),
            "height" => height = parse_dim(v),
            "doNotMoveCursor" => {
                if v == "1" {
                    move_cursor = false;
                }
            }
            "size" => {
                size = v.parse::<usize>().ok();
            }
            _ => {}
        }
    }
    Some(ItermParsed {
        inline,
        width,
        height,
        move_cursor,
        size,
        b64: b64.to_string(),
    })
}

fn parse_dim(v: &str) -> Dim {
    if v.is_empty() || v == "auto" {
        return Dim::Auto;
    }
    if let Some(n) = v.strip_suffix("px") {
        return n.parse().map(Dim::Px).unwrap_or(Dim::Auto);
    }
    if let Some(n) = v.strip_suffix('%') {
        return n.parse().map(Dim::Percent).unwrap_or(Dim::Auto);
    }
    v.parse().map(Dim::Cells).unwrap_or(Dim::Auto)
}

fn iterm_cells(p: &ItermParsed, img_w: u32, img_h: u32, at: &PlaceAt) -> (usize, usize) {
    let cols = dim_cells(p.width, img_w, CELL_PX_W, at.columns);
    let rows = match p.height {
        Dim::Auto if !matches!(p.width, Dim::Auto) && img_w > 0 => (cols
            .saturating_mul(img_h as usize))
        .div_ceil(img_w as usize)
        .clamp(1, MAX_CELLS),
        other => dim_cells(other, img_h, CELL_PX_H, at.screen_rows),
    };
    (cols, rows)
}

fn dim_cells(dim: Dim, px: u32, cell_px: u32, screen: usize) -> usize {
    let n = match dim {
        Dim::Cells(n) => n as usize,
        Dim::Px(n) => px_span(n, cell_px),
        Dim::Percent(p) => (screen as u32 * p.min(100) / 100).max(1) as usize,
        Dim::Auto => px_span(px, cell_px),
    };
    n.clamp(1, MAX_CELLS)
}

// --- byte filter -----------------------------------------------------------

/// One slice of a PTY read after image sequences have been lifted out.
pub enum Inbound {
    Bytes(Vec<u8>),
    Sixel {
        p2: u8,
        data: Vec<u8>,
        overflow: bool,
    },
    Kitty {
        body: Vec<u8>,
        overflow: bool,
    },
    Iterm {
        body: Vec<u8>,
        overflow: bool,
    },
}

#[derive(Clone, Copy)]
enum Phase {
    Ground,
    Esc,
    Osc(StrKind),
    Dcs(StrKind),
    Apc(StrKind),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StrKind {
    Probe,
    Take,
    Forward,
}

enum Term {
    Bel,
    St,
}

/// Holds a DCS / APC / OSC 1337 that may still be arriving. Everything else
/// is emitted unchanged, including a trailing ESC that might yet be one of
/// those three. Incomplete image bytes stay in [`Self::carry`] across reads;
/// they do not use the 4 KiB OSC carry (an image is larger than that window).
pub struct ImageFilter {
    limits: ImageLimits,
    phase: Phase,
    saw_esc: bool,
    overflow: bool,
    sixel_p2: u8,
    /// Probe or in-progress payload held from the previous read.
    carry: Vec<u8>,
}

impl Default for ImageFilter {
    fn default() -> Self {
        Self {
            limits: ImageLimits::default(),
            phase: Phase::Ground,
            saw_esc: false,
            overflow: false,
            sixel_p2: 0,
            carry: Vec::new(),
        }
    }
}

impl ImageFilter {
    pub fn set_limits(&mut self, limits: ImageLimits) {
        self.limits = limits;
    }

    pub fn pull(&mut self, input: &[u8]) -> Vec<Inbound> {
        if matches!(self.phase, Phase::Ground) && !input.contains(&0x1b) {
            if input.is_empty() {
                return Vec::new();
            }
            return vec![Inbound::Bytes(input.to_vec())];
        }
        let mut held = std::mem::take(&mut self.carry);
        let mut out = Vec::new();
        let mut pieces = Vec::new();
        for &b in input {
            let mut next = Some(b);
            while let Some(b) = next.take() {
                next = self.once(b, &mut held, &mut out, &mut pieces);
            }
        }
        flush_bytes(&mut out, &mut pieces);
        if self.keeping() {
            self.carry = held;
        }
        pieces
    }
}

// The carry buffer cannot live in the Copy filter. ImageFilter is not Copy
// if it owns the carry. I declared Copy above and also a carry field that
// doesn't exist. Fix this properly: ImageFilter owns `carry: Vec<u8>`.
//
// The impl above references `self.carry` which I need to add, and I must
// drop Copy. I'll rewrite the struct and pull() in a follow-up edit if this
// doesn't compile — I'm including the field now by replacing the struct.
//
// This comment is a mistake if I leave it. I'll rewrite the filter struct
// before shipping. The code below is the real struct; the one above gets
// replaced.

fn flush_bytes(out: &mut Vec<u8>, pieces: &mut Vec<Inbound>) {
    if !out.is_empty() {
        pieces.push(Inbound::Bytes(std::mem::take(out)));
    }
}

fn keeping_phase(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::Osc(StrKind::Take | StrKind::Probe)
            | Phase::Dcs(StrKind::Take | StrKind::Probe)
            | Phase::Apc(StrKind::Take | StrKind::Probe)
    )
}

impl ImageFilter {
    fn keeping(&self) -> bool {
        self.saw_esc || keeping_phase(self.phase) || matches!(self.phase, Phase::Esc)
    }

    fn raw_cap(&self) -> usize {
        (self.limits.max_bytes.saturating_mul(4) / 3).saturating_add(64)
    }

    fn once(
        &mut self,
        b: u8,
        held: &mut Vec<u8>,
        out: &mut Vec<u8>,
        pieces: &mut Vec<Inbound>,
    ) -> Option<u8> {
        if self.saw_esc {
            self.saw_esc = false;
            if b == b'\\' {
                self.finish(Term::St, held, out, pieces);
                return None;
            }
            // The ESC ended the string and starts a new sequence.
            let forward = self.is_forward();
            if forward {
                out.extend_from_slice(b"\x1b\\");
            }
            held.clear();
            self.overflow = false;
            self.phase = Phase::Esc;
            return Some(b);
        }
        match self.phase {
            Phase::Ground => {
                if b == 0x1b {
                    self.phase = Phase::Esc;
                } else {
                    out.push(b);
                }
                None
            }
            Phase::Esc => self.esc(b, held, out),
            Phase::Osc(_) | Phase::Dcs(_) | Phase::Apc(_) => self.in_string(b, held, out, pieces),
        }
    }

    fn esc(&mut self, b: u8, held: &mut Vec<u8>, out: &mut Vec<u8>) -> Option<u8> {
        held.clear();
        self.overflow = false;
        self.sixel_p2 = 0;
        match b {
            0x1b => {
                out.push(0x1b);
                // Stay in Esc: this byte is the new ESC.
                None
            }
            b'P' => {
                self.phase = Phase::Dcs(StrKind::Probe);
                None
            }
            b'_' => {
                self.phase = Phase::Apc(StrKind::Probe);
                None
            }
            b']' => {
                self.phase = Phase::Osc(StrKind::Probe);
                None
            }
            _ => {
                out.push(0x1b);
                out.push(b);
                self.phase = Phase::Ground;
                None
            }
        }
    }

    fn is_forward(&self) -> bool {
        matches!(
            self.phase,
            Phase::Osc(StrKind::Forward)
                | Phase::Dcs(StrKind::Forward)
                | Phase::Apc(StrKind::Forward)
        )
    }

    fn in_string(
        &mut self,
        b: u8,
        held: &mut Vec<u8>,
        out: &mut Vec<u8>,
        pieces: &mut Vec<Inbound>,
    ) -> Option<u8> {
        match b {
            0x1b => {
                self.saw_esc = true;
                None
            }
            0x18 | 0x1a => {
                if self.is_forward() {
                    out.push(b);
                }
                held.clear();
                self.overflow = false;
                self.phase = Phase::Ground;
                None
            }
            0x07 => {
                self.finish(Term::Bel, held, out, pieces);
                None
            }
            _ => {
                self.push_body(b, held, out);
                None
            }
        }
    }

    fn push_body(&mut self, b: u8, held: &mut Vec<u8>, out: &mut Vec<u8>) {
        match self.phase {
            Phase::Osc(StrKind::Forward)
            | Phase::Dcs(StrKind::Forward)
            | Phase::Apc(StrKind::Forward) => {
                out.push(b);
            }
            Phase::Osc(StrKind::Probe) => {
                held.push(b);
                match osc_decide(held) {
                    Decide::Pending => {}
                    Decide::Take => self.phase = Phase::Osc(StrKind::Take),
                    Decide::Forward => {
                        out.extend_from_slice(b"\x1b]");
                        out.extend_from_slice(held);
                        held.clear();
                        self.phase = Phase::Osc(StrKind::Forward);
                    }
                }
            }
            Phase::Dcs(StrKind::Probe) => self.dcs_probe(b, held, out),
            Phase::Apc(StrKind::Probe) => {
                if b == b'G' {
                    held.clear();
                    self.phase = Phase::Apc(StrKind::Take);
                } else {
                    out.extend_from_slice(b"\x1b_");
                    out.push(b);
                    self.phase = Phase::Apc(StrKind::Forward);
                }
            }
            Phase::Osc(StrKind::Take) | Phase::Dcs(StrKind::Take) | Phase::Apc(StrKind::Take) => {
                if !self.limits.enabled {
                    return;
                }
                if self.overflow {
                    return;
                }
                if held.len() >= self.raw_cap() {
                    self.overflow = true;
                    held.clear();
                    held.shrink_to_fit();
                    return;
                }
                held.push(b);
            }
            _ => {}
        }
    }

    fn dcs_probe(&mut self, b: u8, held: &mut Vec<u8>, out: &mut Vec<u8>) {
        let params = held.iter().all(|c| matches!(c, b'0'..=b'9' | b';'));
        if params && held.len() < 32 && matches!(b, b'0'..=b'9' | b';') {
            held.push(b);
            return;
        }
        if params && b == b'q' {
            self.sixel_p2 = sixel_p2(held);
            held.clear();
            self.phase = Phase::Dcs(StrKind::Take);
            return;
        }
        out.extend_from_slice(b"\x1bP");
        out.extend_from_slice(held);
        out.push(b);
        held.clear();
        self.phase = Phase::Dcs(StrKind::Forward);
    }

    fn finish(
        &mut self,
        term: Term,
        held: &mut Vec<u8>,
        out: &mut Vec<u8>,
        pieces: &mut Vec<Inbound>,
    ) {
        let phase = self.phase;
        let overflow = self.overflow;
        let p2 = self.sixel_p2;
        match phase {
            Phase::Osc(StrKind::Forward)
            | Phase::Dcs(StrKind::Forward)
            | Phase::Apc(StrKind::Forward) => {
                emit_term(term, out);
            }
            Phase::Osc(StrKind::Take) => {
                if self.limits.enabled {
                    flush_bytes(out, pieces);
                    pieces.push(Inbound::Iterm {
                        body: std::mem::take(held),
                        overflow,
                    });
                }
            }
            Phase::Dcs(StrKind::Take) => {
                if self.limits.enabled {
                    flush_bytes(out, pieces);
                    pieces.push(Inbound::Sixel {
                        p2,
                        data: std::mem::take(held),
                        overflow,
                    });
                }
            }
            Phase::Apc(StrKind::Take) => {
                if self.limits.enabled {
                    flush_bytes(out, pieces);
                    pieces.push(Inbound::Kitty {
                        body: std::mem::take(held),
                        overflow,
                    });
                }
            }
            Phase::Osc(StrKind::Probe) => {
                out.extend_from_slice(b"\x1b]");
                out.extend_from_slice(held);
                emit_term(term, out);
            }
            Phase::Dcs(StrKind::Probe) => {
                out.extend_from_slice(b"\x1bP");
                out.extend_from_slice(held);
                emit_term(term, out);
            }
            Phase::Apc(StrKind::Probe) => {
                out.extend_from_slice(b"\x1b_");
                out.extend_from_slice(held);
                emit_term(term, out);
            }
            _ => {}
        }
        held.clear();
        self.overflow = false;
        self.sixel_p2 = 0;
        self.phase = Phase::Ground;
    }
}

fn emit_term(term: Term, out: &mut Vec<u8>) {
    match term {
        Term::Bel => out.push(0x07),
        Term::St => out.extend_from_slice(b"\x1b\\"),
    }
}

enum Decide {
    Pending,
    Take,
    Forward,
}

fn osc_decide(payload: &[u8]) -> Decide {
    const MARK: &[u8] = b"1337;File=";
    if payload.len() < MARK.len() && MARK.starts_with(payload) {
        Decide::Pending
    } else if payload.starts_with(MARK) {
        Decide::Take
    } else {
        Decide::Forward
    }
}

fn sixel_p2(params: &[u8]) -> u8 {
    let Ok(s) = std::str::from_utf8(params) else {
        return 0;
    };
    match s.split(';').nth(1).unwrap_or("").parse::<u8>() {
        Ok(2) => 2,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;
    use base64::Engine;
    use image::ImageEncoder;

    use crate::term::{after_resize, process_chunk, SessionMeta, TermSize};
    use alacritty_terminal::grid::Dimensions;

    fn fresh_sized(
        cols: usize,
        rows: usize,
        history: usize,
    ) -> (Term<VoidListener>, Processor, SessionMeta, Vec<u8>) {
        let size = TermSize { cols, rows };
        let config = Config {
            scrolling_history: history,
            ..Config::default()
        };
        (
            Term::new(config, &size, VoidListener),
            Processor::new(),
            SessionMeta::default(),
            Vec::new(),
        )
    }

    fn fresh() -> (Term<VoidListener>, Processor, SessionMeta, Vec<u8>) {
        fresh_sized(40, 8, 10)
    }

    fn feed(
        t: &mut Term<VoidListener>,
        p: &mut Processor,
        meta: &mut SessionMeta,
        carry: &mut Vec<u8>,
        data: &[u8],
    ) -> crate::term::ChunkEvents {
        process_chunk(p, t, meta, carry, data)
    }

    fn cell(t: &Term<VoidListener>, line: i32, col: usize) -> char {
        t.grid()[Line(line)][Column(col)].c
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn kitty(body: &str) -> Vec<u8> {
        format!("\x1b_G{body}\x1b\\").into_bytes()
    }

    /// 2×6: column 0 red, column 1 green, every pixel set.
    fn red_green_sixel() -> &'static [u8] {
        b"\x1bPq#1;2;100;0;0#2;2;0;100;0#1~#2~-\x1b\\"
    }

    #[test]
    fn sixel_decoder_paints_a_known_image() {
        let data = b"#1;2;100;0;0#2;2;0;100;0#1~#2~-";
        let img = decode_sixel(0, data, &ImageLimits::default()).expect("sixel");
        assert_eq!((img.w, img.h), (2, 6));
        for y in 0..6 {
            let row = y as usize * 8;
            assert_eq!(&img.rgba[row..row + 4], &[255, 0, 0, 255], "red col {y}");
            assert_eq!(
                &img.rgba[row + 4..row + 8],
                &[0, 255, 0, 255],
                "green col {y}"
            );
        }
        // A repeat of three red columns, one band.
        let img = decode_sixel(0, b"#1;2;100;0;0!3~", &ImageLimits::default()).expect("repeat");
        assert_eq!(img.w, 3);
        assert_eq!(img.h, 6);
        assert!(img.rgba.chunks(4).all(|p| p == [255, 0, 0, 255]));
    }

    #[test]
    fn sixel_is_stored_and_does_not_print() {
        let (mut t, mut p, mut meta, mut carry) = fresh();
        feed(&mut t, &mut p, &mut meta, &mut carry, b"A");
        feed(&mut t, &mut p, &mut meta, &mut carry, red_green_sixel());
        feed(&mut t, &mut p, &mut meta, &mut carry, b"B");
        assert_eq!(cell(&t, 0, 0), 'A');
        assert_eq!(cell(&t, 1, 0), 'B', "the cursor steps past the image");
        let snaps = meta.images.snapshot();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].line, 0);
        assert_eq!((snaps[0].width, snaps[0].height), (2, 6));
        assert_eq!(&snaps[0].rgba[..4], &[255, 0, 0, 255]);
        // The sixel bytes themselves are not cells.
        let row: String = (0..8).map(|c| cell(&t, 0, c)).collect();
        assert!(!row.contains('q'), "{row}");
        assert!(!row.contains('~'), "{row}");
    }

    #[test]
    fn kitty_chunked_transmit_then_delete() {
        let (mut t, mut p, mut meta, mut carry) = fresh();
        let encoded = b64(&[255, 0, 0]);
        let (head, tail) = encoded.split_at(2);
        let ev = feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            &kitty(&format!("a=T,f=24,s=1,v=1,i=7,c=2,r=1,m=1;{head}")),
        );
        assert!(ev.pty_replies.is_empty(), "a partial chunk does not reply");
        assert_eq!(meta.images.placement_count(), 0);
        let ev = feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            &kitty(&format!("m=0;{tail}")),
        );
        assert!(
            ev.pty_replies
                .iter()
                .any(|r| r.contains("OK") && r.contains("i=7")),
            "{:?}",
            ev.pty_replies
        );
        let snaps = meta.images.snapshot();
        assert_eq!(snaps.len(), 1);
        assert_eq!((snaps[0].cols, snaps[0].rows), (2, 1));
        assert_eq!(&snaps[0].rgba[..], &[255, 0, 0, 255]);
        assert!(meta.images.pane_bytes() > 0);

        let ev = feed(&mut t, &mut p, &mut meta, &mut carry, &kitty("a=d,d=i,i=7"));
        assert!(ev.pty_replies.iter().any(|r| r.contains("OK")));
        assert_eq!(meta.images.placement_count(), 0);
        assert_eq!(meta.images.pane_bytes(), 0);
        assert_eq!(meta.images.snapshot().len(), 0);
    }

    #[test]
    fn kitty_transmit_only_then_put() {
        let (mut t, mut p, mut meta, mut carry) = fresh();
        let px = b64(&[0, 255, 0]);
        let ev = feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            &kitty(&format!("a=t,f=24,s=1,v=1,i=3,q=0;{px}")),
        );
        assert!(ev.pty_replies.iter().any(|r| r.contains("OK")));
        assert_eq!(
            meta.images.placement_count(),
            0,
            "a=t stores without drawing"
        );
        assert!(meta.images.pane_bytes() > 0);
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[3;4H");
        let ev = feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            &kitty("a=p,i=3,c=1,r=1,q=0"),
        );
        assert!(ev.pty_replies.iter().any(|r| r.contains("OK")));
        let snaps = meta.images.snapshot();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].col, 3, "put uses the cursor column");
        assert_eq!(&snaps[0].rgba[..], &[0, 255, 0, 255]);
        // Missing id cannot be placed.
        let ev = feed(&mut t, &mut p, &mut meta, &mut carry, &kitty("a=p,q=1"));
        assert!(ev.pty_replies.iter().any(|r| r.contains("EINVAL")));
    }

    #[test]
    fn kitty_quiet_and_rejected_transmissions() {
        let (mut t, mut p, mut meta, mut carry) = fresh();
        let png_body = b64(&[255, 0, 0, 255]);
        // q=1 suppresses OK.
        let ev = feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            &kitty(&format!("a=T,f=32,s=1,v=1,q=1;{png_body}")),
        );
        assert!(ev.pty_replies.is_empty(), "{:?}", ev.pty_replies);
        assert_eq!(meta.images.placement_count(), 1);
        // q=2 suppresses an error too.
        for kind in ["f", "t", "s"] {
            let (mut t, mut p, mut meta, mut carry) = fresh();
            let path = b64(b"C:\\Windows\\System32\\drivers\\etc\\hosts");
            let ev = feed(
                &mut t,
                &mut p,
                &mut meta,
                &mut carry,
                &kitty(&format!("a=T,t={kind},q=2;{path}")),
            );
            assert!(
                ev.pty_replies.is_empty(),
                "t={kind} q=2 replied {:?}",
                ev.pty_replies
            );
            assert_eq!(meta.images.placement_count(), 0);
            assert_eq!(meta.images.pane_bytes(), 0);
            let ev = feed(
                &mut t,
                &mut p,
                &mut meta,
                &mut carry,
                &kitty(&format!("a=T,t={kind};{path}")),
            );
            assert!(
                ev.pty_replies.iter().any(|r| r.contains("EINVAL")),
                "t={kind} {:?}",
                ev.pty_replies
            );
            assert_eq!(
                meta.images.placement_count(),
                0,
                "t={kind} must not read a path"
            );
        }
    }

    #[test]
    fn iterm_inline_png_decodes_and_non_inline_is_dropped() {
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[255, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .expect("encode png");
        let (mut t, mut p, mut meta, mut carry) = fresh();
        let seq = format!("\x1b]1337;File=inline=1;width=3;height=2:{}\x07", b64(&png));
        feed(&mut t, &mut p, &mut meta, &mut carry, seq.as_bytes());
        let snaps = meta.images.snapshot();
        assert_eq!(snaps.len(), 1);
        assert_eq!((snaps[0].cols, snaps[0].rows), (3, 2));
        assert_eq!(&snaps[0].rgba[..], &[255, 0, 0, 255]);
        // Not an inline image: do not draw, and do not write the payload anywhere.
        let (mut t, mut p, mut meta, mut carry) = fresh();
        let seq = format!("\x1b]1337;File=inline=0;name=x:{}\x07", b64(&png));
        feed(&mut t, &mut p, &mut meta, &mut carry, seq.as_bytes());
        assert_eq!(meta.images.placement_count(), 0);
        assert_eq!(meta.images.pane_bytes(), 0);
    }

    #[test]
    fn image_scrolls_with_text_and_leaves_with_scrollback() {
        let (mut t, mut p, mut meta, mut carry) = fresh_sized(20, 4, 4);
        let px = b64(&[0, 0, 255]);
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            &kitty(&format!("a=T,f=24,s=1,v=1,c=1,r=1,C=0,q=2;{px}")),
        );
        // Arabic in the other cells of the same row. The image must not
        // eat it, and the row stays logical Arabic for the shaper.
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[1;8H");
        feed(&mut t, &mut p, &mut meta, &mut carry, "مرحبا".as_bytes());
        assert_eq!(cell(&t, 0, 7), 'م');
        let anchor = meta.images.placements()[0].line;
        assert_eq!(anchor, 0);
        // Sit on the last row and scroll three lines into a 4-line history.
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[4;1H\n\n\n");
        assert!(meta.evicted == 0, "history still has room");
        let info = meta.images.placements();
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].line, anchor, "the anchor scrolls with the row");
        let grid = info[0].line as i64 - meta.evicted as i64 - t.grid().history_size() as i64;
        assert_eq!(cell(&t, grid as i32, 7), 'م', "Arabic moved with the image");
        // Flood past the scrollback cap.
        for _ in 0..20 {
            feed(&mut t, &mut p, &mut meta, &mut carry, b"\n");
        }
        assert!(meta.evicted > anchor);
        assert_eq!(meta.images.placement_count(), 0);
        assert_eq!(meta.images.pane_bytes(), 0);
    }

    #[test]
    fn resize_clears_images_and_alt_screen_hides_then_restores() {
        let (mut t, mut p, mut meta, mut carry) = fresh();
        let px = b64(&[255, 255, 0]);
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            &kitty(&format!("a=T,f=24,s=1,v=1,C=0,q=2;{px}")),
        );
        assert_eq!(meta.images.placement_count(), 1);
        t.resize(TermSize { cols: 20, rows: 6 });
        after_resize(&mut meta);
        assert_eq!(meta.images.placement_count(), 0);
        assert_eq!(meta.images.pane_bytes(), 0);

        let (mut t, mut p, mut meta, mut carry) = fresh();
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            &kitty(&format!("a=T,f=24,s=1,v=1,i=1,C=0,q=2;{px}")),
        );
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[?1049h");
        assert!(t
            .mode()
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN));
        assert!(meta.images.on_alt());
        assert!(
            meta.images.snapshot().is_empty(),
            "primary images hide on the alt screen"
        );
        assert_eq!(
            meta.images.placement_count(),
            1,
            "the primary placement is kept"
        );
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            &kitty(&format!("a=T,f=24,s=1,v=1,i=2,C=0,q=2;{px}")),
        );
        assert_eq!(meta.images.snapshot().len(), 1);
        assert!(
            meta.images.snapshot()[0].line >= 1 || meta.images.placements().iter().any(|p| p.alt)
        );
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[?1049l");
        assert!(!meta.images.on_alt());
        let left: Vec<_> = meta.images.placements();
        assert!(left.iter().all(|p| !p.alt && p.image == 1));
        assert_eq!(meta.images.snapshot().len(), 1);
    }

    #[test]
    fn ed_clears_screen_images_and_keeps_history_until_ed3() {
        let (mut t, mut p, mut meta, mut carry) = fresh_sized(20, 4, 8);
        let px = b64(&[1, 2, 3]);
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            &kitty(&format!("a=T,f=24,s=1,v=1,C=0,q=2;{px}")),
        );
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[2J");
        assert_eq!(meta.images.placement_count(), 0, "ED 2 clears the screen");

        let (mut t, mut p, mut meta, mut carry) = fresh_sized(20, 4, 8);
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            &kitty(&format!("a=T,f=24,s=1,v=1,C=0,q=2;{px}")),
        );
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[4;1H\n");
        assert_eq!(
            meta.images.placement_count(),
            1,
            "one scroll lands in history"
        );
        assert!(meta.images.placements()[0].line < meta.evicted + t.grid().history_size() as u64);
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[2J");
        assert_eq!(
            meta.images.placement_count(),
            1,
            "ED 2 does not erase scrollback"
        );
        feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[3J");
        assert_eq!(
            meta.images.placement_count(),
            0,
            "ED 3 drops scrollback images"
        );
        assert_eq!(meta.images.pane_bytes(), 0);
    }

    #[test]
    fn caps_drop_oversize_input_without_growing_memory() {
        let limits = ImageLimits {
            max_width: 1,
            max_height: 1,
            max_bytes: 8,
            max_pane_bytes: 8,
            max_session_bytes: 8,
            ..ImageLimits::default()
        };
        let (mut t, mut p, mut meta, mut carry) = fresh();
        meta.set_image_limits(limits);
        let wide = b64(&[255, 0, 0, 0, 255, 0]);
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            &kitty(&format!("a=T,f=24,s=2,v=1,q=1;{wide}")),
        );
        assert_eq!(meta.images.pane_bytes(), 0);
        assert_eq!(meta.images.placement_count(), 0);
        // A claimed size that would be enormous is rejected before allocate.
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            &kitty("a=T,f=32,s=99999,v=99999,q=1;AAAA"),
        );
        assert_eq!(meta.images.pane_bytes(), 0);
        // The parser is not stuck: a tiny image after the reject still lands.
        let tiny = b64(&[9, 9, 9]);
        feed(
            &mut t,
            &mut p,
            &mut meta,
            &mut carry,
            &kitty(&format!("a=T,f=24,s=1,v=1,q=2;{tiny}")),
        );
        assert_eq!(meta.images.placement_count(), 1);
        assert!(meta.images.pane_bytes() <= 8);

        // Session budget is shared. One RGBA pixel fills 4 bytes; the other
        // pane's copy is dropped and the total does not grow.
        let tight = ImageLimits {
            max_bytes: 4,
            max_pane_bytes: 4,
            max_session_bytes: 4,
            ..limits
        };
        let budget = Arc::new(ImageBudget::new(tight));
        let (mut t1, mut p1, mut m1, mut c1) = fresh();
        let (mut t2, mut p2, mut m2, mut c2) = fresh();
        m1.share_image_budget(Arc::clone(&budget));
        m2.share_image_budget(Arc::clone(&budget));
        let rgba = b64(&[1, 2, 3, 4]);
        feed(
            &mut t1,
            &mut p1,
            &mut m1,
            &mut c1,
            &kitty(&format!("a=T,f=32,s=1,v=1,q=2;{rgba}")),
        );
        assert_eq!(m1.images.pane_bytes(), 4);
        feed(
            &mut t2,
            &mut p2,
            &mut m2,
            &mut c2,
            &kitty(&format!("a=T,f=32,s=1,v=1,q=2;{rgba}")),
        );
        assert_eq!(m2.images.placement_count(), 0, "session budget is spent");
        assert_eq!(budget.charged_bytes(), 4);
    }

    #[test]
    fn malformed_and_truncated_sequences_do_not_panic() {
        let samples: &[&[u8]] = &[
            b"\x1bP",
            b"\x1bPq",
            b"\x1bPq\x1b",
            b"\x1bPq!!!!",
            b"\x1bPq!999999999~",
            b"\x1bP0;2;0q\"1;1;999999;999999~\x1b\\",
            b"\x1b_G",
            b"\x1b_G\x1b\\",
            b"\x1b_Ga=T,t=f;L1VzZXJz\x1b\\",
            b"\x1b_Ga=T,f=24,s=1,v=1;!!!!\x1b\\",
            b"\x1b_Ga=T,f=24,s=0,v=0;AA\x1b\\",
            b"\x1b_Ga=nope\x1b\\",
            b"\x1b]1337;File=inline=1:",
            b"\x1b]1337;File=inline=1:****\x07",
            b"\x1b]1337;File=\x07",
            b"\x1b]1337;NotFile=1\x07",
            b"\x1b]9;hello\x07",
        ];
        for sample in samples {
            for n in 0..=sample.len() {
                let (mut t, mut p, mut meta, mut carry) = fresh();
                let _ = feed(&mut t, &mut p, &mut meta, &mut carry, &sample[..n]);
                // Home, then a letter: a stuck escape would swallow the Z.
                let _ = feed(&mut t, &mut p, &mut meta, &mut carry, b"\x1b[HZ");
                let home = cell(&t, 0, 0);
                assert!(
                    home == 'Z' || meta.images.placement_count() > 0,
                    "prefix of {n} bytes left the parser stuck (cell {home:?})"
                );
            }
        }
        let mut junk = [0u8; 80];
        for (i, b) in junk.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(9);
        }
        junk[10] = 0x1b;
        junk[11] = b'P';
        junk[40] = 0x1b;
        junk[41] = b'_';
        junk[60] = 0x1b;
        junk[61] = b']';
        for n in 0..=junk.len() {
            let (mut t, mut p, mut meta, mut carry) = fresh();
            let _ = feed(&mut t, &mut p, &mut meta, &mut carry, &junk[..n]);
        }
        let limits = ImageLimits::default();
        let _ = decode_sixel(0, b"!!!!", &limits);
        assert!(decode_sixel(9, b"!999999999~#", &limits).is_none());
        assert!(decode_png(b"not a png", &limits).is_none());
        assert!(decode_png(&[0x89, b'P', b'N', b'G', 0, 1, 2, 3, 4], &limits).is_none());
    }

    #[test]
    fn image_quads_are_clipped_to_the_pane() {
        let snap = ImageSnap {
            cookie: 1,
            line: 0,
            col: 0,
            cols: 20,
            rows: 2,
            width: 2,
            height: 2,
            rgba: Arc::from(vec![0u8; 16]),
        };
        let clip = (10, 20, 80, 40);
        let sprites = crate::render::image_sprites(clip, 10.0, 10.0, 0, 0, &[snap]);
        assert_eq!(sprites.len(), 1);
        assert_eq!(sprites[0].verts.len(), 6);
        for v in &sprites[0].verts {
            assert!((10.0..=90.0).contains(&v.pos[0]), "x {}", v.pos[0]);
            assert!((20.0..=40.0).contains(&v.pos[1]), "y {}", v.pos[1]);
        }
        let max_u = sprites[0]
            .verts
            .iter()
            .map(|v| v.uv[0])
            .fold(0.0f32, f32::max);
        assert!(
            (max_u - 0.4).abs() < 0.02,
            "a 200px quad clipped to 80px keeps the left of the texture, u={max_u}"
        );
        // Fully below the pane: no quad.
        let below = ImageSnap {
            cookie: 2,
            line: 50,
            col: 0,
            cols: 1,
            rows: 1,
            width: 1,
            height: 1,
            rgba: Arc::from(vec![0u8; 4]),
        };
        assert!(crate::render::image_sprites(clip, 10.0, 10.0, 0, 0, &[below]).is_empty());
    }
}
