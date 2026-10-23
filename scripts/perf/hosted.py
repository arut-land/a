"""Build before timing; retain every observation for same-runner comparisons."""
import argparse
import faulthandler
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import platform
import queue
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time

from process import snapshot, stop_daemon, run_timed
from stats import distribution

ROOT = Path(__file__).resolve().parents[2]
STD = ["-Zbuild-std=std,panic_unwind", "-Zbuild-std-features=panic-unwind,optimize_for_size"]
SUFFIX = ".exe" if os.name == "nt" else ""


def run(argv, **kwargs):
    print("+", argv, flush=True)
    argv = list(map(str, argv))
    if argv[0] == "cargo" and "PERF_CARGO" in os.environ:
        argv[0] = os.environ["PERF_CARGO"]
    try:
        # Bound each sample rather than the total time of a benchmark suite.
        bounded = argv[0] == "hyperfine" or "timeout" in kwargs
        return (run_timed if bounded else subprocess.run)(argv, check=True, **kwargs)
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        if error.stdout:
            print(error.stdout, flush=True)
        if error.stderr:
            print(error.stderr, file=sys.stderr, flush=True)
        raise


def load_task(name):
    loader = importlib.machinery.SourceFileLoader(name, str(ROOT / "mise-tasks/perf" / name))
    spec = importlib.util.spec_from_loader(name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


def hardware():
    processor = platform.processor()
    if sys.platform == "linux":
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                processor = line.split(":", 1)[1].strip()
                break
    elif sys.platform == "darwin":
        processor = subprocess.check_output(["sysctl", "-n", "machdep.cpu.brand_string"], text=True).strip()
    return dict(processor=processor, cores=os.cpu_count(), image=os.environ.get("ImageOS"),
                image_version=os.environ.get("ImageVersion"), python=sys.version)


def windows_image(binary):
    tool = shutil.which("dumpbin")
    if tool is None:
        installer = Path(os.environ["ProgramFiles(x86)"]) / "Microsoft Visual Studio/Installer/vswhere.exe"
        location = subprocess.check_output([str(installer), "-latest", "-property", "installationPath"], text=True).strip()
        native = "arm64" if platform.machine().lower() == "arm64" else "x64"
        tools = sorted((Path(location) / "VC/Tools/MSVC").glob(f"*/bin/Host*/{native}/dumpbin.exe"), reverse=True)
        tool = str(tools[0])
    headers = subprocess.check_output([tool, "/nologo", "/headers", "/dependents", str(binary)], text=True, errors="replace")
    dependencies = [line.strip() for line in headers.splitlines() if line.strip().lower().endswith(".dll")]
    assert dependencies, "dumpbin must report real PE imports"
    return dict(headers=headers, dependencies=dependencies)


def build(source, output, variant, benchmarks=True, packages=("engine", "protocol", "tui")):
    config = list(variant.get("config", []))
    flags = variant.get("rustflags", [])
    if flags:
        config.append("profile.release.rustflags=" + json.dumps(["-Cllvm-args=-addrsig", *flags]))
    overrides = [arg for value in config for arg in ("--config", value)]
    env = dict(os.environ, CARGO_TARGET_DIR=str(source / "target/perf-build"),
               CARGO_PROFILE_DEV_CODEGEN_BACKEND="llvm")
    release = Path(env["CARGO_TARGET_DIR"]) / "release"
    for fixture in [False, True]:
        run(["cargo", "build-release", "-p", "a", *(["--features", "fixtures"] if fixture else []),
             *overrides], cwd=source, env=env)
        name = "a-fixtures" if fixture else "a"
        shutil.copy2(release / ("a" + SUFFIX), output / (name + SUFFIX))
    (output / "build.json").write_text(json.dumps(dict(
        revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip(),
        variant=variant,
        shipped_bytes=(output / ("a" + SUFFIX)).stat().st_size,
        fixture_bytes=(output / ("a-fixtures" + SUFFIX)).stat().st_size,
        windows_image=windows_image(output / ("a" + SUFFIX)) if os.name == "nt" else None,
    ), indent=2) + "\n")
    if not benchmarks:
        return
    run(["cargo", "build-release", "-p", "a", "--example", "perf-attach", *overrides], cwd=source, env=env)
    shutil.copy2(release / "examples" / ("perf-attach" + SUFFIX), output / ("perf-attach" + SUFFIX))
    for package, target in [("engine", ["--lib"]), ("protocol", ["--test", "bench"]), ("tui", ["--lib"])]:
        if package not in packages:
            continue
        result = run(["cargo", "test", "--release", *STD, "-p", package, *target,
                      "--no-run", "--message-format=json", *overrides], cwd=source, env=env,
                     stdout=subprocess.PIPE, text=True)
        executables = [record["executable"] for line in result.stdout.splitlines()
                       if (record := json.loads(line)).get("executable") and record.get("profile", {}).get("test")]
        assert len(executables) == 1, executables
        shutil.copy2(executables[0], output / (package + "-bench" + SUFFIX))
    run(["rustc", ROOT / "scripts/perf/floor.rs", "-O", "-o", output / ("floor" + SUFFIX)], cwd=source)


def budgets(output, binary=None, probe=None, samples=5):
    binary = str(binary or output / ("a-fixtures" + SUFFIX))
    cold, rss, cpu, client_cpu, attach_p50, attach_p99 = [], [], [], [], [], []
    root = os.environ.get("RUNNER_TEMP")
    for trial in range(samples):
        with tempfile.TemporaryDirectory(prefix="a-hosted-", dir=root) as directory:
            home = Path(directory)
            (home / "config.toml").write_text('[daemon]\nagent = "echo"\nidle_secs = 120\n')
            env = dict(os.environ, A_HOME=str(home), A_TUI="plain")
            env.pop("A_HOST", None)
            client = None
            def cli(*args):
                return run([binary, *args], env=env, cwd=home, capture_output=True, timeout=60)
            try:
                cli("ls")
                stop_daemon(binary, env, home)
                started = time.perf_counter()
                client = subprocess.Popen([binary], env=env, cwd=home, stdin=subprocess.PIPE,
                                          stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
                first = queue.Queue()
                threading.Thread(target=lambda: first.put(client.stdout.readline()), daemon=True).start()
                assert first.get(timeout=60).startswith(b"[session")
                cold.append((time.perf_counter() - started) * 1000)
                pid = int((home / "run/daemon.pid").read_text().splitlines()[0])
                time.sleep(3)
                rss.append(snapshot(pid)["rss_kib"])
                # Blocking workers retire after ten idle seconds; CPU measures steady idle.
                time.sleep(12)
                before = [snapshot(p, threads=True) for p in (pid, client.pid)]
                time.sleep(10)
                after = [snapshot(p, threads=True) for p in (pid, client.pid)]
                (output / f"idle-{trial}.json").write_text(json.dumps(dict(
                    daemon_pid=pid, client_pid=client.pid, before=before, after=after), indent=2))
                cpu.append((after[0]["cpu_seconds"] - before[0]["cpu_seconds"]) * 1000)
                client_cpu.append((after[1]["cpu_seconds"] - before[1]["cpu_seconds"]) * 1000)
                session = cli("new").stdout.decode().strip()
                for n in range(10):
                    cli("-p", f"message {n}", "resume", session)
                result = cli_probe(output, session, env, home, probe)
                samples = sorted(result)
                attach_p50.append(statistics.median(samples))
                attach_p99.append(samples[495])
                (output / f"attach-{trial}.json").write_text(json.dumps(samples))
            except Exception:
                # Retain the daemon's own explanation before its isolated home disappears.
                for path in (home / "logs").glob("*"):
                    if path.is_file():
                        shutil.copy2(path, output / f"failed-{trial}-{path.name}")
                raise
            finally:
                if client:
                    client.stdin.close()
                    try:
                        client.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        client.kill()
                        client.wait()
                stop_daemon(binary, env, home)
    return {name: distribution(values) for name, values in [
        ("cold_reply_ms", cold), ("attach_p50_ms", attach_p50), ("attach_p99_ms", attach_p99),
        ("idle_rss_kib", rss), ("idle_daemon_cpu_ms", cpu), ("idle_line_client_cpu_ms", client_cpu)]}


def cli_probe(output, session, env, home, probe=None):
    result = run([probe or output / ("perf-attach" + SUFFIX), session], env=env, cwd=home,
                 capture_output=True, text=True, timeout=60)
    return json.loads(result.stdout)


def summarize(output, source, variant, runner):
    metrics = budgets(output)
    for path in sorted(output.rglob("*.json")):
        data = json.loads(path.read_text())
        if isinstance(data, dict) and "results" in data:
            for index, result in enumerate(data["results"]):
                key = (["startup", "process_floor"][index] if path.stem == "startup" else path.stem)
                if len(data["results"]) > 1 and path.stem != "startup":
                    key += f"-{index}"
                for field, unit, scale in [("time_wall_clock", "ms", 1000), ("time_cpu", "cpu_ms", 1000),
                                           ("time_user", "user_ms", 1000), ("time_system", "kernel_ms", 1000),
                                           ("memory_peak_resident", "rss_kib", 1 / 1024)]:
                    values = [sample[field]["value"] * scale for sample in result["measurements"] if field in sample]
                    if values:
                        metrics[key + "_" + unit] = distribution(values)
        elif isinstance(data, dict) and "native_samples" in data:
            metrics[path.stem.removesuffix("-native") + "_rss_kib"] = distribution([
                sample["peak_rss_kib"] for sample in data["native_samples"]])
        elif path.stem == "fresh-home-new":
            metrics["fresh_home_new_ms"] = distribution([x["wall_seconds"] * 1000 for x in data])
            if all("cpu_seconds" in x for x in data):
                metrics["fresh_home_new_cpu_ms"] = distribution([x["cpu_seconds"] * 1000 for x in data])
                metrics["fresh_home_new_rss_kib"] = distribution([x["peak_rss_kib"] for x in data])
    floor = metrics["process_floor_ms"]["median"]
    metrics["cold_reply_over_floor_ms"] = distribution([
        max(0, value - floor) for value in metrics["cold_reply_ms"]["samples"]])
    binary = (output / ("a" + SUFFIX)).read_bytes()
    assert b"Waits for a number of seconds." not in binary
    assert b"Edits a text file by exact replacement." in binary
    metrics["size_kib"] = distribution([len(binary) / 1024])
    metrics["fixture_size_kib"] = distribution([(output / ("a-fixtures" + SUFFIX)).stat().st_size / 1024])
    result = dict(schema=1, runner=runner, platform=platform.platform(), machine=platform.machine(),
                  revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip(),
                  workflow_revision=os.environ.get("GITHUB_SHA"), variant=variant, hardware=hardware(),
                  toolchain=subprocess.check_output(["rustc", "-vV"], text=True),
                  idle_window_seconds=10, cpu_settle_seconds=15, rss_settle_seconds=3,
                  cargo_config=(source / ".cargo/config.toml").read_text(),
                  cargo_profiles=(source / ".cargo/profiles.toml").read_text(),
                  windows_image=windows_image(output / ("a" + SUFFIX)) if os.name == "nt" else None,
                  metrics=metrics)
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    # Keep measurements even when a hard budget fails.
    failures = []
    if metrics["idle_rss_kib"]["max"] >= 10240:
        failures.append("idle daemon RSS reaches 10240 KiB")
    if metrics["idle_daemon_cpu_ms"]["max"] > 0 or metrics["idle_line_client_cpu_ms"]["max"] > 0:
        failures.append("idle CPU is nonzero (OS counter resolution)")
    if failures:
        raise RuntimeError("; ".join(failures))


def bloat(source, output):
    env = dict(os.environ, CARGO_TARGET_DIR=str(source / "target/perf-build"))
    for label, extra in [("crates", ["--crates"]), ("functions", ["-n", "100"] )]:
        result = run(["cargo", "bloat", "--release", "-p", "a", *STD,
                      "--config", 'profile.release.debug="line-tables-only"',
                      "--config", 'profile.release.strip="symbols"',
                      "--message-format=json", *extra], cwd=source, env=env,
                     stdout=subprocess.PIPE, text=True)
        (output / ("bloat-" + label + ".json")).write_text(result.stdout)


def paired(output, source, reference):
    reference_source = source.parent / "reference"
    run(["git", "fetch", "--no-tags", "--depth=1", "origin", reference], cwd=source)
    reference = subprocess.check_output(["git", "rev-parse", "FETCH_HEAD"], cwd=source, text=True).strip()
    run(["git", "worktree", "add", "--detach", reference_source, reference], cwd=source)
    for path in ["apps/cli/examples/perf-attach.rs", "crates/engine/src/driver/tests/bench.rs"]:
        shutil.copy2(ROOT / path, reference_source / path)
    root = output / "paired"
    root.mkdir(exist_ok=True)
    reference_output = root / "reference"
    reference_output.mkdir(exist_ok=True)
    build(reference_source, reference_output, {}, packages=("engine",))
    builds = dict(reference=reference_output, candidate=output)
    values = {role: {} for role in builds}
    orders = []
    completed = {role: 0 for role in builds}
    candidate_revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip()
    for role in builds:
        values[role]["size_kib"] = [(builds[role] / ("a" + SUFFIX)).stat().st_size / 1024]

    def checkpoint():
        result = dict(driver_turns=500, large_json_results=200,
                      reference_revision=reference, candidate_revision=candidate_revision,
                      completed_rounds=completed, order=orders,
                      roles={role: {name: distribution(samples) for name, samples in observed.items()}
                             for role, observed in values.items()})
        temporary = output / "paired.json.tmp"
        temporary.write_text(json.dumps(result, indent=2) + "\n")
        temporary.replace(output / "paired.json")

    checkpoint()
    workloads = load_task("workloads")
    for trial in range(5):
        order = ["reference", "candidate"] if trial % 2 == 0 else ["candidate", "reference"]
        orders.append(order)
        for role in order:
            saved = root / f"{trial}-{role}"
            saved.mkdir(exist_ok=True)
            directory = builds[role]
            observed = budgets(saved, directory / ("a-fixtures" + SUFFIX),
                               directory / ("perf-attach" + SUFFIX), samples=1)
            for name, value in observed.items():
                values[role].setdefault(name, []).extend(value["samples"])
            for name, argv, runs in [
                ("startup", [str(directory / ("a" + SUFFIX)), "--version"], 20),
                ("driver", [str(directory / ("engine-bench" + SUFFIX)), "bench_turns_under_each_barrier_setting",
                            "--ignored", "--nocapture", "--test-threads=1"], 1),
                ("large_json", [str(directory / ("engine-bench" + SUFFIX)), "bench_large_tool_result",
                                "--ignored", "--nocapture", "--test-threads=1"], 1),
            ]:
                path = saved / (name + ".json")
                env = dict(os.environ)
                if name == "large_json":
                    env["A_BENCH_JSON_RESULTS"] = "200"
                elif name == "driver":
                    env["A_BENCH_DRIVER_TURNS"] = "250"
                run(["hyperfine", "-N", "--warmup", "1", "--runs", str(runs), "--metrics", "all",
                     "--export-json", path, workloads.command(argv)], env=env)
                result = json.loads(path.read_text())["results"][0]
                for field, unit, scale in [("time_wall_clock", "ms", 1000), ("time_cpu", "cpu_ms", 1000),
                                           ("time_user", "user_ms", 1000), ("time_system", "kernel_ms", 1000),
                                           ("memory_peak_resident", "rss_kib", 1 / 1024)]:
                    samples = [sample[field]["value"] * scale for sample in result["measurements"] if field in sample]
                    if samples:
                        values[role].setdefault(name + "_" + unit, []).extend(samples)
                if os.name == "nt":
                    from process import measure_windows
                    record = measure_windows(argv, env=env)
                    (saved / (name + "-native.json")).write_text(json.dumps(record))
                    values[role].setdefault(name + "_rss_kib", []).append(record["peak_rss_kib"])
            completed[role] += 1
            checkpoint()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--phase", choices=["all", "build", "measure", "paired"], default="all")
    parser.add_argument("--source", type=Path, default=ROOT)
    parser.add_argument("--output", type=Path, default=ROOT / "target/hosted-perf")
    parser.add_argument("--variant", default="{}", help='JSON: {"rustflags": [...], "config": [...]}')
    parser.add_argument("--runner", required=True)
    parser.add_argument("--paired-ref", default="", help="optional reference revision on this same runner")
    parser.add_argument("--build-only", action="store_true", help="diagnose a build variant without timing")
    parser.add_argument("--bloat", action="store_true", help="also collect crate/function size attribution")
    args = parser.parse_args()
    output, source = args.output.resolve(), args.source.resolve()
    if args.phase in ["all", "build"] and output.exists() and any(output.iterdir()):
        parser.error("measurement output must be empty; select a new --output directory")
    output.mkdir(parents=True, exist_ok=True)
    variant = json.loads(args.variant or "{}")
    if args.phase in ["all", "build"]:
        build(source, output, variant, benchmarks=not args.build_only)
        if args.bloat:
            bloat(source, output)
    if args.build_only or args.phase == "build":
        return
    # Native exec and accounting can stall before a subprocess timeout is armed.
    faulthandler.dump_traceback_later(300, repeat=True)
    os.environ.update(A_PERF_PYTHON=sys.executable, A_PERF_PROCESS=str(ROOT / "scripts/perf/process.py"))
    if args.phase == "paired":
        if not args.paired_ref:
            parser.error("paired measurements require --paired-ref")
        paired(output, source, args.paired_ref)
        return
    workloads = load_task("workloads")
    run(["hyperfine", "-N", "--warmup", "10", "--runs", "100", "--metrics", "all",
         "--export-json", output / "startup.json", workloads.command([str(output / ("a" + SUFFIX)), "--version"]),
         workloads.command([str(output / ("floor" + SUFFIX))])])
    if os.name == "nt":
        from process import measure_windows
        samples = [measure_windows([str(output / ("a" + SUFFIX)), "--version"]) for _ in range(5)]
        (output / "startup-native.json").write_text(json.dumps({"native_samples": samples}))
    run([sys.executable, ROOT / "mise-tasks/perf/workloads", output / ("a-fixtures" + SUFFIX), "cli",
         "--runs", "20", "--output", output])
    run([sys.executable, ROOT / "mise-tasks/perf/benches", output, output / "cli/transcript.jsonl", "--runs", "5"])
    try:
        summarize(output, source, variant, args.runner)
    finally:
        if args.phase == "all" and args.paired_ref:
            paired(output, source, args.paired_ref)


if __name__ == "__main__":
    main()
