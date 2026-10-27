"""Execution failures must preserve evidence without turning observations into gates."""
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import shutil
import sys
import tempfile
import time
import unittest
from unittest import mock

import build
import hosted
import lifecycle
import sessions
import evidence
import process


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

    def test_successful_measurement_processes_cannot_hide_failed_calls(self):
        execute = sessions.execute
        def measurement(command, env):
            code = ("import json; print(json.dumps(dict(runtime='workers=4',sessions=1,"
                    "delta_ms=[1],turn_ms=[2],active_cpu_ms=0,active_switches=0,active_wall_ms=2,"
                    "idle_rss_kib=1,idle_threads=4,idle_cpu_ms=0,idle_switches=0,dropped_deltas=0,failed_calls=1)))")
            return execute([sys.executable, "-c", code], env)
        with tempfile.TemporaryDirectory() as directory, \
             mock.patch.object(sessions, "execute", side_effect=measurement), \
             contextlib.redirect_stdout(io.StringIO()):
            status = sessions.main([sys.executable, "--layouts", "memory", "--rounds", "1",
                                    "--sessions", "1", "--output", directory])
            self.assertEqual(status, 1)
            failures = (Path(directory) / "failures.jsonl").read_text().splitlines()
            self.assertEqual(len(failures), 2)
            self.assertTrue(all(json.loads(line)["failed"] == "1 failed calls" for line in failures))


class Builds(unittest.TestCase):
    @unittest.skipUnless(sys.version_info >= (3, 11) and shutil.which("cargo"), "probe builds require Python 3.11+ and Cargo")
    def test_generated_workload_manifests_stay_independent_inside_another_workspace(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text('[workspace]\nmembers=[]\n')
            output = root / "output"
            output.mkdir()
            binary = root / "probe"
            binary.write_bytes(b"artifact")
            def cargo(arguments, **kwargs):
                manifest = arguments[arguments.index("--manifest-path") + 1]
                metadata = subprocess.run(["cargo", "metadata", "--no-deps", "--format-version=1",
                    "--manifest-path", manifest], cwd=kwargs["cwd"], env=kwargs["env"],
                    capture_output=True, text=True, check=True, timeout=120)
                self.assertEqual(Path(json.loads(metadata.stdout)["workspace_root"]).resolve(), Path(manifest).parent.resolve())
                return subprocess.CompletedProcess(arguments, 0, json.dumps(dict(executable=str(binary))))
            build.probe(Path(__file__).resolve().parents[2], output, build.environment(root / "target"),
                        memory_only=True, run=cargo)

    def test_executable_discovery_uses_the_cargo_artifact_in_a_custom_target(self):
        with tempfile.TemporaryDirectory() as directory:
            actual = Path(directory) / "custom" / "a.exe"
            records = [dict(reason="build-finished", success=True), dict(
                executable=str(actual), target=dict(name="a", kind=["bin"]))]
            run = mock.Mock(return_value=subprocess.CompletedProcess([], 0, "\n".join(map(json.dumps, records))))
            self.assertEqual(build.executable(Path(directory), arguments=["--features", "fixtures"], run=run), actual)
            args = run.call_args.args[0]
            self.assertTrue(all(flag in args for flag in build.release_arguments()))
            self.assertIn("--message-format=json", args)

    def test_symbol_staging_keeps_every_native_sidecar_with_binary_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "a.exe"
            source.write_bytes(b"real artifact")
            source.with_suffix(".pdb").write_bytes(b"PDB")
            Path(str(source) + ".dwp").write_bytes(b"DWP")
            destination = root / "fixture.exe"
            build.stage(source, destination)
            identity = json.loads(Path(str(destination) + ".identity.json").read_text())
            self.assertEqual(set(identity["files"]), {"fixture.exe", "fixture.pdb", "fixture.exe.dwp"})
            self.assertEqual(destination.with_suffix(".pdb").read_bytes(), b"PDB")
            self.assertEqual(identity["files"]["fixture.exe"]["bytes"], source.stat().st_size)

    def test_memory_build_never_requests_unrelated_test_binaries(self):
        with tempfile.TemporaryDirectory() as directory, \
             mock.patch.object(hosted, "executable") as executable, \
             mock.patch.object(hosted, "stage"), mock.patch.object(hosted, "probe") as probe, \
             mock.patch.object(hosted, "benchmark") as benchmark:
            hosted.memory_build(Path(directory), Path(directory), {})
            benchmark.assert_not_called()
            self.assertTrue(probe.call_args.kwargs["memory_only"])
            self.assertIn("fixtures", executable.call_args.args[2])

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
                 mock.patch.object(sys, "platform", "win32"), \
                 mock.patch.object(build, "dumpbin", return_value="perf-test-no-native-tool"), \
                 mock.patch.object(build.shutil, "which", return_value=None), \
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
    @unittest.skipUnless(os.environ.get("PERF_TEST_WORKLOAD_PROBE"), "set PERF_TEST_WORKLOAD_PROBE to the real survey probe")
    def test_subscription_churn_and_replay_probes_complete_and_release_their_clients(self):
        binary = Path(os.environ["PERF_TEST_BINARY"]).resolve(strict=True)
        probe = Path(os.environ["PERF_TEST_WORKLOAD_PROBE"]).resolve(strict=True)
        with tempfile.TemporaryDirectory() as directory, lifecycle.daemon(str(binary)) as active:
            active.output, active.memory_only = Path(directory), False
            for mode in ("watch", "attach", "bare", "clients"):
                result = evidence.memory_probe(binary, probe, active, mode, 4)
                self.assertEqual(result["stages"][-1]["stage"], "detached")
                self.assertGreater(result["stages"][-1]["retired"]["rss_kib"], 0)

    @unittest.skipUnless(os.environ.get("PERF_TEST_WORKLOAD_PROBE") and os.name != "nt", "native PTY survey probe")
    def test_pty_cache_probe_renders_real_turns_and_clears_the_display_cache(self):
        binary = Path(os.environ["PERF_TEST_BINARY"]).resolve(strict=True)
        probe = Path(os.environ["PERF_TEST_WORKLOAD_PROBE"]).resolve(strict=True)
        with tempfile.TemporaryDirectory() as directory, lifecycle.daemon(str(binary)) as active:
            active.output, active.memory_only = Path(directory), False
            result = evidence.memory_probe(binary, probe, active, "turns", 32)
            counted = next(stage for stage in result["stages"] if stage["stage"] == "32")
            self.assertGreater(counted["tui"]["rss_kib"], 0)
            self.assertGreater(counted["tui_after_clear"]["rss_kib"], 0)

    def test_concurrent_image_workload_checks_each_real_tool_result(self):
        with tempfile.TemporaryDirectory() as directory:
            result = evidence.sample(("image-concurrent", False),
                Path(os.environ["PERF_TEST_BINARY"]).resolve(strict=True), Path("unused"), Path(directory))
        self.assertEqual(result["count"], 8)
        self.assertEqual(len(result["results"]), 8)
        self.assertEqual(result["input_bytes"], 36000054)

    def test_cli_success_cannot_hide_a_failed_tool(self):
        binary = str(Path(os.environ["PERF_TEST_BINARY"]).resolve(strict=True))
        workload = hosted.load_task("workloads")
        with lifecycle.daemon(binary) as active:
            result = subprocess.run([binary, "--mode", "auto", "-p", "call missing_perf_tool {}"],
                                    env=active.env, cwd=active.home, capture_output=True, timeout=30)
            self.assertEqual(result.returncode, 0)
            with self.assertRaisesRegex(ValueError, "tool did not complete"):
                workload.validate_tools(workload.tool_results(active.home), 1)

    def test_cli_success_cannot_hide_a_nonzero_shell_exit(self):
        binary = str(Path(os.environ["PERF_TEST_BINARY"]).resolve(strict=True))
        workload = hosted.load_task("workloads")
        with lifecycle.daemon(binary) as active:
            subprocess.run([binary, "--mode", "auto", "-p", workload.shell_call("exit 17")],
                           env=active.env, cwd=active.home, capture_output=True, timeout=30, check=True)
            with self.assertRaisesRegex(ValueError, "shell did not succeed"):
                workload.validate_tools(workload.tool_results(active.home), 1)

    def test_large_shell_output_is_validated_from_the_real_blob_reference(self):
        binary = str(Path(os.environ["PERF_TEST_BINARY"]).resolve(strict=True))
        workload = hosted.load_task("workloads")
        with lifecycle.daemon(binary) as active:
            result = subprocess.run([binary, "--mode", "auto", "-p", workload.large_output_prompt()], env=active.env, cwd=active.home,
                capture_output=True, timeout=60)
            result.check_returncode()
            records = workload.tool_results(active.home)
            workload.validate_tools(records, 1, 1048576)
            with self.assertRaisesRegex(ValueError, "expected a"):
                workload.validate_tools(records, 1, 1048575)

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
            self.assertEqual(metrics["cold_reply_ms"]["samples"], [hosted_raw["cold_reply_ms"]])
            self.assertEqual(metrics["first_run_ls_ms"]["samples"], [hosted_raw["first_run_ls_ms"]])

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
                        self.assertEqual(sample["cold_reply_ms"], active.cold_ms)
                        self.assertEqual(sample["first_run_ls_ms"], active.first_run_ms)
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
            self.assertEqual(json.loads((output / "failed-1-client.json").read_text()), dict(stderr=""))


class Diagnostics(unittest.TestCase):
    def test_hardware_metadata_survives_a_build_failure(self):
        with tempfile.TemporaryDirectory() as directory, \
             mock.patch.object(sys, "argv", ["hosted", "--phase", "build", "--runner", "test", "--output", directory]), \
             mock.patch.object(hosted, "hardware", return_value=dict(cores=4)), \
             mock.patch.object(hosted.platform, "platform", return_value="test host"), \
             mock.patch.object(hosted.subprocess, "check_output", return_value="toolchain"), \
             mock.patch.object(hosted, "build", side_effect=RuntimeError("build failed")):
            (Path(directory) / "host.json").write_text('{"before_toolchain":true}')
            with self.assertRaisesRegex(RuntimeError, "build failed"):
                hosted.main()
            self.assertEqual(json.loads((Path(directory) / "host.json").read_text())["hardware"]["cores"], 4)

    def test_windows_map_region_sizes_cannot_be_reported_as_rss(self):
        region = mock.Mock()
        region._asdict.return_value = dict(addr="0x1000", path="a.exe", rss=16384)
        native = mock.Mock()
        native.memory_maps.return_value = [region]
        native.exe.return_value = "a.exe"
        def resident(pid, maps):
            self.assertNotIn("rss", maps[0])
            maps[0]["resident_bytes"] = 4096
        with mock.patch.object(sys, "platform", "win32"), \
             mock.patch.object(process.psutil, "Process", return_value=native), \
             mock.patch.object(process, "windows_residency", side_effect=resident):
            result = process.memory_maps(1)
        self.assertEqual(result["maps"][0]["mapped_bytes"], 16384)
        self.assertEqual(result["executable_resident_bytes"], 4096)

    def test_failed_prep_preserves_both_streams_after_the_home_is_removed(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            with tempfile.TemporaryDirectory() as home:
                with self.assertRaises(subprocess.CalledProcessError):
                    lifecycle.checked([sys.executable, "-c",
                        "import sys; print('prep out'); print('prep err',file=sys.stderr); sys.exit(7)"],
                        output, "prep", cwd=home)
            record = json.loads((output / "failed-prep.json").read_text())
            self.assertEqual(record["returncode"], 7)
            self.assertIn("prep out", record["stdout"])
            self.assertIn("prep err", record["stderr"])

    def test_external_accounting_does_not_launch_children_in_the_measured_process(self):
        code = ("import sys,json,os,subprocess; print('perf-snapshot',file=sys.stderr,flush=True); "
                "s=json.loads(input()); assert s['rss_kib']>0; "
                "import psutil; assert not psutil.Process().children(); print('observed')")
        out, err, failed = sessions.execute([sys.executable, "-c", code],
                                           dict(os.environ, A_PERF_OBSERVER="1"), timeout=10)
        self.assertIsNone(failed, err)
        self.assertEqual(out.strip(), "observed")
        self.assertEqual(err, "")

    def test_external_accounting_timeouts_keep_partial_diagnosis(self):
        out, err, failed = sessions.execute([sys.executable, "-c",
            "import sys,time; print('out',flush=True); print('err',file=sys.stderr,flush=True); time.sleep(60)"],
            dict(os.environ, A_PERF_OBSERVER="1"), timeout=.3)
        self.assertEqual(failed, "timed out")
        self.assertIn("out", out)
        self.assertIn("err", err)

    def test_the_measured_process_stays_blocked_between_idle_snapshots(self):
        code = ("import sys,json; print('perf-idle',file=sys.stderr,flush=True); "
                "window=json.loads(input()); assert window['seconds']>=.2; "
                "assert window['before']['cpu_seconds']==window['after']['cpu_seconds']; print('idle')")
        out, err, failed = sessions.execute([sys.executable, "-c", code],
            dict(os.environ, A_PERF_OBSERVER="1", A_PERF_IDLE_SECONDS=".2"), timeout=10)
        self.assertIsNone(failed, err)
        self.assertEqual(out.strip(), "idle")

    def test_native_fault_counters_are_retained_without_a_debug_task_port(self):
        sample = process.snapshot(os.getpid())
        self.assertGreater(sample["rss_kib"], 0)
        self.assertTrue(set(sample) & {"pfaults", "num_page_faults", "minor_faults"}, sample)

    def test_executable_residency_comes_from_the_native_memory_map(self):
        result = process.memory_maps(os.getpid())
        self.assertGreater(result["executable_resident_bytes"], 0)
        self.assertGreater(result["executable_code_resident_bytes"], 0)

    @unittest.skipUnless(os.name == "nt", "native Windows residency")
    def test_windows_virtual_maps_are_distinct_from_working_set_residency(self):
        result = process.memory_maps(os.getpid())
        self.assertGreater(result["executable_resident_bytes"], 0)
        for region in result["maps"]:
            self.assertNotIn("rss", region)
            self.assertGreaterEqual(region["mapped_bytes"], region["resident_bytes"])
        self.assertLess(sum(m["resident_bytes"] for m in result["maps"]),
                        sum(m["mapped_bytes"] for m in result["maps"]))

    def test_workload_rounds_alternate_controls_and_checkpoint_each_observation(self):
        calls = []
        def sample(case, *args):
            calls.append(case)
            return dict(ms=len(calls))
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(evidence, "sample", side_effect=sample):
            result = evidence.measure(Path("a"), Path("probe"), Path(directory), 3,
                                      cases=["quiet-shell", "reply-history"])
            self.assertEqual(calls[4:8], list(reversed(calls[:4])))
            self.assertEqual(len((Path(directory) / "samples.jsonl").read_text().splitlines()), 12)
            self.assertEqual(result["summary"]["quiet-shell/control_ms"]["min"], 1)
            self.assertEqual(result["summary"]["quiet-shell/control_ms"]["max"], 9)


if __name__ == "__main__":
    unittest.main()
