# Phase C — Tab & Pane UX (design)

**Date:** 2026-11-28 · **Status:** approved roadmap Phase C (owner locked order)
**Precondition:** Phase A/B merged; full suite + GUI harness + perf gate green.

Phase C makes tabs and panes rearrangeable by hand, in three increments, each
its own PR with unit tests and a harness proof where the environment allows.
No speculative refactors (owner no-premature-churn rule); groups, MRU history
and the saved layout stay consistent through every rearrangement.

## C1. Keyboard reorder (landed, PR #121)

`Ctrl+Alt+Left/Right` move the active tab one slot within its group, plus the
matching palette rows. The move is an adjacent swap of the two `self.tabs`
entries; `Mru::remap` relabels history so `Ctrl+Tab` still walks the tabs the
user last visited, and group membership is untouched.

## C2. Mouse drag-to-reorder (this increment)

A left press on a tab still switches to it, and arms a drag. Past a small
horizontal threshold the drag engages: an insertion caret shows in the bar at
the slot the pointer would drop the tab into, and release moves the tab there.
A press that never crosses the threshold is just a click (no reorder).

- Drop slot: left half of a drawn tab inserts before it, right half after it;
  past either end of the drawn row clamps to that end. Pure geometry
  (`chrome::tab_drop_target`), unit-tested over synthetic spans.
- The move reuses C1's adjacent-swap primitive (`move_active_tab`), stepped
  from the dragged tab's slot to the drop slot, so MRU remap, group
  preservation and `save_state` are the already-tested path — not a second
  reorder implementation.
- The drag owns the pointer like a divider drag; a resize, focus loss or tab
  switch cancels it. The indicator is renderer state cleared on release.
- Proof: geometry unit tests + a renderer indicator test. A live SendInput
  drag is not enforced in CI (the hosted harness's injected input is
  unreliable); the gesture is verified locally.

## C3. Pane swap-detach (next increment)

Drag a pane by its header to swap it with another pane in the tab, or pull it
out into a new tab. Design follows once C2 lands; it reuses the same
press/threshold/release shape and the layout's pane tree.

## Standing constraints

Pure-Rust runtime; no new runtime deps; no copied external code; each change
lands as its own PR with unit tests and a perf-gate before/after diff;
security-sensitive paths keep existing guarantees.