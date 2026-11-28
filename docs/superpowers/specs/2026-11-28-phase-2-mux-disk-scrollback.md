# Phase 2 — mux disk scrollback: reconnect-restore + bounded disk history

**Date:** 2026-11-28 · **Status:** approved scope (owner chose "mux restore + disk history")
**Precondition:** Phase A3 mux persistence landed (save side wired via `Effect::Persist`;
`SessionSnap` → `~/.bayan/mux/<id>.json`, bounded by `TAIL_MAX` = 256 KiB).

## Problem

The save side exists but the **load side has no consumer** (`load_saved`/`load_from`
are `#[allow(dead_code)]`). When the mux server restarts (reboot, crash, or exit after
the last live slot is gone), the live `Slot`s are gone but the disk snapshots remain
orphaned: `List` shows nothing and `Reattach` returns `Missing`. The persisted history
is unreachable. The A3 exit ("reconnect restores layout from disk") is unmet.

A second gap: the pipe keeps only a 256 KiB RAM tail (`TAIL_MAX`). The code notes "a
longer history is the file-restore work, not this pipe." Disk scrollback should retain
a **bounded longer history** than the RAM tail and replay it on restore.

## Design

Two parts, landed together in PR #127: **A — reconnect-restore consumer** and
**B — bounded longer disk history**.

### Id stability (correctness prerequisite)

Session ids are server-local (`MuxHub.next++`). After a restart they restart from 1 and
would collide with orphaned disk snapshots. Fix: the server **loads disk snapshots into
a registry on startup and reserves their ids** (`next = max(saved id) + 1`), so new live
sessions never reuse a saved id.

### PR A — reconnect-restore (restore-as-dead)

A mux session is one live shell. After a restart the shell is **dead**; restore replays
its saved output as a read-only history pane (the memory note's "restore-as-dead"
semantics), not a resurrected process.

- `SessionSnap` gains `dead: bool` (`#[serde(default)]` → old files load as `false`).
- `MuxHub` gains a `saved: BTreeMap<u64, SessionSnap>` registry of disk-only sessions,
  populated at startup from `saved_dir()`; `next` reserved above the max saved id.
- `List` merges live-detached slots **and** saved-dead snapshots (saved marked `dead`).
- `Reattach`:
  - live detached slot → existing live pipe (`dead = false`);
  - else saved-dead → `Reattached { snap(dead = true) }`; the client replays `snap.screen`
    then **EOFs** (read-only restored pane, shows exited);
  - else `Missing`.
- `Kill` of a saved-dead session removes it and **deletes its disk files**
  (`Effect::Forget { id }`). Closing a restored pane does NOT delete (only explicit
  Kill does), so viewing history is non-destructive.
- Client: `dispatch_incoming` for a `dead` snap sends the screen to the pending reader
  then drops the route (EOF) instead of inserting a live route. No window change needed:
  `pane_from_attached` builds a session whose reader EOFs → exited pane.

### PR B — bounded longer disk history (implemented)

- The server appends output to a per-session disk ring (`<id>.log`), bounded by
  `DISK_HISTORY_MAX` = 4 MiB (rotates once past `+ ROTATE_SLACK`), **asynchronously**
  via a bounded `sync_channel` (`HISTORY_QUEUE` = 256) + a `bayan-mux-history` writer
  thread. `Ev::Output` only does a non-blocking `try_send` (drop on full — never
  backpressure the PTY). Mux is opt-in and off the perf-gate reader path (local shells),
  so the writer does not touch the gate's reader-throughput metric.
- At startup `load_saved_from` replays the ring (longer than the 256 KiB JSON tail, and
  it kept growing after detach) into the dead snap through `replay_bytes`; the PR A
  restore path then replays that longer history unchanged. `forget_saved` deletes the
  `.log` too.
- Tests: `read_history_tail_keeps_the_last_max_bytes`, `append_history_rotates_back_to_the_bound`,
  `load_saved_replays_the_longer_disk_ring`.

## Security

- Snapshots/logs live in `~/.bayan/mux/` (user profile, same user reads them). They
  capture terminal output (may include secrets); this is the same exposure the existing
  `Effect::Persist` already writes. No new world-readable surface.
- `Forget`/kill deletes only files under `saved_dir()` named `<id>.json`/`<id>.log` for
  a known id — no path traversal (id is a u64).
- Restore-as-dead is read-only: input to a dead session is ignored by the server
  (`Missing`), so a restored pane cannot drive a process.
- Elevated mux still refused (`mux_may_run`); disk registry inherits that guard.

## Verification

Pure `MuxHub`/`reduce` unit tests (the in-process fake): startup loads saved + reserves
ids; `List` merges live + dead; `Reattach` dead replays + EOFs; `Kill` dead emits
`Forget`; live reattach unchanged. Disk round-trip tests for save/load/forget. PR B adds
bounded-ring tests (rotation, UTF-8/escape-safe cut via `replay_bytes`) and a perf-gate
diff. Full suite + clippy + fmt green; CI perf gate is the arbiter for PR B.
