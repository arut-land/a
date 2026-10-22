"""Build before timing; retain every observation for same-runner comparisons."""
import argparse
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

from process import snapshot

ROOT = Path(__file__).resolve().parents[2]
STD = ["-Zbuild-std=std,panic_unwind", "-Zbuild-std-features=panic-unwind,optimize_for_size"]
SUFFIX = ".exe" if os.name == "nt" else ""


def run(argv, **kwargs):
    print("+", argv, flush=True)
    argv = list(map(str, argv))
    if argv[0] == "cargo" and "PERF_CARGO" in os.environ:
        argv[0] = os.environ["PERF_CARGO"]
    return subprocess.run(argv, check=True, **kwargs)


def load_task(name):
    loader = importlib.machinery.SourceFileLoader(name, str(ROOT / "mise-tasks/perf" / name))
    spec = importlib.util.spec_from_loader(name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


def distribution(samples):
    samples = sorted(samples)
    return dict(median=statistics.median(samples), min=min(samples), max=max(samples),
                p10=samples[int((len(samples) - 1) * .1)],
                p90=samples[int((len(samples) - 1) * .9)], samples=samples)


def build(source, output, variant):
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
    ), indent=2) + "\n")
    run(["cargo", "build-release", "-p", "a", "--example", "perf-attach", *overrides], cwd=source, env=env)
    shutil.copy2(release / "examples" / ("perf-attach" + SUFFIX), output / ("perf-attach" + SUFFIX))
    for package, target in [("engine", ["--lib"]), ("protocol", ["--test", "bench"]), ("tui", ["--lib"])]:
        result = run(["cargo", "test", "--release", *STD, "-p", package, *target,
                      "--no-run", "--message-format=json", *overrides], cwd=source, env=env,
                     stdout=subprocess.PIPE, text=True)
        executables = [record["executable"] for line in result.stdout.splitlines()
                       if (record := json.loads(line)).get("executable") and record.get("profile", {}).get("test")]
        assert len(executables) == 1, executables
        shutil.copy2(executables[0], output / (package + "-bench" + SUFFIX))
    run(["rustc", ROOT / "scripts/perf/floor.rs", "-O", "-o", output / ("floor" + SUFFIX)], cwd=source)


def budgets(output, binary=None, probe=None):
    binary = str(binary or output / ("a-fixtures" + SUFFIX))
    cold, rss, cpu, client_cpu, attach_p50, attach_p99 = [], [], [], [], [], []
    root = os.environ.get("RUNNER_TEMP")
    for trial in range(5):
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
                cli("daemon", "stop")
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
                before = [snapshot(p)["cpu_seconds"] for p in (pid, client.pid)]
                time.sleep(10)
                after = [snapshot(p) for p in (pid, client.pid)]
                cpu.append((after[0]["cpu_seconds"] - before[0]) * 1000)
                client_cpu.append((after[1]["cpu_seconds"] - before[1]) * 1000)
                session = cli("new").stdout.decode().strip()
                for n in range(10):
                    cli("-p", f"message {n}", "resume", session)
                result = cli_probe(output, session, env, home, probe)
                samples = sorted(result)
                attach_p50.append(statistics.median(samples))
                attach_p99.append(samples[495])
                (output / f"attach-{trial}.json").write_text(json.dumps(samples))
            finally:
                if client:
                    client.stdin.close()
                    try:
                        client.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        client.kill()
                        client.wait()
                cli("daemon", "stop")
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
                  workflow_revision=os.environ.get("GITHUB_SHA"), variant=variant,
                  toolchain=subprocess.check_output(["rustc", "-vV"], text=True),
                  idle_window_seconds=10, cpu_settle_seconds=15, rss_settle_seconds=3,
                  cargo_config=(source / ".cargo/config.toml").read_text(),
                  cargo_profiles=(source / ".cargo/profiles.toml").read_text(),
                  metrics=metrics)
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    # Keep measurements even when a hard budget fails.
    failures = []
    if len(binary) > 4096 * 1024:
        failures.append("shipped size exceeds 4096 KiB")
    if metrics["idle_rss_kib"]["max"] >= 10240:
        failures.append("idle daemon RSS reaches 10240 KiB")
    if metrics["idle_daemon_cpu_ms"]["max"] > 0 or metrics["idle_line_client_cpu_ms"]["max"] > 0:
        failures.append("idle CPU is nonzero (OS counter resolution)")
    if failures:
        raise RuntimeError("; ".join(failures))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path, default=ROOT)
    parser.add_argument("--output", type=Path, default=ROOT / "target/hosted-perf")
    parser.add_argument("--variant", default="{}", help='JSON: {"rustflags": [...], "config": [...]}')
    parser.add_argument("--runner", required=True)
    args = parser.parse_args()
    output, source = args.output.resolve(), args.source.resolve()
    output.mkdir(parents=True, exist_ok=True)
    variant = json.loads(args.variant or "{}")
    build(source, output, variant)
    os.environ.update(A_PERF_PYTHON=sys.executable, A_PERF_PROCESS=str(ROOT / "scripts/perf/process.py"))
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
    summarize(output, source, variant, args.runner)


if __name__ == "__main__":
    main()
