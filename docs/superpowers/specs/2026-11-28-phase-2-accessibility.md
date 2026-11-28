# Phase 2 — accessibility completion

**Date:** 2026-11-28 · **Status:** approved roadmap item
**Scope (roadmap):** SetTextSelection, magnifier caret boxes, vim count prefixes.

Three independent gaps, landed as separate PRs (most tractable first):

## 1. Vim count prefixes (copy mode) — PR 1 (merged #128)

Copy mode (`copymode.rs`, alacritty vi mode) ignores numeric prefixes: `5j`,
`3w`, `10k` do nothing. Add a count accumulator.

- `copymode::count_digit(physical, shift) -> Option<u8>`: unshifted physical
  digit → 0–9 (physical-key rule, so an Arabic layout keeps it). Shifted digits
  stay symbols (`$`, `%`, `^`) handled by `command`.
- `App.copy_count: usize` (keyroute): `copy_mode_key` accumulates digits
  (`1`–`9`, then `0`–`9`; leading `0` with no count stays the `First` motion).
  A motion applies `max(1, count)` times; non-motion commands ignore the count.
  Reset on exit, on toggle-enter, and on an unknown key. Cap at 9999.
- Pure unit tests: digit accumulation, leading-zero, `5j`/`3w` move the right
  distance, count resets, shifted digits stay symbols.

## 2. Magnifier caret boxes — PR 2 (merged #129)

accesskit nodes carried no bounds (`set_bounds` unused), so Windows Magnifier
could not track the caret. Now the focused pane's document node — the tree's
`focus` node, which Magnifier follows — is bounded to the caret box.

- `PaneItem.caret_bounds: Option<(i32,i32,i32,i32)>` (logical window pixels),
  set only for the focused pane. `reader_view` derives it from the renderer's
  `ime_spot` (the focused caret's physical box, already computed for IME) divided
  by `window.scale_factor()` (accesskit bounds are logical).
- `pane_nodes` calls `doc.set_bounds(Rect::new(x, y, x+w, y+h))` when present.
- Test: `the_focused_caret_bounds_reach_the_document_node_for_the_magnifier`
  (no bounds → unbounded; with bounds → focus node carries the exact rect).
- **Verification gap (flagged):** unit-tested that the bounds reach the focus
  node; live Magnifier tracking + the logical-pixel coordinate assumption need
  an interactive Magnifier session to confirm (NVDA not installed here).

## 3. SetTextSelection — DEFERRED (owner decision 2026-11-28)

`ActionRequested` is a no-op (`main.rs`) and the document does not advertise
`SetTextSelection` (`screen_reader.rs`). Handling it needs an inverse coordinate
mapping (TextPosition → run-node ID parse → `run_spans` inverse → logical column
→ viewport row → absolute grid point; `PaneText` does not currently store the
`source_rows` paragraph→row map) and then a **live-terminal selection write** —
a security-sensitive area (selection feeds the clipboard).

Deferred because it cannot be live-verified here (NVDA not installed) and the
selection write is security-sensitive. Revisit in a dedicated session with a
screen reader available. The a11y milestone ships vim counts (#128) + magnifier
caret boxes (#129) for now; Phase 2 continues with output triggers/snippets.

## Standing constraints

Pure-Rust runtime; no new deps; each change its own PR with unit tests; the
screen-reader items (2, 3) need live a11y verification (NVDA / Magnifier) — flag
what can be unit-tested vs. what needs a live client. Perf gate + full suite green.
