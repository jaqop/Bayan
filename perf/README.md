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
