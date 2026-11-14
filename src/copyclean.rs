//! Clean copy: a selection from an agent TUI, without the TUI's chrome.
//!
//! Claude Code and Codex indent every row, pad it with spaces, and break a
//! paragraph where the grid wrapped. Copied text then pastes with that
//! indentation, those trailing spaces, and a hard break at every wrap
//! (Claude Code #18170). `copy.clean` strips the chrome.
//!
//! Chrome is only the TUI's: on a Claude row, the spaces inside its
//! two-column margin and a gutter marker. Indentation past the margin, and
//! everything on a shell row, is text, so a selected block of code keeps
//! its absolute and relative indent and `tree` keeps its border.
//!
//! A soft wrap is the grid's `WRAPLINE` flag on that row. A row without it
//! is a real newline and stays one. Cells are read in grid order, never by
//! visual x. Claude stores each row in visual order, so clean copy restores
//! that row to logical order first and only then trims trailing spaces and
//! joins soft wraps. Restoring the joined line instead swaps the halves of
//! a wrapped Arabic paragraph. A shell's grid is already logical, so it is
//! trimmed and joined as stored. `copy.clean = off` does neither: it is the
//! old `selection_to_string` bytes, plus the existing whole-block restore
//! when Claude mode is on.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Flags, LineLength};
use alacritty_terminal::term::Term;

/// One selected grid row, top to bottom, in stored cell order.
///
/// `wrapped` is that row's `WRAPLINE` flag: the next selected row continues
/// this one. A row without the flag ends a real line. `col` is the grid
/// column `text` starts at, so a row picked up past the margin has none.
/// `has_padding` is the TUI's two blank columns, read off the grid. A word
/// space that the visual order put at the left of an RTL row is not padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CopyRow {
    pub text: String,
    pub wrapped: bool,
    pub col: usize,
    pub has_padding: bool,
}

/// What `copy.clean` in config.json means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CleanCopy {
    /// The clipboard gets today's selection bytes.
    Off,
    /// Every copy is cleaned, including copy-mode yanks.
    On,
    /// Clean while Claude mode is on. Off at a shell prompt, where the
    /// grid text is already the text.
    #[default]
    ClaudeMode,
}

impl CleanCopy {
    /// Unknown spellings stay `claude_mode`: a typo must not start cleaning
    /// every shell copy, or stop cleaning Claude.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Self::Off,
            "on" => Self::On,
            _ => Self::ClaudeMode,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::ClaudeMode => "claude_mode",
        }
    }

    /// Whether an ordinary copy is cleaned. The copy-clean keybind does not
    /// ask this: it cleans even when the answer is no.
    pub fn enabled(self, claude_active: bool) -> bool {
        match self {
            Self::Off => false,
            Self::On => true,
            Self::ClaudeMode => claude_active,
        }
    }
}

/// Left-edge markers Claude Code draws on a transcript row. The space is
/// part of the marker: `│hello` is text that happens to start with a bar.
const GUTTERS: [&str; 2] = ["⎿ ", "│ "];

/// Columns Claude Code keeps blank left of its transcript text (the width of
/// its `● ` bullet). Only spaces inside these columns are chrome; any indent
/// past them belongs to the text, so code keeps its absolute indentation.
const TUI_MARGIN: usize = 2;

/// Clipboard text for one selection.
///
/// `clean == false` returns `raw` exactly, apart from Claude mode's existing
/// logical restore. `raw` is `Term::selection_to_string`, so the setting off
/// is byte-identical to the copy path this sits on.
///
/// `clean == true` rebuilds from `rows` when the grid supplied them. An
/// empty `rows` cleans `raw` itself; `selection_to_string` has already
/// joined its own soft wraps, and those newlines are real.
pub(crate) fn clipboard_text(raw: &str, rows: &[CopyRow], clean: bool, claude: bool) -> String {
    if !clean {
        return restore_block(raw, claude);
    }
    let source = if rows.is_empty() {
        let body = raw.strip_suffix('\n').unwrap_or(raw);
        body.split('\n')
            .map(|text| CopyRow {
                text: text.to_string(),
                wrapped: false,
                col: 0,
                has_padding: false,
            })
            .collect::<Vec<_>>()
    } else {
        rows.to_vec()
    };
    // Gutter characters sit at the margin. Strip them before a Claude row is
    // restored, or an RTL line moves the marker to the other end.
    assemble(&logical_rows(&source, claude), claude)
}

/// Claude visual rows lose the TUI margin and gutter, then become logical
/// one row at a time. A shell row is already logical and has no chrome: it
/// stays byte for byte apart from the later trim, so `tree` keeps its `│ `.
fn logical_rows(rows: &[CopyRow], claude: bool) -> Vec<CopyRow> {
    if !claude {
        return rows.to_vec();
    }
    // The gutter sits just inside the margin.
    let bare: Vec<String> = rows
        .iter()
        .map(|row| strip_tui_margin(&row.text, row.col).to_string())
        .collect();
    let gutter = uniform_gutter(&bare);
    rows.iter()
        .zip(bare)
        .map(|(row, bare)| {
            let Some(gutter) = gutter else {
                return CopyRow {
                    text: restore_claude_row(&row.text, row.col, row.has_padding),
                    ..row.clone()
                };
            };
            let text = strip_one_gutter(&bare, gutter);
            CopyRow {
                text: crate::bidi::restore_bidi_line(&text).unwrap_or(text),
                ..row.clone()
            }
        })
        .collect()
}

/// One Claude row to logical order without its margin.
///
/// An RTL row is reversed whole, so a margin left in front becomes trailing
/// spaces and a mid-word wrap then joins with a space (`مرحب ا بكم`). The
/// margin comes off first, and only when the grid said the row has it: a
/// word space that the visual order put at column 0 is text, and stripping
/// it would glue a real word boundary. After the reverse, spaces at the
/// logical start are the row's visual-right padding, so they go. An LTR row
/// keeps its margin in front, so it comes off there.
fn restore_claude_row(text: &str, col: usize, has_padding: bool) -> String {
    let (text, col) = if has_padding {
        let bare = strip_tui_margin(text, col);
        let taken = leading_spaces(text).min(TUI_MARGIN.saturating_sub(col));
        (bare.to_string(), col + taken)
    } else {
        (text.to_string(), col)
    };
    let logical = crate::bidi::restore_bidi_line(&text).unwrap_or_else(|| text.clone());
    if crate::bidi::line_is_rtl_visual(&text) {
        logical.trim_start_matches(' ').to_string()
    } else {
        strip_tui_margin(&logical, col).to_string()
    }
}

/// The selection covers the TUI's blank columns, and those columns are spaces.
/// A single space at column 0 is a word space the visual order put there.
fn row_has_padding(text: &str, col: usize) -> bool {
    let room = TUI_MARGIN.saturating_sub(col);
    room > 0 && leading_spaces(text) >= room
}

/// Drop the leading spaces that fall inside the TUI margin of a row whose
/// text starts at grid column `col`.
fn strip_tui_margin(text: &str, col: usize) -> &str {
    let n = leading_spaces(text).min(TUI_MARGIN.saturating_sub(col));
    strip_indent(text, n)
}

fn restore_block(text: &str, claude: bool) -> String {
    if claude {
        crate::bidi::restore_block(text)
    } else {
        text.to_string()
    }
}

/// Join soft wraps. Rows are already in logical order, with the TUI margin
/// gone. Trailing spaces are the end of that string: a wrapped row that ends
/// in spaces joins the next with one space when the continuation does not
/// already start with one, and a row that ends on a character joins
/// mid-word. A Claude continuation repeats its line's indent, which comes
/// off; a shell continuation's leading spaces are text and stay.
fn assemble(rows: &[CopyRow], claude: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut indent = 0usize;
    let mut prev_ended_with_space = false;
    let mut joining = false;
    for (i, row) in rows.iter().enumerate() {
        let text = &row.text;
        let ended_with_space = text.ends_with(' ');
        let trimmed = text.trim_end_matches(' ');
        if !joining {
            indent = leading_spaces(trimmed);
            buf.push_str(trimmed);
        } else {
            let cont = if claude {
                strip_margin(trimmed, indent)
            } else {
                trimmed
            };
            if prev_ended_with_space
                && !buf.is_empty()
                && !cont.is_empty()
                && !cont.starts_with(' ')
            {
                buf.push(' ');
            }
            buf.push_str(cont);
        }
        let last = i + 1 == rows.len();
        // `wrapped` on the last selected row has no next row to join.
        if !rows[i].wrapped || last {
            lines.push(std::mem::take(&mut buf));
            joining = false;
            prev_ended_with_space = false;
        } else {
            joining = true;
            prev_ended_with_space = ended_with_space;
        }
    }
    lines.join("\n")
}

/// The gutter every non-blank line starts with, if there is exactly one
/// such marker. A blank line does not count against it. One line without
/// the marker means none of them lose it.
fn uniform_gutter(lines: &[String]) -> Option<&'static str> {
    let content: Vec<&str> = lines
        .iter()
        .map(|line| line.trim_end_matches(' '))
        .filter(|line| !line.is_empty())
        .collect();
    if content.is_empty() {
        return None;
    }
    GUTTERS.into_iter().find(|gutter| {
        let bare = gutter.trim_end();
        content
            .iter()
            .all(|line| line.starts_with(gutter) || *line == bare)
    })
}

fn strip_one_gutter(line: &str, gutter: &str) -> String {
    if let Some(rest) = line.strip_prefix(gutter) {
        return rest.to_string();
    }
    if line.trim_end_matches(' ') == gutter.trim_end() {
        return String::new();
    }
    line.to_string()
}

fn leading_spaces(text: &str) -> usize {
    text.chars().take_while(|&c| c == ' ').count()
}

/// Drop `n` leading characters. Callers only pass a line that has at least
/// `n` leading spaces.
fn strip_indent(text: &str, n: usize) -> &str {
    text.char_indices()
        .nth(n)
        .map(|(i, _)| &text[i..])
        .unwrap_or("")
}

/// The continuation's leading spaces are the TUI's margin when there are
/// at least as many as the logical line started with. Fewer than that are
/// a space the wrap landed on, and they stay.
fn strip_margin(text: &str, indent: usize) -> &str {
    if indent > 0 && leading_spaces(text) >= indent {
        strip_indent(text, indent)
    } else {
        text
    }
}

/// The rows `selection_to_string` would read, with each row's `WRAPLINE`
/// flag. Block selections are never soft wraps: each row is its own line,
/// which is what a block copy already does.
pub(crate) fn selection_rows<T: EventListener>(term: &Term<T>) -> Vec<CopyRow> {
    let Some(selection) = term.selection.as_ref() else {
        return Vec::new();
    };
    let Some(range) = selection.to_range(term) else {
        return Vec::new();
    };
    let cols = term.columns();
    let last_col = Column(cols.saturating_sub(1));
    let mut rows = Vec::new();
    for line_i in range.start.line.0..=range.end.line.0 {
        let line = Line(line_i);
        let start_col = if range.is_block || line == range.start.line {
            range.start.column
        } else {
            Column(0)
        };
        let end_col = if range.is_block || line == range.end.line {
            range.end.column
        } else {
            last_col
        };
        let wrapped = !range.is_block && end_col.0 + 1 >= cols && row_wraps(term, line);
        let text = cells_to_string(term, line, start_col, end_col);
        let has_padding = row_has_padding(&text, start_col.0);
        rows.push(CopyRow {
            text,
            wrapped,
            col: start_col.0,
            has_padding,
        });
    }
    rows
}

/// Full retained history plus the screen, without changing the selection
/// or viewport. Restore each visual Claude row before joining soft wraps.
pub(crate) fn history_text<T: EventListener>(term: &Term<T>, claude: bool) -> String {
    let last = Column(term.columns().saturating_sub(1));
    let rows: Vec<_> = (-(term.grid().history_size() as i32)..term.screen_lines() as i32)
        .map(|i| {
            let line = Line(i);
            let text = cells_to_string(term, line, Column(0), last);
            CopyRow {
                has_padding: row_has_padding(&text, 0),
                text,
                wrapped: row_wraps(term, line),
                col: 0,
            }
        })
        .collect();
    clipboard_text("", &rows, true, claude)
}

fn row_wraps<T>(term: &Term<T>, line: Line) -> bool {
    let row = &term.grid()[line];
    let len = row.line_length().0;
    len > 0 && row[Column(len - 1)].flags.contains(Flags::WRAPLINE)
}

fn cells_to_string<T>(term: &Term<T>, line: Line, mut start: Column, end: Column) -> String {
    let row = &term.grid()[line];
    if start.0 > 0 && row[start].flags.contains(Flags::WIDE_CHAR_SPACER) {
        start = Column(start.0 - 1);
    }
    let last = row.line_length().0.min(end.0.saturating_add(1));
    let mut text = String::new();
    if start.0 >= last {
        return text;
    }
    for col in start.0..last {
        let cell = &row[Column(col)];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        text.push(cell.c);
        for c in cell.zerowidth().into_iter().flatten() {
            text.push(*c);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::{Point, Side};
    use alacritty_terminal::selection::{Selection, SelectionType};
    use alacritty_terminal::term::Config;

    fn row(text: &str, wrapped: bool) -> CopyRow {
        CopyRow {
            text: text.to_string(),
            wrapped,
            col: 0,
            has_padding: false,
        }
    }

    /// A Claude row whose grid columns 0 and 1 are the TUI margin.
    fn padded(visual: &str, wrapped: bool) -> CopyRow {
        CopyRow {
            text: format!("  {visual}"),
            wrapped,
            col: 0,
            has_padding: true,
        }
    }

    fn clean(rows: &[CopyRow]) -> String {
        clipboard_text("RAW", rows, true, false)
    }

    #[test]
    fn the_claude_margin_comes_off_each_row() {
        // a line with no margin (the bullet row) does not stop the others
        let mixed = [row("● alpha", false), row("  beta", false)];
        assert_eq!(claude(&mixed), "● alpha\nbeta");
        // blank lines stay
        let blank = [row("  alpha", false), row("", false), row("  beta", false)];
        assert_eq!(claude(&blank), "alpha\n\nbeta");
        // A whole function body: the margin comes off, the body's indent
        // and a nested line's extra indent stay.
        let body = [
            row("  fn body() {", false),
            row("      let x = 1;", false),
            row("          nested();", false),
            row("  }", false),
        ];
        assert_eq!(
            claude(&body),
            "fn body() {\n    let x = 1;\n        nested();\n}"
        );
        // A soft wrap must not eat the continuation's extra indent.
        let wrapped_body = [row("  let x = ", true), row("      1;", false)];
        assert_eq!(claude(&wrapped_body), format!("let x ={}1;", " ".repeat(4)));
        // A shell's soft-wrapped continuation: its leading spaces are text.
        let shell_wrap = [row("    let x =", true), row("    1;", false)];
        assert_eq!(clean(&shell_wrap), "    let x =    1;");
    }

    #[test]
    fn trailing_spaces_are_trimmed_on_each_line() {
        let rows = [row("hello   ", false), row("world  ", false)];
        assert_eq!(clean(&rows), "hello\nworld");
        assert_eq!(clean(&[row("hello   ", false)]), "hello");
        // a space between words is not trailing
        assert_eq!(clean(&[row("hello world  ", false)]), "hello world");
    }

    #[test]
    fn soft_wraps_join_and_hard_newlines_stay() {
        // mid-word wrap: the next row is the rest of the word
        let wrapped = [row("hel", true), row("lo", false), row("next", false)];
        assert_eq!(clean(&wrapped), "hello\nnext");
        // the same cells without WRAPLINE are two real lines
        let hard = [row("hel", false), row("lo", false)];
        assert_eq!(clean(&hard), "hel\nlo");
        // a wrap that ends on spaces is a word break, not a mid-word cut
        let word = [row("hello   ", true), row("world", false)];
        assert_eq!(clean(&word), "hello world");
        // trailing spaces on a real line do not glue it to the next
        let kept = [row("hello   ", false), row("world", false)];
        assert_eq!(clean(&kept), "hello\nworld");
        // a wrapped Claude continuation repeats the TUI's left margin
        let margin = [row("  hello ", true), row("  world", false)];
        assert_eq!(claude(&margin), "hello world");
        let mid = [row("  hel", true), row("  lo", false)];
        assert_eq!(claude(&mid), "hello");
    }

    #[test]
    fn arabic_and_english_stay_in_logical_order() {
        // stored order is already logical (a shell, not Claude): cleaning
        // must not reverse the Arabic or swap it with the English
        let mixed = [row("    مرحبا hello   ", false)];
        assert_eq!(clean(&mixed), "    مرحبا hello");
        let wrapped = [row("    مرحبا hel", true), row("lo   ", false)];
        assert_eq!(clean(&wrapped), "    مرحبا hello");
        let lines = [row("مرحبا", false), row("hello", false)];
        assert_eq!(clean(&lines), "مرحبا\nhello");
        // Claude stored the visual order. The existing restore runs on the
        // row, and the margin comes off without touching the letters.
        let visual = [row("  What are you working on ؟كتدعاسم   ", false)];
        assert_eq!(
            clipboard_text("RAW", &visual, true, true),
            "What are you working on مساعدتك؟"
        );
        let arabic = [row("  ابحرم   ", false)];
        assert_eq!(clipboard_text("RAW", &arabic, true, true), "مرحبا");
    }

    /// Grid order is the logical string. A soft wrap is two WRAPLINE rows
    /// of that string, not a walk of the shaped visual x. BiDi restore of
    /// the same letters is a different string and must not run first.
    #[test]
    fn a_soft_wrapped_arabic_line_with_english_and_digits_stays_logical() {
        let logical = "مرحبا hello 123 عالم";
        // 12 columns: "مرحبا hello " fills the row and wraps. The rest,
        // "123 عالم", is the next row, with blanks after the logical end.
        let mut term = grid(12, 2);
        put(&mut term, 0, "مرحبا hello ", true);
        put(&mut term, 1, "123 عالم", true);
        select_all(&mut term, 2);
        let raw = term.selection_to_string().expect("selection");
        let rows = selection_rows(&term);
        assert!(rows[0].wrapped && rows[1].wrapped);
        assert!(
            rows[0].text.starts_with("مرحبا hello"),
            "grid order starts at the logical start: {:?}",
            rows[0].text
        );
        assert!(
            rows[1].text.starts_with("123 عالم"),
            "the continuation keeps the digits and the second word: {:?}",
            rows[1].text
        );
        assert!(
            rows[1].text.ends_with(' '),
            "WRAPLINE keeps the blanks after عالم in the grid string"
        );
        let copied = clipboard_text(&raw, &rows, true, false);
        assert_eq!(copied, logical);
        assert!(!copied.starts_with(' '));
        assert!(copied.contains("hello") && copied.contains("123") && copied.contains("عالم"));
        assert_ne!(
            crate::bidi::restore_block(&raw),
            copied,
            "reordering first would not yield the logical line"
        );
    }

    /// SCP 2 marks an RTL paragraph. Right-alignment will draw the blanks
    /// after the logical end on the visual left. They are still the end of
    /// the grid string, and the trim removes them there.
    #[test]
    fn an_rtl_paragraph_trims_blanks_at_the_logical_end() {
        let logical = "مرحبا hello";
        let mut term = grid(16, 1);
        put(&mut term, 0, logical, true);
        select_all(&mut term, 1);
        let mut bidi = crate::bidi_ctl::BidiState::default();
        bidi.note_scp(crate::bidi_ctl::ParagraphDir::Rtl, 0, 0);
        assert_eq!(
            bidi.direction_at(0),
            crate::bidi_ctl::ParagraphDir::Rtl,
            "SCP 2 stamps this line"
        );
        let raw = term.selection_to_string().expect("selection");
        let rows = selection_rows(&term);
        assert!(
            rows[0].text.ends_with(' ') && rows[0].text.starts_with(logical),
            "blanks sit after the logical text in grid order: {:?}",
            rows[0].text
        );
        let copied = clipboard_text(&raw, &rows, true, false);
        assert_eq!(copied, logical);
        assert_eq!(copied.chars().next(), Some('م'));
        assert_eq!(copied.chars().last(), Some('o'));
        assert!(!copied.ends_with(' '));
        assert_ne!(crate::bidi::restore_block(&raw), copied);
    }

    /// Claude stores each wrapped row in visual order. Restoring the joined
    /// line swaps the halves (`في بيان 2026مرحبا بكم`) and drops the space.
    /// Each row is restored first, then the soft wrap is joined.
    #[test]
    fn claude_wrapped_line_copies_the_logical_arabic_english_and_digits() {
        let first = "مرحبا hello ";
        let second = "123 عالم";
        let logical = "مرحبا hello 123 عالم";
        let (v0, v1) = (visual_of(first), visual_of(second));
        let rows = [row(&v0, true), row(&v1, false)];
        let copied = clipboard_text("RAW", &rows, true, true);
        assert_eq!(copied, logical);
        assert!(!copied.starts_with(' '));
        assert!(copied.contains("hello") && copied.contains("123") && copied.contains("عالم"));
        let joined_visual = format!("{v0}{v1}");
        assert_ne!(
            crate::bidi::restore_block(&joined_visual),
            logical,
            "restoring the joined visual line is the bug"
        );
    }

    /// The two blank columns are the margin, not a word space. Reversing
    /// them with the row turns them into a trailing space, and a mid-word
    /// wrap then joins with one (`مرحب ا بكم`). They come off first.
    #[test]
    fn claude_rtl_margin_does_not_become_a_space_inside_a_word() {
        let rows = [
            padded(&visual_of("مرحب"), true),
            padded(&visual_of("ا بكم"), false),
        ];
        assert_eq!(claude(&rows), "مرحبا بكم");
    }

    /// A wrap that lands on a word boundary keeps that one space. The margin
    /// in front of the visual word-space is chrome; the word-space is not.
    #[test]
    fn claude_rtl_wrap_on_a_word_boundary_keeps_one_space() {
        let rows = [
            padded(&visual_of("مرحب "), true),
            padded(&visual_of("بكم"), false),
        ];
        let copied = claude(&rows);
        assert_eq!(copied, "مرحب بكم");
        assert_eq!(copied.matches(' ').count(), 1, "{copied:?}");
    }

    /// Harakat and ZWJ sitting on the wrap stay with their letter, and the
    /// margin does not push a space in between the two rows.
    #[test]
    fn claude_rtl_wrap_edge_keeps_harakat_and_zwj() {
        let marked = [
            padded(&visual_of("مَرْحَبً"), true),
            padded(&visual_of("ا"), false),
        ];
        assert_eq!(claude(&marked), "مَرْحَبًا");
        let zwj = [
            padded(&visual_of("م\u{200D}"), true),
            padded(&visual_of("ر"), false),
        ];
        assert_eq!(claude(&zwj), "م\u{200D}ر");
    }

    #[test]
    fn claude_wrap_keeps_harakat_zwj_zwnj_and_a_wide_char() {
        let marked = "مَرْحَبًا ";
        let after = "بكم";
        let rows = [row(&visual_of(marked), true), row(&visual_of(after), false)];
        assert_eq!(
            clipboard_text("RAW", &rows, true, true),
            "مَرْحَبًا بكم",
            "tashkeel stays on its letter"
        );

        let zwj = "م\u{200D}ر ";
        let zwnj_next = "ح\u{200C}با";
        let rows = [
            row(&visual_of(zwj), true),
            row(&visual_of(zwnj_next), false),
        ];
        assert_eq!(
            clipboard_text("RAW", &rows, true, true),
            "م\u{200D}ر ح\u{200C}با"
        );

        // The wide character is the wrap boundary: its spacer is the last
        // cell of the row, and the next row continues. It is copied once.
        let mut term = grid(3, 2);
        {
            let row0 = &mut term.grid_mut()[Line(0)];
            row0[Column(0)].c = 'م';
            row0[Column(1)].c = '字';
            row0[Column(1)].flags.insert(Flags::WIDE_CHAR);
            row0[Column(2)].c = ' ';
            row0[Column(2)].flags.insert(Flags::WIDE_CHAR_SPACER);
            row0[Column(2)].flags.insert(Flags::WRAPLINE);
        }
        term.grid_mut()[Line(1)][Column(0)].c = 'ر';
        select_all(&mut term, 2);
        let raw = term.selection_to_string().expect("selection");
        let grid_rows = selection_rows(&term);
        let copied = clipboard_text(&raw, &grid_rows, true, true);
        assert_eq!(copied.matches('字').count(), 1, "{copied:?} {grid_rows:?}");
        assert!(copied.contains('م') && copied.contains('ر'), "{copied}");
        assert!(!copied.contains('\u{FFFD}'));
    }

    /// `logical` is an RTL paragraph. Claude's row is the visual order,
    /// and restoring that visual order returns `logical`.
    fn visual_of(logical: &str) -> String {
        let visual = crate::bidi::restore_bidi_line(logical)
            .unwrap_or_else(|| panic!("no visual form for {logical:?}"));
        assert_eq!(
            crate::bidi::restore_bidi_line(&visual).as_deref(),
            Some(logical),
            "visual {visual:?} did not round-trip {logical:?}"
        );
        visual
    }

    #[test]
    fn a_gutter_comes_off_only_when_every_line_has_it() {
        assert_eq!(
            claude(&[row("│ alpha", false), row("│ beta", false)]),
            "alpha\nbeta"
        );
        assert_eq!(
            claude(&[row("⎿ alpha", false), row("⎿ beta", false)]),
            "alpha\nbeta"
        );
        // one plain line keeps the marker on the lines that have it
        assert_eq!(
            claude(&[row("│ alpha", false), row("beta", false)]),
            "│ alpha\nbeta"
        );
        assert_eq!(
            claude(&[row("│ alpha", false), row("⎿ beta", false)]),
            "│ alpha\n⎿ beta"
        );
        // a blank line does not block a marker the others share
        assert_eq!(
            claude(&[row("│ alpha", false), row("", false), row("│ beta", false)]),
            "alpha\n\nbeta"
        );
        // the space belongs to the marker: a bar glued to a word stays
        assert_eq!(
            claude(&[row("│alpha", false), row("│beta", false)]),
            "│alpha\n│beta"
        );
        // wrapped rows that each carry the gutter lose it, then join
        let wrapped = [row("│ hel", true), row("│ lo", false)];
        assert_eq!(claude(&wrapped), "hello");
    }

    #[test]
    fn setting_off_is_byte_identical_to_the_current_copy() {
        // the rows would clean to something else; off must ignore them
        let raw = "    │ مرحبا hello   \nnext  \n";
        let rows = [row("DIFFERENT", true), row("ROWS", false)];
        assert_eq!(clipboard_text(raw, &rows, false, false), raw);
        assert_eq!(
            clipboard_text(raw, &rows, false, true),
            crate::bidi::restore_block(raw)
        );
        // and an empty raw stays empty, Claude or not
        assert_eq!(clipboard_text("", &rows, false, false), "");
        assert_eq!(clipboard_text("", &rows, false, true), "");
    }

    #[test]
    fn clean_copy_follows_claude_mode_unless_forced_by_the_value() {
        assert!(!CleanCopy::default().enabled(false));
        assert!(CleanCopy::default().enabled(true));
        assert!(!CleanCopy::Off.enabled(true));
        assert!(CleanCopy::On.enabled(false));
        assert_eq!(CleanCopy::parse("off"), CleanCopy::Off);
        assert_eq!(CleanCopy::parse(" ON "), CleanCopy::On);
        assert_eq!(CleanCopy::parse("claude_mode"), CleanCopy::ClaudeMode);
        // a typo keeps the default, it does not flip the feature
        assert_eq!(CleanCopy::parse("banana"), CleanCopy::ClaudeMode);
        assert_eq!(CleanCopy::parse(""), CleanCopy::ClaudeMode);
        for mode in [CleanCopy::Off, CleanCopy::On, CleanCopy::ClaudeMode] {
            assert_eq!(CleanCopy::parse(mode.as_str()), mode);
        }
    }

    fn grid(cols: usize, rows: usize) -> Term<VoidListener> {
        Term::new(
            Config::default(),
            &crate::term::TermSize { cols, rows },
            VoidListener,
        )
    }

    fn put(term: &mut Term<VoidListener>, line: i32, text: &str, wrap: bool) {
        let last = term.columns() - 1;
        let row = &mut term.grid_mut()[Line(line)];
        for (i, ch) in text.chars().enumerate() {
            row[Column(i)].c = ch;
        }
        if wrap {
            row[Column(last)].flags.insert(Flags::WRAPLINE);
        }
    }

    fn select_all(term: &mut Term<VoidListener>, lines: i32) {
        let mut sel = Selection::new(
            SelectionType::Simple,
            Point::new(Line(0), Column(0)),
            Side::Left,
        );
        sel.update(
            Point::new(Line(lines - 1), Column(term.columns() - 1)),
            Side::Right,
        );
        term.selection = Some(sel);
    }

    fn claude(rows: &[CopyRow]) -> String {
        clipboard_text("RAW", rows, true, true)
    }

    /// Only the TUI's own margin comes off. Code indentation, absolute and
    /// relative, is the text.
    #[test]
    fn a_tui_margin_comes_off_and_indented_code_keeps_its_indent() {
        // Claude's prose sits two columns in: that margin is chrome.
        let prose = [row("  alpha", false), row("   beta", false)];
        assert_eq!(claude(&prose), "alpha\n beta");
        // The body of an indented block in Claude's output: the margin
        // goes, the four spaces of the block's own indent stay.
        let body = [
            row("      if x:", false),
            row("          return 1", false),
            row("", false),
            row("      y = 2", false),
        ];
        assert_eq!(claude(&body), "    if x:\n        return 1\n\n    y = 2");
        // A shell's indented code is copied as it stands.
        let shell = [row("    if x:", false), row("        return 1", false)];
        assert_eq!(clean(&shell), "    if x:\n        return 1");
        let yaml = [row("  a:", false), row("    b: 1", false)];
        assert_eq!(clean(&yaml), "  a:\n    b: 1");

        // A drag that starts on the text, past the margin, has no margin on
        // its first row. The rows after it start at column 0 and do.
        let mut term = grid(20, 2);
        put(&mut term, 0, "  def foo():", false);
        put(&mut term, 1, "      return 1", false);
        let mut sel = Selection::new(
            SelectionType::Simple,
            Point::new(Line(0), Column(2)),
            Side::Left,
        );
        sel.update(Point::new(Line(1), Column(19)), Side::Right);
        term.selection = Some(sel);
        let raw = term.selection_to_string().expect("selection");
        let rows = selection_rows(&term);
        assert_eq!(
            clipboard_text(&raw, &rows, true, true),
            "def foo():\n    return 1"
        );
    }

    #[test]
    fn a_shell_tree_keeps_its_border() {
        let tree = [
            row("│   ├── a.rs", false),
            row("│   └── b.rs", false),
            row("│ ", false),
        ];
        assert_eq!(clean(&tree), "│   ├── a.rs\n│   └── b.rs\n│");
        // Claude's own gutter still comes off its rows.
        assert_eq!(
            claude(&[row("│ alpha", false), row("│ beta", false)]),
            "alpha\nbeta"
        );
    }

    #[test]
    fn the_grid_wrapline_flag_is_what_joins_and_off_matches_the_terminal() {
        let mut term = grid(8, 3);
        // "hi" and six spaces. WRAPLINE on the last cell makes the spaces
        // part of the row, which is the padding a full wrapped row carries.
        put(&mut term, 0, "hi", true);
        put(&mut term, 1, "there", false);
        put(&mut term, 2, "  next", false);
        select_all(&mut term, 3);
        let raw = term.selection_to_string().expect("selection");
        let rows = selection_rows(&term);
        assert!(rows[0].wrapped, "WRAPLINE on the first row");
        assert!(!rows[1].wrapped);
        assert!(!rows[2].wrapped);
        // off: the terminal's own string, byte for byte, spaces and all
        assert_eq!(clipboard_text(&raw, &rows, false, false), raw);
        // on: the wrap joins and the padding collapses to one word space.
        // "  next" keeps its spaces: "hi there" has no indent, so there is
        // no shared margin to remove, and the hard break stays.
        assert_eq!(clipboard_text(&raw, &rows, true, false), "hi there\n  next");
        // the same cells with the flag clear are three real lines
        term.grid_mut()[Line(0)][Column(7)]
            .flags
            .remove(Flags::WRAPLINE);
        let rows = selection_rows(&term);
        assert!(!rows[0].wrapped);
        let raw = term.selection_to_string().expect("selection");
        assert_eq!(clipboard_text(&raw, &rows, false, false), raw);
        assert_eq!(
            clipboard_text(&raw, &rows, true, false),
            "hi\nthere\n  next"
        );
    }
}
