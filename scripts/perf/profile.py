"""Instruction attribution on a hosted Linux runner, separate from timings."""
import json
from pathlib import Path
import subprocess
import sys

from hosted import ROOT, run
from build import benchmark, environment, variant_arguments


source = Path(sys.argv[1]).resolve()
output = ROOT / "target/hosted-perf/profile"
output.mkdir(parents=True, exist_ok=True)
env = environment(source / "target/perf-build")
metadata = dict(revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip(),
                profiler=subprocess.check_output(["valgrind", "--version"], text=True).strip(),
                config=['profile.release.strip="none"', 'profile.release.split-debuginfo="off"'])
(output / "build.json").write_text(json.dumps(metadata, indent=2))
for package, target, scenarios in [
    ("engine", ["--lib"], ["bench_turns_under_each_barrier_setting", "bench_large_tool_result"]),
    ("llm", ["--test", "bench"], ["decode_recorded"]),
]:
    binary = benchmark(source, env, ["-p", package, *target],
                       variant_arguments(metadata), run=run)
    for scenario in scenarios:
        trace = output / (scenario + ".callgrind")
        result = run(["valgrind", "--tool=callgrind", "--callgrind-out-file=" + str(trace),
                      binary, scenario, "--ignored", "--nocapture", "--test-threads=1"],
                     cwd=source, env=env, timeout=600, capture_output=True, text=True)
        (output / (scenario + "-output.json")).write_text(json.dumps(dict(stdout=result.stdout, stderr=result.stderr)))
        for inclusive in ["no", "yes"]:
            annotation = run(["callgrind_annotate", "--auto=no", "--inclusive=" + inclusive,
                              "--threshold=95", str(trace)], capture_output=True, text=True)
            (output / (scenario + "-inclusive-" + inclusive + ".md")).write_text("```text\n" + annotation.stdout + "\n```\n")
