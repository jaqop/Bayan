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

| Mark | Baseline | Fastest of five starts, two jobs on `main` (6e3742c + the gate) |
|---|---|---|
| first frame presented | 100 ms | 80.0, 100.2 |
| renderer ready | 135 ms | 100.7, 132.4 |

The two jobs differ by a quarter with the same code, so this group's limit
is 50%, not 10% (`tolerances.startup`): above 150 ms to the first frame or
about 200 ms to a ready renderer fails. That catches work that moves in
front of the first frame, which is what the hidden-until-first-frame
startup is built around; it does not catch a regression of a few
milliseconds. Within one job the four warm starts agree to about 5%.

The first start of each job is cold and is not what the gate reads: 182
and 617 ms to the first frame, 3.0 and 3.9 s to a ready renderer. A hosted
runner has no GPU; the window opens on WARP.

The SGR throughput share read 35 to 41% across four jobs, the widest of
the three corpora. Its floor is 32.4%.
