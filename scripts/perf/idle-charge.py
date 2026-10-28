"""Validate native charge against real idle daemons and a throwaway retained allocation."""
import argparse
import hashlib
import importlib
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

from build import SUFFIX, environment, executable
from lifecycle import checked, daemon
from process import snapshot

assert_memory_budget = importlib.import_module("budget-task").assert_memory_budget

ROOT = Path(__file__).resolve().parents[2]
LIMIT = 10 << 20
RETAINED = 16 << 20


def distribution(values):
    return dict(median=statistics.median(values), min=min(values), max=max(values), samples=values)


def run(output, target, rounds):
    output.mkdir(parents=True, exist_ok=True)
    env = environment(target)
    toolchain = Path(subprocess.check_output(["rustup", "which", "cargo"], cwd=ROOT, text=True).strip()).parent
    env["PATH"] = str(toolchain) + os.pathsep + env["PATH"]
    subprocess.run(["cargo", "test", "-p", "sys", "--test", "process_memory"],
                   cwd=ROOT, env=env, check=True)
    probe = executable(ROOT, env, ["--features", "fixtures", "--example", "perf-attach"],
                       name="perf-attach", kind="example")
    shutil.copy2(probe, output / ("memory-probe" + SUFFIX))
    probe = output / ("memory-probe" + SUFFIX)
    baseline = executable(ROOT, env, ["--features", "fixtures"])
    shutil.copy2(baseline, output / ("baseline" + SUFFIX))
    with tempfile.TemporaryDirectory(prefix="a-charge-source-", dir=target.parent) as temporary:
        source = Path(temporary)
        # Copy only repository inputs; never copy local secrets or build output.
        files = subprocess.check_output(
            ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=ROOT
        ).decode().split("\0")
        for name in filter(None, files):
            if ".env.local" in Path(name).parts:
                continue
            destination = source / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / name, destination)
        engine = source / "crates/engine/src/lib.rs"
        text = engine.read_text()
        start = "    let runtime = Arc::new(rt::Threads::configured(rt::Config::daemon()));"
        end = "    let result = run(listener, home, config, build, start, &runtime);"
        assert text.count(start) == text.count(end) == 1
        text = text.replace(start, f"    let retained = std::hint::black_box(vec![0xA5u8; {RETAINED}]);\n" + start)
        engine.write_text(text.replace(end, end + "\n    std::hint::black_box(&retained);"))
        injected = executable(source, env, ["--features", "fixtures"])
        shutil.copy2(injected, output / ("retained" + SUFFIX))
    builds = {role: output / (role + SUFFIX) for role in ("baseline", "retained")}
    metadata = dict(
        revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        working_tree=subprocess.check_output(["git", "status", "--short"], cwd=ROOT, text=True),
        reader_sha256=hashlib.sha256((ROOT / "crates/sys/src/process_memory.rs").read_bytes()).hexdigest(),
        probe_sha256=hashlib.sha256(probe.read_bytes()).hexdigest(),
        limit_bytes=LIMIT, retained_bytes=RETAINED,
        binaries={role: dict(bytes=path.stat().st_size, sha256=hashlib.sha256(path.read_bytes()).hexdigest())
                  for role, path in builds.items()},
    )
    samples = []
    for trial in range(rounds):
        order = ("baseline", "retained") if trial % 2 == 0 else ("retained", "baseline")
        for role in order:
            with daemon(str(builds[role]), output, f"{trial}-{role}") as active:
                time.sleep(3)
                counters = snapshot(active.pid)
                charge = json.loads(checked([str(probe), "--memory", str(active.pid)],
                                            env=active.env, cwd=active.home, timeout=30).stdout)
                import psutil
                process = psutil.Process(active.pid)
                if sys.platform == "linux":
                    independent = process.memory_full_info().pss
                elif sys.platform == "darwin":
                    independent = snapshot(active.pid)["footprint_kib"] * 1024
                else:
                    independent = process.memory_info().private
                try:
                    assert_memory_budget({
                        "idle_charged_kib": {"max": charge["charged_bytes"] / 1024},
                        "idle_rss_kib": {"max": counters["rss_kib"]},
                    })
                except AssertionError:
                    gate_pass = False
                else:
                    gate_pass = True
                observed = dict(trial=trial, role=role, **charge, rss_bytes=int(counters["rss_kib"] * 1024),
                                independent_charge_bytes=independent, gate_pass=gate_pass)
                samples.append(observed)
                (output / "samples.json").write_text(json.dumps(dict(**metadata, samples=samples), indent=2) + "\n")
                print(json.dumps(observed), flush=True)
                assert abs(charge["charged_bytes"] - independent) < 256 << 10, observed
                assert observed["gate_pass"] == (role == "baseline"), observed
    summary = {role: {field: distribution([x[field] for x in samples if x["role"] == role])
                      for field in ("charged_bytes", "rss_bytes")}
               for role in builds}
    (output / "result.json").write_text(json.dumps(dict(**metadata, roles=summary), indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--target", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=5)
    args = parser.parse_args()
    run(args.output.resolve(), args.target.resolve(), args.rounds)
