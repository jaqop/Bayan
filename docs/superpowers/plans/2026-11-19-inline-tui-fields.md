# Inline TUI Fields Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Primary-screen rows with Box Drawing characters and Arabic draw as TUI fields, so their borders stay on their grid columns.

**Architecture:** One pure predicate beside `tui_row` in `src/render.rs` decides "TUI fields or one shaped line". `paint_row` and both click-side `align_edge` computations in `src/main.rs` call it, so drawing and hit-testing cannot disagree. `draw_line_tui` and `draw_line_bidi` are untouched.

**Tech Stack:** Rust, alacritty_terminal, cosmic-text. Existing `#[cfg(test)]` modules in `render.rs`.

**Spec:** `docs/superpowers/specs/2026-11-19-inline-tui-fields-design.md`

## Global Constraints

- New primary-screen trigger is Box Drawing only: `'\u{2500}'..='\u{257f}'`. Not Block Elements (`U+2580..=U+259F`), not private use (`U+E000..=U+F8FF`).
- Today's rule is kept exactly: `(claude || alt) && tui_row(..)` with `prompt_start = true`.
- `tui_row`'s own `boxy` range, `draw_line_tui`, `draw_line_bidi`, and `render_flow.rs` do not change.
- `cargo fmt` clean; `cargo clippy --all-targets -- -D warnings` clean (no dead `snap_is_tui` left behind).
- Commit messages are a plain sentence about what changed and why, no conventional-commit prefix.
- Tests never read `~/.bayan`.

## Review Focus

1. oh-my-posh prompt row: Powerline glyph `U+E0B0` plus an Arabic cwd on the primary screen must keep today's path (whole line). Pinned in Task 1's truth table.
2. Progress bar with an Arabic label (`████ تحميل`) on the primary screen must keep today's path. Pinned in Task 1's truth table.
3. `tree` output (`├── ملف.txt`) on the primary screen takes the field path. Pinned in Task 1's truth table.
4. Arabic prose with 2 and 3 spaces on the primary screen keeps today's path and right-edge alignment. Pinned in Task 1's truth table, plus the existing `rtl_paragraph_glyphs_flush_right_and_ltr_stays_left` regression.
5. A click on a Gemini-style row resolves through the same path it was drawn with. Pinned in Task 2's agreement test.

---

### Task 1: Predicate and drawing path

**Files:**
- Modify: `src/render.rs` (beside `is_tui_row` / `snap_is_tui` / `tui_row`, ~1647-1702; `paint_row`, ~4487)
- Test: `src/render.rs`, the test module holding `row_cells` at ~8878 and `arabic_tui_text_does_not_move_an_anchored_status` at ~9006

**Interfaces:**
- Produces:
  - `fn tui_path(claude: bool, alt: bool, cells: &[CellInfo]) -> bool`
  - `pub(crate) fn snap_tui_path(claude: bool, alt: bool, cells: &[SnapCell]) -> bool`
  - Both delegate to one private `fn tui_path_at(n: usize, at: impl Fn(usize) -> (usize, usize, char), claude: bool, alt: bool) -> bool`, so the two cannot drift.

- [ ] **Step 1: Write the failing truth-table test** `primary_screen_box_rows_take_the_field_path`

```rust
let p = |t: &str| tui_path(false, false, &row_cells(t, 0));
assert!(p("│ 1. Trust folder (سلام abc مرحبا) │"));
assert!(p("╭── سلام"));
assert!(p("├── ملف.txt"));
assert!(p("───── مرحبا"));
assert!(!p("مرحبا بالعالم  هذا نثر"));
assert!(!p("مرحبا   STATUS"));
assert!(!p("\u{e0b0} C:\\سلام"));
assert!(!p("████ تحميل"));
for (claude, alt) in [(true, false), (false, true), (true, true)] {
    for t in ["│ مرحبا بالعالم │", "مرحبا   STATUS", "مرحبا بالعالم  هذا نثر"] {
        let cells = row_cells(t, 0);
        assert_eq!(tui_path(claude, alt, &cells), is_tui_row(&cells), "{t:?} claude={claude} alt={alt}");
    }
}
```

- [ ] **Step 2: Write the failing render test** `primary_screen_box_border_stays_on_its_column`

Copy the harness of `arabic_tui_text_does_not_move_an_anchored_status`, with two changes: no `\x1b[?1049h` (primary screen), and `claude` is always `false`. The bytes are
`format!("\x1b[2;1H│ {prefix}\x1b[2;41H\x1b[31m│")`. Assert `(status_x("") - status_x("1. Trust (سلام abc مرحبا)")).abs() <= 1.0`, with the message `"Arabic moved the border from {expected} to {actual}"`.

- [ ] **Step 3: Run both tests, verify they fail**

Run: `cargo test primary_screen_box`
Expected: compile error, `tui_path` not found. If you add the functions before wiring `paint_row`, the render test still FAILS with the border about one cell (~14 px at scale 1.0 is not guaranteed; any > 1.0 difference) to the left.

- [ ] **Step 4: Implement `tui_path_at`, `tui_path`, `snap_tui_path`**

`tui_path_at` returns `(claude || alt) && tui_row(n, &at, true)`, or `!claude && !alt && (0..n).any(|i| matches!(at(i).2, '\u{2500}'..='\u{257f}'))`. Doc comment: one line saying the primary screen joins only on Box Drawing, because Powerline (private use) builds shell prompts. `is_tui_row` stays (tests use it). Delete `snap_is_tui` once Task 2 replaces its two callers; until then keep it.

- [ ] **Step 5: Wire `paint_row`**

Replace `(row_claude || alt) && is_tui_row(cells)` with `tui_path(row_claude, alt, cells)`. Leave the comment above it, extended by one clause: inline TUIs that draw boxes on the primary screen take the same path.

- [ ] **Step 6: Run the tests, verify they pass, plus the regressions**

Run: `cargo test -- primary_screen_box rtl_paragraph_glyphs_flush_right arabic_tui_text_does_not_move tui_fields_respect prose_with_double_spaces`
Expected: all PASS.

- [ ] **Step 7: Commit**

```bash
git add src/render.rs
git commit -m "Inline TUI boxes with Arabic keep their borders on the grid: primary-screen Box Drawing rows take the field path"
```

### Task 2: Click path, docs, live check

**Files:**
- Modify: `src/main.rs` (~6017 and ~6081, the two `align_edge` lines)
- Modify: `src/render.rs` (remove `snap_is_tui`)
- Modify: `README.md` (BiDi section around the "LTR paragraphs, TUI fields, and the alternate screen stay on the left" sentence, ~1532, plus the milestone log entry)
- Test: `src/render.rs` test module

**Interfaces:**
- Consumes: `snap_tui_path(claude, alt, &[SnapCell])`, `tui_path(claude, alt, &[CellInfo])` from Task 1.

- [ ] **Step 1: Write the failing agreement test** `snapshot_and_paint_agree_on_the_tui_path`

For each text in Task 1's truth-table list, and each `(claude, alt)` in `[(false,false),(true,false),(false,true)]`: build `SnapCell { col: i, w: 1, c, zw: vec![], fg: AnsiColor::Named(NamedColor::Foreground), bg: AnsiColor::Named(NamedColor::Background), flags: Flags::empty() }` per char, and assert `snap_tui_path(claude, alt, &snaps) == tui_path(claude, alt, &row_cells(t, 0))`.

- [ ] **Step 2: Run it, verify it passes against Task 1's functions**

Run: `cargo test snapshot_and_paint_agree`
Expected: PASS. This pins the agreement before the callers change.

- [ ] **Step 3: Replace both `align_edge` computations in `main.rs`**

`let align_edge = reorder && !alt && !render::snap_tui_path(claude, alt, &cells);` at both sites. Update the comment above each: "Same split as draw_pane (`render::tui_path`)". Then delete `snap_is_tui` from `render.rs`.

- [ ] **Step 4: Full checks**

Run: `cargo fmt; cargo clippy --all-targets -- -D warnings; cargo test`
Expected: fmt makes no changes, clippy prints no warnings, all tests pass.

- [ ] **Step 5: README**

In the BiDi paragraph, after "LTR paragraphs, TUI fields, and the alternate screen stay on the left.", add: on the primary screen, a row with Box Drawing characters (`U+2500..=U+257F`) is drawn as TUI fields, so an inline TUI's borders (Gemini CLI's dialogs, `tree`) stay on their columns. Powerline prompts and Block Elements are not included. Add a milestone log entry with what was measured: a 14 px border shift on the Gemini trust dialog at 2026-11-19, and that it is fixed.

- [ ] **Step 6: Live check**

Run `cargo build`, then launch with `BAYAN_SHOW_NOW=1`, `BAYAN_TYPE="gemini\r"`, the working directory set to a temp folder named `سلام abc مرحبا`, and `~/.bayan/session.json` moved aside and restored afterwards. Capture with `PrintWindow` from a DPI-aware process. Assert that the PID is alive before and after the capture and that it owns the window. Expected: the right border `│` has the same x on the folder-name row and on the rows above and below it. Before this change it was 1597 against 1611 at 1650 px width. Stop the probe's `gemini` node processes and Bayan afterwards.

- [ ] **Step 7: Commit**

```bash
git add src/main.rs src/render.rs README.md
git commit -m "Clicks on inline TUI rows follow the same field path as drawing; README records the Gemini border measurement"
```
