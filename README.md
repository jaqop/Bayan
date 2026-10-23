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
      still font zoom. The suite size after this merged with main is
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
  "hide_single_tab": true, "confirm_close": true, "osc52": "off",
  "editor": "code -g {file}:{line}:{col}",
  "keybinds": { "new-tab": "ctrl+shift+n" },
  "keyboard": { "kitty_protocol": false, "kitty_base_layout_fallback": true }
}
```

Shell and scrollback apply to new tabs (no terminal restarts a live
session); everything else applies immediately. `osc52` is config-file
only (there is no settings-panel row). Leave it out or set `"off"`.
`"copy"` lets a focused window accept OSC 52 clipboard writes from the
focused pane; no value enables a clipboard read. The decode backstop is
64 KiB, and one scan sees about 48–51 KiB.

## Build

```
cargo run --release
```

Requires Rust (MSVC toolchain on Windows). The Amiri font (SIL OFL, see
`fonts/OFL.txt`) is bundled so connected Arabic works from a fresh clone.
