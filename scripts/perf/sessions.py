"""Concurrently active sessions under each runtime layout, paired with an A/A control.

Runs the engine's `concurrent_active_sessions` measurement in a fresh process
per sample (idle RSS must not inherit an earlier layout's heap), alternating
layout order each round. `workers=4` runs twice under two labels; the gap
between them is the noise band any other layout must clear. With `--round-trip`,
rt's echoed-byte timing runs once per round on its fixed Threads::new(4) runtime.
Each layout is also compared with `A` round by round: median paired differences
with bootstrap 95% intervals.

`--layouts memory` compares allocators instead: each `--binary LABEL=PATH` is
the same measurement built with another global allocator, run on the default
runtime beside the two system-allocator controls (docs/perf/allocators.md).
"""
import argparse
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys

TEST = "tests::runtime_demand::concurrent_active_sessions"
MEMORY = ["peak_kib", "idle_footprint_kib", "release_ms", "released_rss_kib", "released_footprint_kib",
          "detached_rss_kib", "detached_released_rss_kib", "detached_released_footprint_kib"]


def percentile(values, q):
    values = sorted(values)
    return values[min(len(values) - 1, int(len(values) * q))]


def memory_layouts(binaries, arena_policy=False):
    plain = "workers=4"
    labels = [("A", plain, {}, None), ("A2", plain, {}, None)]
    labels += [(label, plain, {}, label) for label in binaries]
    if arena_policy and sys.platform == "linux":
        labels.append(("arenas", plain + ",arenas=default", {}, None))
    return labels


def layouts(cores):
    """(label, A_BENCH_RUNTIME, extra environment) for each layout."""
    labels = [("A", "workers=4", {}), ("A2", "workers=4", {})]
    if cores != 4:
        labels.append(("cores", f"workers={cores}", {}))
    labels.append(("threads", "threads", {}))
    # Not a candidate: what each streamed request holding a network worker costs.
    labels.append(("network", "workers=4,network=128", {}))
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
            active_switches=statistics.median(run["active_switches"] for run in runs),
            active_wall_ms=statistics.median(run["active_wall_ms"] for run in runs),
            idle_rss_kib=statistics.median(run["idle_rss_kib"] for run in runs),
            idle_threads=statistics.median(run["idle_threads"] for run in runs),
            idle_cpu_ms=statistics.median(run["idle_cpu_ms"] for run in runs),
            idle_switches=statistics.median(run["idle_switches"] for run in runs),
            dropped_deltas=sum(run["dropped_deltas"] for run in runs),
            failed_calls=sum(run.get("failed_calls", 0) for run in runs),
            **{name: statistics.median(run[name] for run in runs)
               for name in MEMORY if all(run.get(name) is not None for run in runs)},
        )
    return summary


# Per-run metrics compared between layouts in the same round and load.
PAIRED = {
    "run_delta_p99_ms": lambda run: percentile(run["delta_ms"], 0.99),
    "turn_p99_ms": lambda run: percentile(run["turn_ms"], 0.99),
    "active_cpu_ms": lambda run: run["active_cpu_ms"],
    "active_switches": lambda run: run["active_switches"],
    "idle_rss_kib": lambda run: run["idle_rss_kib"],
    "idle_threads": lambda run: run["idle_threads"],
    "idle_cpu_ms": lambda run: run["idle_cpu_ms"],
    "idle_switches": lambda run: run["idle_switches"],
}


def bootstrap(diffs, reference, seed, resamples=2000):
    """The median paired difference and its bootstrap 95% interval (fixed seed)."""
    import random
    rng = random.Random(seed)
    medians = sorted(statistics.median(rng.choices(diffs, k=len(diffs))) for _ in range(resamples))
    return dict(median=statistics.median(diffs), reference=reference,
                low=medians[int(resamples * 0.025)], high=medians[int(resamples * 0.975) - 1])


def paired(records, reference="A"):
    """Each layout minus the reference over rounds where both completed."""
    runs = {(r["label"], r["sessions"], r["round"]): r for r in records}
    out = {}
    for label in sorted({r["label"] for r in records} - {reference}):
        for sessions in sorted({r["sessions"] for r in records}):
            rounds = sorted(k[2] for k in runs if k[:2] == (label, sessions)
                            and (reference, sessions, k[2]) in runs)
            if not rounds:
                continue
            row = dict(label=label, sessions=sessions, pairs=len(rounds))
            for name, metric in PAIRED.items():
                diffs = [metric(runs[(label, sessions, n)]) - metric(runs[(reference, sessions, n)])
                         for n in rounds]
                base = statistics.median(metric(runs[(reference, sessions, n)]) for n in rounds)
                row[name] = bootstrap(diffs, base, f"{label}/{sessions}/{name}")
            out[f"{label}/{sessions}"] = row
    return out


def paired_table(rows):
    names = list(PAIRED)
    lines = ["| layout - A | sessions | pairs | " + " | ".join(names) + " |",
             "| --- | ---: | ---: | " + " | ".join("---:" for _ in names) + " |"]
    for row in rows.values():
        cells = ["{median:+.3g} [{low:+.3g}, {high:+.3g}]".format(**row[name]) for name in names]
        lines.append(f"| {row['label']} | {row['sessions']} | {row['pairs']} | " + " | ".join(cells) + " |")
    return "\n".join(lines) + "\n"


TRIP = ["p50_us", "p90_us", "p99_us", "cpu_ms", "switches"]


def trips_table(trips):
    lines = ["| round trip runtime | runs | " + " | ".join(TRIP) + " |",
             "| --- | ---: | " + " | ".join("---:" for _ in TRIP) + " |"]
    if trips:
        cells = [f"{statistics.median(trip[name] for trip in trips):.4g}" for name in TRIP]
        lines.append(f"| Threads::new(4) | {len(trips)} | " + " | ".join(cells) + " |")
    return "\n".join(lines) + "\n"


def execute(command, env, timeout=300):
    try:
        result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=timeout)
        return result.stdout, result.stderr, f"exit {result.returncode}" if result.returncode else None
    except subprocess.TimeoutExpired as error:
        out, err = (text.decode(errors="replace") if isinstance(text, bytes) else text or ""
                    for text in (error.stdout, error.stderr))
        return out, err, "timed out"
    except OSError as error:
        return "", str(error), "could not execute"


def table(summary):
    lines = ["| layout | sessions | delta p50 ms | delta p99 ms | run p99 ms | turn p50 ms | turn p99 ms "
             "| active CPU ms | active switches | idle RSS KiB | idle threads | idle switches | dropped deltas | failed calls |",
             "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for row in summary.values():
        lines.append("| {label} ({runtime}) | {sessions} | {delta_p50_ms:.3f} | {delta_p99_ms:.3f} "
                     "| {run_delta_p99_ms:.3f} | {turn_p50_ms:.0f} | {turn_p99_ms:.0f} | {active_cpu_ms:.0f} "
                     "| {active_switches:.0f} | {idle_rss_kib:.0f} | {idle_threads:.0f} | {idle_switches:.0f} "
                     "| {dropped_deltas} | {failed_calls} |".format(**row))
    rows = [row for row in summary.values() if "released_rss_kib" in row]
    if rows:
        lines += ["", "| layout | sessions | peak KiB | idle RSS KiB | released KiB | release ms "
                  "| detached KiB | detached released KiB |",
                  "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
        for row in rows:
            lines.append("| {label} | {sessions} | {peak} | {idle_rss_kib:.0f} | {released_rss_kib:.0f} "
                         "| {release_ms:.2f} | {detached_rss_kib:.0f} | {detached_released_rss_kib:.0f} |".format(
                             peak=f"{row['peak_kib']:.0f}" if "peak_kib" in row else "", **row))
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="the engine's release lib test executable")
    parser.add_argument("--round-trip", type=Path, help="rt's round_trip test executable, fixed Threads::new(4) baseline, run once per round")
    parser.add_argument("--binary", dest="binaries", action="append", default=[],
                        help="LABEL=PATH: the same executable built with another allocator")
    parser.add_argument("--layouts", choices=["runtime", "memory"], default="runtime")
    parser.add_argument("--arena-policy", action="store_true",
                        help="compare glibc arena policies; requires a binary supporting arenas=default")
    parser.add_argument("--output", type=Path, default=Path("target/perf-sessions"))
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--sessions", default="1,8,32,128")
    parser.add_argument("--turns", type=int, default=3)
    parser.add_argument("--pace-ms", type=int, default=10)
    args = parser.parse_args(argv)
    args.output.mkdir(parents=True, exist_ok=True)
    cores = os.cpu_count()
    binaries = dict(entry.split("=", 1) for entry in args.binaries)
    labels = (memory_layouts(binaries, args.arena_policy) if args.layouts == "memory"
              else [(label, runtime, extra, None) for label, runtime, extra in layouts(cores)])
    counts = [int(n) for n in args.sessions.split(",")]
    raw = args.output / "sessions.jsonl"
    records, failures, trips = [], [], []
    for round in range(args.rounds):
        order = labels if round % 2 == 0 else list(reversed(labels))
        if args.round_trip:
            out, err, failed = execute(
                [str(args.round_trip), "reactor_round_trip", "--exact", "--ignored", "--nocapture",
                 "--test-threads=1"], env=dict(os.environ))
            if failed:
                failure = dict(runtime="Threads::new(4)", label="round trip", sessions=0,
                               round=round, failed=failed, stdout=out[-4000:], stderr=err[-4000:])
                failures.append(failure)
                with (args.output / "failures.jsonl").open("a") as stream:
                    stream.write(json.dumps(failure) + "\n")
                print("round trip", round, failed, err[-2000:], sep="\n", flush=True)
            else:
                line = next(line for line in out.splitlines() if '{"' in line)
                trip = dict(json.loads(line[line.index('{"'):]), runtime="Threads::new(4)", round=round)
                trips.append(trip)
                with (args.output / "round-trip.jsonl").open("a") as stream:
                    stream.write(json.dumps(trip) + "\n")
                print("round trip", round, f"p50={trip['p50_us']}us p99={trip['p99_us']}us", flush=True)
        for sessions in counts:
            for label, runtime, extra, built in order:
                env = dict(os.environ, **extra, A_BENCH_RUNTIME=runtime, A_BENCH_SESSIONS=str(sessions),
                           A_BENCH_TURNS=str(args.turns), A_BENCH_PACE_MS=str(args.pace_ms))
                out, err, failed = execute(
                    [str(binaries[built] if built else args.binary), TEST, "--exact", "--ignored", "--nocapture", "--test-threads=1"],
                    env=env)
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
                      f"p99={percentile(record['delta_ms'], 0.99):.3f}ms rss={record['idle_rss_kib']:.0f}KiB "
                      f"released={record.get('released_rss_kib', 0):.0f}KiB", flush=True)
    summary, pairs = summarize(records), paired(records)
    stalled = [(f["label"], f["sessions"], f["round"], f["failed"]) for f in failures]
    (args.output / "sessions.json").write_text(
        json.dumps(dict(cores=cores, summary=summary, paired=pairs, round_trip=trips, failures=stalled), indent=2) + "\n")
    report = table(summary) + "\n" + paired_table(pairs) + "\n" + trips_table(trips)
    (args.output / "sessions.md").write_text(report)
    print(report)
    print("failed samples:", stalled)
    if summary_file := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary_file, "a") as stream:
            stream.write(f"Concurrent sessions ({cores} cores)\n\n" + report)

    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
