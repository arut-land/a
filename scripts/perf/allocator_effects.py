"""Paired allocator effects; resample rounds, never individual streamed deltas."""
import argparse
import json
from pathlib import Path
import random
import statistics

from sessions import percentile


def effect(pairs, relative, rounds=None):
    rounds = list(range(len(pairs))) if rounds is None else rounds
    values = [(candidate / reference - 1) * 100 if relative else candidate - reference
              for reference, candidate in pairs]
    rng = random.Random(0)
    boot = sorted(statistics.median(rng.choices(values, k=len(values))) for _ in range(10000))
    return dict(pairs=len(pairs), reference=statistics.median(a for a, _ in pairs),
                candidate=statistics.median(b for _, b in pairs),
                delta=statistics.median(values), ci95=[boot[249], boot[9749]],
                by_order={str(order): statistics.median([value for r, value in zip(rounds, values) if r % 2 == order])
                          for order in (0, 1) if any(r % 2 == order for r in rounds)},
                unit="percent" if relative else "absolute", samples=pairs)


def sessions(path):
    records = [json.loads(line) for line in path.read_text().splitlines()]
    groups = {}
    for record in records:
        if record.get("failed_calls", 0) or record["dropped_deltas"]:
            raise ValueError(f"failed calls or dropped deltas: {path}, {record['label']}, {record['round']}")
        record = dict(record, delta_p99_ms=percentile(record["delta_ms"], .99),
                      turn_p99_ms=percentile(record["turn_ms"], .99))
        groups.setdefault((record["sessions"], record["label"]), {})[record["round"]] = record
    rows = {}
    for (count, label), rounds in sorted(groups.items()):
        if label == "A":
            continue
        reference = groups[count, "A"]
        complete = sorted(rounds.keys() & reference.keys())
        metrics = set.intersection(*(set(rounds[r]) & set(reference[r]) for r in complete))
        for metric in sorted(metrics):
            if not metric.endswith(("_kib", "_ms")) or metric in {"delta_ms", "turn_ms"}:
                continue
            pairs = [(reference[r][metric], rounds[r][metric]) for r in complete]
            if any(a is None or b is None for a, b in pairs):
                continue
            relative = metric in {"active_cpu_ms", "active_wall_ms", "delta_p99_ms", "turn_p99_ms"} and all(a for a, _ in pairs)
            rows[f"{label}/{count}/{metric}"] = dict(
                rounds=complete, allocators=sorted({rounds[r].get("allocator", "unrecorded") for r in complete}),
                **effect(pairs, relative, complete))
    return rows


def paired(path):
    data = json.loads(path.read_text())
    assert data["builds"]["reference"]["variant"] == {}, "reference must use the system allocator"
    rounds = [trial for trial in data["trials"] if set(trial) == {"reference", "candidate"}]
    metrics = set.intersection(*(set(r["reference"]) & set(r["candidate"]) for r in rounds))
    rows = {}
    for metric in sorted(metrics):
        pairs = [(statistics.median(r["reference"][metric]), statistics.median(r["candidate"][metric]))
                 for r in rounds]
        relative = (metric.endswith("_ms") and "idle" not in metric and all(a for a, _ in pairs))
        rows[metric] = effect(pairs, relative)
    rows["size_kib"] = effect([tuple(data["builds"][role]["shipped_bytes"] / 1024
                                    for role in ("reference", "candidate"))], False)
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path, help="sessions.jsonl or paired.json")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    rows = sessions(args.input) if args.input.suffix == ".jsonl" else paired(args.input)
    args.output.write_text(json.dumps(dict(
        method="candidate minus reference; median of matched round effects; fixed-seed 10000-resample percentile 95% CI",
        source=str(args.input), effects=rows), indent=2) + "\n")


if __name__ == "__main__":
    main()
