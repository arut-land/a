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

from compare import ArtifactRedirect, baseline, compare
from stats import distribution


class Comparison(unittest.TestCase):
    def test_allocator_effects_pair_rounds_before_resampling(self):
        import json
        import tempfile
        import allocator_effects
        records = [dict(label=label, sessions=1, round=round, idle_rss_kib=rss,
                        delta_ms=[1], turn_ms=[2], dropped_deltas=0)
                   for label, round, rss in [("A", 0, 10), ("A", 1, 1000), ("A", 2, 100),
                                             ("A2", 2, 103), ("A2", 0, 13)]]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "sessions.jsonl"
            path.write_text("\n".join(map(json.dumps, records)))
            row = allocator_effects.sessions(path)["A2/1/idle_rss_kib"]
        self.assertEqual(row["rounds"], [0, 2])
        self.assertEqual(row["pairs"], 2)
        self.assertEqual(row["delta"], 3)
        self.assertEqual(row["ci95"], [3, 3])
        self.assertEqual(row["by_order"], {"0": 3})

    def test_a_failed_sibling_lane_never_blocks_a_successful_main_baseline(self):
        import io
        import json
        import zipfile
        archive = io.BytesIO()
        with zipfile.ZipFile(archive, "w") as stream:
            stream.writestr("result.json", json.dumps(dict(variant={}, revision="main-sha", metrics={})))
        def api(path, binary=False):
            if "/workflows/" in path:
                self.assertIn("status=completed", path)
                return {"workflow_runs": [{"id": 3, "head_sha": "main-sha", "conclusion": "failure"},
                                          {"id": 2, "head_sha": "main-sha", "conclusion": "failure"}]}
            if "/jobs?" in path:
                return {"jobs": [{"name": "perf (ubuntu-latest)",
                                  "conclusion": "failure" if "/3/" in path else "success"},
                                 {"name": "perf (windows-11-arm)", "conclusion": "failure"}]}
            if "/artifacts/" in path:
                return archive.getvalue()
            self.assertIn("/2/artifacts", path)
            return {"artifacts": [{"name": "perf-ubuntu-latest", "expired": False, "id": 7}]}
        with mock.patch("compare.api", side_effect=api):
            result, run = baseline("ubuntu-latest", "")
        self.assertEqual(run, 2)
        self.assertEqual(result["revision"], "main-sha")

    @unittest.skipUnless(importlib.util.find_spec("psutil"), "native accounting runs in each perf job")
    def test_bloat_preserves_the_measured_variants_flags_and_package_overrides(self):
        import tempfile
        from hosted import bloat, variant_arguments
        variant = dict(config=["profile.release.package.llm.opt-level=3"],
                       rustflags=["-Ctarget-cpu=x86-64-v2"])
        with tempfile.TemporaryDirectory() as directory, mock.patch("hosted.run") as run:
            run.return_value.stdout = "{}"
            bloat(Path(directory), Path(directory), variant)
            for call in run.call_args_list:
                arguments = call.args[0]
                begin = arguments.index("--config")
                expected = variant_arguments(variant)
                self.assertEqual(arguments[begin:begin + len(expected)], expected)
                self.assertIn('profile.release.strip="none"', arguments)
                self.assertNotIn("--features", arguments)

    def test_default_and_selected_variant_builds_produce_every_selected_benchmark(self):
        import json
        import tempfile
        loader = importlib.machinery.SourceFileLoader("build_variant", str(Path(__file__).resolve().parents[2] / "mise-tasks/perf/build-variant"))
        spec = importlib.util.spec_from_loader(loader.name, loader)
        task = importlib.util.module_from_spec(spec)
        loader.exec_module(task)
        for selection in [[], ["llm"]]:
            with tempfile.TemporaryDirectory() as directory:
                binary = Path(directory) / "build/release" / ("a.exe" if os.name == "nt" else "a")
                binary.parent.mkdir(parents=True)
                binary.touch()
                def run(arguments, **kwargs):
                    return mock.Mock(stdout=json.dumps(dict(executable=str(binary), profile=dict(test=True))))
                with mock.patch.object(sys, "argv", ["build-variant", "z", "--output", directory, "--tests", *selection]), \
                     mock.patch.object(task.subprocess, "run", side_effect=run) as cargo, \
                     mock.patch.object(task.subprocess, "check_output", side_effect=[b"", "revision"]):
                    task.main()
                commands = [call.args[0] for call in cargo.call_args_list if call.args[0][1] == "test"]
                packages = {command[command.index("-p") + 1] for command in commands}
                self.assertEqual(packages, set(selection or ["engine", "protocol", "llm", "tui"]))
                for package in packages:
                    self.assertTrue((Path(directory) / "z" / (package + "-bench" + (".exe" if os.name == "nt" else ""))).is_file())

    @unittest.skipUnless(importlib.util.find_spec("psutil"), "native accounting runs in each perf job")
    def test_process_accounting_never_requests_a_macos_debug_task_port(self):
        from process import snapshot
        process = mock.Mock()
        process.cpu_times.return_value = mock.Mock(user=1, system=2)
        process.memory_info.return_value = mock.Mock(rss=4096)
        with mock.patch("psutil.Process", return_value=process), mock.patch("sys.platform", "darwin"):
            result = snapshot(1, threads=True)
        process.threads.assert_not_called()
        self.assertEqual(result["cpu_seconds"], 3)
        self.assertEqual(result["rss_kib"], 4)

    def test_artifact_redirects_never_forward_github_tokens_to_storage(self):
        import urllib.request
        request = urllib.request.Request("https://api.github.com/artifact", headers={"Authorization": "Bearer test"})
        handler = ArtifactRedirect()
        storage = handler.redirect_request(request, None, 302, "found", {}, "https://storage.example/artifact?signature=test")
        github = handler.redirect_request(request, None, 302, "found", {}, "https://api.github.com/new-artifact")
        self.assertFalse(storage.has_header("Authorization"))
        self.assertEqual(github.get_header("Authorization"), "Bearer test")

    @unittest.skipUnless(importlib.util.find_spec("psutil"), "native accounting runs in each perf job")
    def test_captured_output_does_not_wait_for_an_inheriting_descendant(self):
        import tempfile
        import psutil
        from process import run_timed
        code = "import pathlib, subprocess, sys; p = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)']); pathlib.Path(sys.argv[1]).write_text(str(p.pid)); print('complete')"
        with tempfile.TemporaryDirectory() as directory:
            pidfile = Path(directory) / "owned.pid"
            try:
                result = run_timed([sys.executable, "-c", code, str(pidfile)], timeout=5,
                                   capture_output=True, text=True)
                self.assertEqual(result.stdout, "complete\n")
                self.assertEqual(result.stderr, "")
            finally:
                if pidfile.exists():
                    try:
                        psutil.Process(int(pidfile.read_text())).kill()
                    except psutil.NoSuchProcess:
                        pass

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
