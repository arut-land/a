"""Concurrently active sessions under each runtime layout, paired with an A/A control.

Runs the engine's `concurrent_active_sessions` measurement in a fresh process
per sample (idle RSS must not inherit an earlier layout's heap), alternating
layout order each round. `workers=4` runs twice under two labels; the gap
between them is the noise band any other layout must clear.
"""
import argparse
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys

TEST = "tests::runtime_demand::concurrent_active_sessions"


def percentile(values, q):
    values = sorted(values)
    return values[min(len(values) - 1, int(len(values) * q))]


def layouts(cores):
    labels = [("A", "workers=4"), ("A2", "workers=4")]
    if cores != 4:
        labels.append(("cores", f"workers={cores}"))
    labels.append(("threads", "threads"))
    # Not a candidate: what each streamed request holding a network worker costs.
    labels.append(("network", "workers=4,network=128"))
    return labels


def summarize(records):
    groups = {}
    for record in records:
        groups.setdefault((record["label"], record["sessions"]), []).append(record)
    summary = {}
    for (label, sessions), runs in sorted(groups.items()):
        deltas = [d for run in runs for d in run["delta_ms"]]
        turns = [t for run in runs for t in run["turn_ms"]]
        summary[f"{label}/{sessions}"] = dict(
            label=label, runtime=runs[0]["runtime"], sessions=sessions, runs=len(runs),
            delta_samples=len(deltas),
            delta_p50_ms=statistics.median(deltas), delta_p99_ms=percentile(deltas, 0.99),
            run_delta_p99_ms=statistics.median(percentile(run["delta_ms"], 0.99) for run in runs),
            turn_p50_ms=statistics.median(turns), turn_p99_ms=percentile(turns, 0.99),
            active_cpu_ms=statistics.median(run["active_cpu_ms"] for run in runs),
            active_wall_ms=statistics.median(run["active_wall_ms"] for run in runs),
            idle_rss_kib=statistics.median(run["idle_rss_kib"] for run in runs),
            idle_threads=statistics.median(run["idle_threads"] for run in runs),
            idle_cpu_ms=statistics.median(run["idle_cpu_ms"] for run in runs),
            dropped_deltas=sum(run["dropped_deltas"] for run in runs),
            failed_calls=sum(run.get("failed_calls", 0) for run in runs),
        )
    return summary


def table(summary):
    lines = ["| layout | sessions | delta p50 ms | delta p99 ms | run p99 ms | turn p50 ms | turn p99 ms "
             "| active CPU ms | idle RSS KiB | idle threads | dropped deltas | failed calls |",
             "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for row in summary.values():
        lines.append("| {label} ({runtime}) | {sessions} | {delta_p50_ms:.3f} | {delta_p99_ms:.3f} "
                     "| {run_delta_p99_ms:.3f} | {turn_p50_ms:.0f} | {turn_p99_ms:.0f} | {active_cpu_ms:.0f} "
                     "| {idle_rss_kib:.0f} | {idle_threads:.0f} | {dropped_deltas} | {failed_calls} |".format(**row))
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="the engine's release lib test executable")
    parser.add_argument("--output", type=Path, default=Path("target/perf-sessions"))
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--sessions", default="1,8,32,128")
    parser.add_argument("--turns", type=int, default=3)
    parser.add_argument("--pace-ms", type=int, default=10)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    cores = os.cpu_count()
    labels = layouts(cores)
    counts = [int(n) for n in args.sessions.split(",")]
    raw = args.output / "sessions.jsonl"
    records, failures = [], []
    for round in range(args.rounds):
        order = labels if round % 2 == 0 else list(reversed(labels))
        for sessions in counts:
            for label, runtime in order:
                env = dict(os.environ, A_BENCH_RUNTIME=runtime, A_BENCH_SESSIONS=str(sessions),
                           A_BENCH_TURNS=str(args.turns), A_BENCH_PACE_MS=str(args.pace_ms))
                try:
                    result = subprocess.run(
                        [str(args.binary), TEST, "--exact", "--ignored", "--nocapture", "--test-threads=1"],
                        env=env, capture_output=True, text=True, timeout=300)
                    failed = result.returncode and f"exit {result.returncode}"
                    out, err = result.stdout, result.stderr
                except subprocess.TimeoutExpired as error:
                    failed = "timed out"
                    out, err = (text.decode(errors="replace") if isinstance(text, bytes) else text or ""
                                for text in (error.stdout, error.stderr))
                if failed:
                    # A stalled layout is a finding, not the end of the others' samples.
                    failure = dict(label=label, runtime=runtime, sessions=sessions, round=round,
                                   failed=failed, stdout=out[-4000:], stderr=err[-4000:])
                    failures.append(failure)
                    with (args.output / "failures.jsonl").open("a") as stream:
                        stream.write(json.dumps(failure) + "\n")
                    print(label, runtime, sessions, round, failed, err[-2000:], sep="\n", flush=True)
                    continue
                # The harness prints the test's name on the line its record starts.
                line = next(line for line in out.splitlines() if '{"' in line)
                record = json.loads(line[line.index('{"'):])
                record.update(label=label, round=round)
                records.append(record)
                with raw.open("a") as stream:
                    stream.write(json.dumps(record) + "\n")
                print(label, runtime, sessions, round,
                      f"p99={percentile(record['delta_ms'], 0.99):.3f}ms rss={record['idle_rss_kib']:.0f}KiB",
                      flush=True)
    summary = summarize(records)
    stalled = [(f["label"], f["sessions"], f["round"], f["failed"]) for f in failures]
    (args.output / "sessions.json").write_text(
        json.dumps(dict(cores=cores, summary=summary, failures=stalled), indent=2) + "\n")
    (args.output / "sessions.md").write_text(table(summary))
    print(table(summary))
    print("failed samples:", stalled)
    if summary_file := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary_file, "a") as stream:
            stream.write(f"Concurrent sessions ({cores} cores)\n\n" + table(summary))


if __name__ == "__main__":
    main()
