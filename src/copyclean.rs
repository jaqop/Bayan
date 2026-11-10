//! Clean copy: a selection from an agent TUI, without the TUI's chrome.
//!
//! Claude Code and Codex indent every row, pad it with spaces, and break a
//! paragraph where the grid wrapped. Copied text then pastes with that
//! indentation, those trailing spaces, and a hard break at every wrap
//! (Claude Code #18170). `copy.clean` strips the chrome.
//!
//! A soft wrap is the grid's `WRAPLINE` flag on that row. A row without it
//! is a real newline and stays one. Characters are read in grid order, the
//! same order `selection_to_string` uses. Trailing spaces are trimmed and
//! soft wraps are joined on that logical string, before Claude mode's
//! `bidi::restore_block`. An RTL paragraph draws its trailing blanks on the
//! visual left once the row is right-aligned; those blanks are still the
//! end of the grid string, and this path never walks a row by its visual x.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Flags, LineLength};
use alacritty_terminal::term::Term;

/// One selected grid row, top to bottom, in stored cell order.
///
/// `wrapped` is that row's `WRAPLINE` flag: the next selected row continues
/// this one. A row without the flag ends a real line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CopyRow {
    pub text: String,
    pub wrapped: bool,
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
        return restore(raw, claude);
    }
    let stored = if rows.is_empty() {
        let body = raw.strip_suffix('\n').unwrap_or(raw);
        let lines = body
            .split('\n')
            .map(|text| CopyRow {
                text: text.to_string(),
                wrapped: false,
            })
            .collect::<Vec<_>>();
        assemble(&lines)
    } else {
        assemble(rows)
    };
    // `assemble` already trimmed and joined the grid string. Claude's
    // visual-to-logical restore runs on that result and must not decide
    // which spaces were trailing.
    restore(&stored, claude)
}

fn restore(text: &str, claude: bool) -> String {
    if claude {
        crate::bidi::restore_block(text)
    } else {
        text.to_string()
    }
}

/// Join soft wraps, drop a gutter every selected line shares, and remove
/// the indent those lines share. Trailing spaces on a wrapped row are the
/// row's own padding: a row that ends in spaces joins the next with one
/// space, and a row that ends on a character joins mid-word.
fn assemble(rows: &[CopyRow]) -> String {
    let texts = strip_uniform_gutter(&rows.iter().map(|r| r.text.clone()).collect::<Vec<_>>());
    let mut lines: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut indent = 0usize;
    let mut prev_ended_with_space = false;
    let mut joining = false;
    for (i, text) in texts.iter().enumerate() {
        let ended_with_space = text.ends_with(' ');
        let trimmed = text.trim_end_matches(' ');
        if !joining {
            indent = leading_spaces(trimmed);
            buf.push_str(trimmed);
        } else {
            let cont = strip_margin(trimmed, indent);
            if prev_ended_with_space {
                let rest = cont.trim_start_matches(' ');
                if !buf.is_empty() && !rest.is_empty() {
                    buf.push(' ');
                }
                buf.push_str(rest);
            } else {
                buf.push_str(cont);
            }
        }
        let last = i + 1 == texts.len();
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
    let n = common_indent(&lines);
    if n > 0 {
        for line in &mut lines {
            if !line.is_empty() {
                *line = strip_indent(line, n).to_string();
            }
        }
    }
    lines.join("\n")
}

fn strip_uniform_gutter(lines: &[String]) -> Vec<String> {
    let Some(gutter) = uniform_gutter(lines) else {
        return lines.to_vec();
    };
    lines
        .iter()
        .map(|line| strip_one_gutter(line, gutter))
        .collect()
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

fn common_indent(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|line| !line.is_empty())
        .map(|line| leading_spaces(line))
        .min()
        .unwrap_or(0)
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
        rows.push(CopyRow {
            text: cells_to_string(term, line, start_col, end_col),
            wrapped,
        });
    }
    rows
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
        }
    }

    fn clean(rows: &[CopyRow]) -> String {
        clipboard_text("RAW", rows, true, false)
    }

    #[test]
    fn common_leading_indentation_comes_off_every_line() {
        let rows = [row("    alpha", false), row("     beta", false)];
        assert_eq!(clean(&rows), "alpha\n beta");
        // a line with no indent means there is no shared margin
        let mixed = [row("    alpha", false), row("beta", false)];
        assert_eq!(clean(&mixed), "    alpha\nbeta");
        // blank lines stay, and they do not count as zero indent
        let blank = [row("  alpha", false), row("", false), row("  beta", false)];
        assert_eq!(clean(&blank), "alpha\n\nbeta");
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
        // a wrapped continuation repeats the TUI's left margin
        let margin = [row("    hello ", true), row("    world", false)];
        assert_eq!(clean(&margin), "hello world");
        let mid = [row("    hel", true), row("    lo", false)];
        assert_eq!(clean(&mid), "hello");
    }

    #[test]
    fn arabic_and_english_stay_in_logical_order() {
        // stored order is already logical (a shell, not Claude): cleaning
        // must not reverse the Arabic or swap it with the English
        let mixed = [row("    مرحبا hello   ", false)];
        assert_eq!(clean(&mixed), "مرحبا hello");
        let wrapped = [row("    مرحبا hel", true), row("lo   ", false)];
        assert_eq!(clean(&wrapped), "مرحبا hello");
        let lines = [row("مرحبا", false), row("hello", false)];
        assert_eq!(clean(&lines), "مرحبا\nhello");
        // Claude stored the visual order. The existing restore runs on the
        // joined line, and the margin comes off without touching the letters.
        let visual = [row("    What are you working on ؟كتدعاسم   ", false)];
        assert_eq!(
            clipboard_text("RAW", &visual, true, true),
            "What are you working on مساعدتك؟"
        );
        let arabic = [row("    ابحرم   ", false)];
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

    #[test]
    fn a_gutter_comes_off_only_when_every_line_has_it() {
        assert_eq!(
            clean(&[row("│ alpha", false), row("│ beta", false)]),
            "alpha\nbeta"
        );
        assert_eq!(
            clean(&[row("⎿ alpha", false), row("⎿ beta", false)]),
            "alpha\nbeta"
        );
        // one plain line keeps the marker on the lines that have it
        assert_eq!(
            clean(&[row("│ alpha", false), row("beta", false)]),
            "│ alpha\nbeta"
        );
        assert_eq!(
            clean(&[row("│ alpha", false), row("⎿ beta", false)]),
            "│ alpha\n⎿ beta"
        );
        // a blank line does not block a marker the others share
        assert_eq!(
            clean(&[row("│ alpha", false), row("", false), row("│ beta", false)]),
            "alpha\n\nbeta"
        );
        // the space belongs to the marker: a bar glued to a word stays
        assert_eq!(
            clean(&[row("│alpha", false), row("│beta", false)]),
            "│alpha\n│beta"
        );
        // wrapped rows that each carry the gutter lose it, then join
        let wrapped = [row("│    hel", true), row("│    lo", false)];
        assert_eq!(clean(&wrapped), "hello");
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
