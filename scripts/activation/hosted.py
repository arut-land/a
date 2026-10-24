"""Native manager integration and alternating cold-connect samples on hosted runners."""
import argparse
import json
import os
from pathlib import Path
import platform
import plistlib
import statistics
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument("binary", type=Path)
parser.add_argument("--old-binary", type=Path, required=True)
parser.add_argument("--samples", type=int, default=12)
args = parser.parse_args()
binary = args.binary.resolve()
old_binary = args.old_binary.resolve()


def instance(prefix):
    # No A_HOME: exercise native installation under disposable purpose roots.
    root = Path(tempfile.mkdtemp(prefix=prefix))
    environment = dict(os.environ, HOME=str(root))
    environment.pop("A_HOME", None)
    for kind in ["CONFIG", "DATA", "STATE", "CACHE"]:
        environment[f"XDG_{kind}_HOME"] = str(root / kind.lower())
    if platform.system() == "Darwin":
        state = root / "Library/Application Support/dev.a"
        config, run_dir, runtime = state / "config", state / "run", state / "runtime"
    else:
        config, run_dir = root / "config/a", root / "state/a/run"
        runtime = root / "data/a/runtime"
    config.mkdir(parents=True)
    return root, environment, config, run_dir, runtime


root, env, config, run_dir, _ = instance("a-activation-")
(config / "config.toml").write_text("[daemon]\nidle_secs = 1\n")


def command(*words, check=True, environment=None, executable=None):
    result = subprocess.run([str(binary if executable is None else executable), *words], env=env if environment is None else environment, capture_output=True, text=True, timeout=45)
    if check and result.returncode:
        raise RuntimeError(f"{words}: {result.returncode}: {result.stderr}")
    return result


def connect():
    begin = time.perf_counter_ns()
    command("ls")
    return (time.perf_counter_ns() - begin) / 1e6


def idle():
    pid = int((run_dir / "daemon.pid").read_text().splitlines()[0])
    # Inspect records rather than connecting to a manager-owned socket.
    deadline = time.monotonic() + 15
    while (run_dir / "daemon.pid").read_text().strip():
        if time.monotonic() >= deadline:
            raise AssertionError(f"daemon {pid} failed to exit idle")
        time.sleep(.05)
    assert "not running" in command("daemon", "status").stdout
    assert not (run_dir / "daemon.pid").read_text().strip()


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
                command("ls")  # a rapid reconnect must fit the startup deadline
                idle()  # time reactivation after a real idle exit
            samples["activation_ms" if managed else "self_spawn_ms"].append(connect())
            idle()
            if managed:
                socket = Path(os.fsdecode((run_dir / "daemon.endpoint").read_bytes()))
                identity = socket.stat().st_ino
                connect()  # same manager socket reactivates a fresh daemon
                assert socket.stat().st_ino == identity
                command("daemon", "stop")
                command("daemon", "uninstall-activation")
    # Native layout bases may differ for the same user; uninstall must own
    # its manager namespace as strictly as it owns its endpoint claim.
    peer_root, peer_env, peer_config, _, _ = instance("a-activation-peer-")
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
    # Pin the manager to the older executable, as a bundled app entry point is.
    # A newer client must drain it once, then receive a genuinely newer daemon.
    _, upgrade_env, upgrade_config, upgrade_run, upgrade_runtime = instance("a-activation-upgrade-")
    (upgrade_config / "config.toml").write_text("[daemon]\nidle_secs = 60\n")
    try:
        command("daemon", "install-activation", environment=upgrade_env, executable=old_binary)
        command("ls", environment=upgrade_env, executable=old_binary)
        previous = (upgrade_run / "daemon.pid").read_text().splitlines()[3]
        command("daemon", "stop", environment=upgrade_env, executable=old_binary)
        registration = json.loads((upgrade_run / "activation").read_text())
        if platform.system() == "Linux":
            unit = next(Path(registration["directory"]).glob("a-daemon-*.service"))
            lines = unit.read_text().splitlines()
            executable = json.dumps(str(old_binary)).replace("%", "%%").replace("$", "$$")
            unit.write_text("\n".join("ExecStart=" + executable + " __daemon --activated" if line.startswith("ExecStart=") else line for line in lines) + "\n")
            subprocess.run(["systemctl", "--user", "daemon-reload"], env=upgrade_env, check=True, capture_output=True)
        else:
            path = Path(registration["Launchd"]["plist"])
            definition = plistlib.loads(path.read_bytes())
            definition["ProgramArguments"][0] = str(old_binary)
            path.write_bytes(plistlib.dumps(definition))
            domain = f"gui/{os.getuid()}"
            subprocess.run(["launchctl", "bootout", domain + "/" + definition["Label"]], env=upgrade_env, check=True, capture_output=True)
            subprocess.run(["launchctl", "bootstrap", domain, str(path)], env=upgrade_env, check=True, capture_output=True)
            deadline = time.monotonic() + 20
            while command("ls", check=False, environment=upgrade_env, executable=old_binary).returncode:
                if time.monotonic() >= deadline:
                    raise AssertionError("older entry point failed to check in after reload")
                time.sleep(.05)
        command("ls", environment=upgrade_env, executable=old_binary)
        command("daemon", "status", environment=upgrade_env)
        selected = (upgrade_runtime / "current").readlink().name
        assert selected != previous, "upgrade probe needs distinct older/newer executables"
        (upgrade_config / "config.toml").write_text('[daemon]\nagent = "echo"\nidle_secs = 60\n')
        reply = command("-p", "activation-upgrade", environment=upgrade_env).stdout
        assert "activation-upgrade" in reply, "selected fixture daemon did not execute its echo agent"
        assert (upgrade_run / "daemon.pid").read_text().splitlines()[3] == selected
    finally:
        command("daemon", "stop", check=False, environment=upgrade_env)
        command("daemon", "uninstall-activation", check=False, environment=upgrade_env)
    result = {"platform": platform.platform(), "binary_bytes": binary.stat().st_size, "samples": samples, "cross_home_uninstall": True, "managed_upgrade": True,
              "medians": {key: statistics.median(values) for key, values in samples.items()}}
    result["activation_minus_self_spawn_ms"] = result["medians"]["activation_ms"] - result["medians"]["self_spawn_ms"]
    print(json.dumps(result, indent=2))
    Path("activation-result.json").write_text(json.dumps(result, indent=2) + "\n")
except BaseException:
    for log in root.rglob("daemon.jsonl"):
        print(f"Daemon log {log}:\n{log.read_text()}", flush=True)
    if platform.system() == "Darwin" and (run_dir / "activation").exists():
        registration = json.loads((run_dir / "activation").read_text())
        definition = plistlib.loads(Path(registration["Launchd"]["plist"]).read_bytes())
        subprocess.run(["launchctl", "print", f"gui/{os.getuid()}/" + definition["Label"]], check=False)
    raise
finally:
    command("daemon", "stop", check=False)
    command("daemon", "uninstall-activation", check=False)
