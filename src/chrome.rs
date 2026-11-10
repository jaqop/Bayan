//! Window chrome hit-testing: resize edges, caption buttons, the tab bar,
//! pane dividers and the empty title strip, plus the order a left press
//! walks them in.
//!
//! The geometry is pure (`resize_edge`, `rect_at`, `divider_index`, ...)
//! and the `App` wrappers only feed it the renderer's rectangles and the
//! window size. `left_press` is the decision the press chain in `input.rs`
//! acts on, kept apart so the M24 order is a tested fact rather than the
//! shape of an if-chain.

use std::time::Instant;

use winit::dpi::PhysicalPosition;
use winit::window::{CursorIcon, ResizeDirection};

use crate::render::Rect;
use crate::App;

/// The smallest edge band that stays grabbable without stealing clicks
/// from the content.
const EDGE_BAND: f64 = 6.0;
/// On the scrollbar's side the band shrinks, but the outer pixels and the
/// corners stay a resize grip.
const EDGE_RESIZE_MIN: f64 = 2.0;
/// How far either side of a pane divider still grabs it.
const DIVIDER_GRAB: f64 = 4.0;
/// Two presses on the title strip this close together maximise.
const STRIP_DOUBLE_MS: u128 = 400;

/// Which window edge `(x, y)` is on, if any. A maximized window has no
/// edges to drag. Both sides use [`EDGE_BAND`].
pub(crate) fn resize_edge(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    maximized: bool,
) -> Option<ResizeDirection> {
    resize_edge_beside_bar(x, y, width, height, maximized, EDGE_BAND, EDGE_BAND)
}

/// How wide the resize grip is on a side that has a scrollbar in the
/// padding. The bar takes `used` pixels of `pad`; the grip is what's left,
/// never under [`EDGE_RESIZE_MIN`] and never over the ordinary [`EDGE_BAND`].
/// A 24px padding must not grow the grip past the 6px the caption buttons
/// were drawn against.
pub(crate) fn bar_side_band(pad: f64, used: f64) -> f64 {
    (pad - used).clamp(EDGE_RESIZE_MIN, EDGE_BAND)
}

/// A diagonal grip. Side edges are not corners: the close button and the
/// scrollbar win those, and the corner still resizes.
pub(crate) fn edge_is_corner(dir: ResizeDirection) -> bool {
    matches!(
        dir,
        ResizeDirection::NorthWest
            | ResizeDirection::NorthEast
            | ResizeDirection::SouthWest
            | ResizeDirection::SouthEast
    )
}

/// Like [`resize_edge`], but the left and right grips may be narrower so a
/// scrollbar in the padding is not a window resize. Corners stay the full
/// [`EDGE_BAND`]: a diagonal grip is not the thumb.
pub(crate) fn resize_edge_beside_bar(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    maximized: bool,
    left_band: f64,
    right_band: f64,
) -> Option<ResizeDirection> {
    if maximized || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let l_corner = x <= EDGE_BAND;
    let r_corner = x >= width - EDGE_BAND;
    let t = y <= EDGE_BAND;
    let b = y >= height - EDGE_BAND;
    if (l_corner || r_corner) && (t || b) {
        return Some(match (l_corner, r_corner, t, b) {
            (true, _, true, _) => ResizeDirection::NorthWest,
            (_, true, true, _) => ResizeDirection::NorthEast,
            (true, _, _, true) => ResizeDirection::SouthWest,
            _ => ResizeDirection::SouthEast,
        });
    }
    let l = x <= left_band;
    let r = x >= width - right_band;
    Some(match (l, r, t, b) {
        (true, ..) => ResizeDirection::West,
        (_, true, ..) => ResizeDirection::East,
        (_, _, true, _) => ResizeDirection::North,
        (_, _, _, true) => ResizeDirection::South,
        _ => return None,
    })
}

/// Index of the first rectangle containing `(x, y)`. Left and top edges
/// are inside, right and bottom are not.
pub(crate) fn rect_at(rects: &[Rect], x: f64, y: f64) -> Option<usize> {
    rects.iter().position(|&r| crate::render::rect_hit(r, x, y))
}

/// The tab whose span `(index, x, width)` holds `x`. The spans are the
/// renderer's own `tab_rects`, so a click never lands on the neighbour.
pub(crate) fn tab_index_at(spans: &[(usize, i32, i32)], x: f64) -> Option<usize> {
    spans
        .iter()
        .find(|&&(_, sx, sw)| x >= sx as f64 && x < (sx + sw) as f64)
        .map(|&(i, ..)| i)
}

/// A second press on the empty title strip within the window maximises
/// instead of starting a move.
pub(crate) fn strip_double_click(last: Option<Instant>, now: Instant) -> bool {
    last.is_some_and(|t| now.duration_since(t).as_millis() < STRIP_DOUBLE_MS)
}

/// One of the three caption buttons, in the renderer's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Caption {
    Minimize,
    Maximize,
    Close,
}

impl Caption {
    fn from_index(i: usize) -> Self {
        match i {
            0 => Caption::Minimize,
            1 => Caption::Maximize,
            _ => Caption::Close,
        }
    }
}

/// What a tab-bar control does: 0 is the previous tab, 1 the next, 2 a
/// new tab. An arrow at the end of the row does nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabControl {
    Switch(usize),
    NewTab,
}

pub(crate) fn tab_control(control: usize, active: usize, tabs: usize) -> Option<TabControl> {
    match control {
        0 if active > 0 => Some(TabControl::Switch(active - 1)),
        1 if active + 1 < tabs => Some(TabControl::Switch(active + 1)),
        2 => Some(TabControl::NewTab),
        _ => None,
    }
}

/// Everything a left press could land on, gathered once at the pointer.
/// The open panels are part of it because they change what the chrome
/// under them means.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Hits {
    pub edge: Option<ResizeDirection>,
    pub caption: Option<usize>,
    pub shortcuts_open: bool,
    pub settings_open: bool,
    pub tab_control: Option<usize>,
    pub gear: bool,
    pub cockpit_open: bool,
    pub cockpit_row: Option<usize>,
    pub divider: Option<usize>,
    pub tab: Option<usize>,
    /// The × on a tab: closing that tab, not switching to it.
    pub tab_close: Option<usize>,
    /// The group chip at the bar's leading edge (2+ groups only).
    pub group_chip: bool,
    pub drag_strip: bool,
    /// A confirm card (paste, drop, plugin, link) is up.
    pub card_open: bool,
    /// The pointer is on a scrollbar track. It beats a side resize, not a corner.
    pub on_scrollbar: bool,
}

/// Who owns a left press, before the terminal gets a say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeftPress {
    Resize(ResizeDirection),
    Caption(Caption),
    /// The shortcuts editor swallows the click: a row starts a capture,
    /// outside the card closes and saves.
    Shortcuts,
    /// The settings panel swallows the click: apply the control hit, or
    /// close and save outside the card.
    Settings,
    /// A tab-bar arrow or plus. Consumed even when it does nothing.
    TabControl(usize),
    /// The gear opens settings: a click is layout-proof, unlike Ctrl+,.
    OpenSettings,
    /// The cockpit swallows the click: pick a row or dismiss.
    Cockpit(Option<usize>),
    Divider(usize),
    Tab(usize),
    /// The × on a tab closes that tab (EasyTer's tabCloseRequested).
    CloseTab(usize),
    /// The group chip steps to the next group.
    NextGroup,
    /// Empty title strip: double-click maximises, drag moves.
    DragStrip,
    /// Tab-bar chrome under a confirm card: swallowed (R-02). Switching or
    /// closing a tab would hand the card to a pane it was not raised for.
    Blocked,
    /// Nothing chrome-side. Link, program or local selection decide next.
    Terminal,
    /// The scrollbar track. Not a corner, and not a caption button.
    Scrollbar,
}

/// Corners first, then the scrollbar track, then every other edge, then the
/// caption buttons. A side grip beats min/max/close, the way it did before
/// the bar existed; only the track takes a side grip away from the window.
/// [`cursor_for_press`] reads this same result, so the pointer and the click
/// cannot disagree. An open editor or settings panel hides the edges, the
/// buttons, and the bar.
pub(crate) fn left_press(h: &Hits) -> LeftPress {
    if !h.shortcuts_open && !h.settings_open {
        if let Some(dir) = h.edge.filter(|dir| edge_is_corner(*dir)) {
            return LeftPress::Resize(dir);
        }
        if h.on_scrollbar {
            return LeftPress::Scrollbar;
        }
        if let Some(dir) = h.edge {
            return LeftPress::Resize(dir);
        }
        if let Some(i) = h.caption {
            return LeftPress::Caption(Caption::from_index(i));
        }
    }
    if h.shortcuts_open {
        return LeftPress::Shortcuts;
    }
    if h.settings_open {
        return LeftPress::Settings;
    }
    if h.card_open
        && (h.tab_control.is_some()
            || h.gear
            || h.divider.is_some()
            || h.tab.is_some()
            || h.tab_close.is_some()
            || h.group_chip)
    {
        return LeftPress::Blocked;
    }
    if let Some(control) = h.tab_control {
        return LeftPress::TabControl(control);
    }
    if h.gear {
        return LeftPress::OpenSettings;
    }
    if h.cockpit_open {
        return LeftPress::Cockpit(h.cockpit_row);
    }
    if let Some(i) = h.divider {
        return LeftPress::Divider(i);
    }
    if let Some(i) = h.tab_close {
        return LeftPress::CloseTab(i);
    }
    if let Some(i) = h.tab {
        return LeftPress::Tab(i);
    }
    if h.group_chip {
        return LeftPress::NextGroup;
    }
    if h.drag_strip {
        return LeftPress::DragStrip;
    }
    LeftPress::Terminal
}

/// The pointer shape for a [`left_press`] result. `divider_axis` is the
/// seam under a divider press; `on_link` is a hyperlink when the press
/// reached the terminal.
pub(crate) fn cursor_for_press(
    press: LeftPress,
    divider_axis: Option<crate::Orientation>,
    on_link: bool,
) -> CursorIcon {
    match press {
        LeftPress::Resize(dir) => CursorIcon::from(dir),
        LeftPress::Divider(_) => match divider_axis {
            Some(crate::Orientation::Column) => CursorIcon::NsResize,
            _ => CursorIcon::EwResize,
        },
        LeftPress::Terminal if on_link => CursorIcon::Pointer,
        _ => CursorIcon::Default,
    }
}

/// Who owns a right press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RightPress {
    /// The tab bar: a menu of the installed shells, one new tab each.
    ShellMenu,
    /// Chrome that has no right-click meaning (caption buttons, the gear,
    /// a resize edge) or a bar under a card or an open panel: nothing.
    Ignore,
    /// Not the bar. `mouse::right_click` decides (menu, paste, program).
    Terminal,
}

/// A right press anywhere on the tab bar — a tab, its ×, the group chip,
/// the arrows and plus, the empty strip — opens the shell menu. The
/// caption buttons, the gear and the edges keep no right-click meaning.
/// `overlay` is `overlay_blocks_mouse`: a panel, card or the cockpit owns
/// the pointer, and a menu must not open over it (R-02's rule for cards).
pub(crate) fn right_press(h: &Hits, overlay: bool) -> RightPress {
    let on_bar = h.tab.is_some()
        || h.tab_close.is_some()
        || h.group_chip
        || h.tab_control.is_some()
        || h.drag_strip;
    let other_chrome = h.edge.is_some() || h.caption.is_some() || h.gear;
    if other_chrome || (on_bar && (overlay || h.card_open)) {
        RightPress::Ignore
    } else if on_bar {
        RightPress::ShellMenu
    } else {
        RightPress::Terminal
    }
}

impl App {
    /// Which window edge the pointer is on, if any.
    pub(crate) fn resize_edge_at(&self, pos: PhysicalPosition<f64>) -> Option<ResizeDirection> {
        let w = self.window.as_ref()?;
        let s = w.inner_size();
        let (left, right) = self.resize_bands();
        let (x, y) = (pos.x, pos.y);
        let (width, height) = (s.width as f64, s.height as f64);
        let maximized = w.is_maximized();
        if left == EDGE_BAND && right == EDGE_BAND {
            resize_edge(x, y, width, height, maximized)
        } else {
            resize_edge_beside_bar(x, y, width, height, maximized, left, right)
        }
    }

    /// Left and right grip widths. The scrollbar's side keeps only the
    /// padding outside the bar (at least 2px); the other side stays
    /// [`EDGE_BAND`].
    fn resize_bands(&self) -> (f64, f64) {
        let full = EDGE_BAND;
        let narrow = bar_side_band(
            self.padding_physical() as f64,
            self.scrollbar_gutter() as f64,
        );
        match self.config.scrollbar.side() {
            crate::scrollbar::Side::Left => (narrow, full),
            crate::scrollbar::Side::Right => (full, narrow),
            crate::scrollbar::Side::Hidden => (full, full),
        }
    }

    /// Caption button under the pointer: 0 minimise, 1 maximise, 2 close.
    pub(crate) fn caption_button_at(&self, pos: PhysicalPosition<f64>) -> Option<usize> {
        let (r, w) = (self.renderer.as_ref()?, self.window.as_ref()?);
        if r.tab_bar_h() == 0.0 {
            return None;
        }
        let pw = w.inner_size().width as usize;
        rect_at(&r.window_buttons(pw), pos.x, pos.y)
    }

    /// Empty title-strip space: dragging it moves the window, as a real
    /// title bar would. Tabs, the gear and the caption buttons are excluded.
    pub(crate) fn in_drag_strip(&self, pos: PhysicalPosition<f64>) -> bool {
        let Some(r) = self.renderer.as_ref() else {
            return false;
        };
        let Some(w) = self.window.as_ref() else {
            return false;
        };
        if r.tab_bar_h() == 0.0 || pos.y >= r.tab_bar_h() as f64 {
            return false;
        }
        if self.resize_edge_at(pos).is_some() || self.caption_button_at(pos).is_some() {
            return false;
        }
        let pw = w.inner_size().width as usize;
        if r.settings_button_hit(pw, pos.x, pos.y) || r.tab_control_hit(pw, pos.x, pos.y).is_some()
        {
            return false;
        }
        self.tab_at(pos).is_none() && !self.group_chip_at(pos)
    }

    /// Tab index under a pixel position, if it's in the tab bar.
    pub(crate) fn tab_at(&self, pos: PhysicalPosition<f64>) -> Option<usize> {
        let r = self.renderer.as_ref()?;
        if r.tab_bar_h() == 0.0 || pos.y >= r.tab_bar_h() as f64 {
            return None;
        }
        let width = self.window.as_ref()?.inner_size().width as usize;
        // the labels the renderer measured, user names and the broadcast
        // mark included, or a click would land on the neighbour — and only
        // the active group's, which are the only ones drawn
        let (labels, pos_active) = self.visible_labels();
        let titles: Vec<&str> = labels.iter().map(String::as_str).collect();
        let shown = tab_index_at(&r.tab_rects(width, &titles, pos_active), pos.x)?;
        self.visible_tab(shown)
    }

    /// The `self.tabs` index of the bar's `shown`-th tab.
    fn visible_tab(&self, shown: usize) -> Option<usize> {
        let groups: Vec<u32> = self.tabs.iter().map(|t| t.group).collect();
        let gid = self.tabs.get(self.active)?.group;
        crate::groups::members(&groups, gid).get(shown).copied()
    }

    /// Is the pointer on the group chip (only drawn with 2+ groups)?
    pub(crate) fn group_chip_at(&self, pos: PhysicalPosition<f64>) -> bool {
        self.renderer
            .as_ref()
            .is_some_and(|r| r.group_chip_hit(pos.x, pos.y))
    }

    /// Tab whose close × is under the pointer, if any.
    pub(crate) fn tab_close_at(&self, pos: PhysicalPosition<f64>) -> Option<usize> {
        let r = self.renderer.as_ref()?;
        if r.tab_bar_h() == 0.0 || pos.y >= r.tab_bar_h() as f64 {
            return None;
        }
        let width = self.window.as_ref()?.inner_size().width as usize;
        let (labels, pos_active) = self.visible_labels();
        let titles: Vec<&str> = labels.iter().map(String::as_str).collect();
        let shown = r.tab_close_at(width, &titles, pos_active, pos.x, pos.y)?;
        self.visible_tab(shown)
    }

    /// Divider index under the pointer (between pane i and i+1), if any.
    pub(crate) fn divider_at(&self, pos: PhysicalPosition<f64>) -> Option<usize> {
        let t = self.tabs.get(self.active)?;
        // a zoomed tab shows one pane: there is no divider to grab
        if t.panes.len() < 2 || t.zoomed {
            return None;
        }
        crate::layout::divider_at(&self.dividers(), pos.x, pos.y, DIVIDER_GRAB)
    }

    /// The tab-bar arrow or plus under the pointer, and whether the gear is.
    fn tab_bar_controls_at(&self, pos: PhysicalPosition<f64>) -> (Option<usize>, bool) {
        let (Some(r), Some(w)) = (self.renderer.as_ref(), self.window.as_ref()) else {
            return (None, false);
        };
        let pw = w.inner_size().width as usize;
        (
            r.tab_control_hit(pw, pos.x, pos.y),
            r.settings_button_hit(pw, pos.x, pos.y),
        )
    }

    /// Every chrome hit at `pos`, for `left_press`. Reads only.
    pub(crate) fn chrome_hits(&self, pos: PhysicalPosition<f64>) -> Hits {
        let (tab_control, gear) = self.tab_bar_controls_at(pos);
        Hits {
            edge: self.resize_edge_at(pos),
            caption: self.caption_button_at(pos),
            shortcuts_open: self.shortcuts.is_some(),
            settings_open: self.settings.is_some(),
            tab_control,
            gear,
            cockpit_open: self.cockpit,
            cockpit_row: self.cockpit_row_hit(pos),
            divider: self.divider_at(pos),
            on_scrollbar: !self.overlay_blocks_mouse() && self.pointer_on_scrollbar(pos.x, pos.y),
            tab: self.tab_at(pos),
            tab_close: self.tab_close_at(pos),
            group_chip: self.group_chip_at(pos),
            drag_strip: self.in_drag_strip(pos),
            card_open: self.card_open(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_confirm_card_swallows_tab_bar_clicks() {
        // R-02: no tab switch, ×, arrows, gear or divider under a card
        let card = Hits {
            card_open: true,
            ..Hits::default()
        };
        for h in [
            Hits {
                tab: Some(1),
                ..card
            },
            Hits {
                tab_close: Some(1),
                tab: Some(1),
                ..card
            },
            Hits {
                tab_control: Some(2),
                ..card
            },
            Hits { gear: true, ..card },
            Hits {
                divider: Some(0),
                ..card
            },
        ] {
            assert_eq!(left_press(&h), LeftPress::Blocked, "{h:?}");
        }
        // moving or resizing the window, and the window caption, still work
        assert_eq!(
            left_press(&Hits {
                drag_strip: true,
                ..card
            }),
            LeftPress::DragStrip
        );
        assert_eq!(
            left_press(&Hits {
                caption: Some(2),
                ..card
            }),
            LeftPress::Caption(Caption::from_index(2))
        );
        // and without a card the tab is just a tab
        assert_eq!(
            left_press(&Hits {
                tab: Some(1),
                ..Hits::default()
            }),
            LeftPress::Tab(1)
        );
    }

    #[test]
    fn edges_and_corners_resolve_and_a_maximized_window_has_none() {
        let edge = |x, y| resize_edge(x, y, 800.0, 600.0, false);
        assert_eq!(edge(0.0, 0.0), Some(ResizeDirection::NorthWest));
        assert_eq!(edge(799.0, 2.0), Some(ResizeDirection::NorthEast));
        assert_eq!(edge(3.0, 598.0), Some(ResizeDirection::SouthWest));
        assert_eq!(edge(795.0, 595.0), Some(ResizeDirection::SouthEast));
        assert_eq!(edge(6.0, 300.0), Some(ResizeDirection::West));
        assert_eq!(edge(794.0, 300.0), Some(ResizeDirection::East));
        assert_eq!(edge(400.0, 6.0), Some(ResizeDirection::North));
        assert_eq!(edge(400.0, 594.0), Some(ResizeDirection::South));
        assert_eq!(edge(6.1, 300.0), None);
        assert_eq!(edge(400.0, 300.0), None);
        assert_eq!(resize_edge(0.0, 0.0, 800.0, 600.0, true), None);
    }

    #[test]
    fn the_thumb_center_drags_the_scrollbar_and_corners_still_resize() {
        // Defaults: padding 10, bar 8, window 1280×800. The plain 6px band
        // claims the thumb center (W−6). Beside the bar that grip shrinks
        // to the 2px of padding outside the track.
        let w = 1280.0;
        let h = 800.0;
        let pad = 10.0;
        let used = 8.0;
        let band = bar_side_band(pad, used);
        assert_eq!(band, 2.0);
        let thumb_cx = (w - pad) + used / 2.0;
        let y = 400.0;
        assert!(
            (thumb_cx - (w - EDGE_BAND)).abs() < 1e-6,
            "defaults put the thumb center on the old grip"
        );
        assert_eq!(
            resize_edge(thumb_cx, y, w, h, false),
            Some(ResizeDirection::East),
            "a 6px band resizes instead of dragging"
        );
        assert_eq!(
            resize_edge_beside_bar(thumb_cx, y, w, h, false, EDGE_BAND, band),
            None,
            "the thumb center starts a scrollbar drag"
        );
        assert_eq!(
            resize_edge_beside_bar(w - 1.0, y, w, h, false, EDGE_BAND, band),
            Some(ResizeDirection::East),
            "the last 2px still resize"
        );
        assert_eq!(
            resize_edge_beside_bar(w - 1.0, 1.0, w, h, false, EDGE_BAND, band),
            Some(ResizeDirection::NorthEast)
        );
        assert_eq!(
            resize_edge_beside_bar(1.0, 1.0, w, h, false, EDGE_BAND, band),
            Some(ResizeDirection::NorthWest)
        );
        assert_eq!(
            resize_edge_beside_bar(w - 1.0, h - 1.0, w, h, false, EDGE_BAND, band),
            Some(ResizeDirection::SouthEast)
        );
        // padding 4 used to swallow the whole bar inside the 6px band.
        // W−3 is inside the bar and outside the 2px grip; the center is not.
        let band4 = bar_side_band(4.0, 4.0);
        assert_eq!(band4, 2.0);
        let inside = w - 3.0;
        assert_eq!(
            resize_edge_beside_bar(inside, y, w, h, false, EDGE_BAND, band4),
            None
        );
    }

    #[test]
    fn padding_24_close_button_is_not_a_resize() {
        // pad 24, bar 8: an uncapped grip is 16px and eats the close button.
        // The cap is 6, and the button still wins that 6 except at a corner.
        let w = 1280.0;
        let h = 800.0;
        let band = bar_side_band(24.0, 8.0);
        assert_eq!(band, EDGE_BAND, "the grip never grows past 6px");
        assert_eq!(
            resize_edge_beside_bar(w - 10.0, 16.0, w, h, false, EDGE_BAND, band),
            None,
            "10px in from the edge is padding, not a grip"
        );
        // Outside the 6px grip the close button is a close. Inside it the
        // edge wins, and the cursor is the same resize arrow the press uses.
        assert_eq!(
            left_press(&Hits {
                edge: None,
                caption: Some(2),
                ..Hits::default()
            }),
            LeftPress::Caption(Caption::Close)
        );
        let edge = resize_edge_beside_bar(w - 3.0, 16.0, w, h, false, EDGE_BAND, band);
        assert_eq!(edge, Some(ResizeDirection::East));
        let press = left_press(&Hits {
            edge,
            caption: Some(2),
            ..Hits::default()
        });
        assert_eq!(press, LeftPress::Resize(ResizeDirection::East));
        assert_eq!(
            cursor_for_press(press, None, false),
            CursorIcon::from(ResizeDirection::East)
        );
        // the corner of that same button still resizes
        assert_eq!(
            left_press(&Hits {
                edge: Some(ResizeDirection::NorthEast),
                caption: Some(2),
                ..Hits::default()
            }),
            LeftPress::Resize(ResizeDirection::NorthEast)
        );
    }

    #[test]
    fn padding_4_thumb_center_drags_through_left_press() {
        // The 4px bar fills [W−4, W). Its center is W−2, on the 2px grip.
        // The plain band says East; the press is the scrollbar.
        let w = 1280.0;
        let h = 800.0;
        let pad = 4.0;
        let used = 4.0;
        let thumb_cx = (w - pad) + used / 2.0;
        let y = 400.0;
        let center_delta: f64 = thumb_cx - (w - 2.0);
        assert!(center_delta.abs() < 1e-6);
        let band = bar_side_band(pad, used);
        let edge = resize_edge_beside_bar(thumb_cx, y, w, h, false, EDGE_BAND, band);
        assert_eq!(edge, Some(ResizeDirection::East));
        assert_eq!(
            left_press(&Hits {
                edge,
                on_scrollbar: true,
                ..Hits::default()
            }),
            LeftPress::Scrollbar
        );
    }

    #[test]
    fn scale_one_and_a_half_track_is_not_a_resize() {
        // 8×1.5 = 12, padding 10×1.5 = 15. The bar is 12px of that 15, so it
        // is not clipped to an unscaled 10. Any pixel of the track, including
        // one the 2px grip would claim, is a scrollbar press.
        let cfg = crate::config::UserConfig::default();
        let pad = cfg.padding_px_at(1.5) as f64;
        let width = cfg.scrollbar.width_px_at(1.5) as f64;
        assert_eq!((pad, width), (15.0, 12.0));
        assert!(width < pad, "scaled padding has room for the scaled bar");
        let used = width.min(pad);
        let w = 1280.0;
        let h = 800.0;
        let band = bar_side_band(pad, used);
        assert!((EDGE_RESIZE_MIN..=EDGE_BAND).contains(&band));
        let track_x0 = w - pad;
        for i in 0..used as i32 {
            let x = track_x0 + i as f64 + 0.5;
            let edge = resize_edge_beside_bar(x, 400.0, w, h, false, EDGE_BAND, band);
            assert_eq!(
                left_press(&Hits {
                    edge,
                    on_scrollbar: true,
                    ..Hits::default()
                }),
                LeftPress::Scrollbar,
                "x={x} edge={edge:?}"
            );
        }
        // A bar that fills its padding overlaps the 2px grip. The press
        // follows the track; the corner does not.
        assert_eq!(
            left_press(&Hits {
                edge: Some(ResizeDirection::East),
                on_scrollbar: true,
                ..Hits::default()
            }),
            LeftPress::Scrollbar
        );
        assert_eq!(
            left_press(&Hits {
                edge: Some(ResizeDirection::SouthEast),
                on_scrollbar: true,
                ..Hits::default()
            }),
            LeftPress::Resize(ResizeDirection::SouthEast)
        );
    }

    #[test]
    fn rect_hits_are_half_open() {
        let rects = [(10, 0, 20, 10), (30, 0, 20, 10)];
        assert_eq!(rect_at(&rects, 10.0, 0.0), Some(0));
        assert_eq!(rect_at(&rects, 29.9, 9.9), Some(0));
        assert_eq!(rect_at(&rects, 30.0, 5.0), Some(1));
        assert_eq!(rect_at(&rects, 50.0, 5.0), None);
        assert_eq!(rect_at(&rects, 15.0, 10.0), None);
        assert_eq!(rect_at(&[], 0.0, 0.0), None);
    }

    #[test]
    fn tab_spans_return_the_tab_index_not_the_slot() {
        // a scrolled row: the first visible tab is index 3
        let spans = [(3, 0, 100), (4, 100, 60)];
        assert_eq!(tab_index_at(&spans, 0.0), Some(3));
        assert_eq!(tab_index_at(&spans, 99.5), Some(3));
        assert_eq!(tab_index_at(&spans, 100.0), Some(4));
        assert_eq!(tab_index_at(&spans, 160.0), None);
    }

    #[test]
    fn strip_double_click_needs_a_recent_first_press() {
        let t = Instant::now();
        assert!(!strip_double_click(None, t));
        assert!(strip_double_click(Some(t), t + Duration::from_millis(399)));
        assert!(!strip_double_click(Some(t), t + Duration::from_millis(400)));
    }

    #[test]
    fn tab_controls_stop_at_the_ends_of_the_row() {
        assert_eq!(tab_control(0, 0, 3), None);
        assert_eq!(tab_control(0, 2, 3), Some(TabControl::Switch(1)));
        assert_eq!(tab_control(1, 2, 3), None);
        assert_eq!(tab_control(1, 0, 3), Some(TabControl::Switch(1)));
        assert_eq!(tab_control(2, 0, 1), Some(TabControl::NewTab));
        assert_eq!(tab_control(3, 0, 1), None);
    }

    #[test]
    fn corners_and_side_edges_beat_caption_buttons() {
        let h = Hits {
            edge: Some(ResizeDirection::NorthEast),
            caption: Some(2),
            tab_control: Some(2),
            gear: true,
            cockpit_open: true,
            divider: Some(0),
            tab: Some(1),
            drag_strip: true,
            ..Hits::default()
        };
        assert_eq!(
            left_press(&h),
            LeftPress::Resize(ResizeDirection::NorthEast)
        );
        // a side edge, not only a corner, beats the button under it
        assert_eq!(
            left_press(&Hits {
                edge: Some(ResizeDirection::East),
                caption: Some(2),
                ..Hits::default()
            }),
            LeftPress::Resize(ResizeDirection::East)
        );
        assert_eq!(
            left_press(&Hits {
                edge: Some(ResizeDirection::North),
                caption: Some(0),
                ..Hits::default()
            }),
            LeftPress::Resize(ResizeDirection::North)
        );
        let h = Hits { edge: None, ..h };
        assert_eq!(left_press(&h), LeftPress::Caption(Caption::Close));
        let h = Hits {
            caption: Some(0),
            ..h
        };
        assert_eq!(left_press(&h), LeftPress::Caption(Caption::Minimize));
        let h = Hits {
            caption: Some(1),
            ..h
        };
        assert_eq!(left_press(&h), LeftPress::Caption(Caption::Maximize));
    }

    #[test]
    fn cursor_and_press_agree_on_the_caption_buttons() {
        // Top 6px over each caption button, clear of the corner: both say
        // resize from the top. The right 2px (and the full 6px grip) beside
        // close say resize from the side. Past the grip, both say close.
        let agree = |edge, caption| {
            let press = left_press(&Hits {
                edge,
                caption,
                ..Hits::default()
            });
            (press, cursor_for_press(press, None, false))
        };
        for caption in [Some(0), Some(1), Some(2)] {
            assert_eq!(
                agree(Some(ResizeDirection::North), caption),
                (
                    LeftPress::Resize(ResizeDirection::North),
                    CursorIcon::from(ResizeDirection::North)
                )
            );
        }
        assert_eq!(
            agree(Some(ResizeDirection::East), Some(2)),
            (
                LeftPress::Resize(ResizeDirection::East),
                CursorIcon::from(ResizeDirection::East)
            )
        );
        assert_eq!(
            agree(None, Some(2)),
            (LeftPress::Caption(Caption::Close), CursorIcon::Default)
        );
        // the thumb still beats a side edge, and the cursor is not a grip
        let press = left_press(&Hits {
            edge: Some(ResizeDirection::East),
            on_scrollbar: true,
            caption: Some(2),
            ..Hits::default()
        });
        assert_eq!(press, LeftPress::Scrollbar);
        assert_eq!(cursor_for_press(press, None, false), CursorIcon::Default);
    }

    #[test]
    fn an_open_panel_hides_edges_and_captions_and_swallows_the_click() {
        let h = Hits {
            edge: Some(ResizeDirection::East),
            caption: Some(2),
            settings_open: true,
            tab: Some(0),
            ..Hits::default()
        };
        assert_eq!(left_press(&h), LeftPress::Settings);
        let h = Hits {
            shortcuts_open: true,
            ..h
        };
        assert_eq!(left_press(&h), LeftPress::Shortcuts);
    }

    #[test]
    fn a_right_press_on_the_tab_bar_opens_the_shell_menu() {
        let bar_spots = [
            Hits {
                tab: Some(1),
                ..Hits::default()
            },
            Hits {
                tab_close: Some(0),
                tab: Some(0),
                ..Hits::default()
            },
            Hits {
                group_chip: true,
                ..Hits::default()
            },
            Hits {
                tab_control: Some(2),
                ..Hits::default()
            },
            Hits {
                drag_strip: true,
                ..Hits::default()
            },
        ];
        for h in bar_spots {
            assert_eq!(right_press(&h, false), RightPress::ShellMenu, "{h:?}");
            // a card, a panel or the cockpit owns the pointer: no menu
            assert_eq!(right_press(&h, true), RightPress::Ignore, "{h:?}");
            let carded = Hits {
                card_open: true,
                ..h
            };
            assert_eq!(right_press(&carded, false), RightPress::Ignore);
        }
        // the window's own controls keep no right-click meaning
        for h in [
            Hits {
                caption: Some(2),
                ..Hits::default()
            },
            Hits {
                gear: true,
                ..Hits::default()
            },
            Hits {
                edge: Some(ResizeDirection::North),
                drag_strip: true,
                ..Hits::default()
            },
        ] {
            assert_eq!(right_press(&h, false), RightPress::Ignore, "{h:?}");
        }
        // a pane: the pane's own right-click rules decide
        assert_eq!(right_press(&Hits::default(), false), RightPress::Terminal);
        assert_eq!(right_press(&Hits::default(), true), RightPress::Terminal);
    }

    #[test]
    fn tab_bar_then_cockpit_then_dividers_tabs_and_strip() {
        let all = Hits {
            tab_control: Some(1),
            gear: true,
            cockpit_open: true,
            cockpit_row: Some(4),
            divider: Some(0),
            tab: Some(2),
            drag_strip: true,
            ..Hits::default()
        };
        assert_eq!(left_press(&all), LeftPress::TabControl(1));
        let h = Hits {
            tab_control: None,
            ..all
        };
        assert_eq!(left_press(&h), LeftPress::OpenSettings);
        let h = Hits { gear: false, ..h };
        assert_eq!(left_press(&h), LeftPress::Cockpit(Some(4)));
        let h = Hits {
            cockpit_row: None,
            ..h
        };
        assert_eq!(left_press(&h), LeftPress::Cockpit(None));
        let h = Hits {
            cockpit_open: false,
            ..h
        };
        assert_eq!(left_press(&h), LeftPress::Divider(0));
        let h = Hits { divider: None, ..h };
        assert_eq!(left_press(&h), LeftPress::Tab(2));
        let h = Hits {
            tab_close: Some(2),
            ..h
        };
        assert_eq!(left_press(&h), LeftPress::CloseTab(2));
        let h = Hits {
            tab_close: None,
            tab: None,
            group_chip: true,
            ..h
        };
        assert_eq!(left_press(&h), LeftPress::NextGroup);
        // under a confirm card the chip is swallowed like a tab (R-02)
        let carded = Hits {
            card_open: true,
            ..h
        };
        assert_eq!(left_press(&carded), LeftPress::Blocked);
        let h = Hits {
            group_chip: false,
            ..h
        };
        assert_eq!(left_press(&h), LeftPress::DragStrip);
        assert_eq!(left_press(&Hits::default()), LeftPress::Terminal);
    }
}
