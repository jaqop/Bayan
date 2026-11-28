# Phase 2 — output triggers (PR 1 of triggers/snippets)

**Date:** 2026-11-28 · **Status:** approved scope (owner: "both, triggers first")
**Follow-up:** snippets (saved text insertable via palette/hotkey) as PR 2.

## Goal

User-defined patterns matched against pane **output** that fire a notification
action — like iTerm2 Triggers. Lightweight, opt-in, and safe against untrusted
output.

## Design (as implemented)

- **`regex = "1"`** (regex-automata/syntax already in the tree via
  alacritty_terminal). An initial attempt reused alacritty's `RegexIter` over a
  grid range, but its lazy DFA proved unreliable for a bounded range; matching
  extracted text with `regex::Regex` is deterministic and simple.
- `Session::output_text_since(since) -> (String, i32)` (term.rs): under one lock,
  extract the text of the new **complete** output lines and return the new output
  cursor. The cursor is `history_size + cursor.line` — a **monotonic** output
  count. (The screen-bottom line is constant and cannot track new output: output
  writes at the cursor and scrolls into history.) New complete lines are the
  absolute indices `[since, cursor-1]`, floored at 0; line text comes from
  `copyclean::cells_to_string`.
- `src/trigger.rs`:
  - `TriggerAction { Toast, Bell }` — **notify-only**. Bell posts the existing
    `UserEvent::Bell` (honours `BellMode`); Toast goes through the rate-limited
    `toast_from_pty`.
  - `TriggerSet { regexes: Vec<regex::Regex>, meta: Vec<Meta> }` where
    `Meta { action, cooldown, last_fired }`. `compile` skips empty/invalid
    patterns. `fire(session, since, now)` matches the new text and applies a
    per-trigger cooldown, returning the actions to fire + the new cursor.
- **Config** (`config.json`): a `triggers` array of
  `{ "pattern": "...", "action": "toast"|"bell", "cooldown_ms": N }`
  (`cooldown_ms` defaults to 5000). Default: empty (feature off). Recompiled on
  config load/reload.
- **Integration** (`wakeup`, per PTY chunk): when the trigger set is non-empty,
  scan new output since the pane's `trigger_cursor` (initialized to
  `trigger_output_pos()` at pane creation so pre-existing output does not fire),
  fire actions, advance the cursor. Empty set → skipped entirely (zero cost; the
  perf gate runs default config, so it is unaffected).

## Security (untrusted output)

- Actions are **notify-only** — a trigger can never execute a command, write to
  the PTY, or touch the clipboard. Output is untrusted; it may only raise a
  notification.
- Per-trigger cooldown **plus** the existing global toast rate limit
  (`toast_from_pty` / `TOAST_MIN_GAP`) so a matching flood cannot spam.
- Patterns compile once; an invalid pattern is dropped at compile time, not at
  match time. The scan is bounded to new lines.

## Verification

- Pure tests (`trigger::tests`): `compile` drops empty/invalid patterns; cooldown
  suppresses repeats; distinct triggers fire independently; zero cooldown fires
  every hit; a pattern matches anywhere in the new text.
- `output_triggers_scan_only_new_lines` (term): a fed term — only new complete
  lines are returned, the cursor advances monotonically, re-scanning yields
  nothing, and old lines are not re-matched.
- `triggers_parse_with_defaults_and_round_trip` (config): the array parses,
  defaults resolve (toast / 5000), empty stays omitted on serialize.
- Full suite + clippy + fmt green; perf gate unaffected (default off).
