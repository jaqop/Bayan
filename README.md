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
| Window / GPU | `winit` + `wgpu` | glyph-atlas quads, one pipeline, vsync |

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
- [x] **M6** — command-block lights in the gutter (green/red/grey from OSC
      133 exit codes) with Ctrl+Shift+Up/Down prompt jumping; optional
      ~/.bayan/config.json (font_family, font_size); Ctrl+wheel live font
      zoom with Ctrl+0 reset (debounced background renderer rebuilds)
- [x] **M7** — splits: Ctrl+Shift+E side-by-side / Ctrl+Shift+O stacked
      (up to 4 panes per tab, one axis), Alt+arrows cycle focus, click
      focuses, green border marks the focused pane, every pane has its own
      PTY size; Claude-row selections now copy LOGICAL-order Arabic; BEL
      from a background pane lights an amber attention dot on its tab
      (Claude asking for an approval — the cockpit signal)
- [x] **M8** — resizable panes (drag the divider; weighted layout, resize
      cursor on hover) and the agent cockpit (Ctrl+Shift+D): a card listing
      every tab — amber = waiting for your approval, green = working, dim =
      idle, with the running command or idle directory; arrows + Enter or a
      click to jump
- [x] **M9** — the shaped-run cache: the paint loop now re-shapes only
      CHANGED lines (generational LRU, EasyTer's dirty-row lesson) — a
      fully-cached frame's shaping cost dropped from ~1.5ms to ~4µs
      (~340×), which is the honest 80% of what a GPU renderer buys; full
      layout persistence: every tab's panes, split axis, weights and focus
      survive a restart (pre-M9 session files still restore)
- [x] **M10** — the wgpu renderer: every frame is one draw call of quads
      (glyphs from a shelf-packed 2048² atlas, solid rects via a white
      texel), WGSL pipeline with straight-alpha blending, vsync'd present,
      DX12-first with GL fallback; Arabic shapes through the same cosmic-text
      cache and rasterizes into the atlas — verified pixel-identical to the
      CPU renderer. The window stays hidden until the first frame (~0.4s,
      GPU init) so there's no white flash.
- [ ] **M11 (backlog)** — scrollback-cap-proof command marks (needs an
      eviction hook upstream in alacritty_terminal); atlas page growth
      instead of reset; damage-based partial redraw

## Build

```
cargo run --release
```

Requires Rust (MSVC toolchain on Windows). The Amiri font (SIL OFL, see
`fonts/OFL.txt`) is bundled so connected Arabic works from a fresh clone.
