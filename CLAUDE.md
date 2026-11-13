# Bayan (بيان) — working notes for Claude

An Arabic-first, agent-ready terminal emulator in Rust, for Windows. Rust
successor to [EasyTer](https://github.com/jaqop/EasyTer) (Python/Qt), which
remains the behavioral specification: checked out alongside this repo at
`../EasyTer`, its `dev/test_input_ux.py` and friends define what the keyboard,
selection and BiDi paths must do.

## Layout

| File | Role |
|---|---|
| `main.rs` | event loop, tabs, panes, settings panel, cockpit, OSC handling; `window_event` only dispatches |
| `chrome.rs` | window chrome hit-testing (edges, caption buttons, tab bar, dividers, drag strip) and the left-press order |
| `input.rs` | pointer routing: buttons, motion, wheel, drops, and the pointer side of the close guard and palette |
| `scroll.rs` | smooth scrollback: sub-line pixel offset, wheel easing, and when that offset is cleared |
| `keyroute.rs` | keyboard routing: which overlay owns a press, then the window chords before `keys.rs` encodes it |
| `render.rs` | the dual text engine + shaped-run cache (see below) |
| `gpu.rs` | wgpu: one pipeline, one 2048² atlas, one draw call per frame |
| `term.rs` | ConPTY via `portable-pty`, VT via `alacritty_terminal` |
| `bidi.rs` | Claude mode: visual → logical BiDi restoration |
| `bidi_ctl.rs` | BDSM / SCP (terminal-wg BiDi controls), filtered out of the PTY stream before the parser |
| `link.rs` | hyperlinks: OSC 8 spans, detected URLs and `path:line`, the narrow open policy |
| `osc7.rs` | OSC 7 cwd reports, parsed then gated by `term::accept_local_cwd` |
| `mouse.rs` | mouse tracking modes 1000/1002/1003, SGR vs X10, focus events (1004), who owns a right-click (menu, paste, program) |
| `keybinds.rs` | action registry + chord map |
| `copymode.rs` | copy mode: keys → alacritty vi-mode motions and selections; horizontal keys swap on RTL cells |
| `lang.rs` | interface language (Arabic default, English): `tr("عربي", "English")` pairs at the point of use, `rtl()`, `lead_x`/`trail_x` for which edge text starts from |
| `keys.rs` | keyboard → VT byte sequences (ported from EasyTer) |
| `kitty.rs` | kitty keyboard protocol encoder, layout-aware alternate keys |
| `layout.rs` | a tab's split tree: side-by-side and stacked splits nest in any mix; leaves index the tab's flat pane list. The side-by-side seam widens to hold a scrollbar |
| `groups.rs` | tab groups: a group id TAG on each tab (tabs never move in the flat list), the group order and labels, the stay-in-group close rule |
| `localpath.rs` | no-follow walk: is a drive path local in every component, links read but not followed (link probe, new-tab cwd) |
| `mru.rs` | most-recently-used tab order for Ctrl+Tab, walkable within one group |
| `shells.rs` | installed-shell detection (ported from EasyTer's `profiles.py`) |
| `cli.rs` | argv: `#45`'s `-d`/`--cwd`, plus profile, `--` command, `--elevated`, handoff, help, version |
| `context_menu.rs` | per-user Explorer "Open Bayan here" (HKCU only, behind `RegistryStore`) |
| `elevate.rs` | `--elevated` → `ShellExecuteW` verb `runas` only; argv is `--elevated` and optional `-p <name>` |
| `jumplist.rs` | taskbar Jump List (`ICustomDestinationList`) and the explicit AppUserModelID |
| `handoff.rs` | per-user `ITerminalHandoff` COM class, default off; not the Windows default terminal |
| `shellint.rs` | OSC 133 injection for Git Bash, WSL bash/zsh, and cmd (PowerShell's wrapper stays in `term.rs`) |
| `ssh.rs` | OpenSSH tab: System32 `ssh.exe`, `~/.ssh/config` hosts, injection-safe argv |
| `mux.rs` | opt-in per-user mux: named-pipe server, detach / reattach / kill |
| `posh.rs` | oh-my-posh theme picker with a live ANSI preview |
| `plugins.rs` | declarative JSON plugin host (`~/.bayan/plugins/`) |
| `lua_host.rs` | sandboxed `config.lua` and allowlisted executable plugins |
| `scrollbar.rs` | per-pane scrollbar in the padding or the split seam: thumb from viewport/total and `display_offset`, hit-tested only outside the grid |
| `config.rs` | `~/.bayan/config.json`, optional and BOM-tolerant. `scrollbar.position` (`right`/`left`/`hidden`), `scrollbar.width`, `scrollbar.autohide` |
| `config_watch.rs` | reload that file on change: debounce, rename-replace saves, keep the last good config on a parse error |
| `theme.rs` | Windows light/dark: `system`/`dark`/`light`, the palette pair, and the theme-change event |
| `a11y.rs` | High Contrast color mapping and the system text-size multiplier, pure and tested; the Win32 read sits behind `read_snapshot` |
| `toast.rs` | `Shell_NotifyIcon` balloons as native toasts, zero WinRT |
| `screen_reader.rs` | screen-reader tree: each pane is a document in logical order, with the caret, the selection, and a coalesced live region (AccessKit, which speaks UIA on Windows) |
| `conpty_load.rs` | test-only: the planted-`conpty.dll` probe, with vendored `conpty_resolve.rs` |
| `latency.rs` | key-to-frame latency samples for `BAYAN_PROFILE` runs (times only, never the key) |

845 tests across the tree. `render.rs` (105), `term.rs` (100) and `link.rs`
(66) carry the most; `lua_host.rs` has 46, `shellint.rs` 24, `images.rs` and
`localpath.rs` 21 each, `mux.rs` 15, `config_watch.rs` 14, `screen_reader.rs`
11 and `ssh.rs` 7. Three more in `term.rs` are `#[ignore]`d probes, run by
hand (see Measuring).

## Measuring

Measure before optimizing, in release builds:

- **Reader throughput and scrollback memory**, headless:
  `cargo test --release -- --ignored measure_ --nocapture --test-threads=1`.
  It prints MB/s through `process_chunk` (Bayan's whole per-read path) against
  the bare vte parser, for plain, colour-heavy and Arabic output, and private
  bytes once the scrollback is full and after as much again.
  `measure_reader_stages` times each pass `process_chunk` makes over a read on
  its own, so a slow one shows up by name.
- **Key-to-frame latency**, live: `cargo build --release --features dev-hooks`,
  then `scripts\measure-latency.ps1 -Exe <that exe>`. It types through
  SendInput (only while Bayan has the foreground) and summarises the
  `BAYAN_PROFILE` latency lines. It covers winit's key event to the PTY echo
  and to the GPU submit; the OS input queue and the display are outside it.

Figures from 2026-11-10 on the author's machine (22bc5d6 + this harness):
`process_chunk` 83 / 49 / 100 MB/s for plain / SGR / Arabic, which is 57 / 27 /
48% of vte alone; scrollback +33 MB at 10k lines and +303 MB at 100k, flat after
that; latency p50 1.4 ms key->frame (0.5 ms to the echo), p95 1.7 ms over 100
keys. A control run with no shell reading input recorded no samples.

After the reader fixes (Ground fast path and a reused CSI buffer in
`bidi_ctl`, a first-byte `find`, OSC scanners skipped on reads with no
`ESC ]`): `process_chunk` 117 / 88 / 151 MB/s, 79 / 51 / 73% of vte.

## Invariants — do not "simplify" these

**The text engine is deliberately dual.** Rows without Arabic render on a
strict grid (ASCII batched into column-pinned runs; icons, powerline, box
drawing, CJK per cell, compressed to fit). Rows with Arabic shape as whole
lines so cosmic-text applies UAX#9. Grid fidelity wins for TUIs; correct text
wins for prose. Unifying the two paths breaks one or the other — this is the
core design, not an accident.

**On RTL rows, logical column ≠ visual x.** Cursor, selection, and hyperlink
hit-testing must resolve through the shaped layout, never by multiplying a
column index by cell width.

**Hyperlinks never go through a shell.** OSC 8 is read off the cell
(`Cell::hyperlink`), not parsed again. Ctrl+click opens `http` and `https`
via `ShellExecuteW` with the URI as the single `lpFile`. A `file` URI
association-opens only an allowlisted document or media type (`.txt` `.md`
`.log` `.json` `.csv` `.pdf` and the image extensions in `link.rs`).
Everything else, including no extension, is revealed in Explorer. Source
files reach the editor only as `path:line`. UNC, `\\?\`, `\\.\`, `\??\`,
a colon after the drive letter, a comma, a quote, and `\\` anywhere are
rejected with `term::accept_local_cwd` / `unc_or_device` before any
`exists` call. Then `localpath::walk_live` must say `Local` before
anything stats the path: no `DRIVE_REMOTE`, and every symlink or junction
on the way resolved, without following it, to a local drive. Anything else
is refused (`RefuseReason::NotLocal`), never revealed — Explorer would
follow the link. Do not replace the walk with a check on the opened
file's final path: that open has already connected to the share.
An existing 8.3 name is expanded with `GetLongPathNameW`
and both checks run again; a `~` component that cannot be expanded is
revealed. `path:line` goes to the editor's `Command` argv, or to
`%WINDIR%\explorer.exe` with raw `/select,"<path>"` when no editor is
configured — never to the file's association. `{file}` `{line}` `{col}`
are substituted once, after the template is split, and the path is
absolute. A relative path with no pane cwd is refused. Userinfo
(`good.com@evil.com`) and a non-ASCII host show the real host (punycode
for IDN) and wait for the confirm card (`Enter يفتح`). The refuse card
says `Enter يغلق` and does not open. `javascript:`, `ms-*`, `data:`,
control characters, and bidi overrides are refused. `exists` and the open
run on the `bayan-link` thread, which initializes COM first. The window
asks `link::click_owner` before `mouse::Route::press`: Shift keeps local
selection and wins over Ctrl, a plain click while tracking is on belongs
to the program, and `overlay_blocks_mouse` (including the link card)
blocks the click. HIDDEN cells are not clickable. Link spans are cached
per row and dropped on PTY damage, scroll, and relayout; the hover is
cleared on that wakeup too.

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

**Smooth scroll is a pixel offset on the primary screen only.** `scroll.smooth`
defaults on. A touchpad fraction shifts the viewport and draws one extra
history row, clipped to the pane; a wheel notch eases in. `false` is the
old whole-line wheel. Alt-screen wheels stay arrows, and mouse reports use
the shifted row (the history sliver above the viewport is not row 0). A
stale idle fraction is cleared when output arrives at the bottom. A wheel
notch still easing, or a touch that moved within the last easing window,
is kept — continuous output must not swallow the scroll. An alt-screen
transition, a resize, and a font change clear it too. Copy-mode keys snap
it onto a whole line first.

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

**Elevation is a UAC prompt and nothing else.** `--elevated` calls
`ShellExecuteW` with verb `runas` and exits. The new process receives
`--elevated` and, if a profile was named, `-p <name>` only. It resolves
that name with `shells::find_profile_by_name` (the stored command does not
match). It does not restore `session.json` and does not spawn
`config.shell`. It does not load `~/.bayan/plugins`, does not inject oh-my-posh, and does
not open the prompt picker. It does not use `config.json`'s `editor`
template. `Code.exe` comes only from Program Files via
`SHGetKnownFolderPath`. A missing editor is a reveal-only card: Enter runs
`explorer.exe /select` and does not association-open. `explorer.exe` comes
from `GetWindowsDirectoryW`, not `WINDIR`. A host card stores Browse or
RevealOnly, so `HTTPS://` still opens in the browser. `pwsh.exe` is only
`Program Files\PowerShell\7\pwsh.exe`. `save_state` does not write
`session.json` while elevated. A failed elevation query is treated as
elevated. No environment block, registry value,
pipe, or file carries the request. The jump list's admin task is
`--elevated` with no profile.

**Shell integration is opt-out and shell-specific.** PowerShell's prompt
wrapper stays in `term.rs`. Git Bash, MSYS bash, WSL bash and WSL zsh
are built in `shellint.rs` and produce the same nonce-checked OSC 133 A/B/D
bytes. cmd wraps the user's `PROMPT` in A and B with **no nonce**: every
child of cmd inherits `PROMPT`, so a nonce there could be read and used to
forge trusted marks. Its marks are untrusted, like any forged mark. Never
give cmd the nonce in any form. `git-bash.exe`, `bash -c`, and
`bash script.sh` are not injected. An unknown program is never injected.
`$?` is restored before the user's `PROMPT_COMMAND`. A login shell sources
`/etc/profile` and one user profile, not `~/.bashrc` as well. `"shell_integration": false` skips all of them. The nonce is
`BAYAN_PROMPT_NONCE` and is unset before a child inherits it. Do not write
the user's rc files. An elevated token must not source `~/.bashrc`,
`~/.bash_profile`, `~/.inputrc`, or the zsh home files: set `INPUTRC` and
`HISTFILE` to `/dev/null`. Elevated Git Bash is `--noprofile` and must
not source `/etc/profile` (its `git-prompt.sh` runs user files); set
`PATH` and `MSYSTEM` from the bash install. Elevated WSL bash still reads the
distro profile with `HOME` pointed at the rcfile directory. A failed
elevation query is treated as elevated. `shellint` takes the `elevated` flag `Session::spawn` is given, which is
`elevate::process_is_elevated`, the one token query in the tree.
The script directory comes from `SHGetKnownFolderPath(FOLDERID_ProgramData)`
when elevated, never from the `PROGRAMDATA` environment variable. It is a
random name with a DACL that is not world-writable, and zsh's `ZDOTDIR` is
restored to the user's value before the first prompt. Only a directory
whose name we minted is deleted.

**Start-directory precedence, in order:** a restored session's saved cwd, then
the launcher's directory (a shortcut's "Start in"), then `USERPROFILE`. The
session wins deliberately — restoring a tab in the wrong folder is worse than
ignoring a shortcut — but it is also why a shortcut change appears to do
nothing: `~/.bayan/session.json` is answering first. Delete it when testing
start-directory behaviour. The launcher directory is honoured EXCEPT under `%SystemRoot%`,
because Explorer and the Start menu often hand a process `System32`, and
except when it is Bayan's own folder (Explorer's default cwd — a pane opened
in Downloads would let cmd run a planted `git.bat`). It passes the same
`accept_local_cwd` + `usable_spawn_dir` gate as every other start directory.

**High Contrast and text size are not the palette's `text_scale`.**
`PaletteLayout.text_scale` compresses the command palette when the window
is short. Windows Accessibility text size is `a11y::text_scale_factor`
(100–225%, clamped), stored on the renderer as `system_text_scale`, and
multiplied separately from DPI: pixels = points × text scale × dpi. An
explicit `font_size` is the size at 100% and is still multiplied while
`follow_system_text_size` is on. High Contrast maps chrome through
`a11y::map_chrome` and, unless `terminal_follow_high_contrast` is false,
the grid's fg/bg/selection/cursor through `a11y::map_terminal`. The 16
ANSI colors stay on the theme. Both are re-read on `WM_SETTINGCHANGE` and
`WM_SYSCOLORCHANGE`.
**The accessibility tree stays off the paint hot path.** Until the first
UIA client, `update_if_active` does not run the builder and a redraw does
not even stamp. On Windows that client leaves the adapter active for the
session (deactivate is ignored), so later frames use a stamp — output
generation, cursor, selection, focus, tabs, palette, card — and
[`screen_reader::TreePace`]: unchanged means no rebuild, and a moving stamp is
built at about 10 Hz with a trailing update so the last state is sent.
The full cell copy happens only on those builds. The terminal lock for a
build is held only long enough to copy the visible cells; Claude
restoration and the node tree happen after it is dropped. Pane text is
logical grid order, so Arabic is read as the program emitted it — not the
visually reordered line the shaper draws. Claude rows use the same
visual→logical map as copy. A burst of output is one announcement per
500ms (the tail, not every line), and an OSC 133 finish replaces that
tail with the exit code. Do not build this tree on an unchanged stamp,
and do not hand it shaped glyphs.

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
cargo fmt                                   # CI runs cargo fmt --check
cargo clippy --all-targets -- -D warnings   # CI fails on any warning
```

The tree is rustfmt-clean with default settings. Run `cargo fmt` before
committing. The whole-tree reformat is listed in `.git-blame-ignore-revs`,
so `git blame` skips it.

**Tests stay out of `~/.bayan`.** Nothing in a test calls `config::load` or
`plugins::init`, so the live `config.json` and plugins are never read, and
`plugins.rs` sends its failure log to `%TEMP%\bayan-test-plugins.log` under
`cfg(test)`. Up to October 2026 the guidance here was to delete
`config.json` before `cargo test`. That was never the real coupling. The real
one was the plugin tests appending deliberate `b_broken` failures to the
user's `plugins.log`. Keep any new test that needs a home directory on a temp
path, the same way.

**`cargo test` does NOT relink `target/debug/bayan.exe`.** Run `cargo build`
before any launch-and-screenshot round, or you will verify a stale binary. This
already happened once and nearly buried a real fix as a no-op.

Being a GUI app, most behavior isn't unit-testable. The established loop is
build → launch → screenshot → inspect → kill. Every debug hook below exists so
a feature can be reached **without injecting keyboard input** — honour that
principle when adding new ones. The hooks are read through `dev_hook()` in
`main.rs`: they are live in debug builds and compiled out of release builds
unless you build with `--features dev-hooks` (BAYAN-010 — `BAYAN_TYPE` types
into a shell and `BAYAN_PROFILE` writes to `%TEMP%`, so a shipped binary must
not honour them). Read any new hook through `dev_hook()` too, and never log
key identity or typed text:

| Env var | Effect |
|---|---|
| `BAYAN_SHOW_NOW=1` | show the window immediately (it normally stays hidden until the first frame, ~0.4s, to avoid a white flash) |
| `BAYAN_SETTINGS=1` | open the settings panel on startup |
| `BAYAN_SHORTCUTS=1` | open the keybinding editor |
| `BAYAN_PICK_THEME=<n>` | open settings and apply theme *n*, so a click's live effect is screenshot-verifiable |
| `BAYAN_SPLIT=1` | start with the first tab pre-split (pane machinery) |
| `BAYAN_ARABIC_FONT=<family>` | draw terminal Arabic in that font for this run (config.json untouched) — with `BAYAN_TYPE`, compare Arabic fonts side by side |
| `BAYAN_LANG=en` | start in English without touching `config.json` (the English, left-to-right layout) |
| `BAYAN_SHELLS=1` | open the tab bar's shell menu at startup, as a right-click on the bar would (it fills in when shell detection reports) |
| `BAYAN_GROUPS=1` | add a second tab to the first group, then open a second group of one tab: the group chip and the per-group tab bar |
| `BAYAN_COCKPIT=1` | open the agent cockpit |
| `BAYAN_GUARD=1` | arm the PASTE guard with a 3-line sample (not the close guard — the name misleads) |
| `BAYAN_TYPE=<text>` | feed text into the first pane's PTY — the hook for ligature and shaping checks |
| `BAYAN_PASTE=<text>` | once the renderer is ready, put text on the clipboard and right-click the focused pane's centre through the real right-click path (`right_click_owner` → `mouse::right_click`) — right-click paste without injecting mouse input. Overwrites the clipboard |
| `BAYAN_PALETTE=<query\|1>` | open the command palette at startup, pre-filtered by the query (`1` opens it empty) |
| `BAYAN_POSH=1` | open the oh-my-posh prompt picker at startup |
| `BAYAN_TOAST=<title\|1>` | fire a sample toast at startup (M19 A/B loops without rebuilding) |
| `BAYAN_ATLAS_STRESS=1` | shape a large spread of unique glyphs to force page-1 atlas growth |
| `BAYAN_ATLAS_RESET=1` | fill the real allocator to its eight-page cap once per renderer, before frame geometry; verify the white texel, backgrounds, cursor and text survive the reset |
| `BAYAN_PROFILE=1` | startup timeline marks to `%TEMP%\bayan_profile.log` (EasyTer's `EASYTER_PROFILE` pattern), zero cost when unset. Key presses log modifiers only, never the key. Also key-to-frame latency samples and a summary every 25 (see Measuring) |

Start with `Start-Process -PassThru`, sleep ~6s for GPU init, capture the
screen with `System.Drawing`, then `Stop-Process` the pid. Any cleanup in
a capture script deletes one named temp file, never a variable that could
come out empty and fall back to a parent folder: one such script once
issued `Remove-Item` on the user profile root, and only the permission
guard stopped it.

## Conventions

Commits are a sentence about what changed and why, often milestone-prefixed:
`M20: cosmic-text 0.12 -> 0.19 retires the M18 rustybuzz bypass`, or a plain
statement of the fix: `Toast Arabic read backwards: feed the balloon renderer
VISUAL word order`. No conventional-commits prefixes.

Every milestone gets a README entry recording what was built *and what was
proven* — the README is the project log, not a brochure. Keep failed
approaches and upstream limits in it; M14 flagging cosmic-text 0.12's
featureless shape plan is why M20 was a clean swap rather than a surprise.

Window input decisions are pure functions beside the code that acts on them:
`chrome::left_press`, `input::press_route` and `motion_route`,
`keyroute::key_owner`, `link::click_owner`, `mouse::right_click`. A new branch
in the press chain or the key path is a new case there with a test, not another
`if` in `window_event`, which only dispatches.

Dependencies stay few and deliberate. M19 delivered native toasts with zero new
crates by reusing the `windows-sys` features already present for the quake
hotkey. Prefer that over pulling in WinRT/COM.

## Project skills

`skills-lock.json` pins `rust-best-practices`, `rust-testing`,
`m15-anti-pattern`, `windows-desktop-e2e`, and `webgpu-specs` (first-party from
the wgpu repo — WebGPU/WGSL spec text for work on `gpu.rs`). The skill trees
themselves are not in the repo (`.gitignore`); `.claude/skills/` exists only
after `skills experimental_install` restores them.
