//! A thin scrollbar beside each pane, in the padding or the split seam.
//!
//! The thumb is a pure function of how much history exists, how many lines
//! the viewport shows, and `display_offset` (0 is the live bottom, the same
//! direction as `Scroll::Delta`). `fraction` is an optional sub-line
//! remainder in `[0, 1)` added to that offset — the unit smooth scroll keeps
//! as its per-pane remainder. Zero, the default, leaves the thumb on the
//! whole line, so this module does not need to know about pixels.
//!
//! The bar is never drawn on a grid cell. It sits in the gutter just outside
//! the pane: the window padding on an outer edge (the default 10px holds the
//! default 8px bar), or the widened seam between side-by-side panes. It is
//! hidden when there is nothing to scroll, on the alternate screen, and when
//! `scrollbar.position` is `"hidden"`. A press on the bar scrolls; a press
//! on a cell, including the last column, is left to the program.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long a scroll or a hover keeps an auto-hidden bar on screen.
pub const IDLE: Duration = Duration::from_secs(1);

pub const DEFAULT_WIDTH: i32 = 8;
pub const MIN_WIDTH: i32 = 2;
pub const MAX_WIDTH: i32 = 24;
/// A long history must still leave a thumb you can grab.
pub const MIN_THUMB: i32 = 16;

/// Pixel rect: x, y, width, height. The same tuple the renderer uses.
pub type Rect = (i32, i32, i32, i32);

/// Which edge the bar sits on. The default is the right edge, in Arabic
/// and in English; the interface direction does not move it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Right,
    Left,
    Hidden,
}

/// `scrollbar` in config.json. Absent fields keep the defaults, and an
/// unknown position does too: a typo must not hide the bar or reject the file.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ScrollbarConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Hide the bar after [`IDLE`] with no scroll and no hover. Absent
    /// means on, so a still pane does not keep a strip over the last column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autohide: Option<bool>,
}

impl ScrollbarConfig {
    /// Untouched settings are not written back.
    pub fn is_default(&self) -> bool {
        self.position.is_none() && self.width.is_none() && self.autohide.is_none()
    }

    pub fn side(&self) -> Side {
        let Some(raw) = self.position.as_deref() else {
            return Side::Right;
        };
        match raw.trim().to_ascii_lowercase().as_str() {
            "left" => Side::Left,
            "hidden" => Side::Hidden,
            _ => Side::Right,
        }
    }

    pub fn width_px(&self) -> i32 {
        self.width
            .map(|w| (w as i32).clamp(MIN_WIDTH, MAX_WIDTH))
            .unwrap_or(DEFAULT_WIDTH)
    }

    /// `width_px` in physical pixels. `scale` is the window's DPI scale
    /// (1.0 at 100%). A non-positive or non-finite scale counts as 1.
    pub fn width_px_at(&self, scale: f32) -> i32 {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        ((self.width_px() as f32) * scale).round().max(1.0) as i32
    }

    pub fn autohide_on(&self) -> bool {
        self.autohide.unwrap_or(true)
    }
}

/// Where the viewport sits in the scrollback.
///
/// `display_offset` is whole lines toward older history. `fraction` is the
/// sub-line remainder in `[0, 1)` on top of it. [`Place::new`] leaves the
/// remainder at 0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Place {
    pub total: usize,
    pub viewport: usize,
    pub display_offset: usize,
    pub fraction: f32,
}

impl Place {
    pub fn new(total: usize, viewport: usize, display_offset: usize) -> Self {
        Self {
            total,
            viewport,
            display_offset,
            fraction: 0.0,
        }
    }

    pub fn with_fraction(mut self, fraction: f32) -> Self {
        self.fraction = fraction;
        self
    }

    /// Remainder in `[0, 1)`. `1` has already been carried into
    /// `display_offset` by the scroller, and NaN is not a position.
    pub fn fraction(self) -> f32 {
        if (0.0..1.0).contains(&self.fraction) {
            self.fraction
        } else {
            0.0
        }
    }

    /// Lines the thumb can travel. `None` when the viewport already shows
    /// every line: there is no scrollback, so there is no bar.
    pub fn max_offset(self) -> Option<usize> {
        if self.viewport == 0 || self.total <= self.viewport {
            None
        } else {
            Some(self.total - self.viewport)
        }
    }

    /// Thumb top and height inside a track that starts at `track_y`.
    /// Height is `track * viewport / total`, and at least `min_px` (the
    /// caller passes `max(16, bar width)`). Offset 0 pins the thumb to the
    /// bottom; the maximum offset pins it to the top.
    pub fn thumb(self, track_y: i32, track_h: i32, min_px: i32) -> Option<(i32, i32)> {
        let max_off = self.max_offset()?;
        if track_h <= 1 {
            return None;
        }
        let min_px = min_px.max(1).min(track_h - 1);
        let mut thumb_h =
            (track_h as f32 * self.viewport as f32 / self.total as f32).round() as i32;
        thumb_h = thumb_h.clamp(min_px, track_h - 1);
        let pos = (self.display_offset as f32 + self.fraction()).clamp(0.0, max_off as f32);
        let travel = (track_h - thumb_h) as f32;
        let y = if pos <= 0.0 {
            track_y + track_h - thumb_h
        } else if pos >= max_off as f32 {
            track_y
        } else {
            let bottom = (track_y + track_h - thumb_h) as f32;
            (bottom - (pos / max_off as f32) * travel).round() as i32
        };
        Some((y, thumb_h))
    }
}

/// Track and thumb, in window pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geom {
    pub track: Rect,
    pub thumb: Rect,
}

/// True when the two rects share any pixel. Empty rects do not.
pub fn intersects(a: Rect, b: Rect) -> bool {
    let (ax, ay, aw, ah) = a;
    let (bx, by, bw, bh) = b;
    aw > 0
        && ah > 0
        && bw > 0
        && bh > 0
        && ax < bx + bw
        && bx < ax + aw
        && ay < by + bh
        && by < ay + ah
}

/// Pixels of gutter on `side` of pane `index`: the seam until the next
/// pane, or the window padding when this pane is the outer one. `content`
/// is the cell area; `pad` is the padding outside it.
pub fn room_px(index: usize, panes: &[Rect], side: Side, content: Rect, pad: i32) -> i32 {
    let Some(&(x, y, w, h)) = panes.get(index) else {
        return 0;
    };
    if w <= 0 || h <= 0 {
        return 0;
    }
    let pad = pad.max(0);
    let overlaps = |pane: Rect| pane.1 < y + h && y < pane.1 + pane.3;
    match side {
        Side::Hidden => 0,
        Side::Right => {
            let edge = x + w;
            let mut limit = content.0 + content.2 + pad;
            for (i, pane) in panes.iter().copied().enumerate() {
                if i == index || pane.2 <= 0 || pane.3 <= 0 || !overlaps(pane) {
                    continue;
                }
                if pane.0 >= edge {
                    limit = limit.min(pane.0);
                }
            }
            (limit - edge).max(0)
        }
        Side::Left => {
            let mut limit = content.0 - pad;
            for (i, pane) in panes.iter().copied().enumerate() {
                if i == index || pane.2 <= 0 || pane.3 <= 0 || !overlaps(pane) {
                    continue;
                }
                if pane.0 + pane.2 <= x {
                    limit = limit.max(pane.0 + pane.2);
                }
            }
            (x - limit).max(0)
        }
    }
}

/// ConPTY columns for a grid of `grid_px`. The scrollbar is not in that
/// width, so it does not change the count.
pub fn grid_columns(grid_px: i32, cell_w: f32) -> usize {
    if cell_w <= 0.0 {
        return 10;
    }
    ((grid_px as f32 / cell_w) as usize).max(10)
}

/// The bar for one pane, drawn in the `room` pixels just outside `grid`.
/// `None` when it must not be drawn: no scrollback, the alternate screen, a
/// hidden position, or no gutter (the bar is never placed on a cell).
pub fn layout(
    grid: Rect,
    side: Side,
    width: i32,
    room: i32,
    place: Place,
    alt_screen: bool,
) -> Option<Geom> {
    // A 1px seam is the hairline divider, not a bar. Padding 0 leaves that
    // seam and no outer gutter, so nothing is drawn on either.
    if alt_screen || side == Side::Hidden || width <= 1 || room <= 1 {
        return None;
    }
    let (x, y, w, h) = grid;
    if w <= 1 || h <= 1 {
        return None;
    }
    let used = width.min(room);
    let min_thumb = MIN_THUMB.max(used);
    let (thumb_y, thumb_h) = place.thumb(y, h, min_thumb)?;
    let track_x = match side {
        Side::Left => x - used,
        Side::Right => x + w,
        Side::Hidden => return None,
    };
    let track = (track_x, y, used, h);
    let thumb = (track_x, thumb_y, used, thumb_h);
    if intersects(track, grid) || intersects(thumb, grid) {
        return None;
    }
    Some(Geom { track, thumb })
}

/// What a pointer on the bar is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Thumb,
    /// Toward older lines (the top of the track).
    Above,
    /// Toward the live bottom.
    Below,
}

fn contains((rx, ry, rw, rh): Rect, x: f64, y: f64) -> bool {
    rw > 0
        && rh > 0
        && x >= rx as f64
        && x < (rx + rw) as f64
        && y >= ry as f64
        && y < (ry + rh) as f64
}

/// `None` outside the track, so a click on a cell is not a scrollbar click.
pub fn hit(geom: &Geom, x: f64, y: f64) -> Option<Part> {
    if !contains(geom.track, x, y) {
        return None;
    }
    if contains(geom.thumb, x, y) {
        Some(Part::Thumb)
    } else if y < geom.thumb.1 as f64 {
        Some(Part::Above)
    } else {
        Some(Part::Below)
    }
}

/// One page, in lines, with the sign `Scroll::Delta` already uses: positive
/// moves into history. The thumb itself is a drag, not a page.
pub fn page_lines(part: Part, viewport: usize) -> Option<i32> {
    let page = i32::try_from(viewport.max(1)).unwrap_or(i32::MAX);
    match part {
        Part::Thumb => None,
        Part::Above => Some(page),
        Part::Below => Some(-page),
    }
}

/// `display_offset` for a thumb drag. `grab` is the pointer's distance below
/// the thumb's top at the press, so the thumb does not jump under the cursor.
/// The top of the track is the oldest line; the bottom is offset 0.
pub fn offset_from_pointer(geom: &Geom, pointer_y: f64, grab: f64, max_offset: usize) -> usize {
    if max_offset == 0 {
        return 0;
    }
    let travel = geom.track.3 - geom.thumb.3;
    if travel <= 0 {
        return 0;
    }
    let thumb_top = pointer_y - grab;
    let from_top = ((thumb_top - geom.track.1 as f64) / travel as f64).clamp(0.0, 1.0);
    let pos = (1.0 - from_top) * max_offset as f64;
    pos.round().clamp(0.0, max_offset as f64) as usize
}

/// True when the bar should be painted. Auto-hide keeps it up while the
/// pointer is on it, while it is dragged, and for [`IDLE`] after a scroll.
pub fn shown(autohide: bool, engaged: bool, last: Option<Instant>, now: Instant) -> bool {
    if !autohide || engaged {
        return true;
    }
    last.is_some_and(|t| now.saturating_duration_since(t) < IDLE)
}

/// A press that is dragging a thumb.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Drag {
    pub pane: u64,
    pub grab: f64,
}

/// Per-pane hover, drag, and the last time each pane scrolled. The window
/// owns one of these; the decisions above stay pure.
#[derive(Clone, Debug, Default)]
pub struct Bars {
    last: HashMap<u64, Instant>,
    seen: HashMap<u64, usize>,
    pub hover: Option<u64>,
    pub drag: Option<Drag>,
    painted: Vec<u64>,
}

/// What [`Bars::tick`] wants the event loop to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tick {
    pub redraw: bool,
    pub wake: Option<Instant>,
}

impl Bars {
    pub fn begin_frame(&mut self) {
        self.painted.clear();
    }

    pub fn mark_painted(&mut self, id: u64) {
        self.painted.push(id);
    }

    pub fn note(&mut self, id: u64, now: Instant) {
        self.last.insert(id, now);
    }

    pub fn last_of(&self, id: u64) -> Option<Instant> {
        self.last.get(&id).copied()
    }

    pub fn dragging(&self, id: u64) -> bool {
        self.drag.as_ref().is_some_and(|d| d.pane == id)
    }

    /// Remember `offset`. The first time a pane is seen is not a scroll;
    /// a later change is, and starts the auto-hide timer. Returns whether
    /// it changed.
    pub fn observe(&mut self, id: u64, offset: usize, now: Instant) -> bool {
        match self.seen.insert(id, offset) {
            Some(prev) if prev != offset => {
                self.last.insert(id, now);
                true
            }
            _ => false,
        }
    }

    /// Hover moved. Leaving the bar starts the idle timer from now.
    pub fn set_hover(&mut self, id: Option<u64>, now: Instant) {
        if self.hover == id {
            return;
        }
        if let Some(prev) = self.hover {
            self.last.insert(prev, now);
        }
        self.hover = id;
    }

    /// After a frame, hide a painted bar whose idle has passed, and wake
    /// when the next one will. Hover and drag hold the bar up.
    pub fn tick(&self, autohide: bool, now: Instant) -> Tick {
        if !autohide {
            return Tick {
                redraw: false,
                wake: None,
            };
        }
        let mut redraw = false;
        let mut wake: Option<Instant> = None;
        for id in &self.painted {
            if self.hover == Some(*id) || self.dragging(*id) {
                continue;
            }
            match self.last.get(id) {
                Some(t) => {
                    let due = *t + IDLE;
                    if now >= due {
                        redraw = true;
                    } else {
                        wake = Some(wake.map_or(due, |w| w.min(due)));
                    }
                }
                None => redraw = true,
            }
        }
        Tick { redraw, wake }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn place(total: usize, viewport: usize, offset: usize) -> Place {
        Place::new(total, viewport, offset)
    }

    fn bar(offset: usize) -> Geom {
        layout(
            (10, 20, 200, 100),
            Side::Right,
            8,
            10,
            place(100, 25, offset),
            false,
        )
        .expect("scrollback")
    }

    #[test]
    fn thumb_height_is_viewport_over_total_and_the_ends_pin() {
        let bottom = bar(0);
        // just outside the grid, in the padding: grid ends at x = 210
        assert_eq!(bottom.track, (210, 20, 8, 100));
        assert!(!intersects(bottom.track, (10, 20, 200, 100)));
        // 100 * 25/100 = 25, flush with the bottom of the track.
        assert_eq!(bottom.thumb, (210, 95, 8, 25));
        let top = bar(75);
        assert_eq!(top.thumb.1, 20, "the oldest line puts the thumb at the top");
        assert_eq!(top.thumb.3, 25);
        let mid = bar(37);
        assert!(mid.thumb.1 > top.thumb.1 && mid.thumb.1 < bottom.thumb.1);
    }

    #[test]
    fn no_scrollback_and_the_alternate_screen_and_hidden_draw_nothing() {
        let pane = (0, 0, 200, 100);
        let full = place(40, 40, 0);
        assert_eq!(layout(pane, Side::Right, 8, 10, full, false), None);
        assert_eq!(
            layout(pane, Side::Right, 8, 10, place(10, 40, 0), false),
            None
        );
        assert_eq!(
            layout(pane, Side::Right, 8, 10, place(0, 0, 0), false),
            None
        );
        assert_eq!(
            layout(pane, Side::Right, 8, 10, place(100, 25, 10), true),
            None,
            "the alternate screen has no scrollbar"
        );
        assert_eq!(
            layout(pane, Side::Hidden, 8, 10, place(100, 25, 10), false),
            None
        );
        assert_eq!(
            layout((0, 0, 0, 0), Side::Right, 8, 10, place(100, 25, 0), false),
            None
        );
        assert_eq!(
            layout(pane, Side::Right, 8, 0, place(100, 25, 10), false),
            None,
            "no gutter means no bar, rather than a bar on the cells"
        );
        assert_eq!(
            layout(pane, Side::Right, 8, 1, place(100, 25, 10), false),
            None,
            "a 1px seam is the divider, not a bar"
        );
    }

    #[test]
    fn the_bar_sits_in_the_gutter_beside_the_configured_edge() {
        let pane = (10, 20, 200, 100);
        let p = place(100, 25, 0);
        let left = layout(pane, Side::Left, 8, 10, p, false).unwrap();
        assert_eq!(left.track, (2, 20, 8, 100));
        assert!(!intersects(left.track, pane));
        let right = layout(pane, Side::Right, 8, 10, p, false).unwrap();
        assert_eq!(right.track.0, 210);
        // wider than the gutter: stay inside the padding, never on the cells
        let wide = layout(pane, Side::Left, 500, 10, p, false).unwrap();
        assert_eq!(wide.track, (0, 20, 10, 100));
        assert!(!intersects(wide.track, pane));
    }

    #[test]
    fn a_zero_fraction_matches_the_integer_offset_and_a_remainder_moves_the_thumb() {
        let pane = (0, 0, 80, 100);
        let whole = layout(pane, Side::Right, 4, 10, place(10, 5, 0), false).unwrap();
        let explicit = layout(
            pane,
            Side::Right,
            4,
            10,
            place(10, 5, 0).with_fraction(0.0),
            false,
        )
        .unwrap();
        assert_eq!(whole.thumb, explicit.thumb);
        // max offset is 5, thumb is 50px, travel is 50. Half a line is
        // 0.5/5 of that travel: 5px up from the bottom pin at y = 50.
        let frac = layout(
            pane,
            Side::Right,
            4,
            10,
            place(10, 5, 0).with_fraction(0.5),
            false,
        )
        .unwrap();
        assert_eq!(frac.thumb.1, 45);
        let next = layout(pane, Side::Right, 4, 10, place(10, 5, 1), false).unwrap();
        assert_eq!(next.thumb.1, 40);
        assert!(frac.thumb.1 > next.thumb.1 && frac.thumb.1 < whole.thumb.1);
        // 1.0 was already carried into the offset; NaN is not a position.
        let carried = layout(
            pane,
            Side::Right,
            4,
            10,
            place(10, 5, 0).with_fraction(1.0),
            false,
        )
        .unwrap();
        assert_eq!(carried.thumb, whole.thumb);
        let nan = layout(
            pane,
            Side::Right,
            4,
            10,
            place(10, 5, 0).with_fraction(f32::NAN),
            false,
        )
        .unwrap();
        assert_eq!(nan.thumb, whole.thumb);
    }

    #[test]
    fn an_offset_past_the_top_still_pins_the_thumb_there() {
        let geom = layout((0, 0, 80, 100), Side::Right, 4, 10, place(10, 5, 99), false).unwrap();
        assert_eq!(geom.thumb.1, 0);
    }

    #[test]
    fn hits_are_the_thumb_the_track_and_not_the_cell_beside_them() {
        // A mid offset leaves track both above and below the thumb.
        let geom = bar(37);
        let (thumb_y, thumb_h) = (geom.thumb.1, geom.thumb.3);
        assert!(thumb_y > geom.track.1 && thumb_y + thumb_h < geom.track.1 + geom.track.3);
        assert_eq!(hit(&geom, 210.0, thumb_y as f64 + 1.0), Some(Part::Thumb));
        assert_eq!(hit(&geom, 217.9, thumb_y as f64 - 1.0), Some(Part::Above));
        assert_eq!(
            hit(&geom, 214.0, (thumb_y + thumb_h) as f64 + 1.0),
            Some(Part::Below)
        );
        // one pixel into the cells: the program keeps that click
        assert_eq!(hit(&geom, 209.9, thumb_y as f64 + 1.0), None);
        assert_eq!(hit(&geom, 218.0, 70.0), None);
        assert_eq!(hit(&geom, 214.0, 19.0), None);
        // flush with an end, that side of the track is gone
        let at_bottom = bar(0);
        assert_eq!(hit(&at_bottom, 214.0, 100.0), Some(Part::Thumb));
        assert_eq!(hit(&at_bottom, 214.0, 30.0), Some(Part::Above));
        let at_top = bar(75);
        assert_eq!(hit(&at_top, 214.0, 20.0), Some(Part::Thumb));
        assert_eq!(hit(&at_top, 214.0, 90.0), Some(Part::Below));
    }

    #[test]
    fn dragging_maps_the_thumb_onto_the_offset_and_keeps_the_grab() {
        let geom = bar(0);
        let max = 75;
        // grab at the thumb's top. The top of the track is the oldest line.
        assert_eq!(offset_from_pointer(&geom, 20.0, 0.0, max), 75);
        assert_eq!(offset_from_pointer(&geom, 95.0, 0.0, max), 0);
        // travel is 75px for 75 lines: 20px down from the top is offset 55.
        assert_eq!(offset_from_pointer(&geom, 40.0, 0.0, max), 55);
        // grabbed 10px into the thumb. Holding that point still is offset 0
        // (thumb top stays at 95). Moving it to the top of the track is 75.
        assert_eq!(offset_from_pointer(&geom, 105.0, 10.0, max), 0);
        assert_eq!(offset_from_pointer(&geom, 30.0, 10.0, max), 75);
        assert_eq!(offset_from_pointer(&geom, 10.0, 0.0, 0), 0);
    }

    #[test]
    fn a_track_click_pages_and_the_thumb_does_not() {
        assert_eq!(page_lines(Part::Above, 32), Some(32));
        assert_eq!(page_lines(Part::Below, 32), Some(-32));
        assert_eq!(page_lines(Part::Thumb, 32), None);
        assert_eq!(page_lines(Part::Above, 0), Some(1));
    }

    #[test]
    fn config_parses_position_and_width_and_defaults_to_the_right() {
        use crate::config::UserConfig;
        let untouched = UserConfig::default();
        assert_eq!(untouched.scrollbar.side(), Side::Right);
        assert_eq!(untouched.scrollbar.width_px(), DEFAULT_WIDTH);
        assert!(untouched.scrollbar.autohide_on());
        assert!(untouched.scrollbar.is_default());
        let json = serde_json::to_string(&untouched).unwrap();
        assert!(
            !json.contains("scrollbar"),
            "an untouched config stays empty: {json}"
        );

        let left: UserConfig =
            serde_json::from_str(r#"{"scrollbar":{"position":"left","width":10}}"#).unwrap();
        assert_eq!(left.scrollbar.side(), Side::Left);
        assert_eq!(left.scrollbar.width_px(), 10);
        let hidden: UserConfig =
            serde_json::from_str(r#"{"scrollbar":{"position":"hidden"}}"#).unwrap();
        assert_eq!(hidden.scrollbar.side(), Side::Hidden);
        let messy: UserConfig =
            serde_json::from_str(r#"{"scrollbar":{"position":"  LEFT "}}"#).unwrap();
        assert_eq!(messy.scrollbar.side(), Side::Left);
        // a typo keeps the default edge; it does not fail the file
        let typo: UserConfig =
            serde_json::from_str(r#"{"scrollbar":{"position":"centre"}}"#).unwrap();
        assert_eq!(typo.scrollbar.side(), Side::Right);
        let empty: UserConfig = serde_json::from_str(r#"{"scrollbar":{}}"#).unwrap();
        assert_eq!(empty.scrollbar.side(), Side::Right);
        assert_eq!(empty.scrollbar.width_px(), DEFAULT_WIDTH);
        let wild: UserConfig =
            serde_json::from_str(r#"{"scrollbar":{"width":0,"autohide":false}}"#).unwrap();
        assert_eq!(wild.scrollbar.width_px(), MIN_WIDTH);
        assert!(!wild.scrollbar.autohide_on());
        let huge: UserConfig = serde_json::from_str(r#"{"scrollbar":{"width":400}}"#).unwrap();
        assert_eq!(huge.scrollbar.width_px(), MAX_WIDTH);
        let saved = serde_json::to_string(&left.scrollbar).unwrap();
        assert!(saved.contains("\"position\":\"left\""), "{saved}");
        assert!(saved.contains("\"width\":10"), "{saved}");
    }

    #[test]
    fn autohide_shows_on_scroll_and_hover_then_idles() {
        let now = Instant::now();
        assert!(!shown(true, false, None, now));
        assert!(shown(true, true, None, now), "hover or drag shows it");
        assert!(shown(true, false, Some(now), now));
        let idle = now.checked_sub(IDLE + Duration::from_millis(1)).unwrap();
        assert!(!shown(true, false, Some(idle), now));
        assert!(shown(false, false, None, now), "autohide off stays visible");

        let mut bars = Bars::default();
        assert!(
            !bars.observe(7, 0, now),
            "the first sighting is not a scroll"
        );
        assert!(bars.observe(7, 4, now));
        assert!(!bars.observe(7, 4, now));
        assert!(shown(true, false, bars.last_of(7), now));

        bars.mark_painted(7);
        let tick = bars.tick(true, now);
        assert!(!tick.redraw);
        assert_eq!(tick.wake, Some(now + IDLE));
        let later = now + IDLE + Duration::from_millis(1);
        let tick = bars.tick(true, later);
        assert!(tick.redraw);
        assert_eq!(tick.wake, None);

        bars.hover = Some(7);
        let tick = bars.tick(true, later);
        assert!(!tick.redraw, "a hover holds the bar");
        assert_eq!(tick.wake, None);
        bars.hover = None;
        bars.drag = Some(Drag { pane: 7, grab: 0.0 });
        let tick = bars.tick(true, later);
        assert!(!tick.redraw);

        let tick = bars.tick(false, later);
        assert_eq!(
            tick,
            Tick {
                redraw: false,
                wake: None
            }
        );
    }

    #[test]
    fn a_click_on_the_last_column_reaches_that_cell_shown_or_hidden() {
        let grid = (0, 0, 100, 80);
        let cell_w = 10.0;
        let cell_h = 16.0;
        let cols = (grid.2 as f64 / cell_w).floor() as usize;
        let x = (cols as f64 - 0.5) * cell_w;
        let y = cell_h * 0.5;
        let cell = crate::mouse::cell_at(x, y, &[grid], cell_w, cell_h).unwrap();
        assert_eq!(cell.col, cols - 1);
        assert_eq!(cell.row, 0);
        let shown = layout(grid, Side::Right, 8, 10, place(200, 20, 40), false).unwrap();
        assert_eq!(
            shown.track.0,
            grid.0 + grid.2,
            "the track starts where the grid ends"
        );
        assert!(
            x < shown.track.0 as f64,
            "the last column is left of the track"
        );
        assert!(
            hit(&shown, x, y).is_none(),
            "a shown bar does not take the cell"
        );
        assert!(
            hit(&shown, shown.track.0 as f64 + 1.0, y).is_some(),
            "the gutter beside that column is the bar"
        );
        assert!(
            layout(grid, Side::Hidden, 8, 10, place(200, 20, 40), false).is_none(),
            "a hidden bar has no track to hit"
        );
    }

    #[test]
    fn the_gutter_is_the_padding_or_the_seam_and_misses_the_grid() {
        let content = (10, 10, 208, 100);
        let panes = [(10, 10, 100, 100), (118, 10, 100, 100)];
        assert_eq!(room_px(0, &panes, Side::Right, content, 10), 8);
        assert_eq!(room_px(1, &panes, Side::Right, content, 10), 10);
        assert_eq!(room_px(1, &panes, Side::Left, content, 10), 8);
        assert_eq!(room_px(0, &panes, Side::Left, content, 10), 10);
        let left_bar = layout(panes[0], Side::Right, 8, 8, place(80, 20, 10), false).unwrap();
        let right_bar = layout(panes[1], Side::Right, 8, 10, place(80, 20, 10), false).unwrap();
        assert!(!intersects(left_bar.track, panes[0]));
        assert!(!intersects(left_bar.track, panes[1]));
        assert!(!intersects(right_bar.track, panes[0]));
        assert!(!intersects(right_bar.track, panes[1]));
    }

    #[test]
    fn a_long_history_keeps_a_thumb_at_least_sixteen_or_the_bar_width() {
        let slim = layout(
            (0, 0, 80, 400),
            Side::Right,
            8,
            10,
            place(100_000, 40, 0),
            false,
        )
        .unwrap();
        assert_eq!(slim.thumb.3, MIN_THUMB);
        assert!(slim.thumb.3 >= 8);
        let wide = layout(
            (0, 0, 80, 400),
            Side::Right,
            20,
            24,
            place(100_000, 40, 0),
            false,
        )
        .unwrap();
        assert_eq!(wide.thumb.3, 20);
        assert!(!intersects(slim.track, (0, 0, 80, 400)));
    }

    #[test]
    fn a_side_split_at_1280_costs_one_column_per_pane() {
        use crate::layout::{Gaps, Node};
        use crate::Orientation::Row;
        // padding 10, cell 11. A 1px seam is 57/57. The 8px scrollbar seam
        // is 56/56: one column each, and not two (the bar is not subtracted
        // again from the grid).
        let pad = 10;
        let content = (pad, 0, 1280 - pad * 2, 400);
        let cell = 11.0;
        let mut t = Node::single();
        t.split(0, Row, 1);
        let cols = |r: Rect| grid_columns(r.2, cell);
        let hair = t.rects_spaced(content, 2, Gaps::HAIRLINE);
        assert_eq!((cols(hair[0]), cols(hair[1])), (57, 57));
        let seam = t.rects_spaced(
            content,
            2,
            Gaps {
                beside: 8,
                stacked: 1,
            },
        );
        assert_eq!((cols(seam[0]), cols(seam[1])), (56, 56));
    }

    #[test]
    fn width_scales_with_the_display() {
        let d = ScrollbarConfig::default();
        assert_eq!(d.width_px_at(1.0), 8);
        assert_eq!(d.width_px_at(2.0), 16);
        assert_eq!(d.width_px_at(1.5), 12);
        assert_eq!(d.width_px_at(0.0), 8, "a bad scale does not erase the bar");
        assert_eq!(d.width_px_at(f32::NAN), 8);
        let wide = ScrollbarConfig {
            width: Some(10),
            ..ScrollbarConfig::default()
        };
        assert_eq!(wide.width_px_at(2.0), 20);
    }
}
