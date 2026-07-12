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
- [ ] **M2b** — Arabic-row grid fit, cursor position inside RTL, HiDPI polish
- [ ] **M3** — selection, clipboard, scrollback UI, search
- [ ] **M4** — Claude mode (visual→logical reversal), OSC 133/52/9;9 blocks
- [ ] **M5** — tabs/splits, session persistence, agent cockpit, wgpu renderer

## Build

```
cargo run --release
```

Requires Rust (MSVC toolchain on Windows). The Amiri font (SIL OFL, see
`fonts/OFL.txt`) is bundled so connected Arabic works from a fresh clone.
