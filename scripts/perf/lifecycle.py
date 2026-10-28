"""An isolated fixture daemon and the native idle observations shared by adapters."""
from contextlib import contextmanager
from dataclasses import dataclass
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import tempfile
import threading
import time

from process import snapshot, stop_daemon


def checked(argv, output=None, label="command", **kwargs):
    """Keep prep failures before the isolated home and its logs disappear."""
    try:
        return subprocess.run(argv, capture_output=True, check=True, **kwargs)
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        if output is not None:
            def decoded(value):
                return value.decode(errors="replace") if isinstance(value, bytes) else value
            (output / f"failed-{label}.json").write_text(json.dumps(dict(
                argv=list(map(str, argv)), returncode=getattr(error, "returncode", None),
                error=str(error), stdout=decoded(error.stdout), stderr=decoded(error.stderr)), indent=2) + "\n")
        raise


@dataclass
class Daemon:
    home: Path
    env: dict
    client: subprocess.Popen
    pid: int
    cold_ms: float
    first_run_ms: float | None


@contextmanager
def daemon(binary, output=None, trial=0, first_run=False, ready_timeout=60):
    with tempfile.TemporaryDirectory(prefix="a-perf-", dir=os.environ.get("RUNNER_TEMP")) as directory:
        home = Path(directory)
        (home / "config.toml").write_text('[daemon]\nagent = "echo"\nidle_secs = 120\n')
        env = dict(os.environ, A_HOME=str(home), A_TUI="plain")
        env.pop("A_HOST", None)
        client = None
        with tempfile.TemporaryFile() as stderr:
            try:
                try:
                    first_run_ms = None
                    if first_run:
                        started = time.perf_counter()
                        # Pipes avoid polling quantization in short cold-start samples.
                        checked([binary, "ls"], output, f"{trial}-prep-ls", env=env, cwd=home, timeout=60)
                        first_run_ms = (time.perf_counter() - started) * 1000
                        stop_daemon(binary, env, home)
                    started = time.perf_counter()
                    client = subprocess.Popen([binary], env=env, cwd=home, stdin=subprocess.PIPE,
                                              stdout=subprocess.PIPE, stderr=stderr)
                    first = queue.Queue()
                    threading.Thread(target=lambda: first.put(client.stdout.readline()), daemon=True).start()
                    greeting = first.get(timeout=ready_timeout)
                    if not greeting.startswith(b"[session"):
                        raise RuntimeError(f"expected session greeting, received {greeting!r}")
                    cold_ms = (time.perf_counter() - started) * 1000
                    pid = int((home / "run/daemon.pid").read_text().splitlines()[0])
                    yield Daemon(home, env, client, pid, cold_ms, first_run_ms)
                finally:
                    try:
                        if client:
                            client.stdin.close()
                            try:
                                client.wait(timeout=15)
                            except subprocess.TimeoutExpired:
                                client.kill()
                                client.wait()
                            client.stdout.close()
                    finally:
                        stop_daemon(binary, env, home)
            except Exception:
                if output is not None:
                    for path in (home / "logs").glob("*"):
                        if path.is_file():
                            shutil.copy2(path, output / f"failed-{trial}-{path.name}")
                    stderr.seek(0)
                    (output / f"failed-{trial}-client.json").write_text(
                        json.dumps(dict(stderr=stderr.read().decode(errors="replace"))) + "\n")
                raise


def idle_sample(active, idle_seconds, settle_seconds=3, retire_seconds=12, memory_probe=None):
    time.sleep(settle_seconds)
    rss = snapshot(active.pid)["rss_kib"]
    charge = {}
    if memory_probe is not None:
        charge = json.loads(checked([str(memory_probe), "--memory", str(active.pid)],
                                    env=active.env, cwd=active.home, timeout=30).stdout)
    # Blocking workers retire after ten idle seconds; CPU measures steady idle.
    time.sleep(retire_seconds)
    before = [snapshot(p, threads=True) for p in (active.pid, active.client.pid)]
    started = time.monotonic()
    time.sleep(idle_seconds)
    after = [snapshot(p, threads=True) for p in (active.pid, active.client.pid)]
    elapsed = time.monotonic() - started
    return dict(rss=rss, **charge, seconds=elapsed, cold_reply_ms=active.cold_ms,
                first_run_ls_ms=active.first_run_ms, daemon_pid=active.pid, client_pid=active.client.pid,
                before=before, after=after,
                daemon_cpu_seconds=after[0]["cpu_seconds"] - before[0]["cpu_seconds"],
                client_cpu_seconds=after[1]["cpu_seconds"] - before[1]["cpu_seconds"])
