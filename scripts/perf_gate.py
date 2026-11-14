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

    python scripts/perf_gate.py probe.log startup.log              # gate
    python scripts/perf_gate.py probe.log startup.log --print      # the baseline this log would be

Key-to-frame latency is not here: it needs typed input.
"""
import json
import math
import pathlib
import re
import sys

NUMBER = r"[-+]?(?:\d+(?:\.\d+)?|nan|inf(?:inity)?)"
THROUGHPUT = re.compile(rf"PROBE throughput (\w+)\s+\d+ MB: process_chunk\s+({NUMBER}) MB/s, vte alone\s+({NUMBER}) MB/s \(({NUMBER})% of vte\)\s*$", re.IGNORECASE | re.MULTILINE)
SCROLLBACK = re.compile(rf"PROBE scrollback\s+(\d+) lines x \d+ cols: \+\s*({NUMBER}) MB once full, \+\s*({NUMBER}) MB after as much again \(history (\d+)\)\s*$", re.IGNORECASE | re.MULTILINE)
STARTUP = re.compile(rf"(?<![\w.+-])({NUMBER}) ms  ([a-z][a-z ]*[a-z])\s*$", re.MULTILINE | re.IGNORECASE)
REQUIRED = {
    "throughput": {"ascii", "sgr", "arabic"},
    "scrollback": {"10000", "100000"},
    "startup": {"first frame presented", "renderer ready"},
    "startup_ratio": {"first frame / gpu ready"},
}


def number(value, label, *, allow_zero=False):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        raise ValueError(f"{label} must be a finite number")
    if value < 0 or (value == 0 and not allow_zero):
        raise ValueError(f"{label} must be {'nonnegative' if allow_zero else 'positive'}")
    return value


def validate_baseline(baseline):
    if not isinstance(baseline, dict):
        raise ValueError("baseline must be an object")
    if not set(baseline).issubset(set(REQUIRED) | {"tolerance", "tolerances"}):
        raise ValueError("baseline contains unknown policy keys")
    for group, required in REQUIRED.items():
        values = baseline.get(group)
        if not isinstance(values, dict) or not required.issubset(values):
            raise ValueError(f"baseline {group} is missing required metrics")
        for name, value in values.items():
            number(value, f"baseline {group} {name}")
    number(baseline.get("tolerance", 0.10), "tolerance", allow_zero=True)
    tolerances = baseline.get("tolerances", {})
    if not isinstance(tolerances, dict) or not set(tolerances).issubset(REQUIRED):
        raise ValueError("tolerances must name known metric groups")
    for group, value in tolerances.items():
        number(value, f"tolerance {group}", allow_zero=True)


def validate_measured(measured):
    for group, required in REQUIRED.items():
        values = measured.get(group, {})
        if not required.issubset(values):
            raise ValueError(f"measured {group} is missing required metrics")
        for name in required:
            number(values[name], f"measured {group} {name}")


def parse(text):
    """{'throughput': {kind: best %}, 'scrollback': {lines: lowest MB},
    'startup': {mark: lowest ms}, 'startup_ratio': {name: lowest ratio}}"""
    throughput, scrollback, startup, ratio = {}, {}, {}, {}
    for line in text.splitlines():
        for marker, grammar in (("PROBE throughput", THROUGHPUT), ("PROBE scrollback", SCROLLBACK)):
            if marker in line and not grammar.search(line):
                raise ValueError(f"malformed {marker} line")
        if re.search(r"\s+ms  [a-z][a-z ]*[a-z]\s*$", line, re.IGNORECASE) and not STARTUP.search(line):
            raise ValueError("malformed startup mark")
    for kind, chunk_rate, vte_rate, pct in THROUGHPUT.findall(text):
        number(float(chunk_rate), f"process_chunk rate {kind}")
        number(float(vte_rate), f"vte rate {kind}")
        value = number(float(pct), f"throughput {kind}")
        throughput[kind] = max(value, throughput.get(kind, 0.0))
    for lines, mb, after, history in SCROLLBACK.findall(text):
        number(float(after), f"scrollback after refill {lines}")
        if history != lines:
            raise ValueError("scrollback history does not match the requested capacity")
        value = number(float(mb), f"scrollback {lines}")
        scrollback[lines] = min(value, scrollback.get(lines, float("inf")))
    for ms, mark in STARTUP.findall(text):
        value = number(float(ms), f"startup {mark}", allow_zero=mark == "main start")
        startup[mark] = min(value, startup.get(mark, float("inf")))
    # The ratio is between two marks of the same start, and it is read from
    # the fastest start. A cold start spends longer bringing the GPU up, so
    # its ratio is lower and would hide work added after that point.
    fastest = float("inf")
    for block in text.split("--- startup run")[1:]:
        marks = {}
        for ms, mark in STARTUP.findall(block):
            if mark in marks:
                raise ValueError(f"duplicate startup mark: {mark}")
            marks[mark] = float(ms)
        keys = {"gpu ready", "first frame presented", "renderer ready"}
        if not keys.issubset(marks):
            raise ValueError("startup run is missing required marks")
        gpu, frame, ready = (marks[k] for k in ("gpu ready", "first frame presented", "renderer ready"))
        if not 0 < gpu <= frame <= ready:
            raise ValueError("startup marks must satisfy 0 < gpu <= frame <= renderer")
        if frame < fastest:
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


def checked_main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    logs = [a for a in argv[1:] if not a.startswith("--")]
    measured = parse(
        "\n".join(pathlib.Path(a).read_text(encoding="utf-8", errors="replace") for a in logs)
    )
    validate_measured(measured)
    if "--print" in argv:
        print(json.dumps(measured, indent=2, sort_keys=True))
        return 0
    root = pathlib.Path(__file__).resolve().parent.parent
    baseline = json.loads((root / "perf" / "baseline.json").read_text(encoding="utf-8"))
    validate_baseline(baseline)
    tolerance = float(baseline.get("tolerance", 0.10))
    failures, gains, rows = compare(baseline, measured, tolerance)
    print(table(rows))
    for line in gains:
        print(f"note: {line}")
    for line in failures:
        print(f"::error::{line}")
    return 1 if failures else 0


def main(argv):
    try:
        return checked_main(argv)
    except (ValueError, OSError) as error:
        print(f"::error::Invalid performance data: {error}")
        return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
