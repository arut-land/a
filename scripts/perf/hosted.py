"""Build before timing; retain every observation for same-runner comparisons."""
import argparse
import faulthandler
import importlib.machinery
import importlib.util
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import psutil

from process import memory_maps, run_timed, snapshot
from build import STD, SUFFIX, benchmark, dumpbin, environment, executable, probe, release_arguments, stage, variant_arguments
from lifecycle import checked, daemon, idle_sample
from stats import distribution

ROOT = Path(__file__).resolve().parents[2]


def run(argv, **kwargs):
    print("+", argv, flush=True)
    argv = list(map(str, argv))
    try:
        # Bound each sample rather than the total time of a benchmark suite.
        bounded = argv[0] == "hyperfine" or "timeout" in kwargs
        kwargs.setdefault("check", True)
        return (run_timed if bounded else subprocess.run)(argv, **kwargs)
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        if error.stdout:
            print(error.stdout, flush=True)
        if error.stderr:
            print(error.stderr, file=sys.stderr, flush=True)
        raise


def native_toolchain():
    """Select Cargo and its siblings together, including descendant lookups."""
    return Path(subprocess.check_output(["rustup", "which", "cargo"], text=True).strip()).parent


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
    tool = dumpbin()
    headers = subprocess.check_output([tool, "/nologo", "/headers", "/dependents", str(binary)], text=True, errors="replace")
    dependencies = [line.strip() for line in headers.splitlines() if line.strip().lower().endswith(".dll")]
    assert dependencies, "dumpbin must report real PE imports"
    return dict(headers=headers, dependencies=dependencies)


def build(source, output, variant, benchmarks=True, packages=("engine", "protocol", "llm", "tui")):
    overrides = variant_arguments(variant)
    if os.environ.get("PERF_ABLATION") == "true":
        packages = ("engine", "llm")
    env = environment(Path(os.environ.get("CARGO_TARGET_DIR", source / "target/perf-build")))
    # Features select an allocator or heap for `a` and the engine's benches
    # alone (docs/perf/allocators.md); the other benches keep their own.
    features = list(variant.get("features", []))
    for fixture in [False, True]:
        selected = features + (["fixtures"] if fixture else [])
        binary = executable(source, env, [*(["--features", ",".join(selected)] if selected else []),
                                        *overrides], run=run)
        name = "a-fixtures" if fixture else "a"
        stage(binary, output / (name + SUFFIX))
    (output / "build.json").write_text(json.dumps(dict(
        revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip(),
        variant=variant,
        shipped_bytes=(output / ("a" + SUFFIX)).stat().st_size,
        shipped_sha256=hashlib.sha256((output / ("a" + SUFFIX)).read_bytes()).hexdigest(),
        fixture_bytes=(output / ("a-fixtures" + SUFFIX)).stat().st_size,
        windows_image=windows_image(output / ("a" + SUFFIX)) if os.name == "nt" else None,
    ), indent=2) + "\n")
    if not benchmarks:
        return
    chosen = ["--features", ",".join(features)] if features else []
    attach = executable(source, env, ["--example", "perf-attach", *chosen, *overrides],
                        run=run, name="perf-attach", kind="example")
    run(["cargo", "test", "--release", *STD, "-p", "a", "--example", "perf-attach", *chosen, *overrides],
        cwd=source, env=env)
    stage(attach, output / ("perf-attach" + SUFFIX))
    for package, target in [("engine", ["--lib"]), ("protocol", ["--test", "bench"]),
                            ("llm", ["--test", "bench"]), ("tui", ["--lib"])]:
        if package not in packages:
            continue
        allocator = [f for f in features if f.startswith("sys/")] if package == "engine" else []
        benchmark(source, env, ["-p", package, *target], overrides, allocator,
                  output / (package + "-bench" + SUFFIX), run=run)
    metadata = json.loads((output / "build.json").read_text())
    metadata["executables"] = {
        p.name: dict(bytes=p.stat().st_size, sha256=hashlib.sha256(p.read_bytes()).hexdigest())
        for p in output.iterdir()
        if p.is_file() and p.name in ["a" + SUFFIX, "a-fixtures" + SUFFIX, "engine-bench" + SUFFIX, "llm-bench" + SUFFIX]
    }
    (output / "build.json").write_text(json.dumps(metadata, indent=2) + "\n")
    probe(source, output, env, variant, run=run)
    run(["rustc", ROOT / "scripts/perf/floor.rs", "-O", "-o", output / ("floor" + SUFFIX)], cwd=source)


def memory_build(source, output, variant, memory_only=True):
    env = environment(Path(os.environ.get("CARGO_TARGET_DIR", source / "target/perf-build")))
    binary = executable(source, env, ["--features", ",".join(["fixtures", *variant.get("features", [])]),
                                      *variant_arguments(variant)], run=run)
    stage(binary, output / ("a-fixtures" + SUFFIX))
    probe(source, output, env, variant, memory_only=memory_only, run=run)


def budgets(output, binary=None, probe=None, samples=5):
    binary = str(binary or output / ("a-fixtures" + SUFFIX))
    cold, first_run, rss, charged, cpu, client_cpu, attach_p50, attach_p99 = [], [], [], [], [], [], [], []
    for trial in range(samples):
        with daemon(binary, output, trial, first_run=True) as active:
            home, env, pid = active.home, active.env, active.pid

            def cli(*args):
                return checked([binary, *args], output, f"{trial}-prep", env=env, cwd=home, timeout=60)

            observation = idle_sample(active, 10, memory_probe=probe or output / ("perf-attach" + SUFFIX))
            first_run.append(observation["first_run_ls_ms"])
            cold.append(observation["cold_reply_ms"])
            rss.append(observation["rss"])
            charged.append(observation["charged_bytes"] / 1024)
            cpu.append(observation["daemon_cpu_seconds"] * 1000)
            client_cpu.append(observation["client_cpu_seconds"] * 1000)
            (output / f"idle-{trial}.json").write_text(json.dumps(observation, indent=2))
            try:
                diagnostic = dict(counters=snapshot(pid), **memory_maps(pid))
            except (OSError, subprocess.SubprocessError, psutil.Error, NotImplementedError, AttributeError, ValueError) as error:
                diagnostic = dict(maps_error=str(error))
            (output / f"memory-{trial}.json").write_text(json.dumps(diagnostic, indent=2))
            session = cli("new").stdout.decode().strip()
            for n in range(10):
                cli("-p", f"message {n}", "resume", session)
            result = cli_probe(output, session, env, home, probe)
            samples = sorted(result)
            attach_p50.append(statistics.median(samples))
            attach_p99.append(samples[495])
            (output / f"attach-{trial}.json").write_text(json.dumps(samples))
    metrics = {name: distribution(values) for name, values in [
        ("cold_reply_ms", cold), ("first_run_ls_ms", first_run), ("attach_p50_ms", attach_p50), ("attach_p99_ms", attach_p99),
        ("idle_rss_kib", rss), ("idle_charged_kib", charged),
        ("idle_daemon_cpu_ms", cpu), ("idle_line_client_cpu_ms", client_cpu)]}
    metrics["idle_daemon_cpu_ms"]["windows"] = cpu
    metrics["idle_line_client_cpu_ms"]["windows"] = client_cpu
    return metrics


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


def bloat(source, output, variant):
    env = environment(Path(os.environ.get("CARGO_TARGET_DIR", source / "target/perf-build")))
    for label, extra in [("crates", ["--crates"]), ("functions", ["-n", "100"] )]:
        result = run(["cargo", "bloat", *release_arguments(), "-p", "a", *variant_arguments(variant),
                      "--config", 'profile.release.strip="none"',
                      "--message-format=json", *extra], cwd=source, env=env,
                     stdout=subprocess.PIPE, text=True)
        (output / ("bloat-" + label + ".json")).write_text(result.stdout)


def paired(output, source, reference):
    reference_source = source.parent / "reference"
    run(["git", "fetch", "--no-tags", "--depth=1", "origin", reference], cwd=source)
    reference = subprocess.check_output(["git", "rev-parse", "FETCH_HEAD"], cwd=source, text=True).strip()
    run(["git", "worktree", "add", "--detach", reference_source, reference], cwd=source)
    paths = ["apps/cli/examples/perf-attach.rs", "crates/engine/src/driver/tests/bench.rs"]
    for path in paths:
        (reference_source / path).parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / path, reference_source / path)
    root = output / "paired"
    root.mkdir(exist_ok=True)
    reference_output = root / "reference"
    reference_output.mkdir(exist_ok=True)
    build(reference_source, reference_output, {}, packages=("engine", "llm"))
    builds = dict(reference=reference_output, candidate=output)
    values = {role: {} for role in builds}
    orders = []
    trials = []
    host = dict(platform=platform.platform(), machine=platform.machine(), hardware=hardware())
    completed = {role: 0 for role in builds}
    candidate_revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip()
    for role in builds:
        values[role]["size_kib"] = [(builds[role] / ("a" + SUFFIX)).stat().st_size / 1024]

    def checkpoint():
        result = dict(driver_turns=500, large_json_results=200,
                      regular_driver_turns=100, regular_large_json_results=20,
                      reference_revision=reference, candidate_revision=candidate_revision,
                      completed_rounds=completed, order=orders, trials=trials, host=host,
                      builds={role: json.loads((directory / "build.json").read_text()) for role, directory in builds.items()},
                      roles={role: {name: distribution(samples) for name, samples in observed.items()}
                             for role, observed in values.items()})
        temporary = output / "paired.json.tmp"
        temporary.write_text(json.dumps(result, indent=2) + "\n")
        temporary.replace(output / "paired.json")

    checkpoint()
    workloads = load_task("workloads")
    for trial in range(int(os.environ.get("PERF_PAIRED_ROUNDS", "5"))):
        trials.append({})
        order = ["reference", "candidate"] if trial % 2 == 0 else ["candidate", "reference"]
        orders.append(order)
        for role in order:
            saved = root / f"{trial}-{role}"
            saved.mkdir(exist_ok=True)
            directory = builds[role]
            observed = budgets(saved, directory / ("a-fixtures" + SUFFIX),
                               directory / ("perf-attach" + SUFFIX), samples=1)
            trials[-1][role] = {name: value["samples"] for name, value in observed.items()}
            for name, value in observed.items():
                values[role].setdefault(name, []).extend(value["samples"])
            for name, argv, runs in [
                ("startup", [str(directory / ("a" + SUFFIX)), "--version"], 20),
                ("driver", [str(directory / ("engine-bench" + SUFFIX)), "bench_turns_under_each_barrier_setting",
                            "--ignored", "--nocapture", "--test-threads=1"], 1),
                ("driver_regular", [str(directory / ("engine-bench" + SUFFIX)), "bench_turns_under_each_barrier_setting",
                                    "--ignored", "--nocapture", "--test-threads=1"], 1),
                ("large_json", [str(directory / ("engine-bench" + SUFFIX)), "bench_large_tool_result",
                                "--ignored", "--nocapture", "--test-threads=1"], 1),
                ("large_json_regular", [str(directory / ("engine-bench" + SUFFIX)), "bench_large_tool_result",
                                        "--ignored", "--nocapture", "--test-threads=1"], 1),
                ("llm_decode", [str(directory / ("llm-bench" + SUFFIX)), "decode_recorded",
                                "--exact", "--ignored", "--nocapture", "--test-threads=1"], 1),
            ]:
                path = saved / (name + ".json")
                env = dict(os.environ)
                if name.startswith("large_json"):
                    env["A_BENCH_JSON_RESULTS"] = "20" if name.endswith("regular") else "200"
                elif name.startswith("driver"):
                    env["A_BENCH_DRIVER_TURNS"] = "50" if name.endswith("regular") else "250"
                run(["hyperfine", "-N", "--warmup", "1", "--runs", str(runs), "--metrics", "all",
                     "--export-json", path, workloads.command(argv)], env=env)
                result = json.loads(path.read_text())["results"][0]
                for field, unit, scale in [("time_wall_clock", "ms", 1000), ("time_cpu", "cpu_ms", 1000),
                                           ("time_user", "user_ms", 1000), ("time_system", "kernel_ms", 1000),
                                           ("memory_peak_resident", "rss_kib", 1 / 1024)]:
                    samples = [sample[field]["value"] * scale for sample in result["measurements"] if field in sample]
                    if samples:
                        values[role].setdefault(name + "_" + unit, []).extend(samples)
                        trials[-1][role][name + "_" + unit] = samples
                if os.name == "nt":
                    from process import measure_windows
                    record = measure_windows(argv, env=env)
                    (saved / (name + "-native.json")).write_text(json.dumps(record))
                    values[role].setdefault(name + "_rss_kib", []).append(record["peak_rss_kib"])
                    trials[-1][role][name + "_rss_kib"] = [record["peak_rss_kib"]]
            completed[role] += 1
            checkpoint()


def sessions(source, output, variant, reference=""):
    """Build only the engine's measurements, then run the concurrent-session layouts.

    A variant naming `allocators` builds the measurement once per allocator
    and compares them (`segment` is Windows' segment heap, through the
    executable's image options) instead of the runtime layouts."""
    def build_benchmark(name, target=("-p", "engine", "--lib"), features=(), checkout=source):
        return benchmark(checkout, environment(Path(os.environ.get("CARGO_TARGET_DIR", checkout / "target/perf-build"))), target,
                         variant_arguments(variant), features, output / (name + SUFFIX), run=run)

    binary = build_benchmark("engine-bench")
    extra = []
    if reference:
        reference_source = source.parent / "reference"
        run(["git", "fetch", "--no-tags", "--depth=1", "origin", reference], cwd=source)
        reference = subprocess.check_output(["git", "rev-parse", "FETCH_HEAD"], cwd=source, text=True).strip()
        run(["git", "worktree", "add", "--detach", reference_source, reference], cwd=source)
        extra.append(f"candidate={binary}")
        binary = build_benchmark("engine-bench-reference", checkout=reference_source)
    for allocator in variant.get("allocators", []):
        if allocator == "segment":
            if os.name != "nt":
                continue
            segmented = output / ("engine-bench-segment" + SUFFIX)
            shutil.copy2(binary, segmented)
            # FrontEndHeapDebugOptions 0x08 opts this image into the segment heap.
            run(["reg", "add", r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options"
                 "\\" + segmented.name, "/v", "FrontEndHeapDebugOptions", "/t", "REG_DWORD", "/d", "8", "/f"])
            extra.append(f"segment={segmented}")
        elif allocator == "jemalloc" and os.name == "nt":
            continue
        else:
            extra.append(f"{allocator}={build_benchmark('engine-bench-' + allocator, features=['sys/' + allocator])}")
    layouts = ["--layouts", "memory", *(arg for entry in extra for arg in ("--binary", entry))] \
        if "allocators" in variant or reference else []
    if "allocators" in variant and not reference:
        layouts.append("--arena-policy")
    round_trip = [] if layouts else ["--round-trip", build_benchmark("rt-round-trip", ("-p", "rt", "--test", "round_trip"))]
    os.environ.update(A_PERF_PYTHON=sys.executable, A_PERF_PROCESS=str(ROOT / "scripts/perf/process.py"))
    (output / "sessions").mkdir(exist_ok=True)
    (output / "sessions" / "host.json").write_text(json.dumps(dict(
        platform=platform.platform(), machine=platform.machine(), hardware=hardware(),
        revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip(),
        reference_revision=reference or None,
    ), indent=2) + "\n")
    run([sys.executable, ROOT / "scripts/perf/sessions.py", binary, "--output", output / "sessions",
         "--rounds", os.environ.get("PERF_PAIRED_ROUNDS", "5"),
         "--sessions", variant.get("sessions", "1,8,32,128"), *layouts, *round_trip], cwd=source)


def instrument_census(source, output):
    """Apply only the fixture hook to a historical measured source revision."""
    changes = {
        "crates/sys/Cargo.toml": ("[features]\n", "[features]\nfixtures = []\n"),
        "crates/sys/src/lib.rs": ("pub mod autostart;", '#[cfg(feature = "fixtures")]\npub mod census;\npub mod autostart;'),
        "apps/cli/Cargo.toml": ('fixtures = ["ext-fixtures/on"]', 'fixtures = ["ext-fixtures/on", "sys/fixtures"]'),
        "apps/cli/src/main.rs": ("    sys::small_footprint();", '    sys::small_footprint();\n    #[cfg(feature = "fixtures")]\n    sys::census::initialize();'),
    }
    module = source / "crates/sys/src/census.rs"
    if not module.exists():
        for name, (old, new) in changes.items():
            path = source / name
            text = path.read_text(encoding="utf-8")
            if text.count(old) != 1:
                raise ValueError(f"cannot instrument {name}: expected one fixture hook insertion point")
            path.write_text(text.replace(old, new), encoding="utf-8", newline="\n")
    if source != ROOT:
        shutil.copy2(ROOT / "crates/sys/src/census.rs", module)
    (output / "census-instrumentation.patch").write_bytes(
        subprocess.check_output(["git", "diff", "--", *changes], cwd=source))
    shutil.copy2(module, output / "census-module.rs")
    (output / "census-source.json").write_text(json.dumps(dict(
        source_revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip(),
        harness_revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        sha256={name: hashlib.sha256((output / name).read_bytes()).hexdigest()
                for name in ("census-instrumentation.patch", "census-module.rs")}), indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--phase", choices=["toolchain", "all", "build", "measure", "paired", "sessions", "residency", "evidence", "memory", "survey"], default="all")
    parser.add_argument("--source", type=Path, default=ROOT)
    parser.add_argument("--output", type=Path, default=ROOT / "target/hosted-perf")
    parser.add_argument("--variant", default="{}", help='JSON: {"rustflags": [...], "config": [...]}')
    parser.add_argument("--runner", required=True)
    parser.add_argument("--paired-ref", default="", help="optional reference revision on this same runner")
    parser.add_argument("--build-only", action="store_true", help="diagnose a build variant without timing")
    parser.add_argument("--bloat", action="store_true", help="also collect crate/function size attribution")
    args = parser.parse_args()
    if args.phase == "toolchain":
        with open(os.environ["GITHUB_PATH"], "a") as path:
            path.write(str(native_toolchain()) + "\n")
        return
    output, source = args.output.resolve(), args.source.resolve()
    if args.phase in ["all", "build"] and output.exists() and any(p.name != "host.json" for p in output.iterdir()):
        parser.error("measurement output must be empty; select a new --output directory")
    output.mkdir(parents=True, exist_ok=True)
    variant = json.loads(args.variant or "{}")
    (output / "host.json").write_text(json.dumps(dict(
        platform=platform.platform(), machine=platform.machine(), hardware=hardware(),
        revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip(),
        workflow_revision=os.environ.get("GITHUB_SHA"), variant=variant,
        toolchain=subprocess.check_output(["rustc", "-vV"], text=True)), indent=2) + "\n")
    if args.phase in ("memory", "evidence", "survey"):
        from evidence import measure
        if args.phase in ("memory", "survey"):
            memory_build(source, output, variant, memory_only=args.phase == "memory")
        if args.phase == "survey":
            binary = benchmark(source, environment(Path(os.environ.get("CARGO_TARGET_DIR", source / "target/perf-build"))),
                               ["-p", "engine", "--lib"], variant_arguments(variant),
                               features=[feature for feature in variant.get("features", []) if feature.startswith("sys/")],
                               destination=output / ("engine-bench" + SUFFIX), run=run)
            run([sys.executable, ROOT / "scripts/perf/sessions.py", binary, "--layouts", "memory",
                 "--rounds", os.environ.get("PERF_EVIDENCE_ROUNDS", "3"), "--sessions", variant.get("sessions", "1,32,128"),
                 "--output", output / "sessions"], cwd=source)
        measure(output / ("a-fixtures" + SUFFIX), output / ("perf-probe" + SUFFIX),
                output / "evidence", rounds=int(os.environ.get("PERF_EVIDENCE_ROUNDS", "3")),
                memory_only=args.phase == "memory")
        return
    if args.phase == "residency":
        if os.environ.get("PERF_ALLOC_CENSUS") == "true":
            instrument_census(source, output)
        if variant or args.paired_ref:
            parser.error("residency measures unchanged-source A/A ownership waves; omit variant and paired-ref")
        run([sys.executable, ROOT / "scripts/perf/residency.py", "--source", source,
             "--output", output, "--target", Path(os.environ["CARGO_TARGET_DIR"]),
             "--rounds", os.environ.get("PERF_PAIRED_ROUNDS", "5"),
             *(["--census"] if os.environ.get("PERF_ALLOC_CENSUS") == "true" else [])], cwd=source)
        return
    if args.phase == "sessions":
        sessions(source, output, variant, args.paired_ref)
        return
    if args.phase in ["all", "build"]:
        build(source, output, variant, benchmarks=not args.build_only)
        if args.bloat:
            bloat(source, output, variant)
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
         "--runs", "20", "--output", output, "--probe", output / ("perf-probe" + SUFFIX)])
    run([sys.executable, ROOT / "mise-tasks/perf/benches", output, output / "cli/transcript.jsonl", "--runs", "5"])
    try:
        summarize(output, source, variant, args.runner)
    finally:
        if args.phase == "all" and args.paired_ref:
            paired(output, source, args.paired_ref)


if __name__ == "__main__":
    main()
