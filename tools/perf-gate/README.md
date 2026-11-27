# perf-gate (Rust)

The Rust port of `scripts/perf_gate.py`: parse the `PROBE` lines the ignored
`measure_` tests print, validate against `perf/baseline.json`, fail a change
that makes the reader slower or the scrollback heavier.

Dev-only: a standalone crate, **not** a workspace member and not linked into
`bayan.exe`, so the terminal binary stays lean.

```
cargo build --manifest-path tools/perf-gate/Cargo.toml
tools/perf-gate/target/debug/perf-gate.exe probe.log startup.log latency.log idle.log --baseline perf/baseline.json
```

## Equivalence

Proven against the Python gate on the same log and the real baseline:
`--print` JSON identical, and the full verdict table byte-identical
(`preserve_order` keeps rows in baseline-file order, like Python). Unit tests
mirror the Python gate's cases (best-of/lowest-of aggregation, malformed-line
rejection, the 40-sample latency floor, startup mark order, per-group
tolerance overrides, idle-redraw zero baseline with report-only minimized).

The lookbehind `(?<![\w.+-])` in the Python startup regex has no equivalent in
the `regex` crate; matches are post-filtered by their preceding character
instead, which is semantically identical.

## Cutover

`scripts/perf_gate.py` remains the CI gate until this crate is wired into the
workflow and proven green there; only then is the Python retired.
