# GUI Live Smoke Harness Implementation Plan

> **For agentic workers:** steps use checkbox (`- [ ]`) syntax for tracking. Implement task-by-task; each task ends in a commit.

**Goal:** `scripts/gui-smoke.ps1` turns the README's one-off PrintWindow proofs into re-runnable pixel assertions, local-first, CI job after.

**Architecture:** One PowerShell driver (fresh profile → hook-driven launch → PID-checked HWND → DPI-aware PrintWindow → System.Drawing assertions → owned-PID kill) plus one file per scenario under `scripts/gui-smoke/`. No new dev hooks, no SendInput in this milestone.

**Tech Stack:** PowerShell 7 (pwsh), System.Drawing, Win32 via Add-Type (EnumWindows, GetWindowThreadProcessId, PrintWindow, GetDpiForWindow, SetProcessDpiAwarenessContext). The dev-hooks release build the perf probes already use.

**Spec:** `docs/superpowers/specs/2026-11-27-gui-live-smoke-harness-design.md`

## Global Constraints

- The user's `~/.bayan` is never read or written: every scenario runs under a redirected `USERPROFILE`/`HOME`/`TEMP`/`TMP`, the `measure-startup.ps1` pattern.
- Only the `Start-Process` PID is stopped; never a process-name kill. PID alive and HWND owned before AND after capture.
- Bundled fonts only (Kawkab Mono), fixed font size 16, padding 0, opacity 1.0, blink off, smooth scroll off, fixed 1000×700 window rect via the run home's `session.json` (fits the hosted 1024×768 screen).
- Coordinates scale by `GetDpiForWindow`; no hard-coded screen pixels.
- Every assertion is proven non-vacuous: run it once with a deliberately wrong expectation and watch it fail before trusting it.
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` stay clean (this milestone adds no Rust, but the build it captures must pass them).
- Commit messages: a plain sentence about what changed and why, no conventional-commit prefix.
- The perf gate and its scripts are untouched.

## Review Focus

1. A scenario that passes on a blank window is a bug: `selftest-red` exists to prove the capture path sees real pixels.
2. DPI: at 150% scale the same scenario must pass; coordinates come from metrics × DPI, never constants.
3. Ownership: a second Bayan window opened by the user during a run must not be captured or killed.
4. Failure artifacts: a failed scenario leaves its PNG and the profile log for inspection; a pass cleans up.
5. No scenario depends on an installed font, the network, the clipboard, or elevation.

---

### Task 1: Driver, capture, helpers, selftest

**Files:**
- Create: `scripts/gui-smoke.ps1`
- Create: `scripts/gui-smoke/selftest-red.ps1`

**Interfaces:**
- `gui-smoke.ps1 -BinaryPath target/release/bayan.exe [-Scenario <name>] [-KeepArtifacts]` runs all scenarios (or one), prints `PASS <name>` / `FAIL <name>: <reason>` per scenario, exits non-zero on any failure.
- A scenario file dot-sources into the driver with `$bmp` (the captured bitmap), `$geom` (cell metrics + DPI-scaled pane rect), and the assertion helpers available.

- [x] **Step 1: Write the driver skeleton** — profile redirect, deterministic config/session write, launch with `BAYAN_PROFILE=1 BAYAN_SHOW_NOW=1` + scenario env, wait for `renderer ready` + settle, HWND-by-PID, DPI-aware PrintWindow, owned-PID kill, env restore. Mirror `measure-startup.ps1`'s try/finally shape.
- [x] **Step 2: Write the helpers** — `Assert-Pixel`, `Assert-Ink`, `Assert-NoInk`, `Find-ColorX`, `Assert-SameBitmap` per the spec.
- [x] **Step 3: Write `selftest-red.ps1`** — feed `"`e[48;2;255;0;0m`e[2J"` via `BAYAN_TYPE`; assert (255,0,0) ±2 at three spread grid points.
- [x] **Step 4: Build and run** — `cargo build --release --features dev-hooks` (needs the local MSVC toolchain), then `pwsh scripts/gui-smoke.ps1`. Expected: `PASS selftest-red`. (2026-11-27: PASS on the merged-main build; the exe lives in a separate target dir while the owner's running Bayan locks `target/release/bayan.exe`.)
- [x] **Step 5: Prove non-vacuous** — temporarily expect (0,255,0); run; expect `FAIL selftest-red`. Revert. (2026-11-27: `FAIL selftest-vacuous: pixel (500,395) = (255,0,0), want (0,255,0) +/-2`, exit 1, artifact kept; temp scenario deleted.)
- [ ] **Step 6: DPI check** — run once at 100% and once at 150% display scale (or force the window DPI via a second monitor); both pass. (100% verified 2026-11-27, system DPI 96. 150% pending a scaled display — the owner's machine stays at 100%; assertions are fraction-based and DPI-scaled by construction. Re-check on a scaled monitor or CI.)
- [x] **Step 7: Commit** — `git add scripts/gui-smoke.ps1 scripts/gui-smoke/selftest-red.ps1 && git commit -m "A live smoke harness: hook-driven launch, PID-checked PrintWindow capture, pixel assertions, and a red-grid selftest"`

### Task 2: Arabic and TUI scenario set

**Files:**
- Create: `scripts/gui-smoke/arabic-connected.ps1`, `rtl-flush-right.ps1`, `tui-border-columns.ps1`, `ligatures-ab.ps1`

- [x] **Step 1: `arabic-connected`** — positioned A/B (row 12): Arabic prose vs a spaces feed; the Arabic capture must dominate the band's ink (>2x, floor 100). Letter-connection IDENTITY stays pinned by the render unit tests; this pins that shaped Arabic reaches the window. Band 0.62-0.67H excludes the next prompt row. (2026-11-27: PASS, A=459 B=0; inverted-ratio proof FAILed as required.)
- [x] **Step 2: `rtl-flush-right`** — Arabic prose echo; find ink row-bands; one must start past mid-window and touch the right edge (the mixed command-echo line cannot). (2026-11-27: PASS, band x 884-996 of 1000; true-inverse proof FAILed as required.)
- [x] **Step 3: `tui-border-columns`** — three NON-adjacent box rows (10/13/16, adjacent │ glyphs touch vertically), middle one an Arabic field, truecolor-red closing borders at column 41; the three red marks must share one x (±2 px). (2026-11-27: PASS, x=680 thrice; a border shifted to column 45 was caught at x=747.)
- [ ] **Step 4: `ligatures-ab` — DEFERRED.** No bundled font carries `calt` ligatures (Kawkab Mono and Amiri are the bundled faces); Cascadia Code is installed here but not guaranteed on CI. Needs an owner decision: bundle a ligature font (license + size) or gate the scenario on an installed one. The A/B driver support ($SmokeEnv2/$SmokeConfig2) is already in place for it.
- [x] **Step 5: Run the three; prove each non-vacuous; commit** — `"Live pixel proofs for connected Arabic, RTL flush-right, and inline TUI border columns"`

### Task 3: Chrome scenario set

**Files:**
- Create: `scripts/gui-smoke/groups-chip.ps1`, `settings-english.ps1`, `palette.ps1`, `guard-card.ps1`, `progress-bar.ps1`, `atlas-stress.ps1`

- [ ] **Step 1:** `groups-chip` (`BAYAN_GROUPS=1`): chip ink at the bar's leading edge; exactly one tab label region carries ink.
- [ ] **Step 2:** `settings-english` (`BAYAN_SETTINGS=1 BAYAN_LANG=en`): label ink left of center, control ink right — the mirrored ledger.
- [ ] **Step 3:** `palette` (`BAYAN_PALETTE=1`) and `guard-card` (`BAYAN_GUARD=1`): card region ink over a quiet grid.
- [ ] **Step 4:** `progress-bar`: feed OSC 9;4;60 as bytes; green pixels in the 3 px strip under the active tab (M32's proof).
- [ ] **Step 5:** `atlas-stress` (`BAYAN_ATLAS_STRESS=1`): CJK, Latin, Arabic regions all carry ink in one frame.
- [ ] **Step 6: Run all; prove non-vacuous; commit** — `"Live pixel proofs for the group chip, English settings mirror, palette, paste guard, progress bar, and atlas stress"`

### Task 4: CI job and docs

**Files:**
- Modify: `.github/workflows/ci.yml` (new `gui-smoke` job)
- Modify: `README.md` (milestone entry + a "Live smoke harness" section under Build)

- [ ] **Step 1:** Add the job: checkout, stable toolchain, rust-cache, `cargo build --release --features dev-hooks`, `pwsh scripts/gui-smoke.ps1`, upload PNGs on failure. SHA-pinned actions, `permissions: contents: read` — the workflow's existing rules.
- [ ] **Step 2:** actionlint clean; push to a branch; the job runs green on `windows-latest` (WARP).
- [ ] **Step 3:** README milestone entry: what the harness proves, the scenario list, how to run it locally, and which former "not verified on screen" items it now covers.
- [ ] **Step 4: Commit** — `"The live smoke harness runs on CI: eleven pixel-asserted scenarios on the hosted WARP window"`

### Task 5: Multilingual parity matrix (owner scope clarification 2026-11-27 — after the current fixes)

Bayan is universal: CJK and Cyrillic get the same pixel-proof discipline as Arabic.

- [ ] **Step 1:** `cjk-columns` — a CJK line (double-width cells) beside an ASCII ruler line; assert column alignment via positioned marks, and that a mixed Arabic+CJK+Latin line keeps every script's ink in its own cells.
- [ ] **Step 2:** `cyrillic-ltr` — Russian prose echo; the ink band must be LEFT-aligned (mirror of `rtl-flush-right`), proving the BiDi path does not misfire on Cyrillic.
- [ ] **Step 3:** `cyrillic-chords` — physical-key chord guarantee on a Russian layout (needs the SendInput extension; coordinate with the key-routing/IME milestone).
- [ ] **Step 4:** Font-fallback decision for determinism: owner chooses between relying on the CI image's CJK faces (documented) or bundling a subset (size/license). Until then the CJK scenarios gate on detected coverage and report-only when absent.
- [ ] **Step 5: Commit** — `"The live matrix speaks CJK and Cyrillic too: wide-glyph columns, mixed scripts, and LTR Cyrillic"`
