# Bayan — بيان

**An Arabic-first, agent-ready terminal emulator, written in Rust.**
**طرفية بالعربية أولاً، جاهزة لوكلاء الذكاء الاصطناعي، مكتوبة بلغة Rust.**

[![CI](https://github.com/jaqop/Bayan/actions/workflows/ci.yml/badge.svg)](https://github.com/jaqop/Bayan/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.89+](https://img.shields.io/badge/rust-1.89%2B-orange.svg)](Cargo.toml)

Bayan is the Rust successor to [EasyTer](https://github.com/jaqop/EasyTer): same mission — a
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
      into the window title — all scanned split-safe with carry. OSC 7
      (`file://`) reports the same directory; see "BiDi controls and the
      current directory" below
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
      (~340×, release build, 40 rows, at 43017b0; `cargo test --release
      measure_cache_speedup_probe -- --nocapture` prints the two figures
      without asserting them, and on 4a570b7 it gave 1.14ms and 5.1µs,
      ~220×); full
      layout persistence: every tab's panes, split axis, weights and focus
      survive a restart (pre-M9 session files still restore)
- [x] **M10** — the wgpu renderer: every frame is one draw call of quads
      (glyphs from a shelf-packed 2048² atlas, solid rects via a white
      texel), WGSL pipeline with straight-alpha blending, vsync'd present,
      DX12-first with GL fallback; Arabic shapes through the same cosmic-text
      cache and rasterizes into the atlas — verified pixel-identical to the
      CPU renderer. The window stays hidden until the first frame (GPU
      init, about 0.4s on the development machine; not measured by any
      test) so there's no white flash.
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
- [x] **M23** — the tail of the dependency sweep: base64 0.22 → 0.23,
      pollster 0.4 → 1.0, windows-sys 0.59 → 0.61, plus a semver-compatible
      refresh across the tree (bytemuck, serde, serde_json, libc, cc and
      friends). Zero source changes — all three majors landed clean, which is
      worth recording precisely because the two bumps before them didn't.
      **winit stays at 0.30 deliberately:** 0.31 exists only as
      `0.31.0-beta.2`, and a terminal people actually work in doesn't run on a
      beta windowing layer. Verified: 58/58, and a live capture shows the
      OSC 133 marks and Arabic shaping unchanged. Left honest: the toast path
      (Shell_NotifyIcon, the biggest windows-sys consumer) compiles and the app
      runs, but a balloon was not caught on camera — quiet hours file it to the
      Action Center — so that one is built-and-running, not eyeball-verified
- [x] **M24** — kitty keyboard protocol. alacritty_terminal 0.26 already
      parsed `CSI > u` / `CSI < u` / `CSI = u` / `CSI ? u` into a per-screen
      flag stack, and the stack was inert because `Term::new` left
      `kitty_keyboard` at its default of false. Turning it on is the whole
      of the mode tracking. One real bug in that crate came with it:
      `CSI =` updates `TermMode` but the query reply reads the push/pop
      stack, so a set-then-query probe (how an app checks that the flags
      stuck) answered 0. The feed path pops the stale top and pushes the
      mode bits back in front of each query, so the reply matches the flags
      the encoder uses. A zero mode does not push onto an empty stack: a
      startup `CSI ? u` must not invent a level.
      The encoder is EasyTer's `kitty_encode_key` for the disambiguate
      flag — Esc, shift+Enter, ctrl+space, ctrl/alt letters become CSI u;
      plain text and unmodified Enter/Tab/Backspace/arrows stay legacy —
      extended to the other four flags. Alternate keys are layout-aware:
      the primary code is the active layout's unshifted character, the
      shifted alternate is the glyph the layout actually produced (Arabic
      kasra, not `to_uppercase` of ش), and the base-layout alternate is the
      PC-101 key in that physical position (`ش` on the A key reports `97`).
      Protocol off is the old encoder, including Ctrl+C by physical key on
      an Arabic layout. Proven by the encoder tests and a headless term
      test of push/pop/set/query plus the alt-screen stack swap. Not proven
      live: `kitten show-key`, Neovim, and Claude Code need a Windows
      session this tree was not run in.
      The follow-up made the protocol **opt-in**. ConPTY (the system conhost
      behind portable-pty) has been reported to swallow the ESC of `CSI u`
      ([rio#1607](https://github.com/raphamorim/rio/issues/1607),
      [microsoft/terminal#19847](https://github.com/microsoft/terminal/issues/19847)),
      which would turn Ctrl+C into the literal text `[99;5u`. Until that is
      verified on a real Windows session, `keyboard.kitty_protocol` defaults
      to false. Off, `Term::new` is given `kitty_keyboard: false`, so the
      parser never touches its stack — including the forms a digit/semicolon
      scanner misses (`ESC [ > 1 NUL u`, a C0 before the intermediate, a
      `:` sub-parameter). `CSI ? u` is answered by us with `CSI ?0u` (the
      same probe reply Rio sends when the protocol is off), including a
      query vte still accepts because a C0 inside the CSI does not end it
      (`ESC [ ? NUL u`). The encoder
      masks the kitty bits out of `TermMode` so a hostile push cannot make
      a key emit CSI u. Set `kitty_protocol` to true to enable the encoder.
      On, the crate is [alacritty commit `542147067dd016fc5f49120ef0ba605a98a4e3a7`](https://github.com/alacritty/alacritty/commit/542147067dd016fc5f49120ef0ba605a98a4e3a7)
      (`0.26.1-dev`, the fix for [alacritty#9071](https://github.com/alacritty/alacritty/issues/9071)).
      Published 0.26.0's `push_keyboard_mode` evicts from `title_stack` and
      panics at depth 4096, and that panic survives any scanner because vte
      completes a CSI on C0 and `:`. The pin changes the eviction to
      `keyboard_mode_stack`. Recognized pushes still stop at 16, kitty's own
      cap. The requirement is `0.26.1-dev` rather than `0.26` because Cargo
      will not select a pre-release for a plain `0.26` requirement, and the
      patch was then unused. A vendor of 0.26.0 was the fallback if this
      commit did not build against our API; `cargo check --tests` for
      `x86_64-pc-windows-gnu` did build, so the pin is what shipped.
      TODO: remove `[patch.crates-io]` and go back to the crates.io release
      once alacritty_terminal 0.26.1 is published.
      With disambiguate on but alternate keys off, a non-ASCII chord is
      reported as its PC-101 key (`keyboard.kitty_base_layout_fallback`,
      default true) — `ؤ` on the C key is `CSI 99;5u`, not `CSI 1572;5u` —
      so Claude Code can still be interrupted. That is a deliberate
      departure from the spec; turn the fallback off to send the Arabic
      codepoint.
      A crashed full-screen program leaks its flags into the shell. The
      reset pops only the **current** screen's stack, by the number of
      levels that were counted, including a level whose mode is 0. Flag
      bits alone do not make it pop, and it does not emit `1049`.
      Visiting the other screen cleared the selection, clobbered DECSC, and
      wiped the alt grid on the way back in — a prompt turned a TUI blank.
      It is a no-op when the protocol is off or that stack is empty. It runs
      on `OSC 133;A`, on shell exit through the session's own parser (not a
      fresh one), and on Ctrl+Shift+K. That chord is the rebindable action
      `reset-keyboard`: it is taken from the program in the terminal, and
      `keybinds` can move it. RIS (`ESC c`) zeroes both depth counters,
      because the parser clears both stacks and a stale cap would drop the
      next push. AltGr is text when `text` is not that key and not its
      PC-101 shift, compared case-insensitively, so Ctrl+Alt+Shift+A and
      Ctrl+Alt+Shift+1 stay chords. It types with the protocol off too:
      AltGr+E is €.
- [x] **Security (High)** — the four High findings from the audit
      (`docs/SECURITY_AUDIT.md`). OSC 52 no longer writes the OS clipboard
      unless `osc52` is `"copy"`, the window is focused, and the write comes
      from the focused pane of the active tab; the sequence must end in BEL
      or ST, a `?` query is never answered, and the decode backstop is 64 KiB.
      One scan can only see about 48 KiB (a single 64 KiB read) or about
      51 KiB (that read plus the 4 KiB carry). The paste guard treats CR
      (Enter on ConPTY), the other line breaks, other control characters,
      and strong bidi / zero-width spoofs as confirm-first (LRM, RLM, and
      ALM in a paste do not), shows the reason and a
      preview on the card, and strips ESC and the other C0/C1 bytes before
      the text is sent. `accept_local_cwd` is the shared gate for a working
      directory — drive-letter paths only, so a UNC or `\\?\` path from
      OSC 9;9 or `session.json` is dropped before `is_dir` can open SMB. A
      dropped file is quoted for the shell that pane was spawned with
      (PowerShell single quotes, including the smart quotes, cmd double
      quotes, POSIX single quotes for Git Bash and WSL, fish with `\`
      doubled before the apostrophe, a nushell raw string), and a
      drop into vim, less, a shell that is not at its prompt, or a name
      with a bidi mark or override asks first.
      Proven by the unit tests named in the audit; not exercised here as a
      live window
- [x] **M30** — mouse reporting. Clicks, drags and motion reach the program
      when it asks: xterm 1000 (press/release), 1002 (drag), 1003 (any
      motion), encoded as SGR 1006 or the legacy X10 byte form when 1006
      is off. X10 stops at coordinate 95: the wire byte is that plus 32,
      and ConPTY's UTF-8 pipe turns 0x80 and above into U+FFFD, so a click
      past column or row 95 is not reported. Mode 1005
      is not encoded (no silent X10 fallback). Buttons numbered 128 and
      up, and the horizontal wheel, are not reported. Coordinates are the
      pane's cells — padding and split gaps
      are not cells, and a click in the second pane is that pane's column
      0. On a row Bayan reorders (the whole-line BiDi path), the column is
      the glyph under the pointer, not `x / cell_w`: the left of مرحبا is
      the last logical cell, and ١٧٥ is not mirrored. Claude rows go
      through the same restoration as the painter, then the index map
      points the glyph back at the grid column Claude stored. Shift+drag
      stays a local selection, and Shift+wheel scrolls that pane's own
      history even on the alt screen. A focus loss, a tab switch, opening
      a new tab, or a pane close releases a tracked button (or drops the
      grab if tracking is already off) so Alt+Tab cannot keep dragging.
      The paste confirm card and the close card block the pointer too.
      Mode 1004 sends CSI I / CSI O when the window or the focused pane
      changes. A `Focused` event passes its own bit, because
      `Window::has_focus` can still read true for a moment after blur
      and would swallow CSI O; every other path asks `has_focus`, including
      the moment the program turns the
      mode on, and a RIS forgets the last CSI I so the next one is sent
      again. The wheel still scrolls the pane under the pointer and is
      not reported at all over the tab bar or the padding; Ctrl+wheel is
      still font zoom. Scrollback itself moves by pixels when
      `scroll.smooth` is on (the default): a touchpad keeps the fraction
      of a line, a notch eases in, and `"scroll": {"smooth": false}` is
      the old whole-line jump. The alt screen still turns the wheel into
      arrows. The suite size after this merged with main is
      under M31. Not proven here: a live htop /
      vim `mouse=a` / lazygit / Claude Code session — this tree doesn't
      run a window on the machine that built it. Windows CI is the build
      that does.
- [x] **M31** — hyperlinks. OSC 8 is already on the cell
      (`Cell::hyperlink`); a run of cells that share one id is a span, and
      the bytes are not parsed a second time. Cells an OSC link does not
      cover are scanned for `http`/`https` URLs and for `path:line[:col]` /
      `path(line,col)` (rustc, gcc, MSVC, grep). Hover underlines the span
      through the same shaped glyph boxes a click uses, and the pane's
      bottom strip shows the target — so a label that differs from its URI
      is visible. The underline is not a shaped-run style bit: baking it
      into the cache key would reshape the line on every hover change.
      Ctrl+click opens the link. Shift+click stays a local selection and
      wins over Ctrl. A plain click while mouse tracking is on still goes
      to the program, Ctrl modifier bit included; the window asks
      `link::click_owner` before `mouse::Route::press`, and an overlay
      (settings, cockpit, paste guard, close guard, the refused-link card)
      blocks the click so a link under a card does not open.       Only `http`
      and `https` are association-opened, and never through `cmd /c start`:
      `ShellExecuteW` gets the URI as the single `lpFile`. A `file` URI
      association-opens only after `accept_local_cwd` has accepted a drive
      path — UNC, `\\?\`, `\\.\`, `\??\`, a colon after the drive letter,
      a comma, a quote, and `\\` anywhere are refused before `exists`,
      because a stat of a UNC path opens SMB and can leak NTLM — and only
      when the extension is on the document/media allowlist (`.txt` `.md`
      `.log` `.json` `.csv` `.pdf` `.png` `.jpg` `.jpeg` `.gif` `.webp`
      `.bmp` `.svg`). Everything else, including no extension, is revealed.
      An existing 8.3 name is expanded with `GetLongPathNameW` and both
      the UNC gate and the allowlist run again on the long path; a `~`
      component that cannot be expanded is revealed. `javascript:`,
      `ms-*`, `search-ms:`, `data:`, and anything else are refused on a
      red card whose keys say Enter closes. A URL with userinfo, and any
      non-ASCII host, shows the real host first (punycode for IDN) and
      waits for a different card; Enter is what opens it. Control
      characters and bidi overrides are refused too, including after
      percent-decoding (a stray `%` is not one of those), and a target
      longer than 2048 bytes is refused. `path:line` never
      association-opens the file. The editor is a `Command` argument
      vector from `editor` in the config (`code -g {file}:{line}:{col}`);
      `{file}` `{line}` `{col}` are substituted in one pass after the
      template is split, so a path with spaces is one argument and a
      `{line}` inside the filename is left alone. The path is absolute, so
      a file named `-n.rs` cannot become an option. Absent that key,
      `code.exe` on PATH (or the `Code.exe` beside VS Code's `code.cmd`)
      is used; a bare `.cmd` is not spawned, and there is no association
      fallback if the editor fails to start. With no editor, the folder
      opens as `%WINDIR%\explorer.exe` with raw `/select,"<path>"`, not a
      shell. A relative path with no pane cwd is refused rather than
      resolved against Bayan's own directory. A missing file is the card,
      unless the path has a `~` component that could not be expanded, in
      which case it is revealed. Relative paths that do resolve use the
      pane cwd OSC 9;9 already stores (`link::cwd_hook`, the same
      `accept_local_cwd` gate); OSC 7 should write that same field.
      Unquoted paths that contain spaces are not detected, including a
      tail like `y\a.rs` inside `C:\x y\a.rs:12`. HIDDEN cells are not
      clickable. Spans are cached per row and dropped when the pane's PTY
      wakes, when it scrolls, and on relayout; that wakeup also clears the
      hover. `exists`, the editor lookup, and the browser, the editor, and
      explorer are spawned off the UI thread, and that thread initializes
      COM before `ShellExecuteW`. Proven: 158 `#[test]` functions (118 on
      main before these link tests), including OSC 8 open/close, URL
      punctuation and surrounding Arabic, the four file:line shapes, the
      scheme allowlist, the UNC forms, the association allowlist, every
      old blocked extension plus PATHEXT, ADS colons, injected 8.3
      expansion, explorer `/select`, a missing file, one-pass
      placeholders, the absolute path, a stray `%`, the extra invisible
      characters, a spaced path that must not match, hidden cells, the
      row cache, the script hosts, userinfo, an IDN punycode card,
      control/bidi rejection, the editor argv (no shell), an RTL hit
      through `column_for_click`, and the overlay short-circuit in front
      of `Route::press`. Not proven here: a live hover underline,
      `ShellExecuteW` actually launching a browser, or a real editor spawn
      — this tree doesn't run a window on the machine that built it.
- [x] **M32** — the agent's own signals, drawn. OSC 9;4 progress (ConEmu's,
      read by Windows Terminal, Ghostty, WezTerm, Konsole — and emitted by
      Claude Code under `terminalProgressBarEnabled`) becomes a 3px bar along
      the tab's bottom edge: green working, red failed, amber paused, dim
      full-width while indeterminate, with a "42%" lead-in on the cockpit row.
      Desktop-notification escapes in all three dialects — OSC 9;message
      (iTerm2/ConEmu), OSC 777;notify (rxvt, what Ghostty and WezTerm speak),
      OSC 99 (kitty) — raise a native toast when the window is unfocused or the
      tab is in the background; a BEL from an unfocused window does too, so
      `preferredNotifChannel: terminal_bell` in Claude Code lands somewhere you
      can see. The shell gets `TERM_PROGRAM=Bayan`, `TERM_PROGRAM_VERSION` and
      `COLORTERM=truecolor`. Verified live: a 60% report typed through the real
      ConPTY path draws the bar under the tab (PrintWindow capture,
      pixel-sampled). Learned again on the way: `cargo test` does not relink
      `target/debug/bayan.exe` — the first two captures were of a stale binary
- [x] **M33** — RETIRED, and kept here because the log is the point.
      Ctrl+click link opening was built on this branch while M31 was being
      built on main, and `links.rs` was the smaller, later, worse of the
      two: 530 lines of plain-text scanning against `link.rs`'s 4700, with
      no OSC 8, no URI or IDN handling, no editor route, and no confirm
      card. It was deleted in the merge; M31 is the link feature. Do not
      reintroduce a second scanner.

      Three things it found outlived it, because main did not have them.
      SGR underline and strikethrough were simply absent on any row holding
      Arabic — the grid path draws them from columns and the BiDi path has
      no columns — so decorations now travel as byte ranges of the shaped
      string and come back as pixels through `highlight`, correct through
      Claude restoration because `byte_cols` is. `Flags::DIM` computed
      `fg.0 * 2 / 3` on a `u8`, which overflows for every channel above
      127, i.e. most foregrounds: a debug panic on the first faint
      character. And an untouched config serialized `"keyboard"` out of
      nothing, so saving one minted settings the user never set.

      Two limits that applied to `links.rs` applied to `link.rs` too: a
      mapped network drive was still stat'd, and a junction or symlink
      aimed at a UNC share defeated any gate that reads the path as text.
      Both are closed by `localpath.rs` («Local all the way down», below).
      Gating on the canonical path, as first planned, would have been too
      late: the open that produces it has already connected to the share.
- [x] **M34** — the window comes back where you left it: session.json carries
      the last non-maximized rectangle plus the maximized flag, restored only
      if enough of the title strip lands on a monitor that is still connected
      (a window last seen on an unplugged second screen must not return
      off-screen). Verified live: moved and resized through the OS, closed
      cleanly, relaunched — same rectangle to the pixel
- [x] **M35** — Ctrl+Tab returns to the tab you were just on (browser/IDE
      stack behaviour, a top Ghostty request): holding Ctrl and pressing Tab
      again walks further down the history, Shift walks back, releasing Ctrl
      commits the stop. `tab_switch: "order"` restores the classic cycle.
      Verified live with driven input: three tabs, click the first, Ctrl+Tab —
      the third (previously used) tab activates, where a plain cycle would
      have picked the second
- [x] **M36** — right-click pastes. Before this, the right button only reached
      a program that tracks the mouse and was dropped everywhere else, so
      there was no mouse paste at all. The first attempt copied Windows
      Terminal: copy if there is a selection, else paste. It failed in real
      use, because copy-on-select leaves the highlight up after a drag, so the
      next right-click copied a second time instead of pasting. Now right-click
      pastes through the paste guard, like Ctrl+V. It copies only when
      copy-on-select is off. A tracking program still hears the click unless
      Shift is held, and an overlay blocks it (`mouse::right_click`, ordered
      like `link::click_owner`). Verified live with real mouse input: dragging
      across `COPYTEST-12345` put it on the clipboard, and a right-click pasted
      it at the prompt
- [x] **M37** — clean-code gate. First whole-tree `cargo fmt` (a mechanical
      commit, listed in `.git-blame-ignore-revs`). CI now fails on
      `cargo fmt --check`, on any clippy warning (`-D warnings`) and on a
      RustSec vulnerability (`cargo audit`, which also found and dropped a
      yanked clipboard-win 5.4.1). `// SAFETY:` comments went on the `unsafe`
      blocks outside `main.rs`. Two lessons. First, GitHub's stable clippy
      (1.99) rejected `chunks_exact(2)` in `shells.rs` the day the gate went
      on, while the local 1.97 had never heard of that lint, so `-D warnings`
      on `stable` means accepting new lints as they ship. Second, the old
      "delete config.json before cargo test" rule was folklore. No test reads
      it. What the tests really did was append their deliberate `b_broken`
      plugin failures to the user's `~/.bayan/plugins.log`. Under `cfg(test)`
      that log now goes to `%TEMP%`
- [x] **M38** — four features from a survey of what people ask terminals
      for (Ghostty's most-voted discussions, Windows Terminal's 1.18/1.19
      headliners, agent-CLI workflow write-ups). Each is a rebindable action,
      so it is in the shortcuts editor and the palette too.
      **Tab names** (Ctrl+Shift+R): the palette opens in naming mode, the
      typed text becomes the name on Enter, and its last row gives the folder
      name back. Control characters and bidi embeddings, overrides and
      isolates are stripped, so a name cannot reorder the tab bar; 40
      characters at most. The name is saved in `session.json`.
      **Broadcast input** (Ctrl+Shift+B): typed keys and pastes go to every
      pane of the tab. Each pane gets the key encoded for its own modes
      (application cursor, kitty flags) and the paste wrapped for its own
      bracketed-paste mode, not a copy of the focused pane's bytes. The tab
      label starts with `»` while it is on, and it turns off when the tab is
      down to one pane.
      **Pane zoom** (Ctrl+Shift+Z, WezTerm's chord): the focused pane fills
      the tab and the label ends in `+N` for the N hidden panes. Hidden panes
      get an empty rect, so they are not drawn, not hit by the pointer, and
      not resized, and their programs never see a resize to nothing.
      Alt+arrows move the zoom with focus; a split or a pane close ends it.
      **Copy last output** (Ctrl+Shift+L): Enter at a prompt now records the
      row below the cursor on that prompt's OSC 133 mark, so a command's
      output is the rows from there to the next prompt. The block is
      selected, which is the feedback, and copied through `copy_selection`,
      so Claude rows are restored the way a mouse copy restores them. An
      empty Enter does not hide the command before it. It needs shell
      integration, so cmd copies nothing.
      Also fixed: the palette ignored the space bar (winit names it), so no
      label or name with a space in it could be typed.
      Proven by unit tests on the pure parts (`clean_tab_name`, `tab_label`,
      `rename_rows`, `zoom_rects`, `broadcast_targets`, `output_lines`,
      `term::last_output_range`, the saved name round trip), and by a
      Windows-target `cargo check`/`cargo clippy -D warnings`/`cargo fmt
      --check` from a Linux container. Not proven here: a live window. These
      were written where Bayan cannot run, so `cargo test` and a
      launch-and-screenshot round on Windows are still owed
- [x] **M39** — a right-click menu. M36 made right-click paste, and that was
      all it did: there was no menu, so nothing in Bayan could be found by
      right-clicking. Now a right-click on a pane opens a popup at the
      pointer: نسخ التحديد (only with a selection), لصق, copy last output, search,
      both splits, zoom and broadcast (only with a split, each labelled with
      the way it will flip), new tab, rename tab, close pane and settings.
      Every row is a keymap action, so a row does exactly what its shortcut
      does. It is the palette in a menu mode, not a second overlay: arrows,
      Enter, Esc, the wheel and type-to-filter already worked, the hovered
      row is the selected one, a click outside closes it, and a second
      right-click moves it. It opens to the left of the pointer, because the
      UI reads right to left, and flips right or up at the window edges
      (`render::menu_origin`). Who owns the click did not change
      (`mouse::with_menu`): a program that tracks the mouse still hears it,
      Shift still keeps it local, and an overlay still blocks it.
      `"right_click": "paste"` in config.json brings back M36's paste on
      right-click; anything else, or nothing, is the menu. `BAYAN_PASTE`
      still pastes: with the menu on it takes the menu's لصق row
- [x] **Security pass, 2026-10-31** — every open item of that day's
      defensive review (NEW-01…07, BAYAN-005…017) addressed in one change.
      Program toasts (OSC 9 / 777 / 99) are opt-in (`"osc_notify": true`),
      need a BEL/ST terminator, have control/bidi/zero-width characters
      stripped and length caps, and always carry Bayan's own title
      («بيان · tab»); the program's title goes in the body. OSC 133 `B`
      carries the session nonce like `A` and `D`, and a bare `B` clears
      "at prompt" instead of setting it. oh-my-posh theme names outside
      `[A-Za-z0-9._-]` are never listed, previewed or written to `$PROFILE`.
      Plugin palette actions are badged «إضافة», may not reuse a built-in
      label, are capped, and wait on the paste-guard card instead of being
      written raw; Enter then types them into the focused pane only,
      unbracketed (so a trailing CR still runs the command), with every
      control but CR and Tab removed. WSL distro names that would split into extra `wsl.exe`
      arguments are skipped; `split_command` only ends the program at a
      `.exe` that ends a word, preferring one that exists. CI has
      `permissions: contents: read` and every action pinned to a commit
      SHA. `BAYAN_*` hooks compile only into debug builds or
      `--features dev-hooks`, and the profile log no longer records which
      key was pressed. Prompt-picker previews render on a `bayan-posh`
      worker, selected theme first. The cockpit and close guard show
      PTY-shaped text through `link::preview`. Config keybinds must pass
      `is_bindable`. A `D` without an exit code says «انتهى الأمر», not
      success. Restore is capped at 16 tabs, quit kills every shell, and
      `Session` kills its child on drop. `meta`/`parser` locks recover from
      poisoning. The launcher directory goes through `accept_local_cwd` +
      `usable_spawn_dir`, Bayan's own folder is skipped, `is_under`
      understands `\\?\`. A failed `RtlGenRandom` turns
      shell integration off instead of using time+PID. cwd and links share
      `link::is_forbidden` (the cwd form keeps ZWNJ/ZWJ, which Persian folder
      names need). `d3d12`, `dcomp` and `d3dcompiler_47`, which wgpu loads
      dynamically by bare name, are pre-mapped from System32. NEW-04 is only
      partly closed: `dxgi.dll` and `opengl32.dll` are static imports,
      resolved before `main`; closing that needs `/DEPENDENTLOADFLAG:0x800`
      at link time, untested, so install Bayan somewhere only you can
      write. An OSC 8 span whose visible text names a web address for a
      different host (any word of it, punctuation stripped, bare
      `paypal.com` included) waits for the host card — a heuristic: text
      drawn outside the span is not seen.
      An independent review of the patch found the plugin-send semantics,
      the static-import gap and the NEW-07 bypasses above; all three were
      fixed or recorded before this landed. Proven: `cargo test` on
      Windows, 363 passed; a release build launched with the tab × showing;
      CI on PR #24 green (build + test, MSRV, fmt + clippy, audit) with the
      SHA-pinned actions and `permissions: contents: read`.
      **Re-verification, same evening (R-01…R-08).** A second static review
      of the fixed tree found 4 Low and 4 Info items, mostly where M38/M39
      (broadcast, tab ×, middle-click close) meet the confirm cards. A
      confirm card now remembers its pane and sends nothing if that pane
      is no longer focused, and tab-bar clicks and middle-click close are
      swallowed while one is up. A dropped path goes to the focused pane
      only, never to broadcast panes, and a paste card says how many panes
      Enter reaches. The plugin card shows the whole payload, wrapped, with
      every CR visible, and plugin sends cap at 512 chars. The prompt picker
      renders only the featured themes and the selected one on open, the
      rest on highlight, never a theme with a `command` segment, never from
      a non-local `POSH_THEMES_PATH`, and runs oh-my-posh by absolute path,
      skipping Bayan's own folder. The forbidden set covers the invisible
      Hangul fillers and variation selectors, and toasts use the full set.
      The prompt nonce travels in `BAYAN_PROMPT_NONCE`, is copied into
      `$global:__BY_N` and removed before anything else runs, so it is not
      on the PowerShell command line or in any child's environment. Tab
      titles are fitted by measured width: the cell-count version still cut
      "Desktop" to "Deskto" on a real Windows font after the first fix.
      Behaviour changes to know about: program toasts are off until
      `osc_notify` is set; plugin actions ask before typing and can no
      longer send ESC sequences or Ctrl keys; only PowerShell's nonce'd
      `133;B` marks the prompt, so drops into cmd, bash and WSL panes
      always show the confirm card; picker previews of non-featured themes
      appear when highlighted
- [x] **QA follow-up, spawn and restore** — the context menu's copy row is
      «نسخ التحديد», the same string `Action::Copy` carries, so the menu
      stays on the keymap. `wsl.exe` is the `GetSystemDirectoryW` copy.
      Discovery skips WSL when that file is absent, and a saved shell line
      that still says `wsl.exe` is rewritten to that path. Save and a
      relative link take `meta` with `lock_or_recover`, and a scan rejects
      `.lock()` followed by `.unwrap()` even across lines. A restored tab
      is updated only when its first shell started, so its extra panes,
      layout, focus and name stay with that tab. Restore stops at four
      panes, the same cap as a live split. If every saved tab fails to
      start, one new shell still opens. `config.json`, `session.json` and
      `$PROFILE` are flushed to a temp file in the same directory and then
      replaced. The profile also keeps a one-time `.bayan.bak` from before
      the first rewrite. The orphaned "weighted split" doc line above
      `saved_split` is gone. Proven here: `cargo fmt`, `cargo clippy
      --all-targets --target x86_64-pc-windows-gnu -- -D warnings`, and the
      unit tests under wine. Four render tests that measure Windows font
      metrics failed in that environment
      (`cosmic_ligates_batched_ascii_runs`,
      `arabic_tui_text_does_not_move_an_anchored_status`,
      `tab_close_hits_the_drawn_control_and_misses_the_title`,
      `tui_fields_respect_pane_edges_and_leave_vertical_room_for_marks`);
      `render.rs` was not changed. The ConPTY loader tests and the
      PowerShell preview timeouts were not run here. The Windows CI suite
      covers them.
- [x] **Nested splits** — a tab's panes are a tree of splits (`layout.rs`),
      so «تقسيم: جنبًا إلى جنب» and «تقسيم: فوق بعضها» split the FOCUSED pane
      the way they say and mix freely: two side by side, then the right one
      stacked. Before, a tab had one axis taken from its first split, so
      after that both actions did the same thing. Splitting along the axis
      a pane already sits in adds a sibling that shares its size. Closing a
      pane collapses its split; dividers drag at every level and the cursor
      follows the divider under it. Panes stay one flat list, so focus,
      broadcast, zoom and the close guard are unchanged. The session file
      saves the tree; a file from before restores as one row or column.
      Proven by six unit tests on the pure tree (rects, split, remove,
      dividers and drags, session round trip). Chrome Arabic (menus,
      settings, cards) now shapes in Segoe UI: Cascadia Code's monospaced
      Arabic stretched every letter to a full cell
- [x] **Tab groups** — each group holds its own set of tabs, so related
      sessions stay together and switching groups never mixes unrelated
      tabs into the bar. Ctrl+Shift+G opens a new group on one fresh tab;
      Ctrl+Shift+PageDown / PageUp, or a click on the blue chip at the
      bar's leading edge (`⇄ مجموعة 2`), steps between groups and lands on
      the tab each group was last on. The bar, its arrows, the tab × and
      Ctrl+Tab (in both orders) see only the group on screen. The palette
      lists «المجموعة: …» to jump, «نقل التبويب إلى …» and «نقل التبويب إلى
      مجموعة جديدة» to regroup the active tab, and «تسمية المجموعة»; the
      right-click menu offers مجموعة جديدة. With one group nothing changes:
      no chip, the bar is the old bar. A group is a TAG on each tab, not a
      container (`groups.rs`): tabs never move in the one flat list, so a
      background group's panes still get their DSR replies, bells, exits
      and toasts by pane id, and the MRU and close-card indices stay
      valid. Closing the visible tab stays in its group; closing a group's
      last tab drops the group and goes to the most recent tab elsewhere.
      The chip shows an amber dot when a tab in ANOTHER group is waiting
      on you, and the cockpit, which spans every group, prefixes each row
      with its group. Group numbers stay with the group while it lives.
      The session file saves group names and each tab's group; a file from
      before restores as one group, and a one-group session writes no
      group keys. The shortcuts editor's rows now tighten to fit the
      window: at 20 actions its card ran past a 900px window's bottom
      edge (caught by the existing layout tests). Proven: `cargo test`,
      388 passed (11 new: the group model, MRU walks confined to a group,
      the close rule, palette rows, session round trip, chip geometry; the
      chip also joins the press-order and chord round-trip tests); clippy
      and fmt clean; a debug build
      launched with `BAYAN_GROUPS=1`, captured with `PrintWindow` on
      Bayan's own HWND (PID-checked, alive before and after), shows the
      chip and only the second group's one tab, and the same build without
      the hook shows the unchanged bar. Not verified on screen: clicking
      the chip, the PageUp/PageDown chords, restore of a saved multi-group
      session, the attention dot
- [x] **Shell menu** — a right-click anywhere on the tab bar (a tab, its
      ×, the group chip, the arrows and plus, the empty strip) opens a
      menu of the installed shells: PowerShell, PowerShell without its
      profile, PowerShell 7, Command Prompt, Git Bash and each WSL distro,
      as `shells::detect` finds them, the default for new tabs marked
      «الافتراضية». A row opens a new tab with that shell in the group on
      screen, through the palette's existing `PaletteRow::Shell` path (a
      failed spawn shows the same card). The pane's right-click menu gains
      «تبويب جديد بطرفية…», which opens the same list, so it is reachable
      with the bar hidden; it is the one pane-menu row that is not a keymap
      action. The caption buttons, the gear and the resize edges keep no
      right-click meaning, and a card, panel or the cockpit swallows the
      press (`chrome::right_press`). Until background detection reports,
      PowerShell and cmd stand in. Proven: `cargo test`, 390 passed (the
      press decision for every bar spot, the rows and default mark);
      clippy and fmt clean; a debug build with `BAYAN_SHELLS=1`, captured
      with `PrintWindow` on Bayan's own HWND (PID-checked, alive before and
      after), shows the menu listing this machine's five detected shells
      with Git Bash marked default. Not verified on screen: a real mouse
      right-click, picking a row
- [x] **English interface** — Bayan's own chrome can be English. The
      settings header has a language button written in the language it
      switches TO («English» in Arabic, «العربية» in English), so a reader
      of either can always find the way back; the switch is immediate and
      saved as `"language": "en"` in config.json (anything else, or
      nothing, is Arabic). English is left to right: the settings ledger
      and the shortcuts editor are mirrored (labels left, controls right,
      each control group keeping its order, so «‹ name ›» and «− 16 +»
      still read left to right), menus open to the right of the pointer
      with labels left and shortcuts right, and the close card's text
      starts at the left. Every interface string (149) is a
      `tr("عربي", "English")` pair at its point of use (`lang.rs`), so a
      label cannot drift from its translation in a separate table. What
      programs print in a pane is untouched. Shell names are translated
      when drawn, not when detected, because detection runs once on a
      background thread. BAYAN-007's built-in-label check now refuses a
      plugin label that matches a built-in in EITHER language, since
      plugins load once and the language can change afterwards. Under
      `cfg(test)` the language is per thread, so English tests cannot flip
      Arabic ones running in parallel. Proven: `cargo test`, 395 passed
      (mirrored settings geometry, menu direction, English rows, the
      plugin check in both languages); captures of a debug build with
      `BAYAN_LANG=en` through `PrintWindow` on Bayan's own HWND show the
      settings panel, the palette and the shell menu in English and
      mirrored, and the Arabic panel unchanged apart from the new button.
      The first English capture showed «Silent / Sound / Alert» running
      into each other; English bell segments are wider now. Not verified
      on screen: clicking the language button in a live window, the
      shortcuts editor, the cards and the cockpit in English
- [x] **Arabic font, and every installed monospace** — a new settings row,
      «خط العربية» / "Arabic font", picks the font Arabic text in the
      terminal is drawn with: Auto (the terminal font's own Arabic, as
      before) or any installed font that maps the probe letters ب ع ه ي ل —
      on the development machine that is Kawkab Mono, Simplified Arabic
      Fixed, Cairo, Tajawal, Dubai, Sakkal Majalla, Traditional Arabic,
      the DecoType families and the bundled Amiri among them. Only Arabic
      runs change font (`ui_font_runs`, the chrome's splitter); Latin text
      on the same row stays in the terminal font. Rows with Arabic were
      already shaped as whole lines and hit-tested through the shaped
      layout, so a proportional Arabic font (Cairo) needs nothing new from
      the grid. The coverage scan reads each face's cmap once per process
      (`skrifa`, re-exported by cosmic-text, no new crate). A name in
      config.json (`"arabic_font"`) that is not installed or cannot draw
      Arabic counts as Auto. The font cycler now also lists every installed
      MONOSPACED family after the known programming fonts, so Ubuntu Mono,
      Noto Sans Mono, DejaVu Sans Mono, Kawkab Mono and anything installed
      later appear without a code change. Fifteen settings rows no longer
      fitted a 700px window (caught by an existing test): rows tighten to
      fit, down to a floor. Proven: `cargo test`, 398 passed, including a
      shaping test that inspects each glyph's font: with Amiri chosen,
      every Arabic glyph of `echo مرحبا` is Amiri and no Latin glyph is;
      captures of the same typed line with Auto, Kawkab Mono and Cairo
      (`BAYAN_ARABIC_FONT`, `BAYAN_TYPE`) show three different Arabic
      letterforms beside unchanged Latin, and the settings row reading
      Kawkab Mono. Not verified on screen: clicking the cycler arrows
- [x] **Kawkab Mono bundled** — `fonts/KawkabMono-Regular.ttf` (SIL OFL
      1.1, unmodified; notice and licence in `fonts/OFL-KawkabMono.txt`)
      loads beside Amiri, so the Arabic-font row offers a monospaced
      Arabic font on every PC, not only on one that has it installed. It
      is a choice, not a new default: Auto is unchanged. Regular weight
      only (~260 KB); upstream publishes no compiled fonts in its git
      tree, and this file is the one Ghostty redistributes. `fonts/OFL.txt`
      had a header copied from EasyTer that named Vazirmatn and Noto Naskh,
      which Bayan never shipped; it now lists what is in the folder.
      Proven: `font_lists_find_arabic_fonts_and_every_installed_monospace`
      requires Kawkab Mono in the Arabic list from a bare renderer
- [x] **Two defects from the first hands-on pass** (real clicks and keys
      through `SendInput` on a sandboxed profile). (1) In the Arabic
      interface the Arabic-font row read blank on Auto: `draw_val` shaped
      the value with `align_left = false`, so «تلقائي», an RTL paragraph,
      was right-aligned inside the run's 1,000,000 px buffer and drawn far
      off-screen while `measure` gave the right width. «PowerShell سريع» and
      English «Auto» start with a Latin letter, so they never showed it.
      Values now shape left-aligned. (2) A restored session brought every
      tab back on the default shell: `SavedTab` kept no shell. Each pane's
      command is saved beside its cwd (`None` for the default, and no field
      at all when every pane is), and restored only if
      `shells::is_restorable` accepts it — the fixed PowerShell/cmd forms,
      the installed Git Bash, `wsl.exe -d` with a safe distro name — so
      session.json cannot name a program to run; anything else, or a shell
      that no longer starts, comes back as the default. Restore itself
      stays opt-in (`"restore_tabs": true`). Proven: tests for the value's
      glyph positions (and the off-screen cause), the restorable set, and
      the shell round trip including a planted `calc.exe`. Seen in that
      pass and left as is: in Cairo the end-of-line block cursor overlaps
      the next Arabic letter's edge, and the shell menu opened from + covers
      the bottom 10 px of the ‹ › + buttons
- [x] **Local all the way down** — closes the two M33 limits. A Ctrl+clicked
      file and a new tab's directory both pass `localpath::walk_live` before
      anything stats them: the drive must not be `DRIVE_REMOTE`, and each
      component's attributes are read without following it, root first, so
      every read sits under directories already known to be local. A
      symlink or junction is not refused outright (pnpm's `node_modules` is
      made of them): its target is read from the reparse data
      (`FSCTL_GET_REPARSE_POINT` on a handle opened with
      `FILE_FLAG_OPEN_REPARSE_POINT`), and the walk restarts on that target
      when it is a local drive path, through the same `accept_local_cwd`
      gate. A UNC or volume target, a link type it does not decode, a loop
      past 32 hops, or a read that fails for any reason but "not found" is
      refused with «الملف ليس على قرص محلي», never revealed, since Explorer
      would follow the link too. Cloud placeholders (OneDrive) are reparse
      points that are not name redirects, so they pass. The old new-tab
      check read only the LAST component, so a junction in a parent
      (`C:\proj` → `\\server\share`) still reached the share; that is
      closed too. Ghostty's kitty-graphics path check
      (`GetFinalPathNameByHandleW` after the open) was the prior art and
      the reason not to copy it: it checks after the open has already
      connected. Proven by 19 tests on the pure walk and reparse-buffer
      decoder (a fake volume with junctions, relative and rooted symlinks,
      loops, a mapped drive, cloud and WSL tags, truncated buffers), and
      a `link.rs` test that every action refuses rather than reveals. Proven
      live on Windows at review (Nov 2026): a local junction and a relative
      symlink open, a junction to a missing file is Missing, and a symlink
      to `\\127.0.0.1\…`, a local junction chained into it, and a junction
      to a `\\?\Volume{…}\` path are refused as not local. Not run
      against a real SMB share. A volume mount point (a link to
      `\??\Volume{GUID}\`, a volume's root) was refused too; since #36 the
      walk goes through it in place and checks every component below it,
      while a link to a folder inside a volume is still refused. A
      link swapped between the walk and the open (TOCTOU) is not covered.
- [x] **Copy mode** (Ctrl+Shift+X, WezTerm's chord) — select and copy
      scrollback without the mouse. It runs on alacritty_terminal's vi
      mode, which already moves the cursor, scrolls the view after it and
      stretches the selection; `copymode.rs` only maps keys to it. Arrows
      or h j k l move, w b e by word (Shift for whitespace words), 0 and $
      (or Home and End) to the line's ends, g and Shift+G to the top and
      bottom of the scrollback, PageUp/PageDown by a screen. v starts a
      selection, Shift+V a line selection, Ctrl+V a block; pressing the
      same one again drops it. y, Enter or Ctrl+C copies and leaves; Esc or
      q leaves without copying, and leaving returns the view to the prompt.
      Keys resolve by physical key, so the Arabic layout keeps every one,
      and no key reaches the shell while the mode is on. The cursor is a
      solid block that does not blink, and the top-right bar (the search
      bar's slot) reads «وضع النسخ: v تحديد · y نسخ · Esc خروج». On a cell
      laid right to left the horizontal keys swap, so Left still moves
      left; the renderer's shaped layout answers which way a cell runs
      (`Renderer::cell_rtl`), never the column index. A cursor that would
      sit past the end of an Arabic row snaps back onto its last character:
      the shaped path lays the row out to the cursor's column, and in a
      right-to-left line those blanks go visually left, so the word jumped
      toward the cursor (seen live before the fix). The action is in the
      shortcuts editor and the palette. Proven by 10 tests (the key map on
      an Arabic layout, motions and selections on a real `Term`, the snap,
      the bar) and live: real scan-code keys in a debug build copied a
      line to the clipboard, and PrintWindow captures showed the hint, the
      selection, the RTL arrow and a clean prompt after Esc. There is no
      vim count prefix, `gg` is a single g, and the column is not
      remembered across short lines.
- [x] **Follow the Windows app theme** — `"theme": "system"` (the absent
      key means the same) tracks light and dark. `"dark"` and `"light"`
      lock the pair in `theme.rs`; a named scheme, or any hand-set
      `bg`/`fg`/`palette`, is not replaced when Windows changes. The
      signal is winit's `ThemeChanged` and `WM_SETTINGCHANGE` whose
      lParam is `ImmersiveColorSet`; both read
      `HKCU\...\Personalize\AppsUseLightTheme` (DWORD 1 is light, anything
      else including a missing value is dark). The terminal palette, the
      tab bar (it already derives from the page colour), and
      `DWMWA_USE_IMMERSIVE_DARK_MODE` update together. The settings row
      «سمة النظام» offers النظام / داكن / فاتح. OSC 11 is not answered
      here. Proven by the resolution tests (system/dark/light against the
      registry DWORD, a named scheme and a hand-set background stay put)
      and the event-path test (a theme-changed signal replaces the
      palette and the caption flag, and does not touch a locked scheme).
- [x] **Screen readers** — NVDA, JAWS and Narrator reach Bayan through
      AccessKit (`accesskit` + `accesskit_winit`), whose Windows backend is
      UI Automation. A raw `IRawElementProvider` / `ITextProvider`
      implementation was the alternative: no extra crates, and the text
      events would be exactly Windows Terminal's. It would also be a large
      unsafe COM surface that three readers each interpret slightly
      differently. AccessKit already maps a document of text runs, a caret
      and a live region onto those UIA events, and it builds nothing until
      a client connects. On Windows that first client leaves the adapter
      active for the session, so afterwards the tree is rebuilt only when
      the stamp changes, at about 10 Hz, and the last state of a burst is
      always sent. That laziness is the reason to take the dependency
      instead of calling UIA directly. Each visible pane is a document whose
      lines are the grid's logical order (Arabic is not read in the shaped
      visual order; Claude rows go through the same restoration as copy).
      The cursor is the caret and the selection is a range on those runs.
      New output is a polite live region, held for 500ms and then spoken as
      one tail, so `cargo build` is not a line-by-line flood. An OSC 133
      finish replaces that tail with one sentence that includes the exit
      code. AccessKit raises LiveRegionChanged only when the name string
      changes, so the same sentence twice in a row (two commands finishing
      alike) toggles a trailing space to be spoken again. Tabs, panes, the
      command palette, the right-click menu and the
      confirm cards have names and roles in the UI language (Arabic by
      default). Focus follows the active pane, and a card or the palette
      takes it while it is open. The shaping path is unchanged. Proven by
      11 tests (a mixed Arabic+Latin line stays in storage order, including
      off a real `Term`; the caret column, including a wide glyph; the
      selection range; flood coalescing; the exit-code sentence; a repeated
      sentence re-firing; Arabic
      names and roles for tabs, panes, the palette and a confirm card;
      focus following the active pane; an unchanged stamp does not rebuild;
      a 60 Hz burst stays near 10 builds a second and still publishes the
      last stamp). Not verified on a machine: NVDA,
      JAWS or Narrator actually speaking a running Bayan. Character boxes
      for a magnifier's caret highlight are not provided, and a reader
      cannot move the selection: the document does not advertise
      SetTextSelection because nothing handles it.
- [x] **Shell integration for bash, WSL and cmd** — the PowerShell OSC 133
      wrapper is no longer the only one. Git Bash and MSYS bash get a temp
      `--rcfile`, WSL bash the same file after the distro's login shell is
      read from passwd, WSL zsh a `ZDOTDIR`, and cmd its own `PROMPT`
      wrapped in `$E` marks. The bash and zsh sequences are the ones
      PowerShell already emits (`D` with the exit code, `A`, `B`, all
      carrying the session nonce), so prompt jump, command blocks, "at
      prompt" drops and the exit-code gutter see the same marks. Bash and
      zsh also emit `C` at command start; the parser ignores it as a
      boundary. cmd cannot: `A` and `B` only, and no nonce, because every
      program cmd starts inherits `PROMPT` and could read it back. Its marks
      are untrusted: they reset a leftover BiDi mode and do nothing else, so
      cmd keeps the features it had without them. Unknown shells are not
      injected. `"shell_integration": false` turns it off, including for
      PowerShell. User rc files are never rewritten. An elevated token does
      not source them (and does not init oh-my-posh from the profile); the
      scripts sit in `%ProgramData%` with a random name, a protected
      administrator DACL and a high integrity label, and are deleted with
      the pane. A normal pane's files are in the user temp directory, current
      user only, not world-writable. Proven by the `shellint` plan tests
      (bash rcfile, login vs not, elevated, WSL bash/zsh/other, cmd, opt-out,
      unknown shell, WSLENV, the SDDL, the delete guard) and by feeding the
      emitted bytes through the same OSC 133 parser as PowerShell, including
      an Arabic prompt that must not show the sequence. Not run here: a live
      Git Bash, a WSL distro, or an elevated token — this tree was checked
      with `cargo clippy --target x86_64-pc-windows-msvc --all-targets -- -D warnings`
      and `cargo fmt --check`.

## SSH tabs and the session multiplexer

An SSH tab runs the Windows OpenSSH client, `GetSystemDirectoryW()\OpenSSH\ssh.exe`,
in a new ConPTY tab. A copy on `PATH`, beside `bayan.exe`, or named by an
environment variable is never used, including when Bayan is elevated. The
command palette row «فتح SSH» lists hosts from `ssh_hosts` in
`config.json` and from `Host` lines in `~/.ssh/config`. Wildcard patterns
(`*`, `?`) and `Match` blocks are not destinations. The host is the tab
title. A host that starts with `-` is rejected, and `--` is placed before
the destination so the host cannot become an ssh option. Bayan does not
store a password; authentication stays with the ssh agent and keys.

```json
{ "ssh_hosts": ["example.com", "me@bastion.example"] }
```

`Include` directives inside the ssh config are not followed. An IPv6
literal is not a palette token; give it a `Host` alias and Bayan will pass
that alias to `ssh.exe`.

The multiplexer is off unless config says so:

```json
{ "mux": { "enabled": true } }
```

With it on, «فتح SSH» asks a per-user server (`bayan --mux-server`) to own
the ConPTY. The window talks to that process over a named pipe. The pipe's
DACL allows only the current user's SID, the first instance is created with
`FILE_FLAG_FIRST_PIPE_INSTANCE`, and the mode includes
`PIPE_REJECT_REMOTE_CLIENTS`. The client opens the pipe with identification
QoS and accepts it only when the owner and the server process are that same
SID, so another account cannot sit on the name and read keystrokes. An
elevated window does not start the server and does not connect: the pipe
name is only the user SID, and a normal process of that user must not be
able to ask an admin server to spawn a program. Several windows can connect
at once; each gets its own pipe instance. «فصل الجلسة» drops the tab and
leaves the session running in the server. «إعادة الاتصال» lists those
sessions and opens one again. The replay is capped at 256 KiB, starts on a
UTF-8 and escape-sequence boundary, and device queries (`CSI 6n`, `CSI c`,
OSC `?`) are removed so they are not answered into the live session.
«إنهاء الجلسة» kills one. Closing the window kills sessions that are still
attached and leaves the server running while any detached session remains.
The server process exits when the last session ends, and a child exit keeps
the process's status code.

A shell Bayan started itself (PowerShell, cmd, WSL) stays in the window
process, so the existing prompt injection is unchanged. Moving that ConPTY
into the server with `DuplicateHandle` is not done. Scrollback saved to
disk is a different item and is not part of this.

## Settings

Everything lives in the in-app panel (Ctrl+, or the tab-bar gear); the
shortcuts editor is the الاختصارات button in its header. State persists to
`~/.bayan/config.json` — optional, human-editable, BOM-tolerant, and every
key falls back to a sane default:

```json
{
  "theme": "system", "font_family": "Cascadia Code NF", "font_size": 15.0,
  "cursor_style": "bar", "cursor_blink": true, "scrollback": 20000,
  "copy_on_select": true, "ligatures": true, "bell": "attention",
  "shell": "pwsh.exe", "shell_integration": true, "padding": 8, "opacity": 0.9,
  "hide_single_tab": true, "confirm_close": true, "osc52": "off",
  "follow_system_text_size": true, "terminal_follow_high_contrast": true,
  "editor": "code -g {file}:{line}:{col}", "tab_switch": "recent",
  "keybinds": { "new-tab": "ctrl+shift+n" },
  "keyboard": { "kitty_protocol": false, "kitty_base_layout_fallback": true },
  "scroll": { "smooth": true },
  "scrollbar": { "position": "right", "width": 8, "autohide": true }
}
```

`"theme"` is `"system"`, `"dark"`, `"light"`, or a named scheme such as
`"Dracula"`. `"system"` (and leaving the key out) follows the Windows app
theme. `"dark"` and `"light"` stay on that pair. A named scheme, or a
hand-set `bg`/`fg`/`palette`, wins over a later system change. The settings
row «سمة النظام» is the same choice; picking a gallery tile names that scheme.

High Contrast and the Windows text-size slider are honored live, without a
restart. While High Contrast is on, the tab bar, title bar, command palette,
cards, scrollbar, focus rings, selection and cursor take the system colors
(`COLOR_WINDOW`, `COLOR_WINDOWTEXT`, `COLOR_HIGHLIGHT`, `COLOR_HIGHLIGHTTEXT`,
`COLOR_HOTLIGHT`, `COLOR_GRAYTEXT`, `COLOR_BTNFACE`). The terminal grid's
background, foreground, selection and cursor follow those colors too, unless
`"terminal_follow_high_contrast"` is false — then the theme stays inside the
grid and the chrome still follows. The 16 ANSI colors stay on the theme either
way, so a program's red is still red.

`"follow_system_text_size"` (default true) multiplies UI chrome and the
terminal font by Accessibility text size, a percent from 100 to 225 read from
`HKCU\Software\Microsoft\Accessibility\TextScaleFactor` (the value
`UISettings.TextScaleFactor` persists; Bayan does not link WinRT). An explicit
`font_size` is the size at 100% and is still multiplied. DPI is a separate
multiplier applied after that: pixels = points × text scale × dpi. Set the
option to false to keep the point size absolute; DPI still applies. Ctrl+wheel
changes the 100% point size, and the text scale multiplies that too. A change
to either setting is re-read on `WM_SETTINGCHANGE` / `WM_SYSCOLORCHANGE`.

`"osc_notify": true` lets programs raise desktop toasts through OSC 9,
OSC 777 and OSC 99. It is off by default because any output can carry
those sequences (an ssh host, a `cat`'d file, an install script). When on,
the toast's title is always «بيان · tab» and the program's text is cleaned
and capped. Bayan's own toasts (a long command finished, the bell) follow
`notifications` and need no opt-in.

`"shell_integration"` defaults to **on** (leave the key out and it stays
on). `false` starts new panes with no OSC 133 injection. It applies to
PowerShell, Git Bash and MSYS `bash.exe` (not `git-bash.exe`, which is a
GUI launcher), WSL bash and zsh, and cmd. `bash -c` and `bash script.sh`
are not injected. An unknown program — fish, nushell, dash, `sh`, a
Windows `zsh.exe` — is never injected, even when the key is on. Bash is
started with a temporary `--rcfile` (`--login` is dropped, because bash
would ignore the rcfile). A login shell then reads `/etc/profile` and the
first of `~/.bash_profile`, `~/.bash_login`, `~/.profile`, and does not
also read `~/.bashrc` — that is bash's own rule, and Ubuntu's profile
already sources it. A non-login shell reads `~/.bashrc` only. The user's
`PROMPT_COMMAND` is kept; `$?` is restored immediately before it runs, so
starship and oh-my-posh still see a failed command. `PS0` emits `C` as a
raw OSC (not `\[ \]`, which bash would print as SOH/STX). Nothing is
written into the user's home. WSL reads the distro's login shell from
passwd (`getent`, or `/etc/passwd` when getent is absent). Bash gets that
rcfile. zsh gets a `ZDOTDIR` only while its startup files load, then the
user's `ZDOTDIR` is restored (or unset, if it was unset) so history and
tmux keep the user's directory. fish, dash, or any other default shell is
exec'd with no rcfile. Windows paths become Linux paths through `WSLENV`
(`/up`) and, if the value still looks like `C:\…`, `wslpath -u`. OSC 7
percent-encodes the path as UTF-8 bytes, including spaces and Arabic, and
`cygpath`/`wslpath` run once per directory, a directory that does not
convert included. cmd's `PROMPT` keeps the user's own text and adds `$E`
marks for `A` and `B` only. cmd has no hook that runs when a command
starts, and no way to report `ERRORLEVEL` as that command's exit, so the
gutter stays unknown and a `D` mark is not invented. The nonce travels in
`BAYAN_PROMPT_NONCE` and is removed before a child inherits it. cmd never
receives it: it would have to live in `PROMPT`, which every child
inherits, so cmd's marks carry no nonce and are not trusted. An
elevated token (Run as administrator) does not source user rc files, sets
`INPUTRC` and `HISTFILE` to `/dev/null` so `~/.inputrc` cannot run a
macro. Elevated Git Bash is started with `--noprofile` before `-i` and
does not read `/etc/profile`: that file runs `git-prompt.sh`, which
executes `~/.config/git/git-prompt.sh` and `~/bash_completion.d/*.bash` as
administrator. `HOME` is an empty directory in the integration folder
until the rcfile restores it, including a non-login `bash.exe -i`.
`PATH` and `MSYSTEM` come from the Git
install, preferring `/ucrt64` (Git for Windows 2.56) over `/mingw64`.
Git's own prompt is kept. Elevated WSL bash still reads the distro's
`/etc/profile`, with `HOME` pointed at the rcfile directory for that
read. Inherited `XDG_DATA_DIRS`, `XDG_CONFIG_HOME`,
`BASH_COMPLETION_USER_FILE` and `BASH_COMPLETION_COMPAT_DIR` are cleared
first. After `HOME` returns, `BASH_COMPLETION_USER_DIR` stays on the
integration directory so Tab does not source the user's completions. Elevated zsh ends `.zlogin` with
`unsetopt rcs`, so exit does not read `~/.zlogout`. oh-my-posh is not inited from the user
profile. A failed elevation query is treated as elevated. Elevated scripts
are created under the real ProgramData folder
(`SHGetKnownFolderPath(FOLDERID_ProgramData)`, not the `PROGRAMDATA`
environment variable) with a random name, a protected DACL for SYSTEM and
Administrators, and a high integrity label. A normal pane uses the user
temp directory with a DACL for the current user only. The directory is
removed when the pane closes, and a later launch deletes `bayan-si-*`
directories older than 12 hours that this user owns. A name that is not
one of those random directories is not deleted.

`scrollbar.position` is `right` (the default, in Arabic and in English),
`left`, or `hidden`. `scrollbar.width` is the thickness in logical pixels,
clamped to 2–24, default 8, then multiplied by the display scale. `padding`
scales the same way, so at 150% an 8px bar (12 physical pixels) still fits
in the default padding (15) and is not clipped to an unscaled 10. The drawn
width is still capped by that physical padding, and padding 0 hides the
outer bar. `scrollbar.autohide` (default on) drops the bar one second after
the last scroll or hover. The bar is drawn in the window padding, or in the
side-by-side seam, never on a grid cell, so the last column stays visible
and clickable. The resize grip on the bar's side is 2–6px and never wider
than the old edge. Corners and that grip resize the window, including the
top 6px of the caption buttons and the outer pixels of the close button;
the pointer is the resize arrow there, because it is chosen from the same
decision as the click. A scrollbar track beats a side grip, so the thumb
drags instead. The ConPTY column count is the grid width divided by the
cell width. Padding grows with the display scale, so at 150% and 200% the
grid is narrower and a pane can lose a column even with the bar hidden. A
side-by-side split widens the seam to the bar, which can cost one more
column per pane: at 1280px with 11px cells the two panes are 56 and 56,
where a 1px seam was 57 and 57. It is hidden with no scrollback
and on the alternate screen. The thumb is at least 16px, or the bar's
width when that is wider. The thumb follows smooth scroll's sub-line remainder.

`config.json` is watched while Bayan is running. A change is re-read
after about 200 ms of quiet, including when an editor saves by renaming
a temp file over the real one. Theme, colors, font family and size (the
grid is laid out again and the PTY is resized), key bindings, padding,
cursor, opacity, and the other live settings apply without a restart.
A save from the settings panel is recognized as Bayan's own write, so
it is not loaded again. A file that does not parse is left alone: the
last good settings stay in effect and a small notice shows the error.
A read that fails because another program holds the file is retried
for about 150 ms first. Deleting or renaming the file keeps the running
settings (not defaults), so a later panel save writes them back.
Scrollback applies to new panes, the shell to new tabs, restoring tabs
on the next launch, the plugin list after a restart, and the kitty
keyboard protocol in new panes. When one of those changes, the notice
names it. `osc52` is config-file
only (there is no settings-panel row). Leave it out or set `"off"`.
`"copy"` lets a focused window accept OSC 52 clipboard writes from the
focused pane; no value enables a clipboard read. The decode backstop is
64 KiB, and one scan sees about 48–51 KiB.

## Inline images

Sixel (`DCS q`), Kitty graphics (`APC _G`) and iTerm2 (`OSC 1337 ; File=`
with `inline=1`) are placed on the grid cell under the cursor. They scroll
with that row, leave when the row leaves the scrollback, and clear with
ED 2, ED 3, and RIS. EL and a partial ED leave Kitty graphics in place.
A resize drops them only when the column count changes, which is when rows
reflow; switching tabs does not. The alt screen has its own placements; leaving it
drops those and shows the primary screen's images again. Rows that also
contain Arabic still take the whole-line shaper. Image pixels are never
reversed on an RTL line. Where to anchor the quad in an RTL paragraph is
left for a later decision; this build keeps the image on the cursor cell.

```json
{
  "images": {
    "enabled": true,
    "max_bytes": 20971520,
    "max_width": 4096,
    "max_height": 4096,
    "max_pane_bytes": 67108864,
    "max_session_bytes": 134217728
  }
}
```

`enabled` defaults to true. Over-cap or malformed input is dropped.
Kitty `t=f`, `t=t` and `t=s` (file, temp file, shared memory) are always
rejected: nothing in this path opens a local path, and there is no setting
that turns them on. Direct transmission (`t=d`) accepts raw RGB (`f=24`),
RGBA (`f=32`) and PNG (`f=100`), including chunked `m=1` / `m=0`. iTerm2
payloads are PNG.

ConPTY is created **without** `PSEUDOCONSOLE_PASSTHROUGH_MODE` (`0x8`).
The flags are inherit-cursor, the resize quirk, and Win32 input mode
(`vendor/portable-pty/src/win/psuedocon.rs`). In that mode conhost parses
DCS and APC and does not forward them, so a real Windows console host will
not deliver Sixel or Kitty graphics. OSC is forwarded, which is why OSC 7,
9, 52 and 133 already work, so an iTerm2 `OSC 1337` is not stripped the way
DCS and APC are. A 96×48 PNG (224 base64 bytes) printed with `cmd /c type`
arrived intact through conhost on Windows 11 26200; larger payloads against
conhost's OSC limits have not been checked (see wezterm#3624 and Rio#1764).

**The image never moves the cursor.** The first build reserved the image's
rows the way the protocols ask (`ESC D` per row). Under ConPTY that was
wrong, and live: conhost does not know the image, keeps its own cursor, and
repaints from its own buffer with absolute positions (captured after a
resize: `ESC[H` and every row with `ESC[K`). Bayan's rows then disagreed
with conhost's by the image's height, and the line printed after the image
disappeared. Now the image stays on the rows it arrived on and the text
conhost prints next draws over it (images are drawn first, and a cell with
the default background draws nothing). The cost: conhost's repaint clears
lines with EL, and EL clears an iTerm or Sixel image it touches, so a resize
repaint or `cls` (also all EL) removes the image. Reserving rows is worth
revisiting only with `PSEUDOCONSOLE_PASSTHROUGH_MODE`.

PNG payloads (iTerm2 and Kitty `f=100`, chunked uploads included) and Sixel
are decoded on the reader thread before the terminal lock is taken. For a
chunked Kitty upload the reader keeps its own copy of the base64 until the
last chunk; the store uses that early decode only if its own assembled
base64 is identical. The image filter copies text and CSI runs whole and
steps byte by byte only through `ESC P`, `ESC _` and `ESC ]`: SGR-heavy
output through `process_chunk` went from 76 to 82 MB/s (main: 88).


## Lua config and executable plugins

`~/.bayan/config.lua`, next to `config.json`, is optional. Bayan runs it
only after the confirm card has approved that file's SHA-256. The script
receives the JSON config as the global `config` and may change it or
return a table; either way the result is merged over the JSON. On a
syntax or runtime error Bayan logs `~/.bayan/lua.log`, shows the message,
and keeps the JSON. The three keys `lua_approvals`, `lua_plugins`, and
`lua_plugins_dir` stay as they are in config.json.

Executable plugins are `<name>.lua` files in the directory
`lua_plugins_dir` names (absolute, or relative to the config directory).
Nothing in that directory runs until `lua_plugins` lists the stem, and
each file still needs its own hash on the confirm card. A changed file
is a new hash. There is no default directory and no default allowlist.

```json
{
  "lua_plugins_dir": "plugins",
  "lua_plugins": [ { "name": "hello", "spawn": false } ]
}
```

The plugin API is `bayan.bind(chord, fn)`, `bayan.on(event, fn)`, and
`bayan.send(text)`. `bayan.spawn(program, args)` exists only when that
entry's `spawn` is true, and `config.lua` never has it. `send` waits on
the same confirm card as a JSON plugin action. Events are `tab-opened`,
`title-changed`, `bell`, and `osc7-cwd`. Opening a tab dispatches
`tab-opened`. Bytes from the pane — escape sequences, OSC, hyperlinks,
bells, program output — do not enter Lua. One event accepts 32 handlers.
Every handler of one dispatch shares a single deadline.

The sandbox is Lua 5.4 via mlua, with `string`, `table`, `math`, and
`utf8` only. `io`, `os`, `debug`, and `package` are absent. `require`,
`loadfile`, `dofile`, and `string.dump` are removed, and `load` accepts
text chunks only. A metatable with `__gc` is rejected, because Lua 5.4
runs finalizers with the instruction hook switched off. `<close>` is
rejected too: after the budget aborts a call, the `__close` handler
runs with that hook off, and setting `__close` after `setmetatable`
still arms it. Every chunk, including one passed to `load`, is refused
if its bytecode contains `OP_TBC`. The hook also
cannot interrupt one C call, so `string.find`, `match`, `gmatch`, and
`gsub` first bound the matcher's worst case for that subject and pattern
(start positions times the ways the subject splits among the `*` `+` `-`
`?` items, times the pattern length, with `%b` and back-references
rescanning) and refuse a call over 10^7 steps. The first fix here capped
subject and pattern length and counted `.-` chains; `("a"):rep(1000)
:find("a*a*a*b")` passed all three and had not returned after 60 s. Measured in
a debug build, the costliest admitted calls take about 5 ms. These calls,
`string.rep`, and `table.move` also check the deadline themselves, since
a loop body calling one is only a few instructions and the hook runs every
thousand. `string.rep` refuses a result over 1 MiB and answers an empty
one directly: `string.rep("", math.maxinteger)` copies zero bytes 2^63
times, so the memory limit never stops it. `table.move` refuses more than
2^20 elements, because moving nils allocates nothing either. config.lua's
tables become JSON by Rust recursion outside the hook, so a table that
contains itself, nesting past 32 levels, or more than 100,000 values in
all is a config.lua error instead of a stack overflow on every launch.
Each call into Lua gets its
own instruction budget and its own short deadline, so one callback cannot
spend the next one's time. A loop is still cut off. Strings are UTF-8;
Arabic round-trips.

`bayan.spawn` returns `""` immediately. Stdout is discarded, and the
process is killed after two seconds, so it is not a way to open an
editor. At most four of these processes run at once. On Windows the
process is created suspended (`CREATE_SUSPENDED | CREATE_NO_WINDOW`),
placed in a job, and only then resumed, so it cannot outlive the job.
The values `config.lua` changes are not written back to `config.json`,
and editing a shortcut writes only that one action. A script longer than
the card shows its size and hash first. Enter reveals the rest, the card
scrolls, and a second Enter can approve only after a short pause and
only once the last row has been on screen.

An elevated token does not run any of it. That is `--elevated` after the
UAC prompt and Explorer's Run as administrator: the process token, not a
flag Bayan has to be the one to set. `config.lua`, executable plugins, and
the declarative JSON plugins are left unread. The window keeps the plain
JSON settings. When a `config.lua` or a plugin is actually present, the
window shows «سكربتات لوا والإضافات متوقفة لأن النافذة تعمل كمسؤول.»

## BiDi controls and the current directory

Two escape sequences from ECMA-48 / the terminal-wg "BiDi in Terminal
Emulators" recommendation, which `alacritty_terminal` 0.26 does not act on
(mode 8 is an unknown SM/RM parameter; `CSI Ps SP k` is not in its CSI
dispatch). Bayan strips them from the PTY stream before the parser, including
when a sequence is split across reads.

- **BDSM** — `CSI 8 h` implicit (Bayan applies UAX#9; this is the initial
  state, so a shell that never sends BDSM still gets Arabic), `CSI 8 l`
  explicit (the application already ordered the text, so that pane is not
  reordered and Claude-mode restoration stays off).
- **SCP** — `CSI 1 SP k` LTR, `CSI 2 SP k` RTL, `CSI 0 SP k` or `CSI SP k`
  back to the default (first strong character). The direction applies to the
  current line when the cursor is in column 0, and to the lines after it.
- Both apply only while a command is running or the pane is on the alternate
  screen, so `cat` of a file that contains them cannot restyle the prompt.
  OSC 133 A and D reset them as well. They also reset on RIS (`ESC c`,
  including `ESC ESC c`, `ESC` plus a C0 byte then `c`, and `ESC` plus a
  byte >= 0x80 then `c`) and when a `?1049` / `?47` / `?1047` CSI is seen,
  including a colon subparameter such as `?1049:0`, even if a second one in
  the same read toggles the mode back. A resize drops SCP stamps when the
  column count changes (rows reflow); a row-only resize and a same-size
  resize leave them. A BDSM or
  SCP that ends an open OSC is stripped only after that OSC is closed, so
  the text after it is not swallowed.

`bidi.honor_bdsm` defaults to **true**, so explicit mode overrides Bayan's
auto-detection. Set it false to keep that detection when an application
sends `CSI 8 l`:

```json
{ "bidi": { "honor_bdsm": false } }
```

`bidi.mode` is mintty's `Bidi=2/1/0`. `"full"` (2, the default) is the
behavior above. `"alt_off"` (1) does not reorder while the program has
entered the alternate screen with `?1049`, `?1047`, or `?47`, including
when that CSI is split across reads, and reorders again after the reset.
Only `?1049` changes alacritty's screen buffer. `?47` and `?1047` are
tracked beside it for this decision and are not rewritten into `?1049`.
`"off"` (0) does not reorder anywhere, so an agent that already ordered
Arabic is not reversed a second time; Arabic shaping is unchanged. An
explicit BDSM still disables reordering on top of `full` and of `alt_off`
on the primary screen when `honor_bdsm` is true. Ctrl+Shift+F3
(rebindable, `bidi-toggle`; bare F3 stays with htop and mc) cycles the focused pane `full → alt_off → off`; other panes
stay as they are, and the corner chip reads `BiDi 1` or `BiDi 0` until the
pane is back on `full`. An RTL paragraph (SCP 2, or auto when the first
strong character is Arabic) is drawn flush with the pane's right edge on
the primary screen. LTR paragraphs, TUI fields, and the alternate screen
stay on the left.

```json
{ "bidi": { "mode": "alt_off" } }
```

**OSC 7** (`ESC ] 7 ; file://host/path ST`, BEL or `ESC \`) stores the pane's
working directory next to the existing OSC 9;9 report. Only the `file` scheme
is accepted. The path is percent-decoded, `/C:/x` becomes `C:\x`, and the host
must be empty, `localhost`, a loopback address, or this machine's name. Remote
hosts, UNC (`//server/share`, `\\?\`, `\\.\`, `\??\`), control characters, bidi
overrides, and payloads over 4096 bytes are rejected and do not replace a
directory already stored. A new tab or split inherits the directory only when
it exists. `SessionMeta::reported_cwd` is what an OSC 8 resolver should join a
relative `file:line` path against.

After percent-decoding, the path goes through `accept_local_cwd` — the same
gate as OSC 9;9 and `session.json`. UNC, `\\?\`, `\\.\`, `\??\`, a comma or
a quote, a `\\` or `//` anywhere, `..` that climbs past the drive, DOS
device names (`CON`, `NUL`, `PRN`, `AUX`, `CONIN$`, `CONOUT$`, `CLOCK$`,
`COM0`–`COM9`, `LPT0`–`LPT9`, including a superscript digit and a space as
in `CON .txt`), alternate data streams, and bidi controls are rejected
there, before any probe. A report is stored only when no command is running, Enter is not still
pending, and the pane is not on the alt screen. Enter at the prompt sets
`cmd_pending` even when the cursor never moves past the prompt column
(paste and Enter in one chunk, Home then Enter, a wrapped command), and
only a nonce-verified OSC 133 A or D clears it. While it is set, OSC 7
needs the nonce the same way it does while a command is running.
PowerShell's wrapper sends OSC 133 D and A with a per-session nonce, and
only that A arms the report, so a spoofed `133;D` from `cat` cannot. Bash
and zsh emit the same D/A/B bytes (and a C from `PS0` / `preexec`, which
the parser does not treat as a prompt boundary), plus OSC 9;9 and OSC 7
when the directory converts to a Windows path. cmd emits A and B with ST
(`ESC \`), no exit code and no nonce, so they arm nothing. An idle OSC 7
with no trusted 133 (clink in cmd) is still stored. A new tab calls
`GetDriveTypeW` on the drive root and rejects a remote drive, and it
rejects a reparse point without following it, before `is_dir`.

PowerShell gets OSC 7 from the prompt wrapper Bayan already injects (it also
still emits OSC 9;9). cmd does not, so a clink prompt filter does the same
report. Both use an empty host (`file:///C:/…`):

```lua
-- %LOCALAPPDATA%\clink\bayan_osc7.lua
local osc7 = clink.promptfilter(100)

local function encode(path)
  local out = {}
  for i = 1, #path do
    local b = path:byte(i)
    local plain = b == 47 or b == 58
      or (b >= 48 and b <= 57)
      or (b >= 65 and b <= 90)
      or (b >= 97 and b <= 122)
      or b == 45 or b == 46 or b == 95 or b == 126
    out[#out + 1] = plain and string.char(b) or string.format("%%%02X", b)
  end
  return table.concat(out)
end

function osc7:filter(prompt)
  local cwd = (clink.get_cwd and clink.get_cwd() or os.getcwd()):gsub("\\", "/")
  if cwd:match("^%a:") then cwd = "/" .. cwd end
  return "\027]7;file://" .. encode(cwd) .. "\007" .. prompt
end
```

## Build

Bayan now starts with one fresh tab. To opt into reopening saved tabs and panes,
set `"restore_tabs": true` in config. Window position and size still persist.

`bayan -d <folder>` (or `--cwd <folder>`, `--cwd=<folder>`) opens the first tab
in that folder; a relative one is taken from the current folder. It goes through
the same gate as every start folder, and a UNC path, a mapped network drive or a
missing folder stops Bayan with an error instead of opening somewhere else. With
`restore_tabs` on, the saved tabs come back and the folder opens as the active
extra tab. `-p <name>` (or `--profile`) starts that tab in a detected shell
profile, by its name or its command (`PowerShell`, `PowerShell سريع`,
`cmd.exe`). `-- <command>...` runs that program in the tab instead.
`--help` and `--version` print and exit. An unknown argument is an error.

`bayan --register-context-menu` adds **افتح بيان هنا** for the current user
under `HKCU\Software\Classes\Directory\shell\Bayan` and
`Directory\Background\shell\Bayan`, with the command `"<exe>" -d "%V\."`.
The `\.` keeps a drive root from arriving as `C:"`.
`--unregister-context-menu` removes both. This does not write HKLM and does
not ask for an administrator. Windows 11's top-level menu needs package
identity (MSIX, later); until then the entry is under **Show more options**.
`"explorer_context_menu": true` in `config.json` is the installer hook for
the same registration (absent leaves the menu alone; there is no settings row).

The taskbar Jump List lists each detected shell as `"<exe>" -p <name>` (the
profile's name, not its command line), then **نافذة جديدة** (`--new-window`)
and **تشغيل كمسؤول** (`--elevated`). The process sets an explicit
AppUserModelID (`Bayan.Terminal.` plus a hash of the exe path) before the
window exists, and rewrites the list on startup when the detected profiles
differ from the list it last committed. The remembered list is only that
comparison. It is not what gets written back.

`--elevated` is the only way Bayan asks for an administrator. It calls
`ShellExecuteW` with the verb `runas` (the normal UAC prompt) and then exits.
The elevated process is given `--elevated` and, when a profile was named,
`-p <profile name>`. A folder and a command after `--` are refused, and a
profile name that is a path is refused. The elevated process resolves the
name against the shells it detects itself; a command string does not match.
It does not restore `session.json` and it does not start `config.json`'s
`shell` value, because both are files a standard user can write. The window
title and the tab strip show **مسؤول** while the token is elevated. Cancelling
UAC does not open a window. Bayan does not elevate on its own, and the
executable manifest stays `asInvoker`. An elevated window, including one
Explorer started with Run as administrator, does not load `~/.bayan/plugins`
and does not inject oh-my-posh. The prompt picker does not open in that
window, so it neither runs `oh-my-posh.exe` nor rewrites `$PROFILE`. It does
not use `config.json`'s `editor` command. Ctrl+click looks for `Code.exe`
only under Program Files and Program Files (x86), from the known-folder
API, not from `PATH` or `%LOCALAPPDATA%`. If that copy is missing, a confirm
card shows the full path. Enter on that card reveals the file in Explorer
and does not open it by its file association. `explorer.exe` comes from
`GetWindowsDirectoryW`, not from `WINDIR`. A host card remembers whether
Enter browses or reveals, so `HTTPS://` still opens in the browser.
`pwsh.exe` is started only from
`Program Files\PowerShell\7`. `save_state` does not write `session.json`
when the token is elevated, including a rename or a group change, and a
settings change is not written either. The jump-list stamp is not written.
The window says so in Arabic when it opens, and a config-load error keeps
that notice on the same card.

`"terminal_handoff": true` (absent means off; the same switches are
`--register-terminal-handoff` and `--unregister-terminal-handoff`) registers
one per-user COM class,
`HKCU\Software\Classes\CLSID\{A7C3E1B2-4D58-4F0A-9E6C-1B8D5A2F7043}\LocalServer32`,
command `"<exe>" --handoff`. That is the terminal half of Windows Terminal's
handoff (`ITerminalHandoff`, `ITerminalHandoff2`, and `ITerminalHandoff3`).
It does **not** make Bayan the Windows 11 default terminal. The Settings
list is an AppExtension catalog
(`com.microsoft.windows.console.host` and `com.microsoft.windows.terminal.host`
in the same MSIX package). Inbox conhost activates `DelegationConsole` as
`IConsoleHandoff`, which is OpenConsole, and only that process calls
`ITerminalHandoff`, through `OpenConsoleProxy.dll`. Bayan ships neither, and
it does not write `HKCU\Console\%%Startup`. `--handoff` registers the class
and every `EstablishPtyHandoff` returns `E_NOTIMPL`: no shell is started and
no handle is kept. A working default terminal still needs the MSIX package,
OpenConsole, and the proxy DLL.

The default terminal font is 18; settings and tab controls use independent UI
metrics so a small terminal font does not shrink the controls. Ctrl+V pastes in
shells and TUIs; Ctrl+C copies a selection or sends the interrupt when no text
is selected. Explicit key bindings take precedence.

Interactive Claude startup requests one redraw after its header or resumed input
frame appears, removing preceding shell text while retaining the conversation.
PowerShell examples inside Claude replies remain part of the conversation.
Wrapped launch commands
are detected. Arabic fields, selections and Claude's inverse-video cursor share
the shaped source-cell map; earlier shell scrollback retains its own text order.
An Arabic sentence surrounding a long English phrase keeps its Arabic base
direction even when the English phrase contains more letters.

Press **Ctrl+Shift+P** to open the command palette:

The palette has a responsive card, a separate search field and readable rows.
Click a row to choose it, use the mouse wheel or arrow keys to navigate, and
click outside or press Esc to dismiss. Pointer input stays inside the palette.
Its text keeps its own UI size when the terminal is zoomed.

- Search `تبويب جديد` or a shell name to open that installed shell in a new tab
  without changing the default shell or interrupting the current tab.
- Search `claude` to insert a start or continue command. Confirm the paste card
  to insert it, then press Enter to run it.
- Search `نسخ` to copy a visible link or file path with the keyboard. The list
  is a snapshot of the active pane, deduplicated and limited to 100 targets.
  Selecting a copy entry only writes the clipboard; it does not open the link.

Custom palette commands remain available through declarative plugins in
`~/.bayan/plugins/`. For example, a JSON plugin can provide
`{"name":"My commands","actions":[{"label":"Claude Code","send":"claude"}]}`.

Use **Ctrl+=** or **Ctrl+plus** to enlarge terminal text, **Ctrl+-** to reduce it,
and **Ctrl+0** to return to your configured size. Numpad plus/minus work too.
Ctrl+Shift+Minus remains available to the running program for undo.

**Ctrl+F** starts a literal search with the current single-line selection,
including Arabic selected inside Claude. Opening it keeps the viewport where
it is; press Enter to search or Esc to close. Multiline or large selections
start with an empty search field.

```
cargo run --release
```

Requires Rust (MSVC toolchain on Windows). The Amiri font (SIL OFL, see
`fonts/OFL.txt`) is bundled so connected Arabic works from a fresh clone,
and Kawkab Mono so a monospaced Arabic font is always on offer.
