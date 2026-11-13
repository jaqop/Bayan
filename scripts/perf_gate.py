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

  startup: milliseconds from process start to a `BAYAN_PROFILE` mark, per
      mark, lowest of the runs (the first start of a job is cold and several
      times slower). Hosted runners differ by almost a factor of two from
      one job to the next, so this group is a backstop with a wide
      tolerance: it catches hundreds of milliseconds, not tens.
  startup_ratio: `first frame presented` over `gpu ready` within the fastest
      start. Both scale with the machine, so the ratio holds
      from runner to runner; it rises when work lands between the GPU being
      ready and the first frame.

A value more than `tolerance` (10%) worse than the baseline fails. A value
that much better is reported, so the baseline can be raised and the gain
kept. Startup gains are not reported: there they are the runner, not the code.

    python scripts/perf_gate.py probe.log              # gate
    python scripts/perf_gate.py probe.log --print      # the baseline this log would be

Key-to-frame latency is not here: it needs typed input.
"""
import json
import pathlib
import re
import sys

THROUGHPUT = re.compile(r"PROBE throughput (\w+)\s.*\((\d+(?:\.\d+)?)% of vte\)")
SCROLLBACK = re.compile(r"PROBE scrollback\s+(\d+) lines.*?\+\s*(\d+(?:\.\d+)?) MB once full")
# `     107.1 ms  first frame presented`, as prof::mark writes it. CI log
# lines carry a timestamp in front, so the match is not anchored there.
STARTUP = re.compile(r"(\d+(?:\.\d+)?) ms  ([a-z][a-z ]*[a-z])\s*$", re.MULTILINE)


def parse(text):
    """{'throughput': {kind: best %}, 'scrollback': {lines: lowest MB},
    'startup': {mark: lowest ms}, 'startup_ratio': {name: lowest ratio}}"""
    throughput, scrollback, startup, ratio = {}, {}, {}, {}
    for kind, pct in THROUGHPUT.findall(text):
        throughput[kind] = max(float(pct), throughput.get(kind, 0.0))
    for lines, mb in SCROLLBACK.findall(text):
        scrollback[lines] = min(float(mb), scrollback.get(lines, float("inf")))
    for ms, mark in STARTUP.findall(text):
        startup[mark] = min(float(ms), startup.get(mark, float("inf")))
    # The ratio is between two marks of the same start, and it is read from
    # the fastest start. A cold start spends longer bringing the GPU up, so
    # its ratio is lower and would hide work added after that point.
    fastest = float("inf")
    for block in text.split("--- startup run")[1:]:
        marks = {mark: float(ms) for ms, mark in STARTUP.findall(block)}
        gpu, frame = marks.get("gpu ready"), marks.get("first frame presented")
        if gpu and frame and frame < fastest:
            fastest = frame
            ratio["first frame / gpu ready"] = round(frame / gpu, 3)
    return {
        "throughput": throughput,
        "scrollback": scrollback,
        "startup": startup,
        "startup_ratio": ratio,
    }


def compare(baseline, measured, tolerance):
    """(failures, gains, rows). Higher is better for throughput, lower for
    memory and startup. `baseline["tolerances"]` overrides the tolerance per
    group."""
    failures, gains, rows = [], [], []
    checks = (
        ("throughput", "% of vte", True, True),
        ("scrollback", "MB", False, True),
        ("startup", "ms", False, False),
        ("startup_ratio", "x", False, False),
    )
    for group, unit, higher_is_better, report_gains in checks:
        group_tolerance = float(baseline.get("tolerances", {}).get(group, tolerance))
        for name, base in baseline.get(group, {}).items():
            got = measured.get(group, {}).get(name)
            label = f"{group} {name}"
            if got is None:
                failures.append(f"{label}: the probe printed no value")
                rows.append((label, base, None, unit, "missing"))
                continue
            change = (got - base) / base if base else 0.0
            worse = -change if higher_is_better else change
            if worse > group_tolerance:
                verdict = "FAIL"
                failures.append(
                    f"{label}: {got:g} {unit} against a baseline of {base:g} "
                    f"({worse * 100:.0f}% worse, limit {group_tolerance * 100:.0f}%)"
                )
            elif report_gains and -worse > group_tolerance:
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
    logs = [a for a in argv[1:] if not a.startswith("--")]
    measured = parse(
        "\n".join(pathlib.Path(a).read_text(encoding="utf-8", errors="replace") for a in logs)
    )
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
