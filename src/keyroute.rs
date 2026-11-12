//! Keyboard routing: who a key press belongs to, and which window-level
//! chord it is before it is encoded for the PTY.
//!
//! Overlays own the keyboard in a fixed order (`key_owner`). With none
//! open, a press walks the palette and prompt-picker chords, the rebindable
//! keymap, pane focus, tab cycling, zoom, copy and scrollback paging, and
//! only then reaches `encode_key_event`. Every chord resolves by PHYSICAL
//! key, so an Arabic layout keeps them all. The byte encoding itself lives
//! in `keys.rs` and `kitty.rs`; the keymap in `keybinds.rs`.

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::TermMode;
use std::time::Instant;
use winit::event::{ElementState, Ime, KeyEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

use crate::input::zoomed;
use crate::{copymode, key_letter, keybinds, keyboard_zoom, link, App};

/// Which open overlay a key press belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyOwner {
    /// A pending close: Enter closes, Esc keeps.
    CloseGuard,
    /// A pending paste or drop: Enter sends, Esc drops.
    PasteGuard,
    /// A refused or confirm link card. Nothing is typed into the PTY.
    LinkCard,
    Posh,
    Palette,
    Shortcuts,
    Settings,
    Cockpit,
    Search,
    /// The focused pane is in copy mode (`copymode.rs`).
    CopyMode,
}

/// The overlays that are open right now.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Open {
    pub close_guard: bool,
    pub paste_guard: bool,
    pub link_card: bool,
    pub posh: bool,
    pub palette: bool,
    pub shortcuts: bool,
    pub settings: bool,
    pub cockpit: bool,
    pub search: bool,
    pub copy_mode: bool,
}

/// The guards first, then the link card, then the panels from the most
/// modal to the least. `None`: the press is the terminal's.
pub(crate) fn key_owner(open: &Open) -> Option<KeyOwner> {
    [
        (open.close_guard, KeyOwner::CloseGuard),
        (open.paste_guard, KeyOwner::PasteGuard),
        (open.link_card, KeyOwner::LinkCard),
        (open.posh, KeyOwner::Posh),
        (open.palette, KeyOwner::Palette),
        (open.shortcuts, KeyOwner::Shortcuts),
        (open.settings, KeyOwner::Settings),
        (open.cockpit, KeyOwner::Cockpit),
        (open.search, KeyOwner::Search),
        (open.copy_mode, KeyOwner::CopyMode),
    ]
    .into_iter()
    .find_map(|(is_open, owner)| is_open.then_some(owner))
}

/// Where IME-committed text goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitTo {
    Pane,
    Search,
    /// A guard, card or panel owns the keys: text must not reach the PTY
    /// behind it, and these overlays have no text field that takes it.
    Drop,
}

pub(crate) fn commit_target(owner: Option<KeyOwner>) -> CommitTo {
    match owner {
        None => CommitTo::Pane,
        Some(KeyOwner::Search) => CommitTo::Search,
        Some(
            KeyOwner::CloseGuard
            | KeyOwner::PasteGuard
            | KeyOwner::LinkCard
            | KeyOwner::Posh
            | KeyOwner::Palette
            | KeyOwner::Shortcuts
            | KeyOwner::Settings
            | KeyOwner::Cockpit
            // copy mode moves a cursor over the scrollback; nothing is typed
            | KeyOwner::CopyMode,
        ) => CommitTo::Drop,
    }
}

/// The cursor box last handed to `set_ime_cursor_area`, in physical pixels.
pub(crate) type ImeSpot = (i32, i32, i32, i32);

/// Windows reports a key the IME has already consumed as
/// `NamedKey::Process` (`VK_PROCESSKEY`). winit 0.30.13 still delivers
/// that `KeyboardInput` (winit#4508), often before `Ime::Preedit` stores
/// a composition string. The text arrives separately as preedit or
/// commit, so the key must not be encoded for the PTY.
pub(crate) fn is_ime_process_key(key: &Key) -> bool {
    matches!(key, Key::Named(NamedKey::Process))
}

/// Swallow a key the IME owns. A visible preedit owns every key (the
/// IME is composing). `NamedKey::Process` is swallowed even before that
/// string exists, which is when Windows emits it.
pub(crate) fn ime_swallows_key(composing: bool, key: &Key) -> bool {
    composing || is_ime_process_key(key)
}

/// A window event that may rebuild the IME context.
///
/// `ImeEnabled` is `WindowEvent::Ime(Ime::Enabled)`. `FocusGained` is
/// `WindowEvent::Focused(true)`, handled in `input::focus_changed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorAreaEvent {
    ImeEnabled,
    ImePreedit,
    ImeCommit,
    ImeDisabled,
    FocusGained,
    FocusLost,
}

/// Windows drops the composition window on these two events, so the
/// cursor box has to be sent again even when it has not moved
/// (winit#2569). Every other IME phase and a focus loss leave it.
pub(crate) fn cursor_area_reset(event: CursorAreaEvent) -> bool {
    matches!(
        event,
        CursorAreaEvent::ImeEnabled | CursorAreaEvent::FocusGained
    )
}

/// The rectangle to pass to `set_ime_cursor_area`, if a call is due.
///
/// An unchanged box is not sent again. `reset` comes from
/// [`cursor_area_reset`]: `Ime::Enabled` and a focus return both set it.
/// `None` current means there is no cell to anchor yet; the caller keeps
/// `reset` set.
pub(crate) fn ime_area_due(
    last_sent: Option<ImeSpot>,
    current: Option<ImeSpot>,
    reset: bool,
) -> Option<ImeSpot> {
    let current = current?;
    if reset || last_sent != Some(current) {
        Some(current)
    } else {
        None
    }
}

/// Enter is `Some(true)`, Esc `Some(false)`, anything else `None`.
pub(crate) fn enter_or_escape(key: &Key) -> Option<bool> {
    match key {
        Key::Named(NamedKey::Enter) => Some(true),
        Key::Named(NamedKey::Escape) => Some(false),
        _ => None,
    }
}

/// An overlay a fixed chord opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Opens {
    /// Ctrl+Shift+P
    Palette,
    /// Ctrl+Shift+O, O for oh-my-posh
    Posh,
}

/// Physical key, like every other chord here, so an Arabic layout does
/// not break it.
pub(crate) fn overlay_chord(key: PhysicalKey, ctrl: bool, shift: bool) -> Option<Opens> {
    if !(ctrl && shift) {
        return None;
    }
    match key {
        PhysicalKey::Code(KeyCode::KeyP) => Some(Opens::Palette),
        PhysicalKey::Code(KeyCode::KeyO) => Some(Opens::Posh),
        _ => None,
    }
}

/// A keymap Copy on plain Ctrl+C copies and clears in place instead of
/// running the action.
pub(crate) fn is_plain_ctrl_c_copy(action: keybinds::Action, chord: keybinds::Chord) -> bool {
    action == keybinds::Action::Copy
        && chord.is_plain_ctrl_letter()
        && chord.key == keybinds::ChordKey::Letter('c')
}

/// Alt+arrows move pane focus (EasyTer's binding): left and up go back,
/// right and down forward.
pub(crate) fn pane_step(key: &Key) -> Option<i32> {
    match key {
        Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowUp) => Some(-1),
        Key::Named(NamedKey::ArrowRight | NamedKey::ArrowDown) => Some(1),
        _ => None,
    }
}

/// `index` moved by `step` (±1) around a ring of `len`.
pub(crate) fn cycle(index: usize, step: i32, len: usize) -> usize {
    let n = len as i32;
    ((index as i32 + step + n) % n) as usize
}

/// Ctrl+0 resets the font zoom. The physical digit key, so it works on
/// the Arabic layout too.
pub(crate) fn is_zoom_reset(key: PhysicalKey, shift: bool) -> bool {
    !shift && matches!(key, PhysicalKey::Code(KeyCode::Digit0))
}

/// Ctrl+C copies when a selection exists (Windows convention); otherwise
/// it falls through to \x03. The caller checks the selection.
pub(crate) fn is_plain_ctrl_c(letter: Option<char>, shift: bool, alt: bool) -> bool {
    !shift && !alt && letter == Some('c')
}

/// Shift+PageUp pages back through the scrollback, Shift+PageDown forward.
pub(crate) fn page_direction(key: &Key) -> Option<i32> {
    match key {
        Key::Named(NamedKey::PageUp) => Some(1),
        Key::Named(NamedKey::PageDown) => Some(-1),
        _ => None,
    }
}

impl App {
    /// `WindowEvent::KeyboardInput`.
    pub(crate) fn key_input(&mut self, event: &KeyEvent, el: &ActiveEventLoop) {
        match event.state {
            ElementState::Pressed => self.key_pressed(event, el),
            ElementState::Released => self.key_released(event),
        }
    }

    fn open_overlays(&self) -> Open {
        Open {
            close_guard: self.pending_close.is_some(),
            paste_guard: self.pending_paste.is_some(),
            link_card: self.blocked_link.is_some(),
            posh: self.posh.is_some(),
            palette: self.palette.is_some(),
            shortcuts: self.shortcuts.is_some(),
            settings: self.settings.is_some(),
            cockpit: self.cockpit,
            search: self.search.is_some(),
            copy_mode: self.term_mode().contains(TermMode::VI),
        }
    }

    /// IME composition and commits: CJK input, the emoji panel, dictation.
    pub(crate) fn ime_input(&mut self, ime: Ime) {
        let area = match &ime {
            Ime::Enabled => CursorAreaEvent::ImeEnabled,
            Ime::Preedit(_, _) => CursorAreaEvent::ImePreedit,
            Ime::Commit(_) => CursorAreaEvent::ImeCommit,
            Ime::Disabled => CursorAreaEvent::ImeDisabled,
        };
        // Only Enabled rebuilds the IME context. The other phases leave
        // the candidate window where it is (winit#2569).
        self.note_cursor_area(area);
        match ime {
            Ime::Preedit(text, _) => self.set_preedit(Some(text)),
            Ime::Disabled => self.set_preedit(None),
            Ime::Enabled => {}
            Ime::Commit(text) => {
                self.set_preedit(None);
                match commit_target(key_owner(&self.open_overlays())) {
                    CommitTo::Pane => self.send_text(text.as_bytes()),
                    CommitTo::Search => self.search_push(&text),
                    CommitTo::Drop => {}
                }
            }
        }
        self.request_redraw();
    }

    /// Ask for `set_ime_cursor_area` again when `event` rebuilt the context.
    pub(crate) fn note_cursor_area(&mut self, event: CursorAreaEvent) {
        if cursor_area_reset(event) {
            self.ime_area_reset = true;
        }
    }

    fn set_preedit(&mut self, text: Option<String>) {
        if let Some(r) = self.renderer.as_mut() {
            r.preedit = text.filter(|t| !t.is_empty());
        }
    }

    fn composing(&self) -> bool {
        self.renderer.as_ref().is_some_and(|r| r.preedit.is_some())
    }

    /// Committed text to the focused pane and the broadcast targets, as
    /// typing would send it: snap to the bottom, drop the selection.
    fn send_text(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.last_input = Instant::now();
        let mut targets = self.broadcast_targets();
        if let Some(t) = self.tabs.get(self.active) {
            targets.push(t.focused);
        }
        for pi in targets {
            let Some(pane) = self.tabs[self.active].panes.get_mut(pi) else {
                continue;
            };
            {
                let mut t = pane.session.term.lock();
                t.selection = None;
                t.scroll_display(Scroll::Bottom);
            }
            pane.session.write(bytes);
        }
        self.invalidate_focused_links();
    }

    fn key_pressed(&mut self, event: &KeyEvent, el: &ActiveEventLoop) {
        let arrived = Instant::now();
        // Assume the press is swallowed until bytes actually go out. Every
        // early return above the write leaves the key here, so its release
        // stays out of the PTY too.
        self.note_held_away(event);
        // Preedit owns every key. `NamedKey::Process` is the IME's too,
        // and it arrives before the preedit string does (winit#4508).
        if ime_swallows_key(self.composing(), &event.logical_key) {
            return;
        }
        let m = self.modifiers;
        let (ctrl, shift, alt) = (m.control_key(), m.shift_key(), m.alt_key());
        // BAYAN-010: that a key went down, and its modifiers — never which
        // key. The logical key, `text` and even the physical key code
        // (KeyA, KeyP…) spell out what was TYPED, passwords at an ssh or
        // sudo prompt included, and must not reach a log file.
        if crate::prof::on() {
            crate::prof::mark(&format!("key ctrl={ctrl} shift={shift} alt={alt}"));
        }
        if let Some(owner) = key_owner(&self.open_overlays()) {
            self.overlay_key(owner, event, shift, el);
            return;
        }
        match overlay_chord(event.physical_key, ctrl, shift) {
            Some(Opens::Palette) => {
                self.open_palette(String::new());
                self.request_redraw();
                return;
            }
            Some(Opens::Posh) => {
                self.open_posh();
                return;
            }
            None => {}
        }
        if self.keymap_key(event, ctrl, shift, alt, el) {
            return;
        }
        let key = &event.logical_key;
        if alt && !ctrl {
            if let Some(step) = pane_step(key) {
                if self.step_pane_focus(step) {
                    return;
                }
            }
        } else if ctrl && self.ctrl_key(event, shift, alt) {
            return;
        }
        // by the focused pane's REAL height (self.size is only the spawn grid)
        if shift && !ctrl {
            if let Some(dir) = page_direction(key) {
                let rows = self
                    .session()
                    .map_or(self.size.rows, |s| s.term.lock().screen_lines())
                    as i32;
                self.scroll_lines(dir * rows);
                return;
            }
        }
        self.type_key(event, arrived);
    }

    fn overlay_key(
        &mut self,
        owner: KeyOwner,
        event: &KeyEvent,
        shift: bool,
        el: &ActiveEventLoop,
    ) {
        match owner {
            KeyOwner::CloseGuard => {
                if let Some(close) = enter_or_escape(&event.logical_key) {
                    self.resolve_close(close, el);
                }
                self.request_redraw();
            }
            KeyOwner::PasteGuard => {
                match enter_or_escape(&event.logical_key) {
                    Some(true) => self.confirm_pending(),
                    Some(false) => self.pending_paste = None,
                    None => {}
                }
                self.request_redraw();
            }
            KeyOwner::LinkCard => self.link_card_key(event),
            KeyOwner::Posh => self.posh_input(event),
            KeyOwner::Palette => self.palette_input(event, el),
            KeyOwner::Shortcuts => self.shortcuts_input(event, el),
            KeyOwner::Settings => self.settings_input(event),
            KeyOwner::Cockpit => self.cockpit_input(event),
            KeyOwner::Search => self.search_input(event, shift),
            KeyOwner::CopyMode => self.copy_mode_key(event, shift),
        }
    }

    /// Ctrl+Shift+X: the focused pane enters copy mode with the cursor where
    /// the shell's is, or leaves it.
    pub(crate) fn toggle_copy_mode(&mut self) {
        let entering = !self.term_mode().contains(TermMode::VI);
        if entering {
            if let Some(id) = self.focused_pane_id() {
                self.snap_smooth(id);
            }
            if let Some(s) = self.session() {
                let mut t = s.term.lock();
                t.selection = None;
                t.toggle_vi_mode();
            }
            self.clear_link_hover();
            self.request_redraw();
        } else {
            self.exit_copy_mode();
        }
    }

    /// Leave copy mode: the selection goes and the view returns to the
    /// prompt, where typing would have taken it anyway.
    fn exit_copy_mode(&mut self) {
        if let Some(id) = self.focused_pane_id() {
            self.drop_smooth(id);
        }
        if let Some(s) = self.session() {
            let mut t = s.term.lock();
            if t.mode().contains(TermMode::VI) {
                t.toggle_vi_mode();
            }
            t.selection = None;
            t.scroll_display(Scroll::Bottom);
        }
        self.invalidate_focused_links();
        self.request_redraw();
    }

    /// A press while the focused pane is in copy mode. Nothing reaches the
    /// PTY: a key copy mode does not know is swallowed.
    fn copy_mode_key(&mut self, event: &KeyEvent, shift: bool) {
        use copymode::Cmd;
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        // The copy-clean chord yanks and leaves even when `copy.clean` is
        // off, and even though copy mode would otherwise swallow it. `y`,
        // Enter and Ctrl+C stay on copy_selection, so the setting (including
        // claude_mode) covers an ordinary copy-mode yank.
        if let Some(chord) = keybinds::chord_from(event.physical_key, ctrl, shift, alt) {
            if keybinds::lookup(&self.keymap, chord) == Some(keybinds::Action::CopyClean) {
                self.copy_selection_clean();
                self.exit_copy_mode();
                return;
            }
        }
        let rtl = self.copy_cursor_rtl();
        let Some(cmd) = copymode::command(event.physical_key, &event.logical_key, ctrl, shift, rtl)
        else {
            return;
        };
        match cmd {
            Cmd::CopyAndExit => {
                self.copy_selection(false);
                self.exit_copy_mode();
                return;
            }
            Cmd::Exit => {
                self.exit_copy_mode();
                return;
            }
            _ => {
                if let Some(id) = self.focused_pane_id() {
                    self.snap_smooth(id);
                }
            }
        }
        if let Some(s) = self.session() {
            copymode::apply(&mut s.term.lock(), cmd);
        }
        self.invalidate_focused_links();
        self.request_redraw();
    }

    /// Whether the copy-mode cursor sits on a cell the last frame laid right
    /// to left. It asks the shaped layout, never the column index, because on
    /// an RTL row the logical column is not the visual x.
    fn copy_cursor_rtl(&self) -> bool {
        let (Some(r), Some(t)) = (self.renderer.as_ref(), self.tabs.get(self.active)) else {
            return false;
        };
        let Some(rect) = self.pane_rects().get(t.focused).copied() else {
            return false;
        };
        let Some(s) = self.session() else {
            return false;
        };
        let (row, col) = {
            let t = s.term.lock();
            let p = t.vi_mode_cursor.point;
            (p.line.0 + t.grid().display_offset() as i32, p.column.0)
        };
        row >= 0 && r.cell_rtl(rect, row as usize, col)
    }

    /// Esc dismisses. Enter dismisses a refusal and opens only a confirm
    /// card (userinfo, or a non-ASCII host).
    fn link_card_key(&mut self, event: &KeyEvent) {
        let enter = matches!(&event.logical_key, Key::Named(NamedKey::Enter));
        let escape = matches!(&event.logical_key, Key::Named(NamedKey::Escape));
        let may_open = self
            .blocked_link
            .as_ref()
            .is_some_and(|c| c.confirm.is_some());
        match link::card_decision(enter, escape, may_open) {
            link::CardDecision::Open => {
                if let Some(card) = self.blocked_link.take() {
                    if let Some(open) = card.confirm {
                        let action = link::confirm_enter(&open);
                        if link::confirm_enter_settles(&open) {
                            self.spawn_link_open(action);
                        } else if let link::Action::Reveal(path) = action {
                            // Direct explorer /select. settle would classify
                            // config.json as an association open.
                            self.spawn_reveal_only(&path);
                        }
                    }
                }
                self.request_redraw();
            }
            link::CardDecision::Dismiss => {
                self.blocked_link = None;
                self.request_redraw();
            }
            link::CardDecision::Ignore => {}
        }
    }

    /// The rebindable shortcuts (Ctrl+, ⌨ edits them). Plain Ctrl+letters
    /// belong to TUIs, except clipboard paste. True when one ran.
    fn keymap_key(
        &mut self,
        event: &KeyEvent,
        ctrl: bool,
        shift: bool,
        alt: bool,
        el: &ActiveEventLoop,
    ) -> bool {
        let Some(chord) = keybinds::chord_from(event.physical_key, ctrl, shift, alt) else {
            return false;
        };
        let alt_screen = self.term_mode().contains(TermMode::ALT_SCREEN);
        let Some(action) = keybinds::resolve(&self.keymap, chord, alt_screen, self.has_selection())
        else {
            return false;
        };
        if is_plain_ctrl_c_copy(action, chord) {
            self.copy_selection(true);
            self.request_redraw();
        } else {
            self.perform_action(action, el);
        }
        true
    }

    /// Alt+arrow. True when the tab has more than one pane to move between.
    fn step_pane_focus(&mut self, step: i32) -> bool {
        let Some(t) = self.tabs.get_mut(self.active) else {
            return false;
        };
        if t.panes.len() <= 1 {
            return false;
        }
        t.focused = cycle(t.focused, step, t.panes.len());
        // zoomed, the zoom follows focus: the newly focused pane is the
        // one that fills the tab, and it is resized to that rect
        let zoomed = t.zoomed;
        if zoomed {
            self.relayout_active();
        }
        self.sync_focus(None);
        self.request_redraw();
        true
    }

    /// The window's own Ctrl chords: tab cycling, zoom, copy. True when
    /// one ran.
    fn ctrl_key(&mut self, event: &KeyEvent, shift: bool, alt: bool) -> bool {
        // Ctrl+Tab / Ctrl+Shift+Tab cycle tabs (shift handled here because
        // winit reports Tab+shift with the shift modifier)
        if matches!(event.logical_key, Key::Named(NamedKey::Tab)) && !self.tabs.is_empty() {
            // both orders stay inside the group on screen
            let (vis, pos) = self.visible_tabs();
            if self.config.tab_switch_recent() {
                // stack-like: the previous tab first, deeper while Ctrl
                // stays down; ModifiersChanged commits the stop
                if let Some(next) = self.mru.step_among(!shift, self.tabs.len(), &vis) {
                    self.show_tab(next);
                }
            } else if !vis.is_empty() {
                self.switch_tab(vis[cycle(pos, if shift { -1 } else { 1 }, vis.len())]);
            }
            return true;
        }
        if let Some(step) = keyboard_zoom(event.physical_key, true, alt, shift) {
            self.font_delta = zoomed(self.font_delta, step as f32);
            self.rebuild_renderer();
            return true;
        }
        if is_zoom_reset(event.physical_key, shift) {
            self.font_delta = 0.0;
            self.rebuild_renderer();
            return true;
        }
        if is_plain_ctrl_c(key_letter(event), shift, alt) && self.has_selection() {
            self.copy_selection(true);
            self.request_redraw();
            return true;
        }
        false
    }

    /// Legacy Ctrl+letter still resolves by PHYSICAL key inside
    /// encode_input (Arabic: Ctrl+C interrupts even though the C key's
    /// logical char is "ؤ"). A child that pushed kitty flags gets CSI u
    /// instead, from the same call.
    /// `arrived`: when winit delivered the key, for `BAYAN_PROFILE` latency.
    fn type_key(&mut self, event: &KeyEvent, arrived: Instant) {
        let Some(bytes) = self.encode_key_event(event, false) else {
            return;
        };
        // typing snaps to the bottom and clears any selection; it also
        // restarts the blink window, cursor visible
        self.last_input = Instant::now();
        // The press reached the encoder. Drop it from the swallowed set
        // before borrowing the session.
        self.take_held_away(event);
        // Broadcast: every other pane of the tab gets the key too, encoded
        // for its own modes, and snaps to its bottom like the focused one.
        for pi in self.broadcast_targets() {
            let mode = *self.tabs[self.active].panes[pi].session.term.lock().mode();
            let Some(other) = self.encode_key_event_for(event, false, mode) else {
                continue;
            };
            let id = self.tabs[self.active].panes[pi].id;
            self.drop_smooth(id);
            let session = &mut self.tabs[self.active].panes[pi].session;
            {
                let mut t = session.term.lock();
                t.selection = None;
                t.scroll_display(Scroll::Bottom);
            }
            session.write(&other);
        }
        if let Some(id) = self.focused_pane_id() {
            self.drop_smooth(id);
        }
        if let Some(s) = self.session_mut() {
            {
                let mut t = s.term.lock();
                t.selection = None;
                t.scroll_display(Scroll::Bottom);
            }
            s.write(&bytes);
            crate::prof::key_sent(arrived);
        }
        self.invalidate_focused_links();
    }

    /// Key-up is invisible until the child asks for event types. Presses
    /// still own every shortcut; a release only reports, and only when the
    /// press itself reached the PTY.
    fn key_released(&mut self, event: &KeyEvent) {
        // A Process release must not be reported either: the press was
        // the IME's, and a TUI that asked for key-up would see a release
        // whose press it never got.
        if is_ime_process_key(&event.logical_key) {
            let _ = self.take_held_away(event);
            return;
        }
        let held_away = self.take_held_away(event);
        if held_away {
            return;
        }
        // a broadcast press owes its release to every pane that asked for
        // event types, not only the focused one
        for pi in self.broadcast_targets() {
            let mode = *self.tabs[self.active].panes[pi].session.term.lock().mode();
            if !mode.contains(TermMode::REPORT_EVENT_TYPES) {
                continue;
            }
            if let Some(bytes) = self.encode_key_event_for(event, true, mode) {
                self.tabs[self.active].panes[pi].session.write(&bytes);
            }
        }
        if !self.term_mode().contains(TermMode::REPORT_EVENT_TYPES) {
            return;
        }
        if let Some(bytes) = self.encode_key_event(event, true) {
            if let Some(s) = self.session_mut() {
                s.write(&bytes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::SmolStr;

    #[test]
    fn ime_text_reaches_the_pane_or_search_and_nothing_behind_a_guard() {
        assert_eq!(commit_target(None), CommitTo::Pane);
        assert_eq!(commit_target(Some(KeyOwner::Search)), CommitTo::Search);
        for owner in [
            KeyOwner::CloseGuard,
            KeyOwner::PasteGuard,
            KeyOwner::LinkCard,
            KeyOwner::Palette,
            KeyOwner::Settings,
            KeyOwner::CopyMode,
        ] {
            assert_eq!(commit_target(Some(owner)), CommitTo::Drop, "{owner:?}");
        }
        // a pending paste beats search: the commit must not slip past it
        let both = Open {
            paste_guard: true,
            search: true,
            ..Open::default()
        };
        assert_eq!(commit_target(key_owner(&both)), CommitTo::Drop);
    }

    #[test]
    fn ime_process_keys_are_swallowed_and_ordinary_letters_are_not() {
        let process = Key::Named(NamedKey::Process);
        let arabic = Key::Character(SmolStr::new("ب"));
        let ctrl_letter = Key::Character(SmolStr::new("ؤ"));
        // Process is the IME's even before a preedit string exists, so it
        // is not encoded beside the later Commit.
        assert!(is_ime_process_key(&process));
        assert!(ime_swallows_key(false, &process));
        assert!(ime_swallows_key(true, &process));
        // a visible composition still owns every other key
        assert!(ime_swallows_key(true, &arabic));
        // Arabic letters and the logical char of an Arabic-layout Ctrl
        // chord are not Process, so they keep the normal key path
        assert!(!is_ime_process_key(&arabic));
        assert!(!ime_swallows_key(false, &arabic));
        assert!(!is_ime_process_key(&ctrl_letter));
        assert!(!ime_swallows_key(false, &ctrl_letter));
        assert!(!ime_swallows_key(false, &Key::Named(NamedKey::Enter)));
    }

    #[test]
    fn ime_cursor_area_is_sent_again_after_the_context_is_rebuilt() {
        let spot = (40, 80, 10, 20);
        let moved = (50, 80, 10, 20);
        // unchanged and no reset: winit already has this rectangle
        assert_eq!(ime_area_due(Some(spot), Some(spot), false), None);
        // Ime::Enabled or a focus return: Windows dropped it, send again
        assert_eq!(ime_area_due(Some(spot), Some(spot), true), Some(spot));
        // a real move is sent whether or not the context was rebuilt
        assert_eq!(ime_area_due(Some(spot), Some(moved), false), Some(moved));
        assert_eq!(ime_area_due(None, Some(spot), false), Some(spot));
        // nothing to anchor yet: the caller keeps the reset flag
        assert_eq!(ime_area_due(Some(spot), None, true), None);
    }

    /// Both context rebuilds ask for the candidate window again, and the
    /// other IME phases and a blur do not. The handlers call
    /// `cursor_area_reset` with these same variants: `Ime::Enabled` in
    /// `ime_input`, and `Focused(true)` in `input::focus_changed`.
    #[test]
    fn ime_enabled_and_focus_regained_both_resend_the_cursor_area() {
        let spot = (12, 48, 9, 18);
        // already on screen, and the cell has not moved
        assert_eq!(ime_area_due(Some(spot), Some(spot), false), None);
        for event in [CursorAreaEvent::ImeEnabled, CursorAreaEvent::FocusGained] {
            assert!(
                cursor_area_reset(event),
                "{event:?} rebuilds the IME context"
            );
            assert_eq!(
                ime_area_due(Some(spot), Some(spot), cursor_area_reset(event)),
                Some(spot),
                "{event:?} re-sends the same cursor box"
            );
        }
        for event in [
            CursorAreaEvent::ImePreedit,
            CursorAreaEvent::ImeCommit,
            CursorAreaEvent::ImeDisabled,
            CursorAreaEvent::FocusLost,
        ] {
            assert!(
                !cursor_area_reset(event),
                "{event:?} must leave the candidate window where it is"
            );
        }
    }

    #[test]
    fn guards_beat_cards_which_beat_panels_and_copy_mode_comes_last() {
        assert_eq!(key_owner(&Open::default()), None);
        let all = Open {
            close_guard: true,
            paste_guard: true,
            link_card: true,
            posh: true,
            palette: true,
            shortcuts: true,
            settings: true,
            cockpit: true,
            search: true,
            copy_mode: true,
        };
        let order = [
            KeyOwner::CloseGuard,
            KeyOwner::PasteGuard,
            KeyOwner::LinkCard,
            KeyOwner::Posh,
            KeyOwner::Palette,
            KeyOwner::Shortcuts,
            KeyOwner::Settings,
            KeyOwner::Cockpit,
            KeyOwner::Search,
            KeyOwner::CopyMode,
        ];
        // close each owner in turn: the next one in line takes over
        let mut open = all;
        for (i, owner) in order.iter().enumerate() {
            assert_eq!(key_owner(&open), Some(*owner), "step {i}");
            match owner {
                KeyOwner::CloseGuard => open.close_guard = false,
                KeyOwner::PasteGuard => open.paste_guard = false,
                KeyOwner::LinkCard => open.link_card = false,
                KeyOwner::Posh => open.posh = false,
                KeyOwner::Palette => open.palette = false,
                KeyOwner::Shortcuts => open.shortcuts = false,
                KeyOwner::Settings => open.settings = false,
                KeyOwner::Cockpit => open.cockpit = false,
                KeyOwner::Search => open.search = false,
                KeyOwner::CopyMode => open.copy_mode = false,
            }
        }
        assert_eq!(key_owner(&open), None);
    }

    #[test]
    fn only_enter_and_escape_answer_a_guard() {
        assert_eq!(enter_or_escape(&Key::Named(NamedKey::Enter)), Some(true));
        assert_eq!(enter_or_escape(&Key::Named(NamedKey::Escape)), Some(false));
        assert_eq!(enter_or_escape(&Key::Named(NamedKey::Space)), None);
        assert_eq!(enter_or_escape(&Key::Character(SmolStr::new("y"))), None);
    }

    #[test]
    fn palette_and_picker_chords_need_ctrl_shift_and_the_physical_key() {
        let p = PhysicalKey::Code(KeyCode::KeyP);
        let o = PhysicalKey::Code(KeyCode::KeyO);
        assert_eq!(overlay_chord(p, true, true), Some(Opens::Palette));
        assert_eq!(overlay_chord(o, true, true), Some(Opens::Posh));
        assert_eq!(overlay_chord(p, true, false), None);
        assert_eq!(overlay_chord(p, false, true), None);
        assert_eq!(
            overlay_chord(PhysicalKey::Code(KeyCode::KeyQ), true, true),
            None
        );
    }

    #[test]
    fn only_a_plain_ctrl_c_bound_to_copy_copies_in_place() {
        let chord = |ctrl, shift, key| keybinds::Chord {
            ctrl,
            shift,
            alt: false,
            key,
        };
        let c = keybinds::ChordKey::Letter('c');
        let copy = keybinds::Action::Copy;
        assert!(is_plain_ctrl_c_copy(copy, chord(true, false, c)));
        assert!(!is_plain_ctrl_c_copy(copy, chord(true, true, c)));
        assert!(!is_plain_ctrl_c_copy(
            copy,
            chord(true, false, keybinds::ChordKey::Letter('x'))
        ));
        assert!(!is_plain_ctrl_c_copy(
            keybinds::Action::NewTab,
            chord(true, false, c)
        ));
    }

    #[test]
    fn alt_arrows_step_and_wrap_around_the_panes() {
        assert_eq!(pane_step(&Key::Named(NamedKey::ArrowLeft)), Some(-1));
        assert_eq!(pane_step(&Key::Named(NamedKey::ArrowUp)), Some(-1));
        assert_eq!(pane_step(&Key::Named(NamedKey::ArrowRight)), Some(1));
        assert_eq!(pane_step(&Key::Named(NamedKey::ArrowDown)), Some(1));
        assert_eq!(pane_step(&Key::Named(NamedKey::Home)), None);
        assert_eq!(cycle(0, -1, 3), 2);
        assert_eq!(cycle(2, 1, 3), 0);
        assert_eq!(cycle(1, 1, 3), 2);
        assert_eq!(cycle(0, -1, 1), 0); // Ctrl+Shift+Tab with one tab
    }

    #[test]
    fn zoom_reset_is_the_physical_zero_without_shift() {
        let zero = PhysicalKey::Code(KeyCode::Digit0);
        assert!(is_zoom_reset(zero, false));
        assert!(!is_zoom_reset(zero, true));
        assert!(!is_zoom_reset(PhysicalKey::Code(KeyCode::Numpad0), false));
    }

    #[test]
    fn ctrl_c_copies_only_without_shift_or_alt() {
        assert!(is_plain_ctrl_c(Some('c'), false, false));
        assert!(!is_plain_ctrl_c(Some('c'), true, false));
        assert!(!is_plain_ctrl_c(Some('c'), false, true));
        assert!(!is_plain_ctrl_c(Some('v'), false, false));
        assert!(!is_plain_ctrl_c(None, false, false));
    }

    #[test]
    fn page_keys_page_up_into_history_and_down_out_of_it() {
        assert_eq!(page_direction(&Key::Named(NamedKey::PageUp)), Some(1));
        assert_eq!(page_direction(&Key::Named(NamedKey::PageDown)), Some(-1));
        assert_eq!(page_direction(&Key::Named(NamedKey::End)), None);
    }
}
