# Bayan — بيان

**An Arabic-first, agent-ready terminal emulator, written in Rust.**
**طرفية بالعربية أولاً، جاهزة لوكلاء الذكاء الاصطناعي، مكتوبة بلغة Rust.**

Bayan is the Rust successor to [EasyTer](../EasyTer): same mission — a
terminal where Arabic is a first-class citizen and AI agents (Claude Code)
are first-class tenants — rebuilt on a GPU-era foundation.

## Why

Every mainstream terminal fails Arabic: Alacritty renders disconnected
letters, kitty declined BiDi outright, the rest ship half measures. And none
of them treats a fleet of AI agents as a primary workload. Bayan exists for
the intersection nobody serves.

## Architecture (M1)

| Layer | Crate | Origin |
|---|---|---|
| VT emulation | `alacritty_terminal` | Alacritty's battle-tested core |
| ConPTY | `portable-pty` | extracted from WezTerm |
| Shaping / BiDi / fallback | `cosmic-text` | System76's text engine |
| Window / present | `winit` + `softbuffer` | CPU present now, `wgpu` later |

A reader thread feeds the emulator behind a mutex and nudges the UI thread —
the same architecture EasyTer proved in production. EasyTer's regression
suites (`dev/test_input_ux.py` and friends) serve as Bayan's behavioral
specification; `src/keys.rs` carries the same key-encoding tests.

## Milestones

- [x] **M1** — window + PowerShell over ConPTY + shaped, BiDi-correct text
- [x] **M2a** — dual text engine: non-Arabic rows on a strict grid
      (column-pinned ASCII runs; icons/powerline/box-drawing per cell,
      compressed to fit), Nerd Font auto-pick, zero-width diacritics
      (tashkeel) render
- [x] **M2b** — Arabic rows compress to the window when the shaped line
      overflows; the cursor on RTL rows resolves through the shaped layout
      (logical column ≠ visual x); live monitor-scale changes rebuild metrics
- [x] **M3** — mouse selection (drag / double-click word / triple-click
      line, auto-copy on release), clipboard (Ctrl+V pass-through in TUIs,
      bracketed-paste injection guard), wheel scrollback + indicator +
      Shift+PageUp/Down, Ctrl+F literal search with wrap-around; Ctrl
      shortcuts resolve by PHYSICAL key so they work on Arabic layouts
- [x] **M4** — Claude mode: visual→logical BiDi reversal (EasyTer's
      algorithms, ported with their edge cases), auto-enabled when the
      alternate screen belongs to a `claude` command (F2 = manual toggle,
      green badge); OSC 133 prompt marks injected into PowerShell power the
      command detection; OSC 52 sets the clipboard; OSC 9;9 tracks the cwd
      into the window title — all scanned split-safe with carry
- [x] **M5** — tabs (Ctrl+T inherits the cwd, Ctrl+Tab cycles,
      Ctrl+Shift+W closes, click to switch; renderer-drawn bar), session
      persistence (~/.bayan/session.json restores every tab in its
      directory), and the agent-cockpit seed: a green busy dot on any
      background tab producing output — you see the Claude that finished
      while you were elsewhere
- [ ] **M6 (backlog)** — splits, command-blocks gutter + jump, full agent
      cockpit (status/approvals across tabs), wgpu renderer, config file

## Build

```
cargo run --release
```

Requires Rust (MSVC toolchain on Windows). The Amiri font (SIL OFL, see
`fonts/OFL.txt`) is bundled so connected Arabic works from a fresh clone.
