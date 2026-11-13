#!/usr/bin/env python3
"""Fail a pull request that makes the reader slower or the scrollback heavier.

Reads the `PROBE` lines the ignored `measure_` tests print (one or more runs,
concatenated) and compares them with perf/baseline.json:

  throughput: `process_chunk` as a percentage of the bare vte parser, per
      corpus. A ratio, because both sides run on the same machine in the same
      process, so a slow runner moves them together. The best of the runs is
      used: noise only ever makes a run slower.
  scrollback: private megabytes once the history is full, per history size.
      The lowest of the runs is used for the same reason.

A value more than `tolerance` (10%) worse than the baseline fails. A value
that much better is reported, so the baseline can be raised and the gain kept.

    python scripts/perf_gate.py probe.log              # gate
    python scripts/perf_gate.py probe.log --print      # the baseline this log would be

Startup time and key-to-frame latency are not here: they need a window.
"""
import json
import pathlib
import re
import sys

THROUGHPUT = re.compile(r"PROBE throughput (\w+)\s.*\((\d+(?:\.\d+)?)% of vte\)")
SCROLLBACK = re.compile(r"PROBE scrollback\s+(\d+) lines.*?\+\s*(\d+(?:\.\d+)?) MB once full")


def parse(text):
    """{'throughput': {kind: best %}, 'scrollback': {lines: lowest MB}}"""
    throughput, scrollback = {}, {}
    for kind, pct in THROUGHPUT.findall(text):
        throughput[kind] = max(float(pct), throughput.get(kind, 0.0))
    for lines, mb in SCROLLBACK.findall(text):
        scrollback[lines] = min(float(mb), scrollback.get(lines, float("inf")))
    return {"throughput": throughput, "scrollback": scrollback}


def compare(baseline, measured, tolerance):
    """(failures, gains, rows). Higher is better for throughput, lower for memory."""
    failures, gains, rows = [], [], []
    checks = (
        ("throughput", "% of vte", True),
        ("scrollback", "MB", False),
    )
    for group, unit, higher_is_better in checks:
        for name, base in baseline.get(group, {}).items():
            got = measured.get(group, {}).get(name)
            label = f"{group} {name}"
            if got is None:
                failures.append(f"{label}: the probe printed no value")
                rows.append((label, base, None, unit, "missing"))
                continue
            change = (got - base) / base if base else 0.0
            worse = -change if higher_is_better else change
            if worse > tolerance:
                verdict = "FAIL"
                failures.append(
                    f"{label}: {got:g} {unit} against a baseline of {base:g} "
                    f"({worse * 100:.0f}% worse, limit {tolerance * 100:.0f}%)"
                )
            elif -worse > tolerance:
                verdict = "better"
                gains.append(f"{label}: {got:g} {unit} against {base:g}; raise the baseline")
            else:
                verdict = "ok"
            rows.append((label, base, got, unit, verdict))
    return failures, gains, rows


def table(rows):
    out = ["| probe | baseline | measured | |", "|---|---|---|---|"]
    for label, base, got, unit, verdict in rows:
        shown = "—" if got is None else f"{got:g} {unit}"
        out.append(f"| {label} | {base:g} {unit} | {shown} | {verdict} |")
    return "\n".join(out)


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    measured = parse(pathlib.Path(argv[1]).read_text(encoding="utf-8", errors="replace"))
    if "--print" in argv:
        print(json.dumps(measured, indent=2, sort_keys=True))
        return 0
    root = pathlib.Path(__file__).resolve().parent.parent
    baseline = json.loads((root / "perf" / "baseline.json").read_text(encoding="utf-8"))
    tolerance = float(baseline.get("tolerance", 0.10))
    failures, gains, rows = compare(baseline, measured, tolerance)
    print(table(rows))
    for line in gains:
        print(f"note: {line}")
    for line in failures:
        print(f"::error::{line}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
