# Phase 2 — fish / nushell shell integration (design)

**Date:** 2026-11-28 · **Status:** approved — Option A (injected nonce marks), fish-first then nu; owner approved installing nu + WSL fish locally for live characterization
**Precondition:** Phase C merged; full suite + GUI harness + perf gate green.

Goal: OSC 133 prompt marks + cwd reporting in fish and nushell panes, to the
same standard as the existing bash/zsh/PowerShell integration, without bloat
and without weakening the nonce trust model.

## What exists today (verified in code)

- `shellint::ShellKind` = {PowerShell, Bash, Wsl, Cmd, Unknown}. fish and nu
  fall to `Unknown` → `plan()` returns `None` → no injection. (`ShellFamily`
  already has `Fish`/`Nu`, but only for `run`-command quoting, not marks.)
- `plan()` builds a `Plan` (args with `@file:` tokens, hook `files`, `path_env`,
  literal `env` carrying `BAYAN_PROMPT_NONCE`); `materialize()` writes the hooks
  to a secure integration dir outside the user profile (high integrity label
  when elevated) and resolves the tokens. **Nothing rewrites the user's home.**
- Trust (`term::osc133_trusted`): an A/B/D mark is *trusted* only when a
  `;`-field equals the session nonce (32 hex from RtlGenRandom). Trusted marks
  arm OSC 7 / OSC 9;9 cwd reports and full prompt features. cmd's marks carry
  no nonce → untrusted → limited (structure only, no trusted cwd).

## What modern fish / nu do natively (verified via docs)

- **fish 3.8+ / 4.x emits OSC 133 prompt marks itself**, and reports cwd
  natively. kitty therefore injects *no marks* on new fish — only cursor-shape
  and cwd helpers — and loads its script by prepending a vendored dir to
  `XDG_DATA_DIRS` (fish autoloads `fish/vendor_conf.d/*.fish`); the script
  erases that env entry after startup. No user-home files touched.
- **nushell 0.111+ emits OSC 133 natively** when
  `$env.config.shell_integration.osc133` is true (the default). Hooks
  (`pre_prompt`, `pre_execution`) exist for custom marks; vendor autoload dirs
  load scripts without editing the user's config.

## The decision: native (untrusted) vs injected (trusted) marks

Native marks carry **no Bayan nonce**, so under the current trust model they
are untrusted — like cmd. Two ways forward:

- **Option A — injected nonce marks (parity with bash).** Load a Bayan hook
  (fish: `XDG_DATA_DIRS` vendor_conf.d; nu: vendor autoload / `--config`) that
  emits A/B/C/D carrying the nonce, plus cwd. Full trusted features. Cost: must
  suppress or coexist with the shell's *native* marks to avoid double marks
  (fish feature flag / nu `shell_integration.osc133=false`), and the hook is
  security-sensitive (nonce in env not argv, elevated refusal, no home writes).
  Needs live characterization against real fish/nu before landing.
- **Option B — native marks, untrusted (parity with cmd).** No injection; rely
  on the shell's own OSC 133. Zero injection risk, zero home writes, works on
  modern fish/nu out of the box. Cost: marks stay untrusted → no nonce-gated
  cwd / trusted prompt features unless the trust model is extended to accept
  native marks under a separate, clearly-untrusted tier.

Recommendation: **A for feature parity**, delivered fish-first then nu, each
its own PR with hook-string unit tests + live characterization; fall back to B
per shell if suppression of native marks proves unreliable. Owner to confirm.

## Standing constraints

Pure-Rust runtime; no new runtime deps; no copied external code (kitty's
scripts are a reference for *semantics*, not copied text); nonce never on the
command line; no user-home writes; elevated panes refuse unprotected startup;
each change its own PR with unit tests + perf-gate diff; security-sensitive
paths keep existing guarantees.

## Verified on this machine (2026-11-28)

Installed for characterization: **nu 0.117.0** (winget, hash-verified) and
**fish 4.9.3** (MSYS2 `msys/fish`; lighter than a WSL distro, same fish 4.x
semantics). Confirmed:

- `fish --features=no-mark-prompt` turns the `mark-prompt` feature **off**
  (`status features`), suppressing fish's native OSC 133 — so injected nonce
  marks do not duplicate native ones.
- `fish --init-command "..."` runs after config; but for nonce security the
  hook must load **before** user config, so use `XDG_DATA_DIRS` vendor_conf.d
  (kitty's mechanism), not `--init-command`.
- nu default `$env.config.shell_integration` = osc133/osc633/osc9_9/osc2/osc8
  true, osc7 false. nu accepts `--config`, `--env-config`, `--config-home` for
  secure config injection (set osc133/osc633 false, add nonce hooks).
- Nonce pattern to mirror (bash prelude): guard double-load, copy nonce to an
  unexported var, `unset`/`set -e BAYAN_PROMPT_NONCE` **before user files run**,
  bail on empty nonce.

## Fish hook design (landed on feat/phase-2-fish-nu)

- `ShellKind::Fish` recognized (landed); `plan()` routes to `fish_plan`.
- `fish_plan`: interactive only (no `-c`/`--command`, no script file); elevated
  → refuse injection (fail closed) until a protected startup exists. Args gain
  `--features=no-mark-prompt`. A secure dir is **prepended** to `XDG_DATA_DIRS`
  (new `Plan.path_env_prepend`, resolved at materialize) holding
  `fish/vendor_conf.d/bayan.fish`, which fish autoloads before user config.
- The hook takes the nonce before user files, then emits nonce-carrying
  A/B/C/D + cwd (OSC 9;9 / OSC 7) via `fish_prompt`/`fish_preexec`/`fish_postexec`.
- **Verification gate CLOSED (2026-11-28, live fish 4.9 PTY capture):** B must
  land AFTER the prompt text — `term.rs` records the cursor column at B as the
  input start (`prompt_col`), exactly like bash's PS1-append. The `fish_prompt`
  *event* fires before the prompt is drawn, so the hook wraps the `fish_prompt`
  *function* lazily on the first event (user config has run by then) and prints
  B after the original prompt's output; it re-wraps if a theme redefines
  `fish_prompt`. Verified: A → cwd → prompt text → B, cursor at prompt-width+1,
  D;0/D;1 exit codes, native marks suppressed, nonce erased from the env before
  user config (file side-channel check). Hook-string unit tests mirror the bash
  rc assertions.
- **Materialize facts (verified):** MSYS2 fish reads `XDG_DATA_DIRS` entries as
  POSIX paths only — the prepend is translated `C:\dir` → `/c/dir` at
  materialize time. The vendor layout needs 3 safe path components
  (`fish/vendor_conf.d/bayan.fish`), so `rel_is_safe` allows depth 3 (all
  components still `Normal`; traversal guards unchanged) and materialize
  creates every ancestor level with the secure DACL.

## Verification plan

Hook-string unit tests (nonce marks present, nonce unset before exec, cwd
reported, elevated refusal) plus a live REPL characterization: spawn fish/nu in
a PTY with the hook and capture the emitted OSC 133 ordering. nu follows fish
as its own PR (secure `--config` injection + hooks).