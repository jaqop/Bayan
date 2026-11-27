# GUI Live Smoke Harness — Design

**Date:** 2026-11-27
**Status:** approved roadmap milestone A (owner, 2026-11-27)
**Plan:** `docs/superpowers/plans/2026-11-27-gui-live-smoke-harness.md`

## Problem

The README's milestone proofs repeatedly end with "not verified on screen"
or describe a one-off `PrintWindow` capture that nobody can re-run. Every
live check so far was hand-built: launch with `BAYAN_*` hooks, capture the
HWND, eyeball or pixel-sample the PNG, kill the PID. The knowledge of how
to do this safely (PID-checked ownership, DPI-aware capture, fresh profile,
kill only what you started) lives in prose, not in code.

## Goal

One command — `scripts/gui-smoke.ps1` — that launches a dev-hooks build in
a fresh profile, drives it with the existing `BAYAN_*` hooks, captures the
window with `PrintWindow`, and **asserts pixels**, so a live proof is a
re-runnable test instead of an anecdote. Local-first; a CI job follows once
the scenario set is stable.

## Non-goals

- No new dev hooks in this milestone. The existing set (`BAYAN_TYPE`,
  `BAYAN_SETTINGS`, `BAYAN_GROUPS`, `BAYAN_PALETTE`, `BAYAN_GUARD`,
  `BAYAN_SHELLS`, `BAYAN_COCKPIT`, `BAYAN_LANG`, `BAYAN_ARABIC_FONT`,
  `BAYAN_PICK_THEME`, `BAYAN_ATLAS_STRESS`, `BAYAN_SHOW_NOW`, …) already
  reaches every overlay without input injection. A scenario that needs a
  new hook proposes it in its own change.
- No SendInput in the first scenario set. Hook-driven state is
  deterministic; real mouse/keyboard injection (drag, chords) arrives with
  the tab-reorder milestone, which needs it.
- Not a replacement for the perf gate. The harness asserts *what is
  drawn*, never how fast.

## Architecture

```
scripts/gui-smoke.ps1          driver: profile, launch, HWND, capture, assert, kill
scripts/gui-smoke/*.ps1        one scenario per file (data + assertions)
```

**Driver**, per scenario:

1. Fresh run root under `%TEMP%`; redirect `USERPROFILE`, `HOME`, `TEMP`,
   `TMP` exactly as `measure-startup.ps1` does. The user's `~/.bayan` is
   never read or written.
2. Write a deterministic `config.json` into the run home: bundled font
   (Kawkab Mono — ships in `fonts/`, so CI and dev machines agree),
   `font_size` 16, `padding` 0, `opacity` 1.0, `cursor_blink` false,
   `scroll.smooth` false, `restore_tabs` absent. Write a `session.json`
   with a fixed 1000×700 window rectangle (fits the hosted runner's
   1024×768 screen) so coordinates are stable.
3. `Start-Process` the binary with the scenario's `BAYAN_*` environment.
   Only this process is ever stopped.
4. Wait for `renderer ready` in the profile log (`BAYAN_PROFILE=1`), then
   a scenario-settle delay (default 1.5 s) for PTY output to land.
5. Find the HWND by PID (`EnumWindows` + `GetWindowThreadProcessId`),
   assert the PID is alive and owns the window **before and after** the
   capture — the rule every README capture follows.
6. `PrintWindow` with `PW_RENDERFULLCONTENT` from a DPI-aware process
   (`SetProcessDpiAwarenessContext(PerMonitorV2)`); read `GetDpiForWindow`
   and scale expected coordinates by it. Save the PNG under the run root.
7. Run the scenario's assertions against the bitmap (`System.Drawing`,
   present on Windows in both PowerShell 5.1 and pwsh 7).
8. `Stop-Process` the owned PID, restore environment, keep the PNG and a
   pass/fail line per scenario; exit non-zero if any scenario failed.

**Assertion helpers** (the whole vocabulary, deliberately small):

- `Assert-Pixel $bmp $x $y $r $g $b [-Tolerance 2]` — exact-ish color.
- `Assert-Ink $bmp $x0 $y0 $x1 $y1` — the region is not uniformly the
  background color (something was drawn).
- `Assert-NoInk …` — the region IS uniform background (nothing drawn).
- `Find-ColorX $bmp $y $r $g $b` — first/last x of a color on a row, for
  border-column and flush-right checks.
- `Assert-SameBitmap $a $b` — two captures byte-equal (differential
  redraw proof).

**Scenarios** (first set; each automates a proof the README currently
states in prose):

| Scenario | Hooks / feed | Assertion |
|---|---|---|
| `selftest-red` | `BAYAN_TYPE` of `ESC[48;2;255;0;0m ESC[2J` | grid pixel = (255,0,0) ±2 at three spread points — validates capture, coordinates, DPI end-to-end |
| `arabic-connected` | `echo مرحبا بالعالم` | ink in the Arabic run's region; letters connected is asserted by ink density between letter boxes, not by OCR |
| `rtl-flush-right` | Arabic prose line | ink touches the pane's right edge region, none in the left |
| `tui-border-columns` | box rows with an Arabic field (PR #100's Gemini shape, fed as bytes) | closing `│` x equal (±1 px) on the Arabic row and its neighbors |
| `ligatures-ab` | `echo -> => !=` with ligatures on, then off | the two captures differ in the run's region (A/B, not absolute pixels) |
| `groups-chip` | `BAYAN_GROUPS=1` | chip ink at the bar's leading edge; one tab drawn |
| `settings-english` | `BAYAN_SETTINGS=1 BAYAN_LANG=en` | ledger mirrored: label ink left, control ink right |
| `palette` | `BAYAN_PALETTE=1` | card region ink, rows present |
| `guard-card` | `BAYAN_GUARD=1` | confirm card drawn over the grid |
| `progress-bar` | OSC 9;4;60 fed as bytes | green bar pixels under the tab (M32's proof, re-runnable) |
| `atlas-stress` | `BAYAN_ATLAS_STRESS=1` | CJK, Latin, Arabic regions all carry ink in one frame |

## Determinism rules

- Bundled fonts only; fixed size, padding, window rect; blink off; smooth
  scroll off. A scenario that depends on installed fonts is not accepted.
- Coordinates are computed from the fixed geometry (cell metrics derived
  from the font at the captured DPI), never hard-coded screen pixels.
- Every assertion must be shown to fail on a deliberately wrong
  expectation before it is trusted (a vacuous assertion is worse than
  none). The plan enforces this per task.

## Safety

- Fresh profile per scenario; the user's `~/.bayan`, clipboard, and
  windows are untouched. `BAYAN_PASTE` scenarios (later set) save and
  restore the clipboard.
- Kill only the `Start-Process` PID; never `Stop-Process -Name bayan`.
- No network, no elevation, no registry writes.
- The binary is a `--features dev-hooks` build; hooks stay compiled out
  of ordinary release builds (unchanged).

## CI

A follow-up task adds a `gui smoke` job on `windows-latest`: build
`--release --features dev-hooks` (the perf job's binary shape), run the
scenario set, upload PNGs as artifacts on failure. Hosted runners open
the window on WARP — the startup probes already prove that path works.
The job is additive; the perf gate is untouched.

## Open questions (decided here)

- **Why PowerShell, not Rust?** The driver is glue around Win32 calls and
  process lifecycle — the repo's existing probe language, with
  `System.Drawing` available. Pixel assertions stay small and readable.
  A Rust capture crate would add a build target for no gain at this size.
- **Why not assert glyph shapes directly?** Connected-Arabic and ligature
  proofs are A/B or density checks; exact glyph bitmaps would pin the
  shaper version and break on every cosmic-text bump. The render unit
  tests already pin glyph identity; the harness pins what reaches the
  screen.
