"""Decision rules must not manufacture gains from absent or noisy measurements."""
import unittest
import importlib.machinery
import importlib.util
from pathlib import Path
import shlex
import os
import sys
import subprocess
from unittest import mock

from compare import compare
from stats import distribution


class Comparison(unittest.TestCase):
    def test_an_absent_baseline_never_becomes_zero(self):
        self.assertEqual(compare(dict(median=3, p10=2, p90=4), None), "no baseline")

    def test_either_runs_spread_can_explain_a_delta(self):
        quiet = dict(median=10, p10=10, p90=10)
        noisy = dict(median=12, p10=8, p90=16)
        self.assertIn("within noise", compare(quiet, noisy))
        self.assertIn("within noise", compare(noisy, quiet))
        self.assertIn("outside observed spread", compare(quiet, dict(median=12, p10=12, p90=12)))

    def test_windows_paths_survive_hyperfines_posix_tokenizer(self):
        loader = importlib.machinery.SourceFileLoader("workloads", str(Path(__file__).resolve().parents[2] / "mise-tasks/perf/workloads"))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        workloads = importlib.util.module_from_spec(spec)
        loader.exec_module(workloads)
        tokens = [r"D:\a\path with spaces\a.exe", "-p", 'echo "日本語"']
        self.assertEqual(shlex.split(workloads.command(tokens)), tokens)

    @unittest.skipUnless(os.name == "nt", "native Windows accounting")
    def test_exited_windows_children_retain_real_peak_and_cpu_counters(self):
        from process import measure_windows
        result = measure_windows([sys.executable, "-c", "sum(range(10000))"])
        self.assertGreater(result["peak_rss_kib"], 0)
        self.assertGreater(result["wall_seconds"], 0)
        self.assertGreaterEqual(result["cpu_seconds"], 0)

    @unittest.skipUnless(os.name == "nt", "native owned-process cleanup")
    def test_timeouts_stop_owned_descendants_and_leave_unrelated_processes_alive(self):
        from process import run_timed
        code = "import subprocess, sys, time; p = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)']); print(p.pid, flush=True); time.sleep(60)"
        parent = subprocess.Popen([sys.executable, "-c", code], stdout=subprocess.PIPE, text=True)
        unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        try:
            import psutil
            descendant = psutil.Process(int(parent.stdout.readline()))
            with mock.patch("subprocess.Popen", return_value=parent):
                with self.assertRaises(subprocess.TimeoutExpired):
                    run_timed(["owned command"], timeout=.1)
            descendant.wait(timeout=5)
            self.assertIsNotNone(parent.poll())
            self.assertIsNone(unrelated.poll())
        finally:
            if parent.poll() is None:
                parent.kill()
            parent.wait()
            unrelated.kill()
            unrelated.wait()

    def test_five_sample_spreads_do_not_hide_a_single_cpu_tick(self):
        sample = distribution([0, 0, 0, 0, 15.625])
        self.assertEqual(sample["median"], 0)
        self.assertEqual(sample["p90"], 15.625)
        self.assertEqual(sample["max"], 15.625)

    def test_zero_cpu_still_reports_an_absolute_regression(self):
        value = compare(dict(median=15.625, p10=15.625, p90=15.625), dict(median=0, p10=0, p90=0))
        self.assertIn("+15.625", value)
        self.assertIn("outside observed spread", value)
        self.assertNotIn("%", value)


if __name__ == "__main__":
    unittest.main()
