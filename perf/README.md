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
