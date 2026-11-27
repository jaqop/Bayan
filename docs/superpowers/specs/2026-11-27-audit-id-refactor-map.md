# BAYAN-xxx ↔ Refactor Map (evidence for refactor ordering)

**Date:** 2026-11-27 · **Tree verified:** `c1189a9` (main after PR #105)
**Method:** for each audit id, read `docs/SECURITY_AUDIT.md`'s cause + location,
then re-verify against the CURRENT tree whether the causing structure still
exists. Per the reviewer's criterion: an id whose cause was closed only by
patching strengthens the refactor item that would prevent its class; an id
whose cause is already structurally closed weakens that item's bug-precedent
justification. All six ids are marked **Fixed** in the audit, so this table
measures *bug-class recurrence risk*, not open bugs.

Item numbers refer to the refactor report under review (4 = key routing,
6 = vt_frame, 7 = sysbin, 9 = Lua approval, 2/3 = security-ordering-as-data,
5 = ScrollDriver, 8 = Announcer, PaneFrame separate). The report text itself
was not available to this analysis; seams are named so the mapping holds
regardless of numbering.

| Id | Class | Structural cause (audit) | Seam/item that would prevent it | Cause still present at `c1189a9`? | Characterization test ready? |
|---|---|---|---|---|---|
| BAYAN-001 | High | OSC 52 parsed a second time beside `alacritty_terminal` and wrote the clipboard unconditionally | vt_frame / feed-path OSC policy unification (6) | **No.** Library configured `Osc52::OnlyPaste` (term.rs:3144) so it never writes; the single write path is `scan_osc52` gated by `allow_osc52_write` (config + focus + pane). `ClipboardStore` appears only in a comment. | Yes — OSC 52 policy tests + `tests/fixtures/osc52_console.py` |
| BAYAN-003 | High | OSC 9;9 cwd persisted/used as a process dir without a local-path gate (UNC/NTLM) | sysbin / shared cwd gate + localpath walk (7, 2/3) | **No.** `accept_local_cwd` is the single gate across cli/osc7/mux/posh/link/main; `localpath::walk_live` closes reparse cases | Yes — gate + walk tests |
| BAYAN-004 | High | Dropped paths typed raw, not shell-literal | shells quoting seam + paste guard (2/3) | **No.** Per-shell quoting (`quote_fish`, `quote_nu`, `quote_windows_arg`, `quoted_program`) + confirm card on alt-screen/not-at-prompt/bidi names | Yes — quoting + guard tests |
| BAYAN-007 | Medium | Plugin palette actions wrote arbitrary bytes past the paste guard | Lua/plugin approval + send gate (9) | **No.** `send_char_forbidden` + `PendingKind` confirm card; plugin sends capped and badged | Yes — plugins tests |
| BAYAN-010 | Low | Debug hooks compiled into release builds | build-feature split (not a refactor item) | **No.** `dev-hooks` Cargo feature; hooks `cfg`-gated out of ordinary release | Yes — CI release build + gate |
| BAYAN-015 | Low | `Mutex::lock().unwrap()` turns a panic into a dead terminal | lock discipline / PaneFrame lock seam | **No.** `lock_or_recover` throughout; the only remaining `.lock().unwrap()` is inside a `#[test]` helper (pty_writer.rs:247) | Yes — poison-recovery tests |

## Conclusion for ordering

**None of the six bug classes has a live structural cause at `c1189a9`.**
Therefore the audit record does **not** justify prioritizing any refactor
item on bug-precedent grounds — exactly the reviewer's skepticism. The
remaining justification for refactors is:

- **PaneFrame / lock seam:** churn + ~60 lock sites on the perf-critical path
  (a *risk/testability* argument, not a bug-precedent one). Gate it on
  measurement: the `BAYAN_PROFILE` reader tally already emits
  `lock_wait_ms` / `held_ms`; prove the snapshot adds no per-cell copy or
  lock hold before continuing, else redesign or drop.
- **Key routing (4):** bugs live in code untestable without a window — now
  mitigated by the live GUI harness (PR #105) + planned SendInput/IME work.
- **vt_frame (6):** OSC 52 carry is sensitive and the perf gate strict; needs
  an adversarial OSC corpus + measurement before any move.

## Measurement / characterization discipline (step 1 of the agreed order)

Existing automated baselines (CI perf gate): reader throughput, scrollback
memory, startup marks + ratio, key→frame latency p95, idle memory/redraws,
ten-tab memory, 16 MB cat time/peak. Existing lock evidence: reader tally
`read_wait_ms`/`lock_wait_ms`/`held_ms` and `ui redraws/renders` spans.

To add before any refactor step:
1. **Before/after discipline:** capture the perf-gate set + the lock tally on
   the parent commit and on the change; diff them in the PR description.
2. **Characterization runs:** the security-path tests (OSC 52, cwd gate,
   quoting, plugin send, poison recovery) run unchanged before and after a
   move; "moved without behavior change" is only accepted with both runs green.
3. **Adversarial OSC corpus** (for vt_frame): split reads, C0 inside CSI,
   colon subparameters, over-cap payloads, query forms — pinned as tests.
