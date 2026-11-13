"""python -m unittest scripts/test_perf_gate.py"""
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import perf_gate  # noqa: E402

LOG = """
PROBE throughput ascii  64 MB: process_chunk   117.0 MB/s, vte alone   148.1 MB/s (79% of vte)
PROBE throughput sgr    64 MB: process_chunk    88.0 MB/s, vte alone   172.5 MB/s (51% of vte)
PROBE throughput arabic 64 MB: process_chunk   151.0 MB/s, vte alone   206.8 MB/s (73% of vte)
PROBE scrollback  10000 lines x 120 cols: +  33.0 MB once full, +  33.1 MB after as much again (history 10000)
PROBE scrollback 100000 lines x 120 cols: + 303.0 MB once full, + 303.2 MB after as much again (history 100000)
test term::measure::measure_reader_throughput ... ok
PROBE throughput ascii  64 MB: process_chunk   101.0 MB/s, vte alone   148.1 MB/s (68% of vte)
PROBE scrollback  10000 lines x 120 cols: +  35.5 MB once full, +  35.5 MB after as much again (history 10000)
"""

STARTUP = """
--- startup run 1
      0.4 ms  main start
    601.7 ms  gpu ready
    617.0 ms  first frame presented
   3931.3 ms  renderer ready
     40.0 ms  latency key->pty-output 0.5 ms, key->frame 1.4 ms
--- startup run 2
2026-11-13T13:50:01.1Z       0.3 ms  main start
2026-11-13T13:50:01.2Z     107.1 ms  first frame presented
2026-11-13T13:50:01.3Z     138.4 ms  renderer ready
"""

BASE = {
    "throughput": {"ascii": 79, "sgr": 51, "arabic": 73},
    "scrollback": {"10000": 33, "100000": 303},
}


class Gate(unittest.TestCase):
    def test_the_best_of_several_runs_is_kept(self):
        got = perf_gate.parse(LOG)
        self.assertEqual(got["throughput"], {"ascii": 79.0, "sgr": 51.0, "arabic": 73.0})
        self.assertEqual(got["scrollback"], {"10000": 33.0, "100000": 303.0})

    def test_the_baseline_itself_passes(self):
        failures, gains, rows = perf_gate.compare(BASE, perf_gate.parse(LOG), 0.10)
        self.assertEqual((failures, gains), ([], []))
        self.assertEqual(len(rows), 5)

    def test_a_slower_reader_fails_and_ten_percent_is_the_edge(self):
        slow = {"throughput": {"ascii": 70.0, "sgr": 51.0, "arabic": 73.0}, "scrollback": BASE["scrollback"]}
        failures, _, _ = perf_gate.compare(BASE, slow, 0.10)
        self.assertEqual(len(failures), 1)
        self.assertIn("throughput ascii", failures[0])
        edge = {"throughput": {"ascii": 71.2, "sgr": 46.0, "arabic": 66.0}, "scrollback": BASE["scrollback"]}
        self.assertEqual(perf_gate.compare(BASE, edge, 0.10)[0], [])

    def test_more_memory_fails_and_less_is_a_gain(self):
        heavy = {"throughput": BASE["throughput"], "scrollback": {"10000": 33.0, "100000": 340.0}}
        failures, _, _ = perf_gate.compare(BASE, heavy, 0.10)
        self.assertEqual(len(failures), 1)
        self.assertIn("scrollback 100000", failures[0])
        light = {"throughput": BASE["throughput"], "scrollback": {"10000": 33.0, "100000": 150.0}}
        failures, gains, _ = perf_gate.compare(BASE, light, 0.10)
        self.assertEqual(failures, [])
        self.assertEqual(len(gains), 1)

    def test_a_probe_that_printed_nothing_fails(self):
        failures, _, _ = perf_gate.compare(BASE, perf_gate.parse("no probes ran"), 0.10)
        self.assertEqual(len(failures), 5)

    def test_startup_takes_the_warm_start_and_ignores_other_marks(self):
        got = perf_gate.parse(STARTUP)["startup"]
        self.assertEqual(got["first frame presented"], 107.1)
        self.assertEqual(got["renderer ready"], 138.4)
        self.assertNotIn("latency key", " ".join(got))

    def test_the_ratio_is_taken_within_one_start(self):
        got = perf_gate.parse(STARTUP)["startup_ratio"]
        # run 1 has both marks (617.0 / 601.7); run 2 has no gpu mark
        self.assertEqual(got, {"first frame / gpu ready": 1.025})
        base = {"tolerances": {"startup_ratio": 0.15}, "startup_ratio": {"first frame / gpu ready": 1.08}}
        self.assertEqual(perf_gate.compare(base, {"startup_ratio": {"first frame / gpu ready": 1.2}}, 0.1)[0], [])
        late = perf_gate.compare(base, {"startup_ratio": {"first frame / gpu ready": 1.3}}, 0.1)[0]
        self.assertEqual(len(late), 1)

    def test_startup_has_its_own_tolerance_and_reports_no_gain(self):
        base = {
            "tolerances": {"startup": 0.5},
            "startup": {"first frame presented": 100, "renderer ready": 135},
        }
        ok = {"startup": {"first frame presented": 140.0, "renderer ready": 60.0}}
        self.assertEqual(perf_gate.compare(base, ok, 0.10)[:2], ([], []))
        slow = {"startup": {"first frame presented": 151.0, "renderer ready": 135.0}}
        failures, _, _ = perf_gate.compare(base, slow, 0.10)
        self.assertEqual(len(failures), 1)
        self.assertIn("limit 50%", failures[0])


if __name__ == "__main__":
    unittest.main()
