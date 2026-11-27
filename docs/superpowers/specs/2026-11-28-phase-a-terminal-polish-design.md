# Phase A — Remaining Core Terminal Fixes & Protocol Polish (design)

**Date:** 2026-11-28 · **Status:** approved roadmap Phase A (owner locked order)
**Precondition:** M-R1–M-R3 (pure-Rust migration) merged; full suite + GUI
harness + perf gate green.

Scope is limited to three items, each with a concrete, measurement- or
test-backed change. No speculative refactors (owner no-premature-churn rule).

## A1. ConPTY image delivery + repaint contract

**Current state (verified in code/README):** the vendored pty prefers the
pinned Microsoft 1.22+ ConPTY host, which passes VT (DCS/APC) through by
default; the inbox fallback strips DCS/APC. Image store already: scrolls with
the anchor row, evicts on scrollback exit, clears on ED2/ED3/RIS, **leaves
Kitty graphics on EL/partial-ED**, alt-screen isolation, resize-drop only on
column change. So the repaint contract is largely implemented.

**Gap / change:** make the passthrough path explicit and tested end-to-end:
- Add a harness scenario that transmits a Kitty `t=d` PNG and a Sixel frame
  through the **pinned** host and asserts the pixels land on the anchor cell
  (and survive an EL repaint, clear on ED2). This pins the delivery+repaint
  contract that today is only prose.
- Document that Sixel/Kitty require the pinned host (inbox conhost strips
  DCS/APC); keep the iTerm2 OSC path as the inbox fallback.

**Exit:** new harness scenario green locally + CI (ci-safe if it uses
truecolor; else local-only with reason), existing image tests unchanged.

## A2. RTL image anchor decision

**Current state:** image quad stays on the cursor cell; on an RTL paragraph
the anchor is "left for a later decision" (README). Pixels are never reversed.

**Change (decision):** anchor the quad to the **visual cell of the logical
cursor** after BiDi reordering (i.e. where the caret actually draws), not the
raw logical column. On an RTL line the caret visual position is mirrored; the
image must sit under the visible caret. Implement by mapping the placement
column through the same row-reorder used for the cursor, and add a unit test
(RTL row + image → placement visual column == cursor visual column) plus a
harness pixel assertion.

**Exit:** unit + harness assertions green; Arabic/CJK/Cyrillic scenarios
unchanged.

## A3. Mux session persistence + Kitty default-on verification

**Mux persistence:** local shells currently live in the window process; the
mux server owns only SSH. Phase A scope: persist mux session metadata (pane
tree + cwd + shell) to disk so a reconnect restores layout, and move local
shell ownership behind the server only if measurement shows no latency
regression (cat lock-tally already shows the pipe, not locks, is the
bottleneck — so do NOT move local shells into the server this phase; that is
a later, measurement-gated change). Deliver disk scrollback persistence for
mux sessions (bounded replay) as the persistence win.

**Kitty default-on:** `keyboard.kitty_protocol` defaults false. Verify against
the pinned conhost that CSI u / disambiguate escapes survive ConPTY (harness
scenario: enable protocol, send a chord, assert the program receives CSI u).
If green, flip the default to true behind the existing config knob; keep the
hostile-push masking. If ConPTY mangles it, document and keep false.

**Exit:** mux reconnect restores layout from disk; kitty verification scenario
green and default flipped only with evidence; perf gate + full suite green.

## Standing constraints

Pure-Rust runtime; no new runtime deps; no copied external code; each change
lands as its own PR with unit + harness tests and a perf-gate before/after
diff; security-sensitive paths (mux, elevation) keep existing guarantees.
