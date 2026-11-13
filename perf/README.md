# perf/baseline.json

What the `perf` CI job compares against. Figures are from GitHub's hosted
`windows-latest` runner, not the author's machine.

| Probe | Baseline | From (three runs on 7803919 + the gate, 2026-11-13) |
|---|---|---|
| throughput ascii | 78% of vte | 77, 78, 77 |
| throughput sgr | 36% of vte | 35, 36, 36 |
| throughput arabic | 70% of vte | 71, 69, 68 |
| scrollback 10000 | 32.7 MB | 32.7, 32.7, 32.7 |
| scrollback 100000 | 302.0 MB | 301.9, 302.0, 302.0 |

The SGR share is 36% here and 51% on the author's machine: the ratio is
steady on one machine, not the same number on every machine. If GitHub
moves the runner to different hardware and a ratio shifts with no code
change, re-measure and update this file in a pull request of its own.

Raise a figure when a change improves it by more than the tolerance; the
job prints a note when that happens.

## Startup

Fastest of five starts, three jobs on `main` (6e3742c + the gate), same code:

| Job | vte alone, plain | gpu ready | first frame | renderer ready | first frame / gpu ready |
|---|---|---|---|---|---|
| 1 | 141 MB/s | 73.3 ms | 80.0 ms | 100.7 ms | 1.09 |
| 2 | 85 MB/s | 92.1 ms | 100.2 ms | 132.4 ms | 1.09 |
| 3 | 77 MB/s | 131.2 ms | 140.7 ms | 200.7 ms | 1.07 |

Hosted runners are not one machine: the same code took 80 ms on one and
141 ms on another. A first version of this gate allowed 50% over job 2's
figures and job 3 came within 7% of failing it with no change at all. So:

- **`startup`** (milliseconds) is a backstop. The baseline is the slowest
  job seen and the limit is 100% over it: above 280 ms to the first frame
  or 400 ms to a ready renderer fails. It catches hundreds of
  milliseconds, such as the font scan moving in front of the first frame.
- **`startup_ratio`** is `first frame presented` over `gpu ready` within
  one start, at 15%. Most of the time to the first frame is WARP coming
  up, which scales with the machine, so the ratio held at 1.07 to 1.09 on
  all three. Above 1.24 fails: roughly 10 to 20 ms of new work between the
  GPU being ready and the first frame.

Not caught: new work before `gpu ready` that is smaller than the backstop,
and anything after the first frame short of 400 ms. `renderer ready` minus
`first frame` read 21, 32 and 60 ms on the three jobs, too loose to gate.

The first start of each job is cold and is not what the gate reads: up to
617 ms to the first frame and 3.9 s to a ready renderer. A hosted runner
has no GPU; the window opens on WARP.

The SGR throughput share read 35 to 41% across five jobs, the widest of
the three corpora. Its floor is 32.4%.

## Validation and local runs

The CLI requires every metric group and the expected corpus/history keys.
Baseline and measured values must be finite and positive; tolerances must be
finite and nonnegative. Each startup run must have a positive GPU mark,
then the first frame and renderer-ready mark in chronological order. A missing
or malformed policy/log fails instead of silently turning the gate green.

`scripts/measure-startup.ps1` waits for all three marks, uses a fresh home and
temporary directory for each run, and stops only the process it launched.
It uses `cmd.exe /d /q` to avoid executing a user's AutoRun. It does not kill
other Bayan windows or remove the user's profile log. The same script runs
locally and on CI. Run it against a release build with `--features dev-hooks`.

The hosted baseline is retained. Local hardware measurements are evidence,
not a reason to relax hosted-runner thresholds. The startup ratio/backstops
remain a wide net, not a universal 10% cold-start limit.

## Latency, idle memory, idle redraw

Same `bayan.exe` as the startup step (`cargo build --locked --release
--features dev-hooks`). No second full build.

| Probe | What it measures | Gate |
|---|---|---|
| `latency key->frame p95` | 50 keys via `BAYAN_TAP` (`a`, Backspace, 150 ms apart) through `type_key`. Not `BAYAN_TYPE` (that writes the PTY and skips `key_sent`) and not SendInput. | n < 40 fails. Lower is better. Tolerance is 100% over the slowest hosted p95, the same shape as startup, because WARP + ConPTY + cmd echo move together. |
| `latency render` | per sample, key->frame minus key->pty-output. Printed so the spread is on the job log. | Not required. Add it with a tighter tolerance only when three hosted runs show it is stable. |
| `idle_memory private` | `Get-Process` `PrivateMemorySize64` of the Bayan process, one quiet `cmd /d /q` tab, 5 s after the prompt settles. Working set is printed and not gated. cmd and ConPTY are not included. | Lower is better. Median of three hosted runs. 10% when the spread is narrow; wider when it is not, and the spread is recorded here. |
| `idle_redraw visible` | blink off (`cursor_blink: false`), one quiet tab, 5 s. | presents and redraws must both be 0. |
| `idle_redraw minimized` | `BAYAN_IDLE_BUSY=1` keeps a background tab busy; the script calls `ShowWindow(SW_MINIMIZE)`. | **Report-only.** `MINIMIZED_REDRAW_REQUIRED` in `scripts/perf_gate.py` is the one-line switch. A missing or malformed line still fails. Non-zero counts do not, until that flag is true. The 600 ms busy-dot pulse and wakeup/title/bell redraws while minimized are stopped by a separate change; this probe is ready for that change. |

`BAYAN_FRAME_COUNTERS=1` (with `BAYAN_PROFILE`) writes
`%TEMP%\bayan_frame_counters.txt` as `redraws=N presents=M` on each
increment, replaced with `MoveFileExW` so a reader does not see a torn
line. Redraws count at `redraw()` entry. Presents count only after
`gpu.render` returns true (an occluded swapchain does not). The latency
probe does not set `BAYAN_FRAME_COUNTERS`, so that file replace is not on
the key->frame path.

Three complete `windows-latest` runs of the same probe code (runs
43519776309, 43520662670, 43520674052), 2026-11-13:

| Run | key->frame p50 | p95 | n | render p50 | render p95 | private MB | working set MB | visible p/r | minimized p/r |
|---|---|---|---|---|---|---|---|---|---|
| 43519776309 | 126.4 | 149.3 | 46 | 111.2 | 132.4 | 291.1 | 124.8 | 0/0 | 0/733 |
| 43520662670 | 128.5 | 154.7 | 46 | 111.2 | 141.9 | 291.4 | 125.1 | 0/0 | 0/643 |
| 43520674052 | 102.0 | 127.0 | 46 | 88.2 | 107.4 | 291.3 | 124.9 | 0/0 | 0/822 |

- **Latency.** Slowest p95 is 154.7 ms. The baseline is 155 ms and the
  tolerance is 100%, so the job fails above 310 ms. The same binary, on
  three earlier runs whose idle step did not start, read p95 134.5, 140.6
  and 179.1. 179 is inside the 310 ms band. A tighter band would fail a
  quiet runner change.
- **Render gap** (key->frame minus key->pty-output), p95: 132.4, 141.9,
  107.4 on the three runs, and 120.8, 129.8, 151.7 on the earlier ones.
  That is the same kind of spread as key->frame, not a stable remainder,
  so it is printed and not gated.
- **Idle memory.** Private MB 291.1, 291.4, 291.3. Median 291.3. The
  spread is 0.3 MB, so the tolerance stays 10% (fail above 320.4 MB).
  Working set was 124.8, 125.1, 124.9 and is not gated.
- **Visible redraw** was 0 presents and 0 redraws on all three.
- **Minimized redraw** was 0 presents (the swapchain is occluded) and
  733, 643, 822 redraws. Report-only until `MINIMIZED_REDRAW_REQUIRED`.
  The redraws are the 600 ms busy-dot pulse still entering `redraw()`
  while the window is minimized.

Scripts: `scripts/measure-tap-latency.ps1`, `scripts/measure-idle.ps1`.
`scripts/measure-latency.ps1` stays the local SendInput tool.
