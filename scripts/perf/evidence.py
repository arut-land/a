"""Survey workloads: alternating controls, real tools, external process counters."""
import argparse
import concurrent.futures
import contextlib
import json
import os
from pathlib import Path
import queue
import struct
import subprocess
import sys
import threading
import time
import psutil

from build import SUFFIX
from lifecycle import checked, daemon
from process import memory_maps, run_timed, snapshot
from stats import distribution


def outcomes(home):
    from hosted import load_task
    return load_task("workloads").tool_results(home)


def tool(binary, active, prompt, expected=1):
    from hosted import load_task
    workload = load_task("workloads")
    before = len(outcomes(active.home))
    counters = snapshot(active.pid)
    started = time.perf_counter()
    result = checked([str(binary), "--mode", "auto", "-p", prompt], active.output,
                     "tool", cwd=active.home, env=active.env, timeout=120)
    elapsed = (time.perf_counter() - started) * 1000
    after = snapshot(active.pid)
    records = outcomes(active.home)[before:]
    workload.validate_tools(records, expected)
    return dict(ms=elapsed, stdout_bytes=len(result.stdout), results=records,
                daemon_cpu_ms=(after["cpu_seconds"] - counters["cpu_seconds"]) * 1000)


def image(path):
    width, height = 4000, 3000
    stride = width * 3
    size = 54 + stride * height
    with path.open("wb") as stream:
        stream.write(struct.pack('<2sIHHIIIIHHIIIIII', b'BM', size, 0, 0, 54, 40,
                                 width, height, 1, 24, 0, stride * height, 0, 0, 0, 0))
        row = bytes(c for x in range(width) for c in (x % 256, (x // 16) % 256, (x * 7) % 256))
        for _ in range(height):
            stream.write(row)
    return size


@contextlib.contextmanager
def terminal(binary, active, session):
    """A real 100x30 terminal; no pipe-rendering substitute for cache evidence."""
    import fcntl
    import pty
    import termios
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    child = subprocess.Popen([str(binary), "resume", session], cwd=active.home,
                             env=dict(active.env, A_TUI="full", TERM="xterm-256color"),
                             stdin=slave, stdout=slave, stderr=slave)
    os.close(slave)
    visible = threading.Event()
    def drain():
        try:
            while data := os.read(master, 65536):
                if b"message the agent" in data:
                    visible.set()
        except OSError:
            pass
    reader = threading.Thread(target=drain, daemon=True)
    reader.start()
    try:
        if not visible.wait(30):
            raise RuntimeError("TUI did not render its idle prompt")
        yield child, master
    finally:
        if child.poll() is None:
            os.write(master, b"\x03")
            try:
                child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        os.close(master)
        reader.join(timeout=5)


def memory_probe(binary, probe, active, mode, count):
    command = [str(probe), "memory", str(active.home), mode, str(count)]
    if active.memory_only:
        command.remove("memory")
    stages, diagnostics = [], []
    child = subprocess.Popen(command, cwd=active.home, env=active.env, stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    lines = queue.Queue()
    def read():
        for line in child.stdout:
            lines.put(line.strip())
        lines.put(None)
    threading.Thread(target=read, daemon=True).start()
    with contextlib.ExitStack() as stack:
        tui = None
        try:
            while True:
                line = lines.get(timeout=120)
                if line is None:
                    break
                if line.startswith("session:"):
                    tui = stack.enter_context(terminal(binary, active, line.split(":", 1)[1]))
                    continue
                if line.startswith(("welcome_bytes:", "replay_bytes:", "replay_done", "elapsed_us:")):
                    diagnostics.append(line)
                    continue
                time.sleep(.25)
                record = dict(stage=line, daemon=snapshot(active.pid))
                if tui:
                    record["tui"] = snapshot(tui[0].pid)
                    if line == str(count):
                        for _ in range(2):
                            os.write(tui[1], b"\x0f")
                            time.sleep(1)
                        record["tui_after_clear"] = snapshot(tui[0].pid)
                if line == "detached":
                    time.sleep(12)
                    record["retired"] = snapshot(active.pid)
                stages.append(record)
                child.stdin.write("\n")
                child.stdin.flush()
            child.wait(timeout=30)
            stderr = child.stderr.read()
            if child.returncode:
                raise RuntimeError(f"probe exited {child.returncode}: {stderr}")
            if not stages or stages[-1]["stage"] != "detached":
                raise ValueError(f"incomplete {mode} probe: {stages}")
            elapsed = next((float(line.split(":", 1)[1]) / 1000 for line in diagnostics if line.startswith("elapsed_us:")), None)
            return dict(stages=stages, diagnostics=diagnostics, **(dict(ms=elapsed) if elapsed is not None else {}))
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            for stream in (child.stdin, child.stdout, child.stderr):
                stream.close()


def sample(case, binary, probe, output, memory_only=False):
    name, control = case
    if name in ("reply-history", "web-title", "tui-paragraph"):
        mode = {"reply-history": "reply", "web-title": "web", "tui-paragraph": "live"}[name]
        chunks = 2000 if mode == "live" else 4096
        args = [str(probe), mode, "0" if control else "50000", str(chunks)]
        if mode == "live":
            args += ["newline" if control else "paragraph"]
        result = run_timed(args, capture_output=True, text=True, timeout=120)
        return json.loads(result.stdout)
    with daemon(str(binary), output) as active:
        active.output, active.memory_only = output, memory_only
        diagnostic = output / "daemon-memory-map.json"
        if not diagnostic.exists():
            try:
                maps = dict(counters=snapshot(active.pid), **memory_maps(active.pid))
            except (OSError, subprocess.SubprocessError, psutil.Error, ValueError, NotImplementedError) as error:
                maps = dict(error=repr(error))
            diagnostic.write_text(json.dumps(maps, indent=2) + "\n")
        if name == "quiet-shell":
            from hosted import load_task
            workload = load_task("workloads")
            prompt = workload.large_output_prompt() if control else workload.shell_call("exit 0" if os.name == "nt" else "true")
            result = tool(binary, active, prompt)
            if control:
                load_task("workloads").validate_tools(result["results"], 1, 1048576)
            return result
        if name == "edit-64mib":
            path = active.home / "edit.txt"
            with path.open("wb") as stream:
                block = b"ordinary file content.\n" * (1048576 // 22)
                remaining = ((1 if control else 64) << 20) - len(b"old unique marker\n")
                while remaining:
                    written = stream.write(block[:remaining])
                    remaining -= written
                stream.write(b"old unique marker\n")
            result = tool(binary, active, "call edit " + json.dumps(dict(
                path=str(path), edits=[dict(old="old unique marker", new="new unique marker")])))
            if not path.read_bytes().endswith(b"new unique marker\n"):
                raise ValueError("edit did not change the real file")
            return dict(input_bytes=path.stat().st_size, **result)
        if name == "image-concurrent":
            path = active.home / "image.bmp"
            size = image(path)
            count = 1 if control else 8
            before = snapshot(active.pid)
            prompt = "call read " + json.dumps(dict(path=str(path)))
            prior = len(outcomes(active.home))
            start = threading.Barrier(count + 1)
            def read_image(index):
                start.wait(timeout=30)
                return checked([str(binary), "--mode", "auto", "-p", prompt], output, f"image-{index}",
                               cwd=active.home, env=active.env, timeout=120)
            with concurrent.futures.ThreadPoolExecutor(max_workers=count) as pool:
                futures = [pool.submit(read_image, index) for index in range(count)]
                started = time.perf_counter()
                start.wait(timeout=30)
                samples = []
                while not all(future.done() for future in futures):
                    samples.append(snapshot(active.pid))
                    time.sleep(.01)
                results = [future.result() for future in futures]
                elapsed = (time.perf_counter() - started) * 1000
            records = outcomes(active.home)[prior:]
            from hosted import load_task
            load_task("workloads").validate_tools(records, count)
            if not all(len(r.get("held", [])) == 1 for r in records):
                raise ValueError("every image read must retain its resulting image")
            return dict(input_bytes=size, count=count, before=before, samples=samples,
                        peak_sampled_rss_kib=max(s["rss_kib"] for s in samples), ms=elapsed,
                        stdout_bytes=sum(len(result.stdout) for result in results), results=records,
                        daemon_cpu_ms=(snapshot(active.pid)["cpu_seconds"] - before["cpu_seconds"]) * 1000)
        if name == "subscription-churn":
            return memory_probe(binary, probe, active, "list" if control else "watch", 20000)
        if name == "attachment-churn":
            return memory_probe(binary, probe, active, "list" if control else "attach", 20000)
        if name == "replay-buffer":
            return memory_probe(binary, probe, active, "bare" if control else "clients", 240)
        if name == "pty-cache":
            return memory_probe(binary, probe, active, "turns", 32 if control else 5000)
        raise ValueError(case)


CPU = ["quiet-shell", "reply-history", "edit-64mib", "tui-paragraph", "web-title"]
MEMORY = ["subscription-churn", "attachment-churn", "image-concurrent", "replay-buffer", "pty-cache"]


def metrics(observation):
    values = {key: observation[key] for key in ("ms", "daemon_cpu_ms", "peak_sampled_rss_kib") if key in observation}
    if stages := observation.get("stages"):
        baseline = stages[0]["daemon"]["rss_kib"]
        values["held_rss_delta_kib"] = stages[-2]["daemon"]["rss_kib"] - baseline
        values["retired_rss_delta_kib"] = stages[-1]["retired"]["rss_kib"] - baseline
        for stage in stages:
            if "tui_after_clear" in stage:
                values["tui_rss_kib"] = stage["tui"]["rss_kib"]
                values["tui_cleared_rss_kib"] = stage["tui_after_clear"]["rss_kib"]
                if "footprint_kib" in stage["tui"]:
                    values["tui_footprint_kib"] = stage["tui"]["footprint_kib"]
                    values["tui_cleared_footprint_kib"] = stage["tui_after_clear"]["footprint_kib"]
    return values


def measure(binary, probe, output, rounds=3, memory_only=False, cases=None):
    output.mkdir(parents=True, exist_ok=True)
    cases = cases or (MEMORY if memory_only else CPU + MEMORY)
    records, failures, skipped = [], [], []
    for trial in range(rounds):
        order = [(name, control) for name in cases for control in (True, False)]
        if trial % 2:
            order.reverse()
        for case in order:
            name, control = case
            if name == "pty-cache" and os.name == "nt":
                skipped.append(dict(case=case, reason="no portable ConPTY launcher in the owned sys seam"))
                continue
            print("evidence", trial, case, flush=True)
            try:
                record = dict(trial=trial, name=name, control=control,
                              observation=sample(case, binary, probe, output, memory_only))
                records.append(record)
                with (output / "samples.jsonl").open("a") as stream:
                    stream.write(json.dumps(record) + "\n")
            except Exception as error:
                failure = dict(trial=trial, case=case, error=repr(error))
                failures.append(failure)
                with (output / "failures.jsonl").open("a") as stream:
                    stream.write(json.dumps(failure) + "\n")
    summary = {}
    for name in cases:
        for control in (True, False):
            observations = [metrics(r["observation"]) for r in records if r["name"] == name and r["control"] == control]
            for metric in sorted({key for row in observations for key in row}):
                values = [row[metric] for row in observations if metric in row]
                summary[f"{name}/{'control' if control else 'load'}_{metric}"] = distribution(values)
    result = dict(rounds=rounds, samples=records, summary=summary, failures=failures, skipped=skipped)
    (output / "evidence.json").write_text(json.dumps(result, indent=2) + "\n")
    if failures:
        raise RuntimeError(f"{len(failures)} evidence workloads failed; see {output}")
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("build", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--memory-only", action="store_true")
    parser.add_argument("--rounds", type=int, default=3)
    args = parser.parse_args()
    measure(args.build / ("a-fixtures" + SUFFIX), args.build / ("perf-probe" + SUFFIX),
            args.output, args.rounds, args.memory_only)
