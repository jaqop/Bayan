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
- [x] **M11** — cap-proof command marks WITHOUT upstream changes: Grid's
      scroll_up advances a non-zero display_offset by exactly the scrolled
      amount, so the feed parks the offset at 1, reads how far it moved,
      subtracts history growth — an exact eviction counter (EasyTer's
      dropped counter, reborn); text attribute fidelity: bold (real Amiri
      Bold bundled), italic, underline, strikethrough, dim, hidden, all
      through the shaping cache and quad pipeline; atlas resets now
      self-heal with one scheduled redraw
- [x] **M12** — the EasyTer parity tail: paste guard (a confirm card before
      any multi-line/huge paste — Enter sends, Esc drops), drag & drop file
      paths (quoted, shell-ready), long-command finish alerts (taskbar flash
      + amber tab dot when unfocused), a global quake hotkey (Ctrl+Alt+`
      summons/hides Bayan from anywhere), and full theme colors
      (bg/fg/16-color palette) in ~/.bayan/config.json
- [x] **M13** — the glyph atlas grows a NEW page (texture-array layer)
      when one fills, instead of M10's wipe-everything reset — no flash even
      with big fonts + emoji + Arabic + CJK all live; the GPU texture array
      is recreated with more layers only on the rare growth. Verified by
      forcing thousands of distinct glyphs across several pages: page-0
      Latin/Arabic and page-N CJK render together, uncorrupted, in one call.
- [x] **M14** — differential redraw: a frame whose quad set is byte-identical
      to the last (and no new atlas glyph) skips the GPU submit entirely, so
      a still terminal stops re-rendering (laptop battery win); ligature
      infrastructure — a `ligatures` config toggle, ligature-capable font
      preference (Cascadia Code / Fira / JetBrains lead when on), and batched
      ASCII-run shaping that forms `-> => != …` the moment the shaper enables
      `calt` (dormant today: cosmic-text 0.12 builds its shape plan with no
      user features — an upstream limit, honestly flagged, not a Bayan bug)
- [x] **M15** — a real in-app settings panel (Ctrl+, or the tab-bar gear —
      a clickable button is layout-proof where a comma shortcut fought the
      Arabic layout): clickable theme tiles with palette previews, live
      apply, persisted to ~/.bayan/config.json on close
- [x] **M16** — the table-stakes settings set, from a survey of what
      Windows Terminal / Alacritty / WezTerm / Ghostty / kitty ALL offer:
      cursor style (block/bar/underline) + blink (530ms rhythm that parks
      solid after 15s idle, so M14's still-terminal-stops-rendering win
      survives), scrollback size, font-family cycler over INSTALLED
      monospace fonts, copy-on-select toggle, bell modes (attention dot /
      system sound / silent), default shell for new tabs (PowerShell 5 /
      pwsh 7 / cmd — the PS family keeps UTF-8 + oh-my-posh + OSC 133,
      others degrade gracefully), content padding, window opacity (layered
      alpha; winit wipes the bit on set_visible, so it's reapplied),
      hide-tab-bar-with-one-tab, and a close guard (Enter/Esc card when
      closing a pane or the window mid-command). The panel itself became a
      ledger: every row reads [control] ··· leader dots ··· [label], the
      book-index idiom. Bonus fix: a UTF-8 BOM in config.json (what
      Windows editors write) no longer silently discards the whole config
- [x] **M17** — the keybinding editor: 12 rebindable actions behind one
      keymap (chords resolve by PHYSICAL key, extending the Arabic-layout
      guarantee to custom bindings), keycap-chip rows, press-to-capture
      with conflict detection, Delete restores defaults, only non-default
      bindings persist. Fixed by design: Ctrl+Tab, zoom, Alt+arrows,
      plain Ctrl+C, the quake hotkey
- [x] **M18** — real programming ligatures: pure-ASCII grid runs shape
      directly through rustybuzz WITH the font's default features (calt/liga
      finally fire — cosmic-text 0.12's shape plan is featureless), then
      rasterize through the SAME SwashCache/atlas via hand-built CacheKeys.
      The baseline comes from the startup probe so both shapers draw on the
      identical line; a font whose substituted advances leave the terminal
      grid falls back to cosmic (ligature fonts encode -> as glyph PAIRS
      precisely to keep per-cell advances). Style quadrants (bold/italic)
      resolve their own faces; Arabic, mixed text and ligatures-off keep
      the cosmic path untouched. Verified live: -> => != === ~~> render as
      joined glyphs in Cascadia Code NF, column-exact, and toggle off cleanly
- [x] **M19** — native toast notifications with zero WinRT/COM: a
      Shell_NotifyIcon tray icon whose NIF_INFO balloons render as real
      Windows 10/11 toasts (Action Center included). Fires when a long
      command finishes while Bayan is unfocused; NIIF_RESPECT_QUIET_TIME
      defers to focus assist, so do-not-disturb files them silently in the
      notification center instead of interrupting. Clicking a toast — or
      the tray icon — summons the window (the msg hook that already serves
      the quake hotkey). A settings toggle (default on) previews itself
      when flipped on; every exit path funnels through one quit() so no
      ghost icon lingers in the tray. The balloon text renderer turned out
      to lay words in LOGICAL order left-to-right — no BiDi reordering,
      RLM/RLO ignored (established by A/B captures) — so Arabic strings
      are handed over in VISUAL word order: Claude mode's philosophy,
      aimed the other way
- [x] **M20** — cosmic-text 0.12 → 0.19: the new engine shapes through
      harfrust with default OpenType features (and grows an explicit
      FontFeatures API), so the ordinary batched Buffer ligates — the M18
      rustybuzz bypass is retired wholesale (enum, direct shaper, hand-built
      CacheKeys, the dependency itself: rustybuzz left the tree, harfrust
      arrived with the bump). Proven equivalent before surgery: a probe
      crate showed 0.19 producing the IDENTICAL glyph pair for `->` that
      the bypass produced. Bonus the bypass never had: mixed Arabic+code
      lines go through the whole-line BiDi path, which now ligates its
      ASCII segments too. Verified live: → ⇒ ≠ ≡ joined AND مرحبا بالعالم
      connected, correctly ordered, on the same line
- [x] **M21** — wgpu 24 → 30, six majors in one hop. The migration is 37
      lines of `gpu.rs`; the obstacle was that wgpu 30 wouldn't build on
      Windows AT ALL. wgpu-hal 30 depends on windows 0.62 directly and on
      gpu-allocator 0.28, which declares `windows = ">=0.53, <=0.62"` — a
      RANGE, and cargo resolved it to 0.58 off our old lockfile, so
      dx12/suballocation.rs passed a 0.58 `ID3D12Heap` into a 0.62
      `CreatePlacedResource` and failed inside a crate we don't own. The fix
      is resolution, not code: `cargo update -p windows@0.58.0 --precise
      0.62.2` pins the range to its ceiling and collapses the graph to one
      windows-core (upstream, same family as gfx-rs/wgpu#6687). The API
      changes that carried meaning: `get_current_texture` no longer returns
      `Result` but a `CurrentSurfaceTexture` enum whose **Suboptimal variant
      carries a usable texture** — render it, or resizes blink;
      `SurfaceConfiguration` gained `color_space`, whose Default is
      documented as reproducing wgpu's historical behavior, which is exactly
      what M10 proved pixel-identical to the CPU renderer, so Default is the
      correct choice and not the lazy one; `present()` moved to `Queue`;
      push constants became `immediate_size`; `multiview` → `multiview_mask`.
      Verified, not assumed: 58 tests pass, a live capture shows ligatures
      still joining and مرحبا بالعالم still connected and ordered on the same
      line (M20 intact through a GPU rewrite), and BAYAN_ATLAS_STRESS forces
      page growth with thousands of CJK glyphs rendering uncorrupted beside
      the page-0 powerline prompt in one frame (M13 intact). Honest scope:
      clippy isn't installed on this toolchain (the build is warning-clean
      instead), and M14's differential redraw wasn't re-measured — the quad
      set is untouched by this change, but that's an argument, not a benchmark
- [x] **M22** — alacritty_terminal 0.24 → 0.26 and portable-pty 0.8 → 0.9.
      One breaking change in the whole tree: vte 0.15 takes `advance(handler,
      &[u8])` instead of one byte per call, batching internally — so the feed
      loop collapses into a single call and gets faster for free. The risk here
      was never the compiler, it was silence: M11's cap-proof eviction counter
      leans on Grid's exact `scroll_up`/`display_offset` semantics, and a minor
      bump could have shifted them without breaking the build. That's what the
      unit test pinning "27 scrolled - 8 kept = 19 evicted" is for, and it still
      passes — 58/58 green. Verified live beyond the tests: the PTY spawns under
      portable-pty 0.9 and real commands run, and the OSC 133 command-block
      lights still read exit codes correctly — green beside `echo ok`, red
      beside a failing command, in the same frame

## Settings

Everything lives in the in-app panel (Ctrl+, or the tab-bar gear); the
shortcuts editor is the الاختصارات button in its header. State persists to
`~/.bayan/config.json` — optional, human-editable, BOM-tolerant, and every
key falls back to a sane default:

```json
{
  "theme": "Bayan", "font_family": "Cascadia Code NF", "font_size": 15.0,
  "cursor_style": "bar", "cursor_blink": true, "scrollback": 20000,
  "copy_on_select": true, "ligatures": true, "bell": "attention",
  "shell": "pwsh.exe", "padding": 8, "opacity": 0.9,
  "hide_single_tab": true, "confirm_close": true,
  "keybinds": { "new-tab": "ctrl+shift+n" }
}
```

Shell and scrollback apply to new tabs (no terminal restarts a live
session); everything else applies immediately.

## Build

```
cargo run --release
```

Requires Rust (MSVC toolchain on Windows). The Amiri font (SIL OFL, see
`fonts/OFL.txt`) is bundled so connected Arabic works from a fresh clone.
