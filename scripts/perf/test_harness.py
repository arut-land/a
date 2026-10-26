"""Execution failures must preserve evidence without turning observations into gates."""
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

import build
import hosted
import lifecycle
import sessions


class Sessions(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.summary = Path(directory.name) / "summary.md"
        summary_env = mock.patch.dict(os.environ, GITHUB_STEP_SUMMARY=str(self.summary))
        summary_env.start()
        self.addCleanup(summary_env.stop)

    def test_failed_subprocesses_write_artifacts_before_returning_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run([sys.executable, str(Path(sessions.__file__)), sys.executable,
                                     "--round-trip", sys.executable, "--rounds", "1", "--sessions", "1",
                                     "--output", directory], capture_output=True, text=True, timeout=30)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Concurrent sessions", self.summary.read_text())
            output = Path(directory)
            summary = json.loads((output / "sessions.json").read_text())
            failures = [json.loads(line) for line in (output / "failures.jsonl").read_text().splitlines()]
            self.assertTrue((output / "sessions.md").is_file())
            self.assertEqual(len(summary["failures"]), len(failures))
            self.assertTrue(all(row["stderr"] for row in failures))
            self.assertEqual(sum(row["runtime"] == "Threads::new(4)" for row in failures), 1)

    def test_timeouts_retain_both_streams_and_finish_the_report(self):
        execute = sessions.execute
        def timeout(*args, **kwargs):
            return execute([sys.executable, "-c",
                            "import sys,time; print('partial out',flush=True); print('partial err',file=sys.stderr,flush=True); time.sleep(60)"],
                           os.environ, timeout=.2)
        with tempfile.TemporaryDirectory() as directory, \
             mock.patch.object(sessions, "execute", side_effect=timeout), \
             contextlib.redirect_stdout(io.StringIO()):
            status = sessions.main([sys.executable, "--round-trip", sys.executable,
                                    "--layouts", "memory", "--rounds", "1", "--sessions", "1", "--output", directory])
            self.assertEqual(status, 1)
            output = Path(directory)
            self.assertTrue((output / "sessions.md").is_file())
            self.assertTrue((output / "sessions.json").is_file())
            failures = [json.loads(line) for line in (output / "failures.jsonl").read_text().splitlines()]
            self.assertEqual(len(failures), 3)
            for row in failures:
                self.assertEqual(row["failed"], "timed out")
                self.assertIn("partial out", row["stdout"])
                self.assertIn("partial err", row["stderr"])

    def test_round_trip_is_one_fixed_baseline_and_partial_sessions_survive_failure(self):
        execute = sessions.execute
        def measurement(command, env):
            if "reactor_round_trip" in command:
                code = "import json; print(json.dumps(dict(p50_us=1,p90_us=2,p99_us=3,cpu_ms=4,switches=5)))"
            elif env["A_BENCH_RUNTIME"] == "threads":
                code = "import sys; print('failed layout',file=sys.stderr); sys.exit(7)"
            else:
                code = ("import json,os; print(json.dumps(dict(runtime=os.environ['A_BENCH_RUNTIME'],sessions=1,"
                        "delta_ms=[1],turn_ms=[2],active_cpu_ms=1000000,active_switches=0,active_wall_ms=2,"
                        "idle_rss_kib=1000000,idle_threads=4,idle_cpu_ms=1000000,idle_switches=0,dropped_deltas=0)))")
            return execute([sys.executable, "-c", code], env)
        with tempfile.TemporaryDirectory() as directory, \
             mock.patch.object(sessions, "execute", side_effect=measurement), \
             contextlib.redirect_stdout(io.StringIO()):
            status = sessions.main([sys.executable, "--round-trip", sys.executable, "--rounds", "2",
                                    "--sessions", "1", "--output", directory])
            self.assertEqual(status, 1)
            output = Path(directory)
            report = (output / "sessions.md").read_text()
            self.assertEqual(report.count("Threads::new(4)"), 1)
            data = json.loads((output / "sessions.json").read_text())
            self.assertEqual(len(data["round_trip"]), 2)
            self.assertNotIn("label", data["round_trip"][0])
            self.assertEqual(data["summary"]["A/1"]["runs"], 2)
            self.assertEqual(len(data["failures"]), 2)
            self.assertTrue((output / "sessions.jsonl").read_text())

    def test_numeric_observations_do_not_fail_a_completed_run(self):
        execute = sessions.execute
        def measurement(command, env):
            code = ("import json,os; print(json.dumps(dict(runtime=os.environ['A_BENCH_RUNTIME'],sessions=1,"
                    "delta_ms=[1000000],turn_ms=[1000000],active_cpu_ms=1000000,active_switches=0,active_wall_ms=2,"
                    "idle_rss_kib=1000000,idle_threads=4,idle_cpu_ms=1000000,idle_switches=0,dropped_deltas=0)))")
            return execute([sys.executable, "-c", code], env)
        with tempfile.TemporaryDirectory() as directory, \
             mock.patch.object(sessions, "execute", side_effect=measurement), \
             contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(sessions.main([sys.executable, "--rounds", "1", "--sessions", "1", "--output", directory]), 0)
            summary = json.loads((Path(directory) / "sessions.json").read_text())["summary"]
            self.assertEqual(summary["A/1"]["idle_rss_kib"], 1000000)


class Builds(unittest.TestCase):
    def test_benchmark_staging_preserves_symbols_and_selected_release_arguments(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "bench"
            binary.write_text("executable")
            symbols = root / "bench.dSYM"
            symbols.mkdir()
            (symbols / "symbols").write_text("debug symbols")
            destination = root / "staged"
            run = mock.Mock(return_value=subprocess.CompletedProcess([], 0, json.dumps(
                dict(executable=str(binary), profile=dict(test=True)))))
            overrides = build.variant_arguments(dict(config=['profile.release.strip="none"'], rustflags=['-Ctarget-cpu=native']))
            result = build.benchmark(root, build.environment(root / "target"), ["-p", "engine", "--lib"],
                                     overrides, ["sys/mimalloc"], destination, run=run)
            self.assertEqual(result.read_text(), "executable")
            self.assertEqual((root / "staged.dSYM/symbols").read_text(), "debug symbols")
            command = run.call_args.args[0]
            self.assertEqual(command[-len(overrides):], overrides)
            self.assertIn("sys/mimalloc", command)
            self.assertIn("--release", command)
            self.assertTrue(all(flag in command for flag in build.STD))

    def test_sessions_preserve_variant_flags_and_sample_selection(self):
        variant = dict(config=['profile.release.opt-level=3'], rustflags=['-Ctarget-cpu=native'], sessions="2,6")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "built"
            binary.touch()
            result = subprocess.CompletedProcess([], 0, json.dumps(dict(executable=str(binary), profile=dict(test=True))))
            with mock.patch.object(hosted, "run", return_value=result) as run, \
                 mock.patch.object(hosted.subprocess, "check_output", return_value="revision"), \
                 mock.patch.object(hosted, "hardware", return_value={}), \
                 mock.patch.object(hosted.platform, "platform", return_value="test host"):
                hosted.sessions(root, root, variant, "")
            builds = [call.args[0] for call in run.call_args_list if call.args[0][0] == "cargo"]
            self.assertEqual(len(builds), 2)
            for command in builds:
                self.assertEqual(command[-len(build.variant_arguments(variant)):], build.variant_arguments(variant))
            command = run.call_args.args[0]
            self.assertEqual(command[command.index("--sessions") + 1], "2,6")


@unittest.skipUnless(os.environ.get("PERF_TEST_BINARY"), "set PERF_TEST_BINARY to a real fixture-enabled a")
class RealDaemon(unittest.TestCase):
    @unittest.skipUnless(os.environ.get("PERF_TEST_PROBE"), "set PERF_TEST_PROBE to the real perf-attach example")
    def test_local_and_hosted_adapters_preserve_the_shared_raw_observations(self):
        binary = str(Path(os.environ["PERF_TEST_BINARY"]).resolve(strict=True))
        local = hosted.load_task("idle-rss")
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            # Shorten this lifecycle check; resource measurements use the production windows.
            def observe(active, seconds):
                return lifecycle.idle_sample(active, .1, settle_seconds=0, retire_seconds=0)
            with mock.patch.object(local, "idle_sample", side_effect=observe):
                raw = local.sample(binary, .1, output)
            self.assertEqual(raw["daemon_cpu_seconds"], raw["after"][0]["cpu_seconds"] - raw["before"][0]["cpu_seconds"])
            with mock.patch.object(hosted, "idle_sample", side_effect=observe):
                metrics = hosted.budgets(output, binary, os.environ["PERF_TEST_PROBE"], samples=1)
            hosted_raw = json.loads((output / "idle-0.json").read_text())
            self.assertEqual(raw.keys(), hosted_raw.keys())
            self.assertEqual(metrics["idle_rss_kib"]["samples"], [hosted_raw["rss"]])
            self.assertEqual(metrics["idle_daemon_cpu_ms"]["samples"], [hosted_raw["daemon_cpu_seconds"] * 1000])
            self.assertEqual(len(json.loads((output / "attach-0.json").read_text())), 500)
            self.assertGreater(metrics["cold_reply_ms"]["median"], 0)
            self.assertGreater(metrics["first_run_ls_ms"]["median"], 0)

    def test_isolated_daemons_stop_after_success_and_failure_and_retain_diagnostics(self):
        import psutil
        binary = str(Path(os.environ["PERF_TEST_BINARY"]).resolve(strict=True))
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            for fail in [False, True]:
                try:
                    with lifecycle.daemon(binary, output, int(fail), first_run=True) as active:
                        daemon = psutil.Process(active.pid)
                        client = psutil.Process(active.client.pid)
                        home = active.home
                        self.assertGreater(active.cold_ms, 0)
                        self.assertGreater(active.first_run_ms, 0)
                        started = time.monotonic()
                        sample = lifecycle.idle_sample(active, .1, settle_seconds=0, retire_seconds=0)
                        self.assertGreaterEqual(sample["seconds"], .1)
                        self.assertLessEqual(sample["seconds"], time.monotonic() - started)
                        self.assertEqual(sample["daemon_cpu_seconds"], sample["after"][0]["cpu_seconds"] - sample["before"][0]["cpu_seconds"])
                        self.assertGreater(sample["rss"], 0)
                        if fail:
                            (home / "logs/retained.txt").write_text("failure diagnostic")
                            raise RuntimeError("sample failed")
                except RuntimeError as error:
                    self.assertTrue(fail)
                    self.assertEqual(str(error), "sample failed")
                self.assertFalse(daemon.is_running())
                self.assertFalse(client.is_running())
                self.assertFalse(home.exists())
            self.assertEqual((output / "failed-1-retained.txt").read_text(), "failure diagnostic")
            self.assertTrue((output / "failed-1-client.stderr").is_file())


if __name__ == "__main__":
    unittest.main()
