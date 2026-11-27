//! Copy mode (Ctrl+Shift+X, WezTerm's chord): select and copy scrollback
//! from the keyboard alone. It runs on alacritty_terminal's vi mode, which
//! already owns the cursor, the motions, the viewport following the cursor
//! and the selection tracking it.
//!
//! Keys resolve by PHYSICAL key, so an Arabic layout keeps them (the same
//! rule as every chord in `keybinds.rs`). The arrows and h/l use the
//! painted cell order when the renderer has reordered the row.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::Term;
use alacritty_terminal::vi_mode::ViMotion;
use winit::keyboard::{Key, NamedKey, PhysicalKey};

/// What one key press does in copy mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cmd {
    Motion(ViMotion),
    /// v, Shift+V, Ctrl+V: start this kind of selection at the cursor,
    /// switch an existing one to it, or drop it when it is already this kind.
    Select(SelectionType),
    /// g: the first line of the scrollback
    Top,
    /// Shift+G: the last line
    Bottom,
    PageUp,
    PageDown,
    /// y, Enter, Ctrl+C
    CopyAndExit,
    /// Esc, q
    Exit,
}

/// The command for a press, or None for a key copy mode ignores. Copy
/// mode is modal: an ignored key is swallowed, never typed into the shell.
pub(crate) fn command(
    physical: PhysicalKey,
    logical: &Key,
    ctrl: bool,
    shift: bool,
) -> Option<Cmd> {
    use Cmd::*;
    use ViMotion as M;
    if let Key::Named(named) = logical {
        let cmd = match named {
            NamedKey::Escape => Exit,
            NamedKey::Enter => CopyAndExit,
            NamedKey::ArrowUp => Motion(M::Up),
            NamedKey::ArrowDown => Motion(M::Down),
            NamedKey::ArrowLeft => Motion(M::Left),
            NamedKey::ArrowRight => Motion(M::Right),
            NamedKey::Home if ctrl => Top,
            NamedKey::End if ctrl => Bottom,
            NamedKey::Home => Motion(M::First),
            NamedKey::End => Motion(M::Last),
            NamedKey::PageUp => PageUp,
            NamedKey::PageDown => PageDown,
            _ => return None,
        };
        return Some(cmd);
    }
    let PhysicalKey::Code(code) = physical else {
        return None;
    };
    let c = crate::keys::pc101_unshifted(code)?;
    if ctrl {
        return match c {
            'c' => Some(CopyAndExit),
            'v' => Some(Select(SelectionType::Block)),
            _ => None,
        };
    }
    Some(match (c, shift) {
        ('q', _) => Exit,
        ('y', _) => CopyAndExit,
        ('v', false) => Select(SelectionType::Simple),
        ('v', true) => Select(SelectionType::Lines),
        ('h', false) => Motion(M::Left),
        ('l', false) => Motion(M::Right),
        ('j', false) => Motion(M::Down),
        ('k', false) => Motion(M::Up),
        ('h', true) => Motion(M::High),
        ('m', true) => Motion(M::Middle),
        ('l', true) => Motion(M::Low),
        ('w', false) => Motion(M::SemanticRight),
        ('w', true) => Motion(M::WordRight),
        ('b', false) => Motion(M::SemanticLeft),
        ('b', true) => Motion(M::WordLeft),
        ('e', false) => Motion(M::SemanticRightEnd),
        ('e', true) => Motion(M::WordRightEnd),
        ('0', false) => Motion(M::First),
        ('4', true) => Motion(M::Last),          // $
        ('6', true) => Motion(M::FirstOccupied), // ^
        ('5', true) => Motion(M::Bracket),       // %
        ('g', false) => Top,
        ('g', true) => Bottom,
        _ => return None,
    })
}

/// Run a command on a term in vi mode. CopyAndExit and Exit need the
/// clipboard and the window, so the caller handles them; here they do nothing.
pub(crate) fn apply<T: EventListener>(t: &mut Term<T>, cmd: Cmd) {
    match cmd {
        Cmd::Motion(m) => t.vi_motion(m),
        Cmd::Select(ty) => {
            let at = t.vi_mode_cursor.point;
            match t.selection.as_ref().map(|sel| sel.ty == ty) {
                Some(true) => t.selection = None,
                // same anchor, new kind (vim's v then V)
                Some(false) => {
                    if let Some(sel) = t.selection.as_mut() {
                        sel.ty = ty;
                    }
                }
                None => {
                    let mut sel = Selection::new(ty, at, Side::Left);
                    // a fresh selection is empty, and vi motions only extend
                    // a non-empty one: take the cursor cell now
                    sel.include_all();
                    t.selection = Some(sel);
                }
            }
            return;
        }
        Cmd::Top => {
            let p = Point::new(t.topmost_line(), Column(0));
            t.vi_goto_point(p);
        }
        Cmd::Bottom => {
            let p = Point::new(t.bottommost_line(), Column(0));
            t.vi_goto_point(p);
        }
        Cmd::PageUp => t.scroll_display(Scroll::PageUp),
        Cmd::PageDown => t.scroll_display(Scroll::PageDown),
        Cmd::CopyAndExit | Cmd::Exit => {}
    }
    snap_to_arabic_text(t);
}

pub(crate) fn move_horizontal<T: EventListener>(
    t: &mut Term<T>,
    layout: Option<(&crate::render::Renderer, crate::render::Rect)>,
    motion: ViMotion,
) {
    let target = layout.and_then(|(renderer, pane)| {
        renderer.visual_horizontal_target(pane, t, motion == ViMotion::Right)
    });
    if let Some(point) = target {
        t.vi_goto_point(point);
    } else {
        apply(t, Cmd::Motion(motion));
    }
}

/// A cursor past the last character of a row the renderer shapes as
/// Arabic moves back onto that character. The shaped path lays the row out
/// to the cursor's column, and in a right-to-left line those trailing
/// blanks go visually LEFT, so the text would jump toward the cursor and
/// the block would land at the far edge. Grid rows keep any column.
fn snap_to_arabic_text<T: EventListener>(t: &mut Term<T>) {
    let at = t.vi_mode_cursor.point;
    let row = &t.grid()[at.line];
    let cols = t.columns();
    if !(0..cols).any(|c| crate::render::is_rtl(row[Column(c)].c)) {
        return;
    }
    let Some(last) = (0..cols).rev().find(|&c| row[Column(c)].c != ' ') else {
        return;
    };
    if at.column.0 > last {
        t.vi_goto_point(Point::new(at.line, Column(last)));
    }
}

/// The bar text while copy mode is on: the keys that leave it.
pub(crate) fn hint() -> &'static str {
    crate::lang::tr(
        "وضع النسخ: v تحديد · y نسخ · Esc خروج",
        "Copy mode: v select · y copy · Esc exit",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::{KeyCode, SmolStr};

    fn letter(code: KeyCode, ctrl: bool, shift: bool) -> Option<Cmd> {
        // the logical char is what an Arabic layout produces: the physical
        // key must win over it
        let logical = Key::Character(SmolStr::new("ض"));
        command(PhysicalKey::Code(code), &logical, ctrl, shift)
    }

    fn named(key: NamedKey, ctrl: bool) -> Option<Cmd> {
        command(
            PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified),
            &Key::Named(key),
            ctrl,
            false,
        )
    }

    #[test]
    fn letters_resolve_by_physical_key_on_an_arabic_layout() {
        assert_eq!(
            letter(KeyCode::KeyJ, false, false),
            Some(Cmd::Motion(ViMotion::Down))
        );
        assert_eq!(
            letter(KeyCode::KeyK, false, false),
            Some(Cmd::Motion(ViMotion::Up))
        );
        assert_eq!(letter(KeyCode::KeyY, false, false), Some(Cmd::CopyAndExit));
        assert_eq!(letter(KeyCode::KeyQ, false, false), Some(Cmd::Exit));
        assert_eq!(letter(KeyCode::KeyG, false, false), Some(Cmd::Top));
        assert_eq!(letter(KeyCode::KeyG, false, true), Some(Cmd::Bottom));
        assert_eq!(
            letter(KeyCode::Digit4, false, true),
            Some(Cmd::Motion(ViMotion::Last))
        );
    }

    #[test]
    fn v_picks_the_selection_kind_by_modifier() {
        assert_eq!(
            letter(KeyCode::KeyV, false, false),
            Some(Cmd::Select(SelectionType::Simple))
        );
        assert_eq!(
            letter(KeyCode::KeyV, false, true),
            Some(Cmd::Select(SelectionType::Lines))
        );
        assert_eq!(
            letter(KeyCode::KeyV, true, false),
            Some(Cmd::Select(SelectionType::Block))
        );
    }

    #[test]
    fn horizontal_keys_keep_their_screen_direction() {
        for (key, letter_key, motion) in [
            (NamedKey::ArrowLeft, KeyCode::KeyH, ViMotion::Left),
            (NamedKey::ArrowRight, KeyCode::KeyL, ViMotion::Right),
        ] {
            assert_eq!(named(key, false), Some(Cmd::Motion(motion)));
            assert_eq!(letter(letter_key, false, false), Some(Cmd::Motion(motion)));
        }
        // vertical keys and word motions keep reading order
        assert_eq!(
            named(NamedKey::ArrowUp, false),
            Some(Cmd::Motion(ViMotion::Up))
        );
        assert_eq!(
            letter(KeyCode::KeyW, false, false),
            Some(Cmd::Motion(ViMotion::SemanticRight))
        );
    }

    /// A 20x4 term holding `bytes`, in vi mode with the cursor where the
    /// shell's is.
    fn vi_term(bytes: &[u8]) -> Term<alacritty_terminal::event::VoidListener> {
        use alacritty_terminal::term::{Config, TermMode};
        let size = crate::term::TermSize { cols: 20, rows: 4 };
        let mut t = Term::new(
            Config::default(),
            &size,
            alacritty_terminal::event::VoidListener,
        );
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut t, bytes);
        t.toggle_vi_mode();
        assert!(t.mode().contains(TermMode::VI));
        t
    }

    fn run(t: &mut Term<alacritty_terminal::event::VoidListener>, cmds: &[Cmd]) {
        for &c in cmds {
            apply(t, c);
        }
    }

    #[test]
    fn v_then_motions_selects_from_the_cursor_cell() {
        // cursor parks after "two" on the last line; 0 goes to its start
        let mut t = vi_term(b"one alpha\r\ntwo");
        run(
            &mut t,
            &[
                Cmd::Motion(ViMotion::Up),
                Cmd::Motion(ViMotion::First),
                Cmd::Select(SelectionType::Simple),
                Cmd::Motion(ViMotion::SemanticRightEnd),
            ],
        );
        assert_eq!(t.selection_to_string().as_deref(), Some("one"));
        // the selection follows the cursor across lines
        run(&mut t, &[Cmd::Motion(ViMotion::Down)]);
        assert_eq!(t.selection_to_string().as_deref(), Some("one alpha\ntwo"));
    }

    #[test]
    fn a_selection_picked_alone_holds_its_one_cell() {
        // without include_all the fresh selection is empty and copies nothing
        let mut t = vi_term(b"xyz");
        run(
            &mut t,
            &[
                Cmd::Motion(ViMotion::First),
                Cmd::Select(SelectionType::Simple),
            ],
        );
        assert_eq!(t.selection_to_string().as_deref(), Some("x"));
    }

    #[test]
    fn v_again_drops_and_shift_v_turns_it_into_whole_lines() {
        let mut t = vi_term(b"first\r\nsecond");
        run(
            &mut t,
            &[
                Cmd::Motion(ViMotion::First),
                Cmd::Select(SelectionType::Simple),
            ],
        );
        run(&mut t, &[Cmd::Select(SelectionType::Lines)]);
        assert_eq!(
            t.selection.as_ref().map(|s| s.ty),
            Some(SelectionType::Lines)
        );
        assert_eq!(t.selection_to_string().as_deref(), Some("second\n"));
        run(&mut t, &[Cmd::Select(SelectionType::Lines)]);
        assert!(t.selection.is_none());
    }

    #[test]
    fn g_and_shift_g_reach_the_ends_of_the_scrollback() {
        // ten lines into a four-row screen: six of them are history
        let text: String = (0..10).map(|i| format!("line{i}\r\n")).collect();
        let mut t = vi_term(text.as_bytes());
        run(&mut t, &[Cmd::Top]);
        assert_eq!(t.vi_mode_cursor.point.line, t.topmost_line());
        assert!(
            t.grid().display_offset() > 0,
            "the view follows the cursor up"
        );
        run(&mut t, &[Cmd::Select(SelectionType::Lines)]);
        assert_eq!(t.selection_to_string().as_deref(), Some("line0\n"));
        run(&mut t, &[Cmd::Bottom]);
        assert_eq!(t.vi_mode_cursor.point.line, t.bottommost_line());
        assert_eq!(t.grid().display_offset(), 0);
    }

    #[test]
    fn past_the_end_of_an_arabic_row_the_cursor_snaps_onto_its_text() {
        // a long Latin line under a short Arabic one: k from column 15
        let mut t = vi_term("عليكم\r\nabcdefghijklmnop".as_bytes());
        let col = t.vi_mode_cursor.point.column.0;
        assert!(col > 5, "starts past the Arabic word: {col}");
        run(&mut t, &[Cmd::Motion(ViMotion::Up)]);
        assert_eq!(t.vi_mode_cursor.point.column.0, 4, "the word's last cell");
        // a Latin row keeps the column it is given
        run(&mut t, &[Cmd::Motion(ViMotion::Down)]);
        let mut latin = vi_term(b"abc\r\nabcdefghijklmnop");
        run(&mut latin, &[Cmd::Motion(ViMotion::Up)]);
        assert_eq!(latin.vi_mode_cursor.point.column.0, col);
    }

    #[test]
    fn a_copy_mode_selection_cleans_when_asked_and_stays_raw_when_not() {
        // 20 columns. "    hello world this" fills the row and wraps, then
        // a real newline and an indented next line. y copies this selection.
        let mut t = vi_term(b"    hello world this is long\r\n    next");
        run(
            &mut t,
            &[
                Cmd::Top,
                Cmd::Motion(ViMotion::First),
                Cmd::Select(SelectionType::Simple),
                Cmd::Motion(ViMotion::Down),
                Cmd::Motion(ViMotion::Down),
                Cmd::Motion(ViMotion::Last),
            ],
        );
        let raw = t.selection_to_string().expect("the yank has text");
        let rows = crate::copyclean::selection_rows(&t);
        assert!(
            rows.iter().any(|r| r.wrapped),
            "the long line wrapped: {rows:?}"
        );
        // setting off: the bytes copy mode already put on the clipboard
        assert_eq!(
            crate::copyclean::clipboard_text(&raw, &rows, false, false),
            raw
        );
        // setting on: the wrap joins and the hard break stays. A shell's
        // indent is its text, so it stays too.
        assert_eq!(
            crate::copyclean::clipboard_text(&raw, &rows, true, false),
            "    hello world this is long\n    next"
        );
    }

    #[test]
    fn enter_escape_and_ctrl_c_leave_and_other_ctrl_keys_are_swallowed() {
        assert_eq!(named(NamedKey::Enter, false), Some(Cmd::CopyAndExit));
        assert_eq!(named(NamedKey::Escape, false), Some(Cmd::Exit));
        assert_eq!(letter(KeyCode::KeyC, true, false), Some(Cmd::CopyAndExit));
        assert_eq!(letter(KeyCode::KeyT, true, false), None);
        assert_eq!(named(NamedKey::Home, true), Some(Cmd::Top));
        assert_eq!(
            named(NamedKey::Home, false),
            Some(Cmd::Motion(ViMotion::First))
        );
        assert_eq!(named(NamedKey::Tab, false), None);
        assert_eq!(letter(KeyCode::KeyZ, false, false), None);
    }
}
