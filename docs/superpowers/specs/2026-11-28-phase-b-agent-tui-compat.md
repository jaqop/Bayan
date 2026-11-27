# Phase B — CLI AI-agent terminal compatibility (inspection + resolution)

**Date:** 2026-11-28 · **Status:** approved roadmap Phase B
**Method:** source inspection of the agents' TUI terminal interaction only;
findings translated into Bayan harness scenarios / tests or documented
non-applicable. No external code copied, no new runtime deps.

## Inspected

- **Codex** (`openai/codex`, `codex-rs/tui`): Rust, ratatui fork
  (`custom_terminal.rs`). Diff-based draw, single flush per frame; cursor
  hide-before-draw / move-before-show; ED2 paired with explicit home
  (Terminal.app quirk); combined `ESC[r ESC[0m ESC[H ESC[2J ESC[3J ESC[H`
  scrollback purge; OSC 8 hyperlinks with **BEL** terminator; CPR (`ESC[6n`)
  fallback to origin when the PTY does not answer; kitty graphics via APC
  (`a=T/p/d`), tmux DCS passthrough; NO_COLOR suppression.
- **OpenCode** (`anomalyco/opencode`, `packages/tui`): TypeScript / Bun TUI.
- **Pi** (`earendil-works/pi`, `packages/tui`): custom TypeScript TUI.
  **Synchronized output CSI ?2026h/l**; kitty keyboard protocol; bracketed
  paste (large-paste markers); SGR mouse + OSC 52 copy; Kitty graphics +
  iTerm2 inline images; ANSI-aware width/truncation; IME via CURSOR_MARKER;
  differential rendering.
- **oh-my-pi**: Pi-derived; same TUI expectations assumed, inspection pending.

## Compatibility matrix (expectation → Bayan state → resolution)

| Expectation | Bayan state | Resolution |
|---|---|---|
| DEC 2026 synchronized output | private mode accepted; wrapped redraw renders | harness `sync-output` (PASS) |
| Kitty graphics / iTerm2 images | supported; pinned ConPTY forwards APC/OSC | harness `kitty-image` (PASS) |
| OSC 8 hyperlinks (BEL terminator) | supported | existing link tests |
| CPR fallback when PTY silent | Bayan answers / tolerates | existing conpty tests |
| ED2 + home / ED3 scrollback purge | image store clears on ED2/ED3/RIS, EL leaves Kitty | existing image tests |
| Bracketed paste + guard | supported | harness `guard-card` |
| SGR mouse + OSC 52 | supported (OSC 52 gated) | existing tests |
| Kitty keyboard protocol (input) | encoder unit-covered; **ConPTY drops CSI u on input** | characterization test pins limitation; default-off (Phase A3) |
| ANSI-aware CJK width | wide-char cells handled | harness `cjk-columns` |
| IME composition | gap (no IME path) | documented; Phase C candidate |
| BiDi / Arabic mixed prompts | whole-line shaper + RTL anchor | harness `arabic-connected`, `rtl-flush-right`, Phase A2 |

## Exit criteria

Every inspected expectation is either pinned by a harness scenario / unit
test or documented non-applicable with a technical reason (kitty input via
ConPTY; IME deferred to Phase C). oh-my-pi inspection remains to confirm it
adds nothing beyond Pi.
