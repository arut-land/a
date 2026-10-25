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
    """(label, A_BENCH_RUNTIME, extra environment) for each layout."""
    # TEMPORARY (B1): async workers parking on the thread, as main does, against
    # workers that wait in the reactor; A/A2 is the reference's noise band.
    park = {"A_RT_PARK": "thread"}
    return [("A", "workers=4", park), ("A2", "workers=4", park), ("reactor", "workers=4", {})]


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
    lines = ["| layout − A | sessions | pairs | " + " | ".join(names) + " |",
             "| --- | ---: | ---: | " + " | ".join("---:" for _ in names) + " |"]
    for row in rows.values():
        cells = ["{median:+.3g} [{low:+.3g}, {high:+.3g}]".format(**row[name]) for name in names]
        lines.append(f"| {row['label']} | {row['sessions']} | {row['pairs']} | " + " | ".join(cells) + " |")
    return "\n".join(lines) + "\n"


TRIP = ["p50_us", "p90_us", "p99_us", "cpu_ms", "switches"]


def paired_trips(trips, reference="A"):
    """rt's round trip, each layout minus the reference in the same round."""
    runs = {(t["label"], t["round"]): t for t in trips}
    out = {}
    for label in sorted({t["label"] for t in trips} - {reference}):
        rounds = sorted(n for (l, n) in runs if l == label and (reference, n) in runs)
        if rounds:
            out[label] = dict(label=label, pairs=len(rounds), **{
                name: bootstrap([runs[(label, n)][name] - runs[(reference, n)][name] for n in rounds],
                                statistics.median(runs[(reference, n)][name] for n in rounds),
                                f"trip/{label}/{name}")
                for name in TRIP})
    return out


def trips_table(rows):
    lines = ["| round trip − A | pairs | A median: " + " | ".join(TRIP) + " |",
             "| --- | ---: | " + " | ".join("---:" for _ in TRIP) + " |"]
    for row in rows.values():
        cells = ["{reference:.4g}: {median:+.3g} [{low:+.3g}, {high:+.3g}]".format(**row[name]) for name in TRIP]
        lines.append(f"| {row['label']} | {row['pairs']} | " + " | ".join(cells) + " |")
    return "\n".join(lines) + "\n"


def table(summary):
    lines = ["| layout | sessions | delta p50 ms | delta p99 ms | run p99 ms | turn p50 ms | turn p99 ms "
             "| active CPU ms | active switches | idle RSS KiB | idle threads | idle switches | dropped deltas |",
             "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for row in summary.values():
        lines.append("| {label} ({runtime}) | {sessions} | {delta_p50_ms:.3f} | {delta_p99_ms:.3f} "
                     "| {run_delta_p99_ms:.3f} | {turn_p50_ms:.0f} | {turn_p99_ms:.0f} | {active_cpu_ms:.0f} "
                     "| {active_switches:.0f} | {idle_rss_kib:.0f} | {idle_threads:.0f} | {idle_switches:.0f} "
                     "| {dropped_deltas} |".format(**row))
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="the engine's release lib test executable")
    parser.add_argument("--round-trip", type=Path, help="rt's round_trip test executable, run per layout and round")
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
    records, failures, trips = [], [], []
    for round in range(args.rounds):
        order = labels if round % 2 == 0 else list(reversed(labels))
        for label, _, extra in order if args.round_trip else []:
            result = subprocess.run(
                [str(args.round_trip), "reactor_round_trip", "--exact", "--ignored", "--nocapture",
                 "--test-threads=1"], env=dict(os.environ, **extra), capture_output=True, text=True, timeout=300)
            if result.returncode:
                print(label, "round trip", round, result.stderr[-2000:], sep="\n", flush=True)
                continue
            line = next(line for line in result.stdout.splitlines() if '{"' in line)
            trip = dict(json.loads(line[line.index('{"'):]), label=label, round=round)
            trips.append(trip)
            with (args.output / "round-trip.jsonl").open("a") as stream:
                stream.write(json.dumps(trip) + "\n")
            print(label, "round trip", round, f"p50={trip['p50_us']}us p99={trip['p99_us']}us", flush=True)
        for sessions in counts:
            for label, runtime, extra in order:
                env = dict(os.environ, **extra, A_BENCH_RUNTIME=runtime, A_BENCH_SESSIONS=str(sessions),
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
    summary, pairs = summarize(records), paired(records)
    trip_pairs = paired_trips(trips)
    stalled = [(f["label"], f["sessions"], f["round"], f["failed"]) for f in failures]
    (args.output / "sessions.json").write_text(
        json.dumps(dict(cores=cores, summary=summary, paired=pairs, round_trip=trip_pairs, failures=stalled), indent=2) + "\n")
    report = table(summary) + "\n" + paired_table(pairs) + "\n" + trips_table(trip_pairs)
    (args.output / "sessions.md").write_text(report)
    print(report)
    print("failed samples:", stalled)
    if summary_file := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary_file, "a") as stream:
            stream.write(f"Concurrent sessions ({cores} cores)\n\n" + report)


if __name__ == "__main__":
    main()
