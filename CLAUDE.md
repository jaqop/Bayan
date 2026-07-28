# Bayan (بيان) — working notes for Claude

An Arabic-first, agent-ready terminal emulator in Rust, for Windows. Rust
successor to [EasyTer](https://github.com/jaqop/EasyTer) (Python/Qt), which
remains the behavioral specification: checked out alongside this repo at
`../EasyTer`, its `dev/test_input_ux.py` and friends define what the keyboard,
selection and BiDi paths must do.

## Layout

| File | Role |
|---|---|
| `main.rs` | event loop, tabs, panes, settings panel, cockpit, OSC handling |
| `render.rs` | the dual text engine + shaped-run cache (see below) |
| `gpu.rs` | wgpu: one pipeline, one 2048² atlas, one draw call per frame |
| `term.rs` | ConPTY via `portable-pty`, VT via `alacritty_terminal` |
| `bidi.rs` | Claude mode: visual → logical BiDi restoration |
| `keybinds.rs` | action registry + chord map |
| `keys.rs` | keyboard → VT byte sequences (ported from EasyTer) |
| `config.rs` | `~/.bayan/config.json`, optional and BOM-tolerant |
| `toast.rs` | `Shell_NotifyIcon` balloons as native toasts, zero WinRT |

58 tests across the tree. `render.rs` (17) and `term.rs` (12) carry the most.

## Invariants — do not "simplify" these

**The text engine is deliberately dual.** Rows without Arabic render on a
strict grid (ASCII batched into column-pinned runs; icons, powerline, box
drawing, CJK per cell, compressed to fit). Rows with Arabic shape as whole
lines so cosmic-text applies UAX#9. Grid fidelity wins for TUIs; correct text
wins for prose. Unifying the two paths breaks one or the other — this is the
core design, not an accident.

**On RTL rows, logical column ≠ visual x.** Cursor and selection must resolve
through the shaped layout, never by multiplying a column index by cell width.

**Claude mode is narrow on purpose.** Claude Code (Ink) applies BiDi itself on
Windows and emits Arabic in reversed visual order, so `bidi.rs` reverses UAX#9
rule L2 to get logical order back. Every other full-screen program — vim,
less, htop — emits LOGICAL Arabic and must never be reversed. PowerShell emits
logical too. Detection matches the *invoked program* (`cmd_is_claude`):
`claude`, `npx claude`, `C:\...\claude.exe` match; `vim claude.py` and
`git log claude` do not.

**Arabic-Indic digits are not Arabic letters.** `٠-٩` and Persian `۰-۹` form
weak LTR runs; ١٧٥ must stay ١٧٥. `is_arabic_letter` excludes them explicitly.

**Combining marks travel with their base.** Tashkeel, shadda, tanwin — a plain
`.rev()` puts marks before the letter they sit on. Reverse clusters, not chars.

**Toasts are the mirror image of Claude mode.** The `Shell_NotifyIcon` balloon
renderer lays words in logical order left-to-right with no BiDi reordering and
ignores RLM/RLO (established by A/B capture). So Arabic handed to a toast must
be pre-reversed into VISUAL word order — the opposite direction from `bidi.rs`.

**Chords resolve by PHYSICAL key.** Ctrl+T must fire when that key's logical
char is "ف". This is the Arabic-layout guarantee and it extends to user
rebindings. Note the scar: Ctrl+, for settings lost this fight, which is why
there is also a clickable gear button. Prefer a click target over a punctuation
chord for anything new.

**The M18 rustybuzz bypass is retired — do not resurrect it.** M20 moved to
cosmic-text 0.19, which shapes through harfrust with default OpenType features,
so the ordinary batched `Buffer` ligates on its own. rustybuzz left the tree.
If ligatures regress, fix the cosmic path; do not reintroduce a second shaper.

**Mica and Acrylic are NOT reachable — do not retry.** Measured, not assumed:
this machine's DX12 surface reports `alpha_modes = [Opaque]`, so the swapchain
cannot carry per-pixel alpha, and wgpu exposes no composition swapchain
(`CreateSwapChainForComposition`) to get one. Both Windows 11 materials show
through per-pixel-transparent regions, so with an opaque surface there is
nothing for them to show through. The pre-WinUI fallback fails too, and for a
reason worth remembering: `SetWindowCompositionAttribute` with
`ACCENT_ENABLE_ACRYLICBLURBEHIND` was tried and produced a sharp, unblurred
desktop behind the window — `LWA_ALPHA` blends the whole window uniformly and
bypasses DWM's blur, so the layered-window transparency Bayan already has and
the accent blur are mutually exclusive. The existing `opacity` setting is the
only transparency available on this path.

**Start-directory precedence, in order:** a restored session's saved cwd, then
the launcher's directory (a shortcut's "Start in"), then `USERPROFILE`. The
session wins deliberately — restoring a tab in the wrong folder is worse than
ignoring a shortcut — but it is also why a shortcut change appears to do
nothing: `~/.bayan/session.json` is answering first. Delete it when testing
start-directory behaviour, the same way `config.json` has to go before
`cargo test`. The launcher directory is honoured EXCEPT under `%SystemRoot%`,
because Explorer and the Start menu often hand a process `System32`.

**Two performance properties are load-bearing.** The shaped-run cache reshapes
only changed lines (~1.5ms → ~4µs on a cached frame). Differential redraw skips
the GPU submit entirely when the quad set is byte-identical to the last frame,
so a still terminal stops rendering. Anything that churns state every frame
silently destroys both. The atlas grows a new texture-array *page* when one
fills — it does not wipe.

## Build, test, verify

```
cargo build              # debug
cargo run --release
cargo clippy
```

**Before `cargo test`, remove `~/.bayan/config.json`.** The live user config
perturbs the config tests. This has bitten before:

```powershell
Remove-Item "$env:USERPROFILE\.bayan\config.json" -Force -ErrorAction SilentlyContinue
cargo test --quiet
```

**`cargo test` does NOT relink `target/debug/bayan.exe`.** Run `cargo build`
before any launch-and-screenshot round, or you will verify a stale binary. This
already happened once and nearly buried a real fix as a no-op.

Being a GUI app, most behavior isn't unit-testable. The established loop is
build → launch → screenshot → inspect → kill. Every debug hook below exists so
a feature can be reached **without injecting keyboard input** — honour that
principle when adding new ones:

| Env var | Effect |
|---|---|
| `BAYAN_SHOW_NOW=1` | show the window immediately (it normally stays hidden until the first frame, ~0.4s, to avoid a white flash) |
| `BAYAN_SETTINGS=1` | open the settings panel on startup |
| `BAYAN_SHORTCUTS=1` | open the keybinding editor |
| `BAYAN_PICK_THEME=<n>` | open settings and apply theme *n*, so a click's live effect is screenshot-verifiable |
| `BAYAN_SPLIT=1` | start with the first tab pre-split (pane machinery) |
| `BAYAN_COCKPIT=1` | open the agent cockpit |
| `BAYAN_GUARD=1` | show the close-guard card |
| `BAYAN_TYPE=<text>` | feed text into the first pane's PTY — the hook for ligature and shaping checks |
| `BAYAN_TOAST=<title\|1>` | fire a sample toast at startup (M19 A/B loops without rebuilding) |
| `BAYAN_ATLAS_STRESS=1` | shape a large spread of unique glyphs to force page-1 atlas growth |
| `BAYAN_PROFILE=1` | startup timeline marks to `%TEMP%\bayan_profile.log` (EasyTer's `EASYTER_PROFILE` pattern), zero cost when unset |

Start with `Start-Process -PassThru`, sleep ~6s for GPU init, capture the
screen with `System.Drawing`, then `Stop-Process` the pid. Pre-approved
variants of this are already in `.claude/settings.local.json`.

## Conventions

Commits are a sentence about what changed and why, often milestone-prefixed:
`M20: cosmic-text 0.12 -> 0.19 retires the M18 rustybuzz bypass`, or a plain
statement of the fix: `Toast Arabic read backwards: feed the balloon renderer
VISUAL word order`. No conventional-commits prefixes.

Every milestone gets a README entry recording what was built *and what was
proven* — the README is the project log, not a brochure. Keep failed
approaches and upstream limits in it; M14 flagging cosmic-text 0.12's
featureless shape plan is why M20 was a clean swap rather than a surprise.

Dependencies stay few and deliberate. M19 delivered native toasts with zero new
crates by reusing the `windows-sys` features already present for the quake
hotkey. Prefer that over pulling in WinRT/COM.

## Project skills

`.claude/skills/` carries `rust-best-practices`, `rust-testing`,
`m15-anti-pattern`, `windows-desktop-e2e`, and `webgpu-specs` (first-party from
the wgpu repo — use it to pull WebGPU/WGSL spec text when touching `gpu.rs`).
