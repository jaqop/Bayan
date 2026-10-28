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
use winit::event::{ElementState, KeyEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

use crate::input::zoomed;
use crate::{key_letter, keybinds, keyboard_zoom, link, App};

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
    ]
    .into_iter()
    .find_map(|(is_open, owner)| is_open.then_some(owner))
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

/// Ctrl+Tab forward, Ctrl+Shift+Tab back, in tab order. `len` > 0.
pub(crate) fn next_tab(active: usize, len: usize, back: bool) -> usize {
    if back {
        (active + len - 1) % len
    } else {
        (active + 1) % len
    }
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
        }
    }

    fn key_pressed(&mut self, event: &KeyEvent, el: &ActiveEventLoop) {
        // Assume the press is swallowed until bytes actually go out. Every
        // early return above the write leaves the key here, so its release
        // stays out of the PTY too.
        self.note_held_away(event);
        let m = self.modifiers;
        let (ctrl, shift, alt) = (m.control_key(), m.shift_key(), m.alt_key());
        crate::prof::mark(&format!(
            "key logical={:?} physical={:?} text={:?} ctrl={ctrl} shift={shift}",
            event.logical_key, event.physical_key, event.text
        ));
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
        self.type_key(event);
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
                    Some(true) => {
                        if let Some(pending) = self.pending_paste.take() {
                            self.send_paste(&pending.text);
                        }
                    }
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
        }
    }

    /// Esc dismisses. Enter dismisses a refusal and opens only a confirm
    /// card (userinfo, or a non-ASCII host).
    fn link_card_key(&mut self, event: &KeyEvent) {
        let enter = matches!(&event.logical_key, Key::Named(NamedKey::Enter));
        let escape = matches!(&event.logical_key, Key::Named(NamedKey::Escape));
        let may_open = self
            .blocked_link
            .as_ref()
            .is_some_and(|c| c.confirm_uri.is_some());
        match link::card_decision(enter, escape, may_open) {
            link::CardDecision::Open => {
                if let Some(card) = self.blocked_link.take() {
                    if let Some(uri) = card.confirm_uri {
                        self.spawn_link_open(link::Action::Browse(uri));
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
        self.update_title();
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
            let n = self.tabs.len();
            if self.config.tab_switch_recent() {
                // stack-like: the previous tab first, deeper while Ctrl
                // stays down; ModifiersChanged commits the stop
                if let Some(next) = self.mru.step(!shift, n) {
                    self.show_tab(next);
                }
            } else {
                self.switch_tab(next_tab(self.active, n, shift));
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
    fn type_key(&mut self, event: &KeyEvent) {
        let Some(bytes) = self.encode_key_event(event, false) else {
            return;
        };
        // typing snaps to the bottom and clears any selection; it also
        // restarts the blink window, cursor visible
        self.last_input = Instant::now();
        // The press reached the encoder. Drop it from the swallowed set
        // before borrowing the session.
        self.take_held_away(event);
        if let Some(s) = self.session_mut() {
            {
                let mut t = s.term.lock();
                t.selection = None;
                t.scroll_display(Scroll::Bottom);
            }
            s.write(&bytes);
        }
        self.invalidate_focused_links();
    }

    /// Key-up is invisible until the child asks for event types. Presses
    /// still own every shortcut; a release only reports, and only when the
    /// press itself reached the PTY.
    fn key_released(&mut self, event: &KeyEvent) {
        let held_away = self.take_held_away(event);
        if held_away || !self.term_mode().contains(TermMode::REPORT_EVENT_TYPES) {
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
    fn guards_beat_cards_which_beat_panels_and_search_comes_last() {
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
    }

    #[test]
    fn ctrl_tab_walks_tab_order_both_ways() {
        assert_eq!(next_tab(0, 3, false), 1);
        assert_eq!(next_tab(2, 3, false), 0);
        assert_eq!(next_tab(0, 3, true), 2);
        assert_eq!(next_tab(0, 1, true), 0);
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
