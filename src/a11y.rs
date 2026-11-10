//! Screen-reader tree for NVDA, JAWS and Narrator.
//!
//! AccessKit turns this tree into UI Automation on Windows. The adapter
//! does nothing until a client connects, and [`crate::App`] only calls
//! [`OutputGate`] / [`build_tree`] from inside `update_if_active`, so an
//! idle terminal with no reader never walks the grid.
//!
//! Pane text is the grid's storage order. That is the logical order the
//! program emitted. The renderer reorders Arabic for the screen; a reader
//! must not be handed that visual order, or Arabic is spoken backwards.
//! Claude's visual grid is the exception, restored with the same map the
//! copy path uses.
//!
//! A flood (`cargo build`) must not raise a text-changed or live-region
//! event per line. [`OutputGate`] keeps the document on the last published
//! snapshot until [`GAP_MS`] has elapsed, then publishes one tail. An OSC
//! 133 finish replaces that tail with a single exit-code sentence.

use accesskit::{
    Live, Node, NodeId, Role, TextDirection, TextPosition, TextSelection, TreeId, TreeInfo,
    TreeUpdate,
};
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point;
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Term;

use crate::bidi;
use crate::lang::{self, tr};

/// How long a burst of output is held before one announcement.
pub const GAP_MS: u64 = 500;

/// Lines kept in one announcement. Older lines are counted, not read.
const TAIL_LINES: usize = 8;

/// Characters kept in that tail, from the end.
const TAIL_CHARS: usize = 280;

/// `word_starts` is a `u8` index, so a run stays under 256 characters.
const RUN_CHARS: usize = 200;

const ID_ROOT: NodeId = NodeId(1);
const ID_TABS: NodeId = NodeId(2);
const ID_LIVE: NodeId = NodeId(3);
const ID_PALETTE: NodeId = NodeId(4);
const ID_DIALOG: NodeId = NodeId(5);
const ID_PALETTE_LIST: NodeId = NodeId(6);
const ID_PALETTE_QUERY: NodeId = NodeId(7);
const ID_DIALOG_BODY: NodeId = NodeId(8);

pub fn tab_node(index: usize) -> NodeId {
    NodeId(0x0001_0000 + index as u64)
}

pub fn pane_node(tab: usize, pane: usize) -> NodeId {
    NodeId(0x0002_0000 + (tab as u64) * 64 + pane as u64)
}

fn run_node(tab: usize, pane: usize, line: usize, part: usize) -> NodeId {
    NodeId(
        0x1_0000_0000
            | ((tab as u64) << 24)
            | ((pane as u64) << 18)
            | ((line as u64) << 4)
            | part as u64,
    )
}

fn option_node(index: usize) -> NodeId {
    NodeId(0x0004_0000 + index as u64)
}

pub fn tab_list_name() -> &'static str {
    tr("التبويبات", "Tabs")
}

pub fn pane_name(index: usize, count: usize) -> String {
    if count <= 1 {
        tr("اللوحة", "Pane").to_string()
    } else if lang::rtl() {
        format!("اللوحة {}", index + 1)
    } else {
        format!("Pane {}", index + 1)
    }
}

pub fn palette_name() -> &'static str {
    tr("لوحة الأوامر", "Command palette")
}

pub fn menu_name() -> &'static str {
    tr("القائمة", "Menu")
}

pub fn live_name() -> &'static str {
    tr("المخرجات الجديدة", "New output")
}

/// The live-region label that announces `next` after `on_screen`. AccessKit
/// raises UIA LiveRegionChanged only when the name string differs, so a
/// repeat gets a trailing ASCII space: unspoken, and unlike U+200B it is
/// not printed as an escape by braille tables.
pub fn live_label(on_screen: &str, mut next: String) -> String {
    if next == on_screen {
        next.push(' ');
    }
    next
}

pub fn paste_dialog_name() -> &'static str {
    tr("تأكيد اللصق", "Confirm paste")
}

pub fn search_name() -> &'static str {
    tr("بحث", "Search")
}

/// The close card's question, shared with the drawn card.
pub fn close_pane_message(cmd: &str) -> String {
    if lang::rtl() {
        format!("الأمر «{cmd}» ما يزال يعمل — إغلاق اللوحة؟")
    } else {
        format!("“{cmd}” is still running — close the pane?")
    }
}

/// `tab` is the tab card; the window card names Bayan.
pub fn close_count_message(n: usize, tab: bool) -> String {
    if lang::rtl() {
        if tab {
            format!("{n} أمر ما يزال يعمل — إغلاق التبويب؟")
        } else {
            format!("{n} أمر ما يزال يعمل — إغلاق بيان؟")
        }
    } else if tab {
        format!("{n} command(s) still running — close the tab?")
    } else {
        format!("{n} command(s) still running — close Bayan?")
    }
}

/// One spoken sentence for an OSC 133 D. The exit code is in the sentence
/// so a reader does not have to guess success from the title alone.
pub fn finish_sentence(exit: Option<i32>, command: &str) -> String {
    let head = match exit {
        Some(0) => tr("اكتمل الأمر", "Command finished"),
        Some(_) => tr("فشل الأمر", "Command failed"),
        None => tr("انتهى الأمر", "Command ended"),
    };
    let code = match exit {
        Some(n) if lang::rtl() => format!("رمز الخروج {n}"),
        Some(n) => format!("exit code {n}"),
        None if lang::rtl() => "بدون رمز خروج".to_string(),
        None => "no exit code".to_string(),
    };
    let cmd = command.trim();
    if cmd.is_empty() {
        format!("{head}، {code}")
    } else if lang::rtl() {
        format!("{head}: {cmd}، {code}")
    } else {
        format!("{head}: {cmd}, {code}")
    }
}

/// One stored cell, spacer cells already dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GridCell {
    pub col: usize,
    pub ch: char,
    pub width: usize,
    pub extra: String,
}

/// A visible row in logical order, with the column each character covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalLine {
    pub text: String,
    pub cols: Vec<usize>,
    pub widths: Vec<usize>,
}

/// Storage order, trimmed of the blank cells a row is padded with.
/// `claude` runs the visual-to-logical map; every other program is already
/// logical and must be left alone.
pub fn logical_line(cells: &[GridCell], claude: bool) -> LogicalLine {
    let mut ordered = cells.to_vec();
    ordered.sort_by_key(|c| c.col);
    let mut text = String::new();
    let mut cols = Vec::new();
    let mut widths = Vec::new();
    let mut expect = 0usize;
    for cell in &ordered {
        let width = cell.width.max(1);
        while expect < cell.col {
            text.push(' ');
            cols.push(expect);
            widths.push(1);
            expect += 1;
        }
        text.push(cell.ch);
        cols.push(cell.col);
        widths.push(width);
        expect = cell.col + width;
        for ch in cell.extra.chars() {
            text.push(ch);
            cols.push(cell.col);
            widths.push(0);
        }
    }
    let (text, cols, widths) = if claude {
        match bidi::restore_bidi_map(&text) {
            Some((logical, map)) => {
                let mut ncols = Vec::with_capacity(map.len());
                let mut nwidths = Vec::with_capacity(map.len());
                for i in map {
                    ncols.push(cols.get(i).copied().unwrap_or(0));
                    nwidths.push(widths.get(i).copied().unwrap_or(1));
                }
                (logical, ncols, nwidths)
            }
            None => (text, cols, widths),
        }
    } else {
        (text, cols, widths)
    };
    let mut line = LogicalLine { text, cols, widths };
    while line.text.ends_with(' ') {
        line.text.pop();
        line.cols.pop();
        line.widths.pop();
    }
    line
}

/// Cells copied out of the grid. No BiDi work: that happens after the lock.
#[derive(Clone, Debug)]
pub struct RawPane {
    pub rows: Vec<Vec<GridCell>>,
    pub cursor: Option<(usize, usize)>,
    pub selection: Option<((usize, usize), (usize, usize))>,
}

/// What one pane shows the reader: viewport rows, caret, selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneText {
    pub lines: Vec<LogicalLine>,
    pub cursor: Option<(usize, usize)>,
    pub selection: Option<((usize, usize), (usize, usize))>,
}

/// Copy the visible grid. The caller drops the terminal lock before
/// [`pane_text`] and [`build_tree`].
pub fn copy_pane<T: EventListener>(term: &Term<T>) -> RawPane {
    let content = term.renderable_content();
    let off = content.display_offset as i32;
    let rows = term.screen_lines();
    let mut cells = vec![Vec::new(); rows];
    for cell in content.display_iter {
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let vrow = cell.point.line.0 + off;
        if vrow < 0 || vrow as usize >= rows {
            continue;
        }
        let hidden = cell.flags.intersects(Flags::HIDDEN);
        let ch = if hidden { ' ' } else { cell.c };
        let width = if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        let extra = if hidden {
            String::new()
        } else {
            cell.zerowidth()
                .map(|marks| marks.iter().collect())
                .unwrap_or_default()
        };
        cells[vrow as usize].push(GridCell {
            col: cell.point.column.0,
            ch,
            width,
            extra,
        });
    }
    RawPane {
        rows: cells,
        cursor: viewport_cursor(content.cursor.point, off, rows),
        selection: content
            .selection
            .and_then(|sel| viewport_selection(sel, off, rows)),
    }
}

/// Logical lines from a [`copy_pane`] snapshot. `claude` restores visual
/// order; every other grid is already logical.
pub fn pane_text(raw: RawPane, claude: bool) -> PaneText {
    PaneText {
        lines: raw
            .rows
            .iter()
            .map(|row| logical_line(row, claude))
            .collect(),
        cursor: raw.cursor,
        selection: raw.selection,
    }
}

/// Copy and restore. The window drops the lock between the two steps, so
/// this combined helper is for tests.
#[cfg(test)]
fn read_pane<T: EventListener>(term: &Term<T>, claude: bool) -> PaneText {
    pane_text(copy_pane(term), claude)
}

fn viewport_cursor(point: Point, off: i32, rows: usize) -> Option<(usize, usize)> {
    let v = point.line.0 + off;
    (v >= 0 && (v as usize) < rows).then_some((v as usize, point.column.0))
}

/// Inclusive endpoints. A range that misses the viewport entirely is none;
/// a range that crosses the edge is clipped to the visible rows.
fn viewport_selection(
    sel: SelectionRange,
    off: i32,
    rows: usize,
) -> Option<((usize, usize), (usize, usize))> {
    if rows == 0 {
        return None;
    }
    let a = sel.start.line.0 + off;
    let b = sel.end.line.0 + off;
    if b < 0 || a >= rows as i32 {
        return None;
    }
    let a_row = a.max(0) as usize;
    let b_row = (b as usize).min(rows - 1);
    let a_col = if a < 0 { 0 } else { sel.start.column.0 };
    let b_col = if b >= rows as i32 {
        usize::MAX
    } else {
        sel.end.column.0
    };
    Some(((a_row, a_col), (b_row, b_col)))
}

/// Lines added under `prev` by a scroll, not an in-place edit of the last
/// row (that is the user typing). The first snapshot returns nothing: a
/// reader connecting mid-session must not hear the whole screen.
pub fn appended_lines(prev: &[String], next: &[String]) -> Vec<String> {
    if prev.is_empty() {
        return Vec::new();
    }
    if next.len() >= prev.len() && next[..prev.len()] == prev[..] {
        return next[prev.len()..].to_vec();
    }
    let max_k = prev.len().min(next.len());
    for k in 1..=max_k {
        let tail = &prev[k..];
        if next.len() >= tail.len() && next[..tail.len()] == tail[..] {
            return next[tail.len()..].to_vec();
        }
    }
    Vec::new()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Finish {
    exit: Option<i32>,
    command: String,
}

/// Coalesces output lines and a command-finish sentence.
#[derive(Clone, Debug, Default)]
struct Announcer {
    lines: Vec<String>,
    skipped: usize,
    started_ms: Option<u64>,
    finish: Option<Finish>,
}

impl Announcer {
    fn push_lines(&mut self, fresh: &[String], now_ms: u64) {
        for line in fresh {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            if self.started_ms.is_none() {
                self.started_ms = Some(now_ms);
            }
            self.lines.push(line.to_string());
            if self.lines.len() > TAIL_LINES {
                let drop_n = self.lines.len() - TAIL_LINES;
                self.lines.drain(..drop_n);
                self.skipped += drop_n;
            }
        }
    }

    fn command_finished(&mut self, exit: Option<i32>, command: &str, now_ms: u64) {
        if self.started_ms.is_none() {
            self.started_ms = Some(now_ms);
        }
        self.finish = Some(Finish {
            exit,
            command: command.trim().to_string(),
        });
        // The flood is not also read. The finish sentence is the announcement.
        self.lines.clear();
        self.skipped = 0;
    }

    fn poll(&mut self, now_ms: u64) -> Option<String> {
        if let Some(finish) = self.finish.take() {
            self.started_ms = None;
            self.lines.clear();
            self.skipped = 0;
            return Some(finish_sentence(finish.exit, &finish.command));
        }
        let due = self
            .started_ms
            .is_some_and(|t| now_ms.saturating_sub(t) >= GAP_MS);
        if !due || self.lines.is_empty() {
            return None;
        }
        let text = coalesce_tail(&self.lines, self.skipped);
        self.lines.clear();
        self.skipped = 0;
        self.started_ms = None;
        Some(text)
    }

    fn is_pending(&self) -> bool {
        self.finish.is_some() || self.started_ms.is_some()
    }

    /// Absolute millisecond at which [`Self::poll`] will yield, if anything is held.
    fn flush_at_ms(&self) -> Option<u64> {
        if self.finish.is_some() {
            return Some(0);
        }
        self.started_ms.map(|t| t.saturating_add(GAP_MS))
    }
}

fn coalesce_tail(lines: &[String], skipped: usize) -> String {
    let mut chosen: Vec<&str> = lines.iter().map(String::as_str).collect();
    while chosen.len() > 1
        && chosen.iter().map(|s| s.chars().count() + 1).sum::<usize>() > TAIL_CHARS
    {
        chosen.remove(0);
    }
    let omitted = skipped + lines.len() - chosen.len();
    let body = chosen.join("\n");
    if omitted == 0 {
        body
    } else if lang::rtl() {
        format!("… {omitted} سطراً أقدم\n{body}")
    } else {
        format!("… {omitted} earlier lines\n{body}")
    }
}

/// What [`OutputGate::push`] tells the tree builder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateUpdate {
    /// False while a burst is held: the document stays on the previous
    /// snapshot so UI Automation does not raise text-changed per line.
    pub refresh_document: bool,
    /// A new live-region name, when one announcement is due.
    pub live: Option<String>,
}

/// Diff, hold, and flush. The document text moves only when
/// [`GateUpdate::refresh_document`] is set.
#[derive(Clone, Debug, Default)]
pub struct OutputGate {
    announcer: Announcer,
    seen: Vec<String>,
    seeded: bool,
}

impl OutputGate {
    pub fn push(&mut self, lines: &[String], now_ms: u64) -> GateUpdate {
        if !self.seeded {
            self.seeded = true;
            self.seen = lines.to_vec();
            return GateUpdate {
                refresh_document: true,
                live: None,
            };
        }
        let fresh = appended_lines(&self.seen, lines);
        self.seen = lines.to_vec();
        self.announcer.push_lines(&fresh, now_ms);
        let live = self.announcer.poll(now_ms);
        let refresh_document = live.is_some() || !self.announcer.is_pending();
        GateUpdate {
            refresh_document,
            live,
        }
    }

    pub fn command_finished(&mut self, exit: Option<i32>, command: &str, now_ms: u64) {
        self.announcer.command_finished(exit, command, now_ms);
    }

    pub fn flush_at_ms(&self) -> Option<u64> {
        self.announcer.flush_at_ms()
    }

    /// A different pane must not be diffed against this one's lines.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// One tab in the bar.
#[derive(Clone, Debug)]
pub struct TabItem {
    pub index: usize,
    pub name: String,
    pub selected: bool,
}

/// One pane of the active tab.
#[derive(Clone, Debug)]
pub struct PaneItem {
    pub index: usize,
    pub name: String,
    pub text: PaneText,
}

/// The command palette, or the right-click menu when `menu` is set.
#[derive(Clone, Debug)]
pub struct PaletteItem {
    pub name: String,
    pub query: String,
    pub rows: Vec<String>,
    pub selected: usize,
    pub menu: bool,
}

/// A confirm card or an error card.
#[derive(Clone, Debug)]
pub struct DialogItem {
    pub name: String,
    pub body: String,
}

/// Everything the tree is built from. Strings are already in the UI language.
#[derive(Clone, Debug)]
pub struct View {
    pub tabs: Vec<TabItem>,
    pub active_tab: usize,
    pub panes: Vec<PaneItem>,
    pub focused_pane: usize,
    pub live: String,
    pub palette: Option<PaletteItem>,
    pub dialog: Option<DialogItem>,
}

/// Full tree. AccessKit's proxy activation returns no initial tree, so
/// every update from Bayan is a full tree.
pub fn build_tree(view: &View) -> TreeUpdate {
    let mut nodes = Vec::new();
    let mut root_children = Vec::new();

    let mut tab_ids = Vec::new();
    for tab in &view.tabs {
        let id = tab_node(tab.index);
        tab_ids.push(id);
        let mut node = Node::new(Role::Tab);
        node.set_label(tab.name.clone());
        if tab.selected {
            node.set_selected(true);
            for pane in &view.panes {
                node.push_child(pane_node(view.active_tab, pane.index));
            }
        }
        nodes.push((id, node));
    }
    let mut tabs = Node::new(Role::TabList);
    tabs.set_label(tab_list_name());
    tabs.set_children(tab_ids);
    nodes.push((ID_TABS, tabs));
    root_children.push(ID_TABS);

    let nested = view.tabs.iter().any(|tab| tab.selected);
    for pane in &view.panes {
        let id = pane_node(view.active_tab, pane.index);
        nodes.extend(pane_nodes(view.active_tab, pane));
        if !nested {
            root_children.push(id);
        }
    }

    let mut live = Node::new(Role::Log);
    live.set_label(view.live.clone());
    live.set_live(Live::Polite);
    nodes.push((ID_LIVE, live));
    root_children.push(ID_LIVE);

    if let Some(palette) = &view.palette {
        nodes.extend(palette_nodes(palette));
        root_children.push(ID_PALETTE);
    }
    if let Some(dialog) = &view.dialog {
        nodes.extend(dialog_nodes(dialog));
        root_children.push(ID_DIALOG);
    }

    let mut root = Node::new(Role::Window);
    root.set_label("Bayan — بيان");
    root.set_children(root_children);
    nodes.push((ID_ROOT, root));

    TreeUpdate {
        nodes,
        tree: Some(TreeInfo::new(ID_ROOT)),
        tree_id: TreeId::ROOT,
        focus: focus_of(view),
    }
}

fn focus_of(view: &View) -> NodeId {
    if view.dialog.is_some() {
        return ID_DIALOG;
    }
    if let Some(palette) = &view.palette {
        if palette.rows.is_empty() {
            return ID_PALETTE;
        }
        let sel = palette.selected.min(palette.rows.len() - 1);
        return option_node(sel);
    }
    if view.panes.is_empty() {
        return ID_ROOT;
    }
    let pane = view.focused_pane.min(view.panes.len() - 1);
    let index = view.panes[pane].index;
    pane_node(view.active_tab, index)
}

fn pane_nodes(tab: usize, pane: &PaneItem) -> Vec<(NodeId, Node)> {
    let mut runs = Vec::new();
    let mut children = Vec::new();
    for (li, line) in pane.text.lines.iter().enumerate() {
        let built = split_runs(tab, pane.index, li, line);
        let dir = text_direction(&line.text);
        for (i, run) in built.iter().enumerate() {
            let mut node = Node::new(Role::TextRun);
            node.set_value(run.value.clone());
            node.set_character_lengths(run.lengths.clone());
            node.set_word_starts(run.words.clone());
            node.set_text_direction(dir);
            if let Some(next) = built.get(i + 1) {
                node.set_next_on_line(next.id);
            }
            if i > 0 {
                node.set_previous_on_line(built[i - 1].id);
            }
            children.push(run.id);
            runs.push((run.id, node));
        }
    }
    if children.is_empty() {
        let id = run_node(tab, pane.index, 0, 0);
        let mut node = Node::new(Role::TextRun);
        node.set_value("\n");
        node.set_character_lengths(vec![1]);
        node.set_text_direction(TextDirection::LeftToRight);
        children.push(id);
        runs.push((id, node));
    }

    let mut doc = Node::new(Role::Document);
    doc.set_label(pane.name.clone());
    doc.set_children(children.clone());
    // No SetTextSelection: ActionRequested does not handle it, so a reader
    // must not be told it can move the selection.
    doc.set_font_family("monospace");
    if let Some((anchor, focus)) = text_selection(tab, pane) {
        doc.set_text_selection(TextSelection { anchor, focus });
    }
    let mut out = runs;
    out.push((pane_node(tab, pane.index), doc));
    out
}

struct BuiltRun {
    id: NodeId,
    value: String,
    lengths: Vec<u8>,
    words: Vec<u8>,
}

fn split_runs(tab: usize, pane: usize, line: usize, src: &LogicalLine) -> Vec<BuiltRun> {
    let chars: Vec<char> = src.text.chars().collect();
    let parts = chars.len().div_ceil(RUN_CHARS).max(1);
    let mut runs = Vec::with_capacity(parts);
    for part in 0..parts {
        let start = part * RUN_CHARS;
        let end = (start + RUN_CHARS).min(chars.len());
        let mut value: String = chars[start..end].iter().collect();
        let last = part + 1 == parts;
        if last {
            value.push('\n');
        }
        let lengths: Vec<u8> = value.chars().map(|c| c.len_utf8() as u8).collect();
        let words = word_starts(&value);
        runs.push(BuiltRun {
            id: run_node(tab, pane, line, part),
            value,
            lengths,
            words,
        });
    }
    runs
}

fn word_starts(value: &str) -> Vec<u8> {
    let mut starts = Vec::new();
    let mut in_word = false;
    for (i, ch) in value.chars().enumerate() {
        if i > u8::MAX as usize {
            break;
        }
        if ch == '\n' {
            break;
        }
        if ch.is_whitespace() {
            in_word = false;
        } else if !in_word {
            if i > 0 {
                starts.push(i as u8);
            }
            in_word = true;
        }
    }
    starts
}

/// First strong character. This is metadata for the run; the string itself
/// stays in logical order.
fn text_direction(text: &str) -> TextDirection {
    for ch in text.chars() {
        if bidi::is_arabic_letter(ch) {
            return TextDirection::RightToLeft;
        }
        if ch.is_ascii_alphabetic() {
            return TextDirection::LeftToRight;
        }
    }
    TextDirection::LeftToRight
}

/// Character under `col`, or the line break when the caret sits past the row.
fn char_index_at(line: &LogicalLine, col: usize) -> usize {
    for (i, (&c, &w)) in line.cols.iter().zip(line.widths.iter()).enumerate() {
        if w == 0 {
            continue;
        }
        if col < c + w {
            return i;
        }
    }
    line.cols.len()
}

fn position_at(
    tab: usize,
    pane: usize,
    line_index: usize,
    line: &LogicalLine,
    col: usize,
) -> TextPosition {
    let index = char_index_at(line, col);
    let part = index / RUN_CHARS;
    let parts = line.text.chars().count().div_ceil(RUN_CHARS).max(1);
    let part = part.min(parts - 1);
    let local = index - part * RUN_CHARS;
    TextPosition {
        node: run_node(tab, pane, line_index, part),
        character_index: local,
    }
}

fn text_selection(tab: usize, pane: &PaneItem) -> Option<(TextPosition, TextPosition)> {
    let lines = &pane.text.lines;
    if lines.is_empty() {
        return None;
    }
    if let Some((anchor, focus)) = pane.text.selection {
        let anchor = point_on(tab, pane.index, lines, anchor);
        let focus = point_on(tab, pane.index, lines, focus);
        return Some((anchor, focus));
    }
    let (row, col) = pane.text.cursor?;
    let row = row.min(lines.len() - 1);
    let at = position_at(tab, pane.index, row, &lines[row], col);
    Some((at, at))
}

fn point_on(
    tab: usize,
    pane: usize,
    lines: &[LogicalLine],
    (row, col): (usize, usize),
) -> TextPosition {
    let row = row.min(lines.len() - 1);
    position_at(tab, pane, row, &lines[row], col)
}

fn palette_nodes(palette: &PaletteItem) -> Vec<(NodeId, Node)> {
    let mut nodes = Vec::new();
    let mut row_ids = Vec::new();
    let sel = if palette.rows.is_empty() {
        0
    } else {
        palette.selected.min(palette.rows.len() - 1)
    };
    for (i, row) in palette.rows.iter().enumerate() {
        let id = option_node(i);
        let mut node = Node::new(if palette.menu {
            Role::MenuItem
        } else {
            Role::ListBoxOption
        });
        node.set_label(row.clone());
        if i == sel && !palette.rows.is_empty() {
            node.set_selected(true);
        }
        row_ids.push(id);
        nodes.push((id, node));
    }
    if palette.menu {
        let mut menu = Node::new(Role::Menu);
        menu.set_label(palette.name.clone());
        menu.set_children(row_ids);
        nodes.push((ID_PALETTE, menu));
        return nodes;
    }
    let mut query = Node::new(Role::SearchInput);
    query.set_label(search_name());
    query.set_value(palette.query.clone());
    nodes.push((ID_PALETTE_QUERY, query));
    let mut list = Node::new(Role::ListBox);
    list.set_label(palette.name.clone());
    list.set_children(row_ids);
    nodes.push((ID_PALETTE_LIST, list));
    let mut dialog = Node::new(Role::Dialog);
    dialog.set_label(palette.name.clone());
    dialog.set_children(vec![ID_PALETTE_QUERY, ID_PALETTE_LIST]);
    nodes.push((ID_PALETTE, dialog));
    nodes
}

fn dialog_nodes(dialog: &DialogItem) -> Vec<(NodeId, Node)> {
    let mut body = Node::new(Role::Label);
    body.set_label(dialog.body.clone());
    let mut node = Node::new(Role::Dialog);
    node.set_label(dialog.name.clone());
    node.push_child(ID_DIALOG_BODY);
    vec![(ID_DIALOG_BODY, body), (ID_DIALOG, node)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{self, Lang};
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    struct LangGuard(Lang);
    impl Drop for LangGuard {
        fn drop(&mut self) {
            lang::set(self.0);
        }
    }
    fn with_lang(lang: Lang) -> LangGuard {
        let prev = lang::get();
        lang::set(lang);
        LangGuard(prev)
    }

    fn cells_of(text: &str) -> Vec<GridCell> {
        text.chars()
            .enumerate()
            .map(|(col, ch)| GridCell {
                col,
                ch,
                width: 1,
                extra: String::new(),
            })
            .collect()
    }

    fn find(update: &TreeUpdate, id: NodeId) -> &Node {
        update
            .nodes
            .iter()
            .find(|(nid, _)| *nid == id)
            .map(|(_, node)| node)
            .unwrap_or_else(|| panic!("missing node {id:?}"))
    }

    fn line_of(text: &str) -> LogicalLine {
        logical_line(&cells_of(text), false)
    }

    fn one_pane(
        text: &str,
        cursor: (usize, usize),
        selection: Option<((usize, usize), (usize, usize))>,
    ) -> View {
        View {
            tabs: vec![TabItem {
                index: 0,
                name: "المستندات".into(),
                selected: true,
            }],
            active_tab: 0,
            panes: vec![PaneItem {
                index: 0,
                name: pane_name(0, 1),
                text: PaneText {
                    lines: vec![line_of(text)],
                    cursor: Some(cursor),
                    selection,
                },
            }],
            focused_pane: 0,
            live: live_name().into(),
            palette: None,
            dialog: None,
        }
    }

    #[test]
    fn mixed_arabic_latin_line_stays_in_logical_order() {
        // Storage order is what the program wrote. Visual BiDi would put
        // the Arabic on the right and a column walk of the shaped line
        // would not equal this string.
        let ltr = logical_line(&cells_of("hello مرحبا"), false);
        assert_eq!(ltr.text, "hello مرحبا");
        assert!(ltr.text.find('م') < ltr.text.find('ا'));
        let rtl = logical_line(&cells_of("مرحبا hello"), false);
        assert_eq!(rtl.text, "مرحبا hello");
        assert!(rtl.text.starts_with('م'));
        assert!(rtl.text.ends_with("hello"));

        let mut term = Term::new(
            Config::default(),
            &crate::term::TermSize { cols: 40, rows: 4 },
            VoidListener,
        );
        let mut parser = Processor::<StdSyncHandler>::new();
        parser.advance(&mut term, "hello مرحبا".as_bytes());
        let pane = read_pane(&term, false);
        assert_eq!(pane.lines[0].text, "hello مرحبا");
    }

    #[test]
    fn caret_is_the_character_under_the_cursor_column() {
        let _lang = with_lang(Lang::Ar);
        let view = one_pane("hello مرحبا", (0, 6), None);
        let tree = build_tree(&view);
        let doc = find(&tree, pane_node(0, 0));
        let sel = doc.text_selection().expect("caret");
        assert_eq!(sel.anchor, sel.focus, "a caret is a degenerate selection");
        assert_eq!(sel.focus.character_index, 6);
        let run = find(&tree, sel.focus.node);
        assert_eq!(run.role(), Role::TextRun);
        let ch = run.value().unwrap().chars().nth(6).unwrap();
        assert_eq!(ch, 'م');

        // A wide glyph covers two columns. The caret on the spacer column
        // is still that one character, not the next.
        let wide = LogicalLine {
            text: "a😀b".into(),
            cols: vec![0, 1, 3],
            widths: vec![1, 2, 1],
        };
        assert_eq!(char_index_at(&wide, 1), 1);
        assert_eq!(char_index_at(&wide, 2), 1);
        assert_eq!(char_index_at(&wide, 3), 2);
        let view = View {
            panes: vec![PaneItem {
                index: 0,
                name: pane_name(0, 1),
                text: PaneText {
                    lines: vec![wide],
                    cursor: Some((0, 2)),
                    selection: None,
                },
            }],
            ..one_pane("x", (0, 0), None)
        };
        let tree = build_tree(&view);
        let sel = find(&tree, pane_node(0, 0)).text_selection().unwrap();
        assert_eq!(sel.focus.character_index, 1);
        assert_eq!(
            find(&tree, sel.focus.node).value().unwrap().chars().nth(1),
            Some('😀')
        );
    }

    #[test]
    fn selection_range_covers_the_chosen_cells() {
        let _lang = with_lang(Lang::Ar);
        let view = one_pane("hello مرحبا", (0, 10), Some(((0, 6), (0, 10))));
        let tree = build_tree(&view);
        let sel = find(&tree, pane_node(0, 0)).text_selection().unwrap();
        assert_eq!(sel.anchor.character_index, 6);
        assert_eq!(sel.focus.character_index, 10);
        assert_eq!(sel.anchor.node, sel.focus.node);
        let value = find(&tree, sel.anchor.node).value().unwrap();
        let chosen: String = value.chars().skip(6).take(5).collect();
        assert_eq!(chosen, "مرحبا");
    }

    #[test]
    fn output_announcements_coalesce_a_flood_into_one_tail() {
        let _lang = with_lang(Lang::Ar);
        let mut gate = OutputGate::default();
        let seed = gate.push(&["ready".into()], 0);
        assert!(seed.live.is_none());
        assert!(seed.refresh_document);

        for i in 0..100 {
            let mut lines = vec!["ready".into()];
            for j in 0..=i {
                lines.push(format!("compiling item {j}"));
            }
            let held = gate.push(&lines, 0);
            assert!(held.live.is_none(), "line {i} must not announce alone");
            assert!(
                !held.refresh_document,
                "the document stays put during the burst"
            );
        }
        assert_eq!(gate.flush_at_ms(), Some(GAP_MS));

        let mut lines = vec!["ready".into()];
        for j in 0..100 {
            lines.push(format!("compiling item {j}"));
        }
        let flushed = gate.push(&lines, GAP_MS);
        let live = flushed.live.expect("one announcement for the flood");
        assert!(flushed.refresh_document);
        assert!(live.contains("compiling item 99"), "{live}");
        assert!(!live.contains("compiling item 0"), "{live}");
        assert!(live.contains("92"), "{live}");
        let again = gate.push(&lines, GAP_MS);
        assert!(again.live.is_none(), "the same tail is not spoken twice");
    }

    #[test]
    fn command_finish_announces_the_exit_code_once() {
        let _lang = with_lang(Lang::Ar);
        let mut gate = OutputGate::default();
        gate.push(&["ready".into()], 0);
        for i in 0..40 {
            let mut lines = vec!["ready".into()];
            for j in 0..=i {
                lines.push(format!("line {j}"));
            }
            gate.push(&lines, 10);
        }
        gate.command_finished(Some(0), "cargo build", 20);
        let mut lines = vec!["ready".into(), "done".into()];
        lines.extend((0..40).map(|j| format!("line {j}")));
        let update = gate.push(&lines, 20);
        let live = update.live.expect("finish is one announcement");
        assert!(live.contains("اكتمل الأمر"), "{live}");
        assert!(live.contains("cargo build"), "{live}");
        assert!(live.contains("رمز الخروج 0"), "{live}");
        assert!(!live.contains("line 0"), "{live}");
        let again = gate.push(&lines, 20);
        assert!(again.live.is_none());

        gate.command_finished(Some(2), "cargo test", 30);
        let failed = gate.push(&lines, 30).live.unwrap();
        assert!(failed.contains("فشل الأمر"), "{failed}");
        assert!(failed.contains("رمز الخروج 2"), "{failed}");
    }

    /// Two commands that finish the same way produce the same sentence. The
    /// second must still change the live region's name, or UIA stays silent.
    #[test]
    fn a_repeated_finish_sentence_changes_the_live_label() {
        let _lang = with_lang(Lang::Ar);
        let mut gate = OutputGate::default();
        let lines = vec!["ready".to_string()];
        gate.push(&lines, 0);
        let mut on_screen = live_name().to_string();
        let mut labels = Vec::new();
        for t in [10, 20, 30] {
            gate.command_finished(Some(0), "cargo build", t);
            let live = gate.push(&lines, t).live.expect("each finish announces");
            on_screen = live_label(&on_screen, live);
            labels.push(on_screen.clone());
        }
        assert_ne!(labels[0], labels[1], "the repeat must re-fire");
        assert_ne!(labels[1], labels[2], "and so must the one after it");
        assert_eq!(labels[0].trim_end(), labels[1].trim_end());
        assert!(labels.iter().all(|l| !l.contains('\u{200b}')));
        // A different sentence goes through untouched.
        assert_eq!(live_label(&on_screen, "other".into()), "other");
    }

    #[test]
    fn tabs_and_panes_have_arabic_names_and_roles() {
        let _lang = with_lang(Lang::Ar);
        let view = View {
            tabs: vec![
                TabItem {
                    index: 0,
                    name: "المستندات".into(),
                    selected: true,
                },
                TabItem {
                    index: 1,
                    name: "src".into(),
                    selected: false,
                },
            ],
            active_tab: 0,
            panes: vec![
                PaneItem {
                    index: 0,
                    name: pane_name(0, 2),
                    text: PaneText {
                        lines: vec![line_of("one")],
                        cursor: Some((0, 0)),
                        selection: None,
                    },
                },
                PaneItem {
                    index: 1,
                    name: pane_name(1, 2),
                    text: PaneText {
                        lines: vec![line_of("two")],
                        cursor: Some((0, 0)),
                        selection: None,
                    },
                },
            ],
            focused_pane: 1,
            live: live_name().into(),
            palette: None,
            dialog: None,
        };
        let tree = build_tree(&view);
        let tabs = find(&tree, ID_TABS);
        assert_eq!(tabs.role(), Role::TabList);
        assert_eq!(tabs.label(), Some("التبويبات"));
        let first = find(&tree, tab_node(0));
        let second = find(&tree, tab_node(1));
        assert_eq!(first.role(), Role::Tab);
        assert_eq!(first.label(), Some("المستندات"));
        assert_eq!(first.is_selected(), Some(true));
        assert_eq!(second.label(), Some("src"));
        assert_eq!(second.is_selected(), None);
        let pane = find(&tree, pane_node(0, 1));
        assert_eq!(pane.role(), Role::Document);
        assert_eq!(pane.label(), Some("اللوحة 2"));
        assert!(
            !pane.supports_action(accesskit::Action::SetTextSelection),
            "ActionRequested does not handle it, so it is not advertised"
        );
        assert_eq!(find(&tree, pane_node(0, 0)).label(), Some("اللوحة 1"));
        let live = find(&tree, ID_LIVE);
        assert_eq!(live.role(), Role::Log);
        assert_eq!(live.live(), Some(Live::Polite));
        assert_eq!(live.label(), Some("المخرجات الجديدة"));

        drop(_lang);
        let _en = with_lang(Lang::En);
        let mut english = view;
        english.panes[0].name = pane_name(0, 2);
        english.panes[1].name = pane_name(1, 2);
        english.live = live_name().into();
        let tree = build_tree(&english);
        assert_eq!(find(&tree, ID_TABS).label(), Some("Tabs"));
        assert_eq!(find(&tree, pane_node(0, 0)).label(), Some("Pane 1"));
        assert_eq!(find(&tree, pane_node(0, 1)).label(), Some("Pane 2"));
    }

    #[test]
    fn focus_follows_the_active_pane() {
        let _lang = with_lang(Lang::Ar);
        let mut view = View {
            tabs: vec![
                TabItem {
                    index: 0,
                    name: "أ".into(),
                    selected: false,
                },
                TabItem {
                    index: 3,
                    name: "ب".into(),
                    selected: true,
                },
            ],
            active_tab: 3,
            panes: vec![
                PaneItem {
                    index: 0,
                    name: pane_name(0, 2),
                    text: PaneText {
                        lines: vec![line_of("left")],
                        cursor: Some((0, 0)),
                        selection: None,
                    },
                },
                PaneItem {
                    index: 1,
                    name: pane_name(1, 2),
                    text: PaneText {
                        lines: vec![line_of("right")],
                        cursor: Some((0, 1)),
                        selection: None,
                    },
                },
            ],
            focused_pane: 1,
            live: live_name().into(),
            palette: None,
            dialog: None,
        };
        let tree = build_tree(&view);
        assert_eq!(tree.focus, pane_node(3, 1));
        view.focused_pane = 0;
        let tree = build_tree(&view);
        assert_eq!(tree.focus, pane_node(3, 0));
        view.tabs[0].selected = true;
        view.tabs[1].selected = false;
        view.active_tab = 0;
        let tree = build_tree(&view);
        assert_eq!(tree.focus, pane_node(0, 0));
        assert_ne!(tree.focus, pane_node(3, 0));
    }

    #[test]
    fn palette_and_confirm_card_have_names_and_roles() {
        let _lang = with_lang(Lang::Ar);
        let mut view = one_pane("ready", (0, 0), None);
        view.palette = Some(PaletteItem {
            name: palette_name().into(),
            query: "تبويب".into(),
            rows: vec!["تبويب جديد".into(), "نسخ".into()],
            selected: 1,
            menu: false,
        });
        let tree = build_tree(&view);
        let palette = find(&tree, ID_PALETTE);
        assert_eq!(palette.role(), Role::Dialog);
        assert_eq!(palette.label(), Some("لوحة الأوامر"));
        assert_eq!(find(&tree, ID_PALETTE_QUERY).role(), Role::SearchInput);
        assert_eq!(find(&tree, ID_PALETTE_QUERY).label(), Some("بحث"));
        assert_eq!(find(&tree, ID_PALETTE_QUERY).value(), Some("تبويب"));
        let row = find(&tree, option_node(1));
        assert_eq!(row.role(), Role::ListBoxOption);
        assert_eq!(row.label(), Some("نسخ"));
        assert_eq!(row.is_selected(), Some(true));
        assert_eq!(tree.focus, option_node(1));

        view.palette.as_mut().unwrap().menu = true;
        view.palette.as_mut().unwrap().name = menu_name().into();
        let tree = build_tree(&view);
        assert_eq!(find(&tree, ID_PALETTE).role(), Role::Menu);
        assert_eq!(find(&tree, ID_PALETTE).label(), Some("القائمة"));
        assert_eq!(find(&tree, option_node(0)).role(), Role::MenuItem);

        view.dialog = Some(DialogItem {
            name: paste_dialog_name().into(),
            body: "ليس عند المحث".into(),
        });
        let tree = build_tree(&view);
        let dialog = find(&tree, ID_DIALOG);
        assert_eq!(dialog.role(), Role::Dialog);
        assert_eq!(dialog.label(), Some("تأكيد اللصق"));
        assert_eq!(find(&tree, ID_DIALOG_BODY).label(), Some("ليس عند المحث"));
        assert_eq!(tree.focus, ID_DIALOG);

        let close = close_pane_message("cargo build");
        assert!(close.contains("إغلاق اللوحة"), "{close}");
        assert!(close.contains("cargo build"), "{close}");
        view.dialog = Some(DialogItem {
            name: close.clone(),
            body: close.clone(),
        });
        let tree = build_tree(&view);
        assert_eq!(find(&tree, ID_DIALOG).role(), Role::Dialog);
        assert_eq!(find(&tree, ID_DIALOG).label(), Some(close.as_str()));
    }
}
