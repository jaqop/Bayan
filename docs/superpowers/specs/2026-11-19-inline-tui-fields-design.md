# Inline TUI rows with Arabic keep their borders on the grid

Date: 2026-11-19. Size: small change.

## Why

Measured in a live window: Gemini CLI 0.68 draws its trust dialog inline on
the primary screen. On the row that holds an Arabic folder name, the box's
right border `│` lands at x=1597 instead of 1611 (one 14 px cell left of the
rows above and below). The Arabic shapes narrower than its cells, and the
whole-line path carries everything after it along.

The per-field path that prevents this already exists (`draw_line_tui`:
borders on the grid, each Arabic field shaped from its own left column). It
only runs when `(row_claude || alt) && is_tui_row(cells)`
(`render.rs` `paint_row`). Gemini and Codex are neither Claude nor on the
alternate screen, so their rows take `draw_line_bidi`.

## Goal

A primary-screen row that contains box-drawing characters and Arabic takes
the field path, so its borders sit on their grid columns.

## Non-goals

- The lam-alef / narrow-Arabic shift inside one field. That is the dual
  engine's accepted trade-off and stays.
- `ls`-style columns and other gap-only TUI rows on the primary screen
  (approach B, rejected below).
- Any change to the alternate screen, Claude mode, explicit BDSM, or reflow
  (`render_flow.rs` already excludes TUI rows on every screen).

## Anti-goals

- Arabic prose on the primary screen losing its right-edge alignment or
  being split into fields. A plain Arabic sentence, with or without double
  or triple spaces, must draw exactly as it does today.
- Drawing and clicking disagreeing about the path a row took.

## Approaches

- **A (picked).** On the primary screen, the field path also applies when
  the row has a box-drawing character. Other `tui_row` signals (3 spaces, a
  column gap, a leading `>`) stay limited to the alternate screen and Claude.
- **B.** Every row `is_tui_row` accepts goes to the field path on the
  primary screen too. Rejected: `"مرحبا   STATUS"` already counts as a TUI
  row, so Arabic prose with a wide gap would be split into fields laid out
  in grid order, reversing an RTL paragraph's parts and dropping its
  right-edge alignment.
- **C.** Keep the whole-line path and pin box-drawing glyphs to their grid
  columns after shaping. Rejected: UAX#9 mirrors those glyphs' positions
  in an RTL paragraph, so pinning them would separate where a glyph is drawn
  from where clicks and the cursor resolve it ("logical column ≠ visual x").
  Also the most code.

## Design

1. One pure decision beside `tui_row` in `render.rs`:

   ```rust
   /// Whether a row with Arabic draws as TUI fields instead of one shaped line.
   pub(crate) fn tui_path(claude: bool, alt: bool, row: impl RowCells) -> bool
   ```

   True when `(claude || alt) && tui_row(..)` (today's rule), or when
   `!alt && !claude` and the row contains a character in `U+2500..=U+257F`
   (Box Drawing).

2. `paint_row` (render.rs) and both `align_edge` computations in `main.rs`
   (around lines 6017 and 6081) call `tui_path` instead of repeating the
   condition. `align_edge` becomes `reorder && !alt && !tui_path(..)`.
   Drawing and the click snapshot then follow the same path by construction.

3. No change inside `draw_line_tui` or `draw_line_bidi`.

### Calls made (mine, for review)

- **Box Drawing only, `U+2500..=U+257F`.** Not Block Elements
  (`U+2580..=U+259F`, used for progress bars and shading) and not the
  private-use range. Powerline and Nerd Font glyphs sit in private use, and
  oh-my-posh prompts are built from them: a prompt row with an Arabic cwd or
  Arabic typed input would switch to fields and, through per-field auto
  direction, could reorder what the user is typing. Prompts stay as they are.
  `tui_row`'s own `boxy` range for the alternate screen is unchanged.
- **The direction inside a field is unchanged.** `tree` output such as
  `├── ملف.txt` moves from flush-right to its grid column, and keeps the RTL
  paragraph order it has today.

## Tests

- `tui_path` truth table: primary screen with `│`, `╭`, `├`, and `─` → true;
  primary screen Arabic prose with 2 and 3 spaces → false; primary screen
  Powerline glyph `U+E0B0` with Arabic → false; Block Elements `█` → false;
  alternate screen and Claude cases match today's `is_tui_row` results.
- Render test in the style of `arabic_tui_text_does_not_move_an_anchored_status`,
  but on the primary screen: `│ 1. Trust folder (سلام abc مرحبا)` with a
  closing `│` at a fixed column. The closing border's quad x equals
  `column × cell_w`.
- Same harness: a primary-screen Arabic prose line still draws flush with
  the pane's right edge.
- Live check (build → launch → capture, as in this session's probe): the
  Gemini trust dialog from a folder named `سلام abc مرحبا` shows the right
  border on one x across all rows.

## Left to the builder

- Exact signature of `tui_path` (closure like `tui_row`, or two thin wrappers
  for `CellInfo` and `SnapCell` like `is_tui_row` / `snap_is_tui`).
- Whether `is_tui_row` / `snap_is_tui` remain as separate functions or fold
  into the new one.
- The README entry for the change (project log convention).
