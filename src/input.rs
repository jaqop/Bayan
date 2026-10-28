//! Pointer routing: what a mouse button, a move or a wheel notch does,
//! from the window edge inward to the cell under the pointer.
//!
//! `App::window_event` dispatches here. Each decision that can be stated
//! without the window (`press_route`, `motion_route`, `hover_cursor`,
//! `selection_type`, the wheel arithmetic) is a pure function with tests;
//! the `App` methods carry out what they decide. Chrome geometry and the
//! left-press order live in `chrome.rs`; link, program and selection
//! ownership in `link::click_owner` and `mouse::right_click`.

use std::time::Instant;

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::ModifiersState;
use winit::window::{CursorIcon, ResizeDirection};

use crate::chrome::{self, Caption, LeftPress, TabControl};
use crate::{
    blur_forgets_held_keys, consume_guard_release, keybinds, link, mouse, prepare_window_close,
    term, App, Orientation,
};

/// The font zoom range, in points on top of the configured size.
const FONT_DELTA_MIN: f32 = -7.0;
const FONT_DELTA_MAX: f32 = 25.0;

/// The buttons a program can hear. Back, forward and the rest are not
/// reported and do nothing locally.
pub(crate) fn button_of(button: MouseButton) -> Option<mouse::Btn> {
    match button {
        MouseButton::Left => Some(mouse::Btn::Left),
        MouseButton::Middle => Some(mouse::Btn::Middle),
        MouseButton::Right => Some(mouse::Btn::Right),
        _ => None,
    }
}

/// Where a press goes before anything is hit-tested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Press {
    /// Right copies, pastes or reports. It never touches chrome.
    Right,
    /// Middle never touches chrome; it only reports when the program asked.
    Report(mouse::Btn),
    /// Left walks the chrome first, then the terminal.
    Left,
    Ignore,
}

pub(crate) fn press_route(button: MouseButton) -> Press {
    match button {
        MouseButton::Right => Press::Right,
        MouseButton::Left => Press::Left,
        other => button_of(other).map_or(Press::Ignore, Press::Report),
    }
}

/// What a pointer move feeds, in priority order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Motion {
    /// A divider drag follows the pointer and nothing else happens.
    Divider(usize),
    /// A local selection drag, Shift+drag included even while the program
    /// tracks the mouse — the bypass is the whole point of Shift.
    Select,
    /// 1002/1003: the program owns the drag. No local selection, and no
    /// redraw — a still frame stays still.
    ProgramDrag,
    /// No button held: link hover, cursor shape, caption hover, 1003.
    Hover,
}

pub(crate) fn motion_route(divider: Option<usize>, selecting: bool, grabbed: bool) -> Motion {
    match divider {
        Some(i) => Motion::Divider(i),
        None if selecting => Motion::Select,
        None if grabbed => Motion::ProgramDrag,
        None => Motion::Hover,
    }
}

/// The pointer shape over a spot. M24: an undecorated window has to
/// advertise its own edges, or the user has no way to learn they can grab.
pub(crate) fn hover_cursor(
    edge: Option<ResizeDirection>,
    on_divider: bool,
    orientation: Option<Orientation>,
    on_link: bool,
) -> CursorIcon {
    if let Some(dir) = edge {
        CursorIcon::from(dir)
    } else if on_divider {
        match orientation {
            Some(Orientation::Column) => CursorIcon::NsResize,
            _ => CursorIcon::EwResize,
        }
    } else if on_link {
        CursorIcon::Pointer
    } else {
        CursorIcon::Default
    }
}

/// 1 click = simple drag anchor, 2 = word, 3 = line.
pub(crate) fn selection_type(clicks: u8) -> SelectionType {
    match clicks {
        2 => SelectionType::Semantic,
        3 => SelectionType::Lines,
        _ => SelectionType::Simple,
    }
}

/// A wheel delta in terminal lines: three per notch, or the touchpad's
/// pixels over the cell height.
pub(crate) fn wheel_lines(delta: MouseScrollDelta, cell_h: f32) -> f32 {
    match delta {
        MouseScrollDelta::LineDelta(_, y) => y * 3.0,
        MouseScrollDelta::PixelDelta(p) => p.y as f32 / cell_h,
    }
}

/// Add `lines` to the running remainder and take the whole lines out.
/// Precision touchpads send many sub-line deltas; they add up here.
pub(crate) fn take_whole_lines(accum: f32, lines: f32) -> (i32, f32) {
    let accum = accum + lines;
    let n = accum as i32;
    (n, accum - n as f32)
}

/// The font zoom after `step` points, kept in range.
pub(crate) fn zoomed(font_delta: f32, step: f32) -> f32 {
    (font_delta + step).clamp(FONT_DELTA_MIN, FONT_DELTA_MAX)
}

/// One palette wheel notch in rows. Pixel deltas count 30px a row.
pub(crate) fn palette_wheel_rows(delta: MouseScrollDelta) -> f32 {
    match delta {
        MouseScrollDelta::LineDelta(_, y) => y,
        MouseScrollDelta::PixelDelta(p) => (p.y / 30.0) as f32,
    }
}

/// The palette selection after a wheel step, and the fraction of a row
/// kept for the next one. Up moves toward row 0, at most 12 rows a step;
/// any nonzero step also clamps the selection to the last of `count` rows.
pub(crate) fn palette_scroll(sel: usize, wheel: f32, step: f32, count: usize) -> (usize, f32) {
    if step == 0.0 {
        return (sel, wheel);
    }
    let wheel = wheel + step;
    let whole = wheel.trunc();
    let distance = whole.abs().min(12.0) as usize;
    let sel = if whole > 0.0 {
        sel.saturating_sub(distance)
    } else {
        sel.saturating_add(distance).min(count.saturating_sub(1))
    };
    (sel, wheel - whole)
}

/// A palette row runs only when the release lands on the row the press
/// did, and that row still holds the same action.
pub(crate) fn palette_release<T: PartialEq>(
    pressed: Option<(usize, T)>,
    hit: Option<usize>,
    rows: &[(T, String)],
) -> Option<T> {
    let (index, action) = pressed?;
    let hit = hit?;
    (index == hit && rows.get(hit).is_some_and(|(row, _)| *row == action)).then_some(action)
}

impl App {
    /// CloseRequested and the caption X alike: a running command deserves
    /// a second look before the kill.
    pub(crate) fn request_close(&mut self, el: &ActiveEventLoop) {
        let running = self.running_count();
        if prepare_window_close(
            &mut self.pending_close,
            self.config.confirm_close_on(),
            running,
        ) {
            self.quit(el);
        } else {
            self.request_redraw();
        }
    }

    /// Pointer events a modal owns, before the main dispatch: the release
    /// of a press that dismissed a guard, the close guard, the palette.
    /// True when the event was consumed.
    pub(crate) fn modal_pointer(&mut self, event: &WindowEvent, el: &ActiveEventLoop) -> bool {
        if matches!(
            event,
            WindowEvent::Focused(false) | WindowEvent::CursorLeft { .. }
        ) {
            self.swallow_guard_release = false;
            if let Some(p) = self.palette.as_mut() {
                p.pressed = None;
                p.wheel = 0.0;
            }
        }
        if let WindowEvent::MouseInput { state, button, .. } = event {
            if consume_guard_release(&mut self.swallow_guard_release, *state, *button) {
                return true;
            }
        }
        if self.pending_close.is_some() && self.close_guard_pointer(event, el) {
            return true;
        }
        self.palette.is_some() && self.palette_pointer(event, el)
    }

    /// A modal owns pointer input too: no tab changes, selections, dropped
    /// commands or wheel reports may reach the terminal below.
    fn close_guard_pointer(&mut self, event: &WindowEvent, el: &ActiveEventLoop) -> bool {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = *position;
                self.abandon_mouse();
                self.drag_divider = None;
                if let Some(w) = &self.window {
                    w.set_cursor(CursorIcon::Default);
                }
                true
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.abandon_mouse();
                self.drag_divider = None;
                if *state == ElementState::Pressed && *button == MouseButton::Left {
                    self.swallow_guard_release = true;
                    let hit =
                        self.renderer
                            .as_ref()
                            .zip(self.window.as_ref())
                            .and_then(|(r, w)| {
                                let size = w.inner_size();
                                r.close_guard_layout(size.width as usize, size.height as usize)
                                    .hit(self.cursor_pos.x, self.cursor_pos.y)
                            });
                    if let Some(confirm) = hit {
                        self.resolve_close(confirm, el);
                    }
                }
                true
            }
            WindowEvent::MouseWheel { .. } | WindowEvent::DroppedFile(_) => true,
            _ => false,
        }
    }

    /// The palette owns the pointer while open: press and release on the
    /// same row runs it, a press outside closes it, the wheel moves the
    /// selection.
    fn palette_pointer(&mut self, event: &WindowEvent, el: &ActiveEventLoop) -> bool {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = *position;
                true
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if *button == MouseButton::Left {
                    self.palette_click(*state, el);
                }
                true
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let step = palette_wheel_rows(*delta);
                let Some(p) = self.palette.as_ref() else {
                    return true;
                };
                let count = self.palette_rows(&p.query).len();
                if let Some(p) = self.palette.as_mut() {
                    p.pressed = None;
                    (p.sel, p.wheel) = palette_scroll(p.sel, p.wheel, step, count);
                }
                self.request_redraw();
                true
            }
            WindowEvent::DroppedFile(_) => true,
            WindowEvent::Resized(_)
            | WindowEvent::ScaleFactorChanged { .. }
            | WindowEvent::Focused(false) => {
                if let Some(p) = self.palette.as_mut() {
                    p.pressed = None;
                }
                false
            }
            _ => false,
        }
    }

    fn palette_click(&mut self, state: ElementState, el: &ActiveEventLoop) {
        let Some(p) = self.palette.as_ref() else {
            return;
        };
        let rows = self.palette_rows(&p.query);
        let layout = self
            .renderer
            .as_ref()
            .zip(self.window.as_ref())
            .map(|(r, w)| {
                let size = w.inner_size();
                r.palette_layout(size.width as usize, size.height as usize, rows.len(), p.sel)
            });
        let (x, y) = (self.cursor_pos.x, self.cursor_pos.y);
        let hit = layout.as_ref().and_then(|l| l.row_at(x, y));
        if state == ElementState::Pressed {
            if layout.is_some_and(|l| !l.contains(x, y)) {
                self.palette = None;
                self.swallow_guard_release = true;
            } else if let Some(p) = self.palette.as_mut() {
                p.wheel = 0.0;
                p.pressed = hit.and_then(|i| rows.get(i).map(|(row, _)| (i, row.clone())));
            }
        } else {
            let pressed = self.palette.as_mut().and_then(|p| p.pressed.take());
            if let Some(action) = palette_release(pressed, hit, &rows) {
                self.palette_run(action, el);
            }
        }
        self.request_redraw();
    }

    /// `WindowEvent::CursorMoved`, after the modals.
    pub(crate) fn cursor_moved(&mut self, position: PhysicalPosition<f64>) {
        self.cursor_pos = position;
        match motion_route(
            self.drag_divider,
            self.mouse.selecting,
            self.mouse.grabbed_pane().is_some(),
        ) {
            Motion::Divider(i) => self.drag_divider_to(i, position),
            Motion::Select => {
                self.clear_link_hover();
                self.extend_selection(position);
            }
            Motion::ProgramDrag => {
                self.report_drag(position);
                self.clear_link_hover();
            }
            Motion::Hover => self.hover(position),
        }
    }

    /// The drag stays in the pane where it started.
    fn extend_selection(&mut self, position: PhysicalPosition<f64>) {
        let Some((pi, row, col, side)) = self.pane_cell_at(position) else {
            return;
        };
        if pi != self.sel_pane {
            return;
        }
        if let Some(s) = self.session() {
            let mut t = s.term.lock();
            let off = t.grid().display_offset() as i32;
            let point = Point::new(Line(row as i32 - off), Column(col));
            if let Some(sel) = t.selection.as_mut() {
                sel.update(point, side);
            }
        }
        self.request_redraw();
    }

    /// No button held. One hit-test: the hover and the 1003 report share
    /// the logical cell `locate` mapped through column_for_click.
    fn hover(&mut self, position: PhysicalPosition<f64>) {
        let cell = self.refresh_link_hover(position);
        if let Some(w) = &self.window {
            let edge = self.resize_edge_at(position);
            let on_divider = edge.is_none() && self.divider_at(position).is_some();
            let orientation = self.tabs.get(self.active).map(|t| t.orientation);
            w.set_cursor(hover_cursor(
                edge,
                on_divider,
                orientation,
                self.link_hover.is_some(),
            ));
            // caption/gear hover feedback; a redraw only when it changed
            let cap = self.caption_button_at(position);
            let pw = w.inner_size().width as usize;
            let gear = self
                .renderer
                .as_ref()
                .is_some_and(|r| r.settings_button_hit(pw, position.x, position.y));
            if let Some(r) = self.renderer.as_mut() {
                if r.hover_caption != cap || r.hover_gear != gear {
                    r.hover_caption = cap;
                    r.hover_gear = gear;
                    self.request_redraw();
                }
            }
        }
        // 1003: motion with no button. Shift and overlays already dropped
        // out inside the report.
        if let Some((id, row, col)) = cell {
            self.report_motion_at(id, row, col);
        }
    }

    /// `WindowEvent::MouseWheel`, after the modals.
    pub(crate) fn mouse_wheel(&mut self, delta: MouseScrollDelta) {
        let cell_h = self.renderer.as_ref().map_or(20.0, |r| r.cell_h);
        let (n, rest) = take_whole_lines(self.wheel_accum, wheel_lines(delta, cell_h));
        self.wheel_accum = rest;
        if n == 0 {
            return;
        }
        // Ctrl+wheel = live font zoom (EasyTer's favorite)
        if self.modifiers.control_key() {
            self.font_delta = zoomed(self.font_delta, n as f32);
            self.rebuild_renderer();
            return;
        }
        // the pane under the pointer, else the focused one. A notch over
        // the tab bar or the padding is not a cell: tracking programs do
        // not hear 1;1. Shift scrolls locally, including on the alt screen.
        let (at, view_id) = match self.locate(self.cursor_pos) {
            Some((_, id, row, col)) => (Some(mouse::Cell { pane: id, row, col }), Some(id)),
            None => (None, self.focused_pane_id()),
        };
        let Some(id) = view_id else {
            return;
        };
        match mouse::Route::wheel(at, self.wheel_view(id), n, self.mods_now()) {
            Some(mouse::Wheel::Report { pane, bytes })
            | Some(mouse::Wheel::Arrows { pane, bytes }) => {
                self.write_pane(pane, &bytes);
            }
            Some(mouse::Wheel::Scroll { pane, lines }) => {
                if let Some((ti, pi)) = self.find_pane(pane) {
                    self.tabs[ti].panes[pi]
                        .session
                        .term
                        .lock()
                        .scroll_display(Scroll::Delta(lines));
                }
                self.link_cache.invalidate_pane(pane);
                self.request_redraw();
            }
            None => {}
        }
    }

    /// Quote for the shell this pane was spawned with. A name like $(calc)
    /// must arrive as a literal; a control character is not typed at all.
    /// vim, less, and a shell that is not at its prompt (cmd inside
    /// PowerShell never emits OSC 133 B) confirm first. A program that
    /// asked for bracketed paste gets the wrap.
    pub(crate) fn drop_file(&mut self, path: &std::path::Path) {
        let (arg, mode, at_prompt) = {
            let Some(session) = self.session() else {
                return;
            };
            let Some(arg) = term::dropped_path_arg(path, session.shell()) else {
                return;
            };
            // term, then meta — the lock order the reader uses too.
            let mode = *session.term.lock().mode();
            let at_prompt = session.meta.lock().unwrap().at_prompt;
            (arg, mode, at_prompt)
        };
        let alt = mode.contains(TermMode::ALT_SCREEN);
        if term::drop_needs_confirm(alt, at_prompt, &arg) {
            let why = if alt {
                term::PasteRisk::AltScreen
            } else {
                term::PasteRisk::NotAtPrompt
            };
            self.arm_pending_paste(arg, Some(why));
        } else if let Some(s) = self.session_mut() {
            let body = term::normalize_paste(&arg, mode.contains(TermMode::BRACKETED_PASTE));
            s.write(body.as_bytes());
        }
    }

    pub(crate) fn focus_changed(&mut self, focused: bool) {
        self.focused = focused;
        // A blur drops keys we were holding back. The matching release may
        // never arrive (the user alt-tabbed mid-chord), and a later focus
        // must not swallow an unrelated press.
        blur_forgets_held_keys(focused, &mut self.keys_held_away);
        if focused {
            // you're here now: the active tab's attention is served
            if let Some(t) = self.tabs.get_mut(self.active) {
                t.attention = false;
            }
            self.request_redraw();
        } else {
            // Alt+Tab with the button down must not keep dragging
            self.abandon_mouse();
        }
        // The event bit, not has_focus: that query can still say true
        // after Focused(false) and would drop CSI O.
        self.sync_focus(Some(focused));
    }

    pub(crate) fn modifiers_changed(&mut self, state: ModifiersState) {
        self.modifiers = state;
        self.refresh_link_hover(self.cursor_pos);
        if !self.modifiers.control_key() && self.mru.walking() {
            self.mru.release(self.active); // the Ctrl+Tab walk ends here
        }
    }

    pub(crate) fn resized(&mut self, px: PhysicalSize<u32>) {
        if px.width == 0 || px.height == 0 {
            return;
        }
        self.track_geometry();
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.resize(px.width, px.height);
        }
        self.relayout_active();
        self.request_redraw();
    }

    /// `WindowEvent::MouseInput`, after the guard and the modals.
    pub(crate) fn mouse_input(
        &mut self,
        state: ElementState,
        button: MouseButton,
        el: &ActiveEventLoop,
    ) {
        match state {
            ElementState::Pressed => match press_route(button) {
                Press::Right => self.right_press(),
                Press::Report(b) => {
                    let _ = self.try_report_press(b);
                }
                Press::Left => self.left_press(el),
                Press::Ignore => {}
            },
            ElementState::Released => self.mouse_release(button),
        }
    }

    /// Right pastes (it copies a selection only when copy-on-select is
    /// off), unless the program tracks the mouse and Shift is up. The
    /// release is reported or ignored in `mouse_release`.
    fn right_press(&mut self) {
        match self.right_click_owner() {
            Some((_, mouse::RightClick::Program)) => {
                let _ = self.try_report_press(mouse::Btn::Right);
            }
            Some((_, mouse::RightClick::Copy)) => {
                self.copy_selection(true);
                self.request_redraw();
            }
            Some((pane, mouse::RightClick::Paste)) => {
                self.focus_pane_at(pane);
                self.sync_focus(None);
                self.paste();
            }
            Some((_, mouse::RightClick::Blocked)) | None => {}
        }
    }

    /// BAYAN_PASTE: put `text` on the clipboard and right-click the centre
    /// of the focused pane, through the same `right_press` a real click
    /// takes. A pane whose program tracks the mouse hears the click instead.
    pub(crate) fn debug_right_click_paste(&mut self, text: String) {
        let Some(focused) = self.tabs.get(self.active).map(|t| t.focused) else {
            return;
        };
        let Some(&(x, y, w, h)) = self.pane_rects().get(focused) else {
            return;
        };
        self.set_clipboard(text);
        self.cursor_pos =
            PhysicalPosition::new(x as f64 + w as f64 / 2.0, y as f64 + h as f64 / 2.0);
        self.right_press();
    }

    /// The chrome first, in `chrome::left_press` order, then the terminal.
    fn left_press(&mut self, el: &ActiveEventLoop) {
        let pos = self.cursor_pos;
        match chrome::left_press(&self.chrome_hits(pos)) {
            LeftPress::Resize(dir) => {
                if let Some(w) = self.window.as_ref() {
                    let _ = w.drag_resize_window(dir);
                }
            }
            LeftPress::Caption(caption) => {
                if let Some(w) = self.window.clone() {
                    match caption {
                        Caption::Minimize => w.set_minimized(true),
                        Caption::Maximize => w.set_maximized(!w.is_maximized()),
                        // identical to CloseRequested: the running-command
                        // guard must not be bypassed just because the
                        // button is ours now
                        Caption::Close => self.request_close(el),
                    }
                }
            }
            LeftPress::Shortcuts => self.shortcuts_click(pos.x, pos.y),
            LeftPress::Settings => self.settings_click(pos.x, pos.y),
            LeftPress::TabControl(control) => {
                match chrome::tab_control(control, self.active, self.tabs.len()) {
                    Some(TabControl::Switch(i)) => self.switch_tab(i),
                    Some(TabControl::NewTab) => self.perform_action(keybinds::Action::NewTab, el),
                    None => {}
                }
                self.request_redraw();
            }
            LeftPress::OpenSettings => {
                self.settings = Some(0);
                self.request_redraw();
            }
            LeftPress::Cockpit(row) => {
                self.cockpit = false;
                if let Some(row) = row {
                    self.switch_tab(row);
                }
                self.request_redraw();
            }
            LeftPress::Divider(i) => self.drag_divider = Some(i),
            LeftPress::Tab(i) => self.switch_tab(i),
            LeftPress::DragStrip => {
                // the two gestures a real title bar owes the user
                if let Some(w) = self.window.as_ref() {
                    let now = Instant::now();
                    let dbl = chrome::strip_double_click(self.last_strip_click, now);
                    self.last_strip_click = Some(now);
                    if dbl {
                        w.set_maximized(!w.is_maximized());
                    } else {
                        let _ = w.drag_window();
                    }
                }
            }
            LeftPress::Terminal => self.terminal_left_press(),
        }
    }

    /// Link, program, or local selection. Shift wins over Ctrl. An overlay
    /// (including the refused-link card) is neither a link open nor a
    /// selection. The cell is the one Route::press reports.
    fn terminal_left_press(&mut self) {
        match self.left_click_owner() {
            link::ClickOwner::Blocked => return,
            link::ClickOwner::Link => {
                let _ = self.activate_link_at(self.cursor_pos);
                return;
            }
            link::ClickOwner::Program => {
                let _ = self.try_report_press(mouse::Btn::Left);
                return;
            }
            link::ClickOwner::Local => {}
        }
        if let Some((pi, row, col, side)) = self.pane_cell_at(self.cursor_pos) {
            self.start_selection(pi, row, col, side);
        }
    }

    /// A local press in pane `pi`: focus it and anchor a selection whose
    /// kind follows the click count.
    fn start_selection(&mut self, pi: usize, row: usize, col: usize, side: Side) {
        if let Some(t) = self.tabs.get_mut(self.active) {
            if t.focused != pi {
                t.focused = pi;
                self.update_title();
            }
        }
        self.sync_focus(None);
        self.sel_pane = pi;
        let n = self.clicks.click(Instant::now(), (row, col));
        let ty = selection_type(n);
        if let Some(s) = self.session() {
            let mut t = s.term.lock();
            let off = t.grid().display_offset() as i32;
            let point = Point::new(Line(row as i32 - off), Column(col));
            t.selection = Some(Selection::new(ty, point, side));
        }
        self.mouse.selecting = true;
        self.clear_link_hover();
        if n >= 2 && self.config.copy_on_select_on() {
            // word/line selections are complete on click
            self.copy_selection(false);
        }
        self.request_redraw();
    }

    fn mouse_release(&mut self, button: MouseButton) {
        if self.drag_divider.take().is_some() {
            return; // a resize drag copies nothing
        }
        if let Some(b) = button_of(button) {
            if self.release_mouse_report(b) {
                return;
            }
        }
        if button != MouseButton::Left {
            return;
        }
        self.mouse.selecting = false;
        // EasyTer convention: auto-copy the selection on release (the
        // settings panel can turn this off; explicit Ctrl+C / Ctrl+Shift+C
        // always copy)
        if self.config.copy_on_select_on() {
            self.copy_selection(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;

    #[test]
    fn right_and_left_route_apart_and_only_middle_reports_alone() {
        assert_eq!(press_route(MouseButton::Right), Press::Right);
        assert_eq!(press_route(MouseButton::Left), Press::Left);
        assert_eq!(
            press_route(MouseButton::Middle),
            Press::Report(mouse::Btn::Middle)
        );
        assert_eq!(press_route(MouseButton::Back), Press::Ignore);
        assert_eq!(press_route(MouseButton::Forward), Press::Ignore);
        assert_eq!(press_route(MouseButton::Other(7)), Press::Ignore);
        assert_eq!(button_of(MouseButton::Back), None);
        assert_eq!(button_of(MouseButton::Right), Some(mouse::Btn::Right));
    }

    #[test]
    fn a_divider_drag_beats_a_selection_which_beats_a_program_drag() {
        assert_eq!(motion_route(Some(1), true, true), Motion::Divider(1));
        assert_eq!(motion_route(None, true, true), Motion::Select);
        assert_eq!(motion_route(None, false, true), Motion::ProgramDrag);
        assert_eq!(motion_route(None, false, false), Motion::Hover);
    }

    #[test]
    fn edges_beat_dividers_which_beat_links() {
        assert_eq!(
            hover_cursor(Some(ResizeDirection::West), true, None, true),
            CursorIcon::from(ResizeDirection::West)
        );
        assert_eq!(
            hover_cursor(None, true, Some(Orientation::Column), true),
            CursorIcon::NsResize
        );
        assert_eq!(
            hover_cursor(None, true, Some(Orientation::Row), true),
            CursorIcon::EwResize
        );
        assert_eq!(hover_cursor(None, true, None, false), CursorIcon::EwResize);
        assert_eq!(hover_cursor(None, false, None, true), CursorIcon::Pointer);
        assert_eq!(hover_cursor(None, false, None, false), CursorIcon::Default);
    }

    #[test]
    fn click_counts_pick_simple_word_and_line() {
        assert_eq!(selection_type(1), SelectionType::Simple);
        assert_eq!(selection_type(2), SelectionType::Semantic);
        assert_eq!(selection_type(3), SelectionType::Lines);
        assert_eq!(selection_type(0), SelectionType::Simple);
    }

    #[test]
    fn wheel_notches_are_three_lines_and_pixels_divide_by_the_cell() {
        assert_eq!(
            wheel_lines(MouseScrollDelta::LineDelta(0.0, 1.0), 20.0),
            3.0
        );
        assert_eq!(
            wheel_lines(MouseScrollDelta::LineDelta(0.0, -2.0), 20.0),
            -6.0
        );
        let px = MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 30.0));
        assert_eq!(wheel_lines(px, 20.0), 1.5);
    }

    #[test]
    fn sub_line_deltas_accumulate_and_keep_their_sign() {
        let (n, rest) = take_whole_lines(0.0, 0.4);
        assert_eq!(n, 0);
        let (n, rest) = take_whole_lines(rest, 0.7);
        assert_eq!(n, 1);
        assert!((rest - 0.1).abs() < 1e-6);
        let (n, rest) = take_whole_lines(0.0, -2.5);
        assert_eq!(n, -2);
        assert!((rest + 0.5).abs() < 1e-6);
    }

    #[test]
    fn font_zoom_stays_in_range() {
        assert_eq!(zoomed(0.0, 3.0), 3.0);
        assert_eq!(zoomed(24.0, 3.0), 25.0);
        assert_eq!(zoomed(-6.0, -3.0), -7.0);
    }

    #[test]
    fn palette_wheel_moves_whole_rows_and_keeps_the_fraction() {
        assert_eq!(
            palette_wheel_rows(MouseScrollDelta::LineDelta(0.0, -1.0)),
            -1.0
        );
        let px = MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 45.0));
        assert_eq!(palette_wheel_rows(px), 1.5);
        // down two rows
        assert_eq!(palette_scroll(3, 0.0, -2.0, 10), (5, 0.0));
        // up past the top stops at row 0
        assert_eq!(palette_scroll(1, 0.0, 3.0, 10), (0, 0.0));
        // down past the end stops at the last row
        assert_eq!(palette_scroll(8, 0.0, -5.0, 10), (9, 0.0));
        // at most 12 rows a step
        assert_eq!(palette_scroll(0, 0.0, -40.0, 100), (12, 0.0));
        // half a row waits for the next half
        let (sel, rest) = palette_scroll(4, 0.0, 0.5, 10);
        assert_eq!((sel, rest), (4, 0.5));
        assert_eq!(palette_scroll(sel, rest, 0.5, 10), (3, 0.0));
        // a zero step changes nothing, not even an out-of-range sel
        assert_eq!(palette_scroll(20, 0.25, 0.0, 10), (20, 0.25));
        // a nonzero step that moves no whole row still clamps to the list
        assert_eq!(palette_scroll(20, 0.0, -0.5, 10), (9, -0.5));
    }

    #[test]
    fn a_palette_row_runs_only_on_a_release_over_the_pressed_row() {
        let rows = vec![('a', String::new()), ('b', String::new())];
        assert_eq!(palette_release(Some((1, 'b')), Some(1), &rows), Some('b'));
        assert_eq!(palette_release(Some((1, 'b')), Some(0), &rows), None);
        assert_eq!(palette_release(Some((1, 'b')), None, &rows), None);
        assert_eq!(palette_release(None, Some(1), &rows), None);
        // the list was refiltered under the press: same index, other action
        assert_eq!(palette_release(Some((1, 'c')), Some(1), &rows), None);
        assert_eq!(palette_release(Some((2, 'c')), Some(2), &rows), None);
    }
}
