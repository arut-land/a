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


def command(*words, check=True, environment=None):
    result = subprocess.run([str(binary), *words], env=env if environment is None else environment, capture_output=True, text=True, timeout=45)
    if check and result.returncode:
        raise RuntimeError(f"{words}: {result.returncode}: {result.stderr}")
    return result


def connect():
    begin = time.perf_counter_ns()
    command("ls")
    return (time.perf_counter_ns() - begin) / 1e6


samples = {"self_spawn_ms": [], "activation_ms": []}
try:
    unregistered = command("__daemon", "--activated", check=False)
    assert unregistered.returncode and "not installed for this home" in unregistered.stderr
    assert not (run_dir / "daemon.endpoint").exists()
    command("ls")
    command("daemon", "stop")
    for sample in range(args.samples):
        # Alternate order to reduce runner drift; installation time is excluded.
        for managed in ([False, True] if sample % 2 == 0 else [True, False]):
            if managed:
                command("daemon", "install-activation")
                command("ls")
                command("daemon", "stop")
            samples["activation_ms" if managed else "self_spawn_ms"].append(connect())
            pid = int((run_dir / "daemon.pid").read_text().splitlines()[0])
            # Prove idle exit without opening the socket (which would reactivate it).
            deadline = time.monotonic() + 15
            while (run_dir / "daemon.pid").read_text().strip():
                if time.monotonic() >= deadline:
                    raise AssertionError(f"daemon {pid} failed to exit idle")
                time.sleep(.05)
            assert "not running" in command("daemon", "status").stdout
            assert not (run_dir / "daemon.pid").read_text().strip()
            if managed:
                socket = Path(os.fsdecode((run_dir / "daemon.endpoint").read_bytes()))
                identity = socket.stat().st_ino
                connect()  # same manager socket reactivates a fresh daemon
                assert socket.stat().st_ino == identity
                command("daemon", "stop")
                command("daemon", "uninstall-activation")
    # Native layout bases may differ for the same user; uninstall must own
    # its manager namespace as strictly as it owns its endpoint claim.
    peer_root = Path(tempfile.mkdtemp(prefix="a-activation-peer-"))
    peer_env = dict(env, HOME=str(peer_root))
    for kind in ["CONFIG", "DATA", "STATE", "CACHE"]:
        peer_env[f"XDG_{kind}_HOME"] = str(peer_root / kind.lower())
    peer_config = peer_root / ("Library/Application Support/dev.a/config" if platform.system() == "Darwin" else "config/a")
    peer_config.mkdir(parents=True)
    for directory in [config, peer_config]:
        (directory / "config.toml").write_text("[daemon]\nidle_secs = 60\n")
    try:
        for environment in [env, peer_env]:
            command("daemon", "install-activation", environment=environment)
            command("ls", environment=environment)
        before = command("daemon", "status", environment=peer_env).stdout.splitlines()[0].split(",", 1)[0]
        assert before.startswith("running (pid "), before
        uninstall_env = dict(env)
        if platform.system() == "Linux":
            uninstall_env["XDG_CONFIG_HOME"] = str(root / "another-config")
        command("daemon", "uninstall-activation", environment=uninstall_env)
        after = command("daemon", "status", environment=peer_env).stdout.splitlines()[0].split(",", 1)[0]
        assert after == before, "uninstall affected another native state home"
    finally:
        command("daemon", "stop", check=False, environment=peer_env)
        command("daemon", "uninstall-activation", check=False, environment=peer_env)
    result = {"platform": platform.platform(), "binary_bytes": binary.stat().st_size, "samples": samples, "cross_home_uninstall": True,
              "medians": {key: statistics.median(values) for key, values in samples.items()}}
    result["activation_minus_self_spawn_ms"] = result["medians"]["activation_ms"] - result["medians"]["self_spawn_ms"]
    print(json.dumps(result, indent=2))
    Path("activation-result.json").write_text(json.dumps(result, indent=2) + "\n")
finally:
    command("daemon", "stop", check=False)
    command("daemon", "uninstall-activation", check=False)
