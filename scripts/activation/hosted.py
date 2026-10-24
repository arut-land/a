"""Native manager integration and alternating cold-connect samples on hosted runners."""
import argparse
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument("binary", type=Path)
parser.add_argument("--samples", type=int, default=12)
args = parser.parse_args()
binary = args.binary.resolve()
# No A_HOME: exercise native installation under disposable per-purpose roots.
root = Path(tempfile.mkdtemp(prefix="a-activation-"))
env = dict(os.environ)
env.pop("A_HOME", None)
env["HOME"] = str(root)
for kind in ["CONFIG", "DATA", "STATE", "CACHE"]:
    env[f"XDG_{kind}_HOME"] = str(root / kind.lower())
if platform.system() == "Darwin":
    config = root / "Library/Application Support/dev.a/config"
    run_dir = root / "Library/Application Support/dev.a/run"
else:
    config = root / "config/a"
    run_dir = root / "state/a/run"
config.mkdir(parents=True)
(config / "config.toml").write_text("[daemon]\nidle_secs = 1\n")


def command(*words, check=True):
    result = subprocess.run([str(binary), *words], env=env, capture_output=True, text=True, timeout=45)
    if check and result.returncode:
        raise RuntimeError(f"{words}: {result.returncode}: {result.stderr}")
    return result


def connect():
    begin = time.perf_counter_ns()
    command("ls")
    return (time.perf_counter_ns() - begin) / 1e6


samples = {"self_spawn_ms": [], "activation_ms": []}
try:
    command("ls")
    command("daemon", "stop")
    for sample in range(args.samples):
        # Alternate order to reduce runner drift; installation time is excluded.
        for managed in ([False, True] if sample % 2 == 0 else [True, False]):
            if managed:
                command("daemon", "install-activation")
                # RunAtLoad on launchd publishes its secure socket before first attach.
                deadline = time.monotonic() + 20
                while not (run_dir / "daemon.endpoint").exists():
                    if time.monotonic() >= deadline:
                        raise AssertionError("activation did not publish discovery")
                    time.sleep(.05)
                command("daemon", "stop")
            samples["activation_ms" if managed else "self_spawn_ms"].append(connect())
            pid = int((run_dir / "daemon.pid").read_text())
            # Prove idle exit without opening the socket (which would reactivate it).
            deadline = time.monotonic() + 15
            while (run_dir / "daemon.pid").read_text().strip():
                if time.monotonic() >= deadline:
                    raise AssertionError(f"daemon {pid} failed to exit idle")
                time.sleep(.05)
            if managed:
                assert (run_dir / "daemon.endpoint").exists()
                connect()  # same manager socket reactivates a fresh daemon
                command("daemon", "stop")
                command("daemon", "uninstall-activation")
    result = {"platform": platform.platform(), "binary_bytes": binary.stat().st_size, "samples": samples,
              "medians": {key: statistics.median(values) for key, values in samples.items()}}
    result["activation_minus_self_spawn_ms"] = result["medians"]["activation_ms"] - result["medians"]["self_spawn_ms"]
    print(json.dumps(result, indent=2))
    Path("activation-result.json").write_text(json.dumps(result, indent=2) + "\n")
finally:
    command("daemon", "stop", check=False)
    command("daemon", "uninstall-activation", check=False)
