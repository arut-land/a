#!/usr/bin/env python3
"""Measure the actual sys sink against a listening OS collector on a hosted VM."""
import argparse
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import time
import xml.etree.ElementTree as ET

TEST = "log::native::measure::listening_collector_cost_per_record"


def run(args, **kwargs):
    if args[0] == "cargo" and "PERF_CARGO" in os.environ:
        args = [os.environ["PERF_CARGO"], *args[1:]]
    print("+", args, flush=True)
    try:
        return subprocess.run(args, check=True, **kwargs)
    except subprocess.CalledProcessError as error:
        if error.stdout:
            print(error.stdout, flush=True)
        if error.stderr:
            print(error.stderr, file=sys.stderr, flush=True)
        raise


def wait_for(probe, child, seconds=30):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = probe()
        if result:
            return result
        if child is not None and child.poll() is not None:
            raise RuntimeError(f"collector/test exited early: {child.returncode}")
        time.sleep(0.05)
    raise TimeoutError("collector did not reach the expected state")


def read_json(path):
    try:
        return json.loads(path.read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        return None


def messages(path, windows):
    if not path.exists():
        return []
    if windows:
        # tracerpt/TDH decodes the TraceLogging Json field; XML unescapes it.
        values = [node.text for node in ET.parse(path).iter() if node.text]
    else:
        values = []
        for line in path.read_text(errors="replace").splitlines():
            try:
                event = json.loads(line)
                values.append(event.get("MESSAGE", event.get("eventMessage", "")))
            except (ValueError, AttributeError):
                continue
    result = []
    for value in values:
        try:
            event = json.loads(value)
            if isinstance(event, dict):
                result.append(event)
        except (ValueError, TypeError):
            continue
    return result


def summarize(rounds):
    values = sorted(value for sample in rounds for value in sample)
    return {
        "p50_ns": statistics.median(values),
        "p95_ns": values[math.ceil(len(values) * 0.95) - 1],
        "round_mean_ns": [statistics.mean(sample) for sample in rounds],
        "round_p50_ns": [statistics.median(sample) for sample in rounds],
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    if any((output / name).exists() for name in
           ["metadata.json", "ready", "ack", "samples.json", "result.json"]):
        raise FileExistsError("choose a fresh measurement output directory")
    build = run(["cargo", "test", "-p", "sys", "--release", "--lib", "--no-run",
                 "--message-format=json"], capture_output=True, text=True)
    (output / "build.log").write_text(build.stdout + build.stderr)
    artifacts = [json.loads(line) for line in build.stdout.splitlines() if line.startswith("{")]
    executable = next(item["executable"] for item in artifacts
                      if item.get("reason") == "compiler-artifact"
                      and item.get("executable") and item["target"]["name"] == "sys")
    env = dict(os.environ, A_NATIVE_PROBE_DIR=str(output), A_HOME=str(output / "isolated-home"))
    windows = sys.platform == "win32"
    collector = None
    session = None
    capture = output / ("collector.xml" if windows else "collector.ndjson")
    with (output / "test.log").open("w") as test_log, (output / "collector.stderr").open("w") as errors:
        child = subprocess.Popen([executable, TEST, "--exact", "--ignored", "--nocapture",
                                  "--test-threads=1"], env=env, stdout=test_log, stderr=subprocess.STDOUT)
        try:
            meta = wait_for(lambda: read_json(output / "metadata.json"), child)
            if windows:
                session = f"ANativeSink{meta['pid']}"
                run(["logman", "start", session, "-ets", "-o", str(output / "collector.etl"),
                     "-p", "{" + meta["provider"] + "}", "0xffffffffffffffff", "4",
                     "-bs", "256", "-nb", "16", "64", "-ft", "1"], cwd=output)
            else:
                if sys.platform == "darwin":
                    command = ["/usr/bin/log", "stream", "--style", "ndjson", "--level", "default",
                               "--process", str(meta["pid"]), "--predicate",
                               'subsystem == "land.arut.a" AND category == "daemon"']
                else:
                    command = ["sudo", "-n", "journalctl", "--follow", "--lines=0", "--output=json",
                               "SYSLOG_IDENTIFIER=a", f"_PID={meta['pid']}"]
                with capture.open("w") as records:
                    collector = subprocess.Popen(command, stdout=records, stderr=errors,
                                                 start_new_session=True)
            (output / "ready").touch()
            if not windows:
                wait_for(lambda: any(event.get("native_probe") == meta["token"]
                                     for event in messages(capture, False)), collector)
            # ETW readiness is additionally checked through Provider::enabled in sys.
            (output / "ack").touch()
            status = child.wait(timeout=120)
            if status != 0:
                raise RuntimeError(f"native probe failed: {status}; see test.log")
            expected = meta["rounds"] * meta["records_per_round"]
            if windows:
                run(["logman", "stop", session, "-ets"], cwd=output)
                session = None
                run(["tracerpt", str(output / "collector.etl"), "-o", str(capture),
                     "-of", "XML", "-y"], cwd=output)
            else:
                wait_for(lambda: len([event for event in messages(capture, False)
                                      if event.get("native_probe") == meta["token"]
                                      and event.get("sequence", -1) >= 0]) >= expected,
                         collector, seconds=20)
            received = [event["sequence"] for event in messages(capture, windows)
                        if event.get("native_probe") == meta["token"]
                        and event.get("sequence", -1) >= 0]
            assert sorted(received) == list(range(expected)), (
                f"collector received {len(received)}/{expected} records; missing/duplicate sequences")
            samples = read_json(output / "samples.json")
            result = dict(meta, revision=subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
                          platform=platform.platform(), architecture=platform.machine(),
                          received=len(received), enabled=summarize(samples["enabled_ns"]),
                          disabled=summarize(samples["disabled_ns"]))
            (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
            print(json.dumps(result, indent=2), flush=True)
        finally:
            if session:
                subprocess.run(["logman", "stop", session, "-ets"], cwd=output, check=False)
            if collector and collector.poll() is None:
                if sys.platform == "darwin":
                    collector.terminate()
                else:
                    subprocess.run(["sudo", "-n", "kill", "-TERM", "--", f"-{collector.pid}"], check=False)
                collector.wait(timeout=10)
            if child.poll() is None:
                child.kill()
                child.wait(timeout=10)


if __name__ == "__main__":
    main()
