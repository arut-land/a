"""Autostart on hosted runners: the native registration round trip, residency,
and the first connect after an emulated login against a cold self-spawn.

Uses the runner's native installation (no A_HOME), so run it only on a
disposable hosted runner. Unix redirects the native bases under a temporary
HOME; Windows resolves its known folders, which no environment can redirect.
"""
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
parser.add_argument("--samples", type=int, default=12)
args = parser.parse_args()
binary = args.binary.resolve()
system = platform.system()

env = dict(os.environ)
env.pop("A_HOME", None)
if system == "Windows":
    state = Path(os.environ["LOCALAPPDATA"]) / "a"
    config, run_dir = state / "config", state / "state/run"
else:
    root = Path(tempfile.mkdtemp(prefix="a-autostart-"))
    env["HOME"] = str(root)
    for kind in ["CONFIG", "DATA", "STATE", "CACHE"]:
        env[f"XDG_{kind}_HOME"] = str(root / kind.lower())
    if system == "Darwin":
        state = root / "Library/Application Support/dev.a"
        config, run_dir = state / "config", state / "run"
    else:
        config, run_dir = root / "config/a", root / "state/a/run"
config.mkdir(parents=True, exist_ok=True)
# Short enough that a daemon which would idle-exit does so within the probe.
IDLE, SETTLED = 1, 2
(config / "config.toml").write_text(f"[daemon]\nidle_secs = {IDLE}\n")


def a(*words, check=True, environment=None):
    result = subprocess.run([str(binary), *words], env=env if environment is None else environment,
                            capture_output=True, text=True, timeout=60)
    if check and result.returncode:
        raise RuntimeError(f"{words}: {result.returncode}: {result.stderr}")
    return result


def pid():
    try:
        record = (run_dir / "daemon.pid").read_text().splitlines()
    except FileNotFoundError:
        return None
    return int(record[0]) if record and record[0].strip() else None


def wait(condition, what, within=20):
    deadline = time.monotonic() + within
    while not condition():
        if time.monotonic() >= deadline:
            raise AssertionError(what)
        time.sleep(.05)


def timed_ls():
    begin = time.perf_counter_ns()
    a("ls")
    return (time.perf_counter_ns() - begin) / 1e6


def registration():
    """Where `autostart on` registered this home, as the OS reads it."""
    if system == "Linux":
        return next((Path(env["XDG_CONFIG_HOME"]) / "systemd/user").glob("a-daemon-*.service"), None)
    if system == "Darwin":
        return next((root / "Library/LaunchAgents").glob("dev.a.daemon.*.plist"), None)
    import winreg
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Run") as key:
        index = 0
        while True:
            try:
                name, value, _ = winreg.EnumValue(key, index)
            except OSError:
                return None
            if name.startswith("a-daemon-"):
                return name, value
            index += 1


def login():
    """Starts the registered entry the way the OS does at login."""
    entry = registration()
    if system == "Linux":
        subprocess.run(["systemctl", "--user", "start", entry.name], env=env, check=True, capture_output=True)
    elif system == "Darwin":
        subprocess.run(["launchctl", "bootstrap", f"gui/{os.getuid()}", str(entry)], env=env, check=True, capture_output=True)
    else:
        subprocess.Popen(entry[1], env=env, creationflags=subprocess.DETACHED_PROCESS | subprocess.CREATE_NEW_PROCESS_GROUP,
                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def logout():
    """Leaves the manager as a fresh login finds it, with the daemon gone."""
    a("daemon", "stop", check=False)
    entry = registration()
    if system == "Darwin" and entry is not None:
        label = plistlib.loads(entry.read_bytes())["Label"]
        subprocess.run(["launchctl", "bootout", f"gui/{os.getuid()}/{label}"], env=env, capture_output=True)
    wait(lambda: pid() is None, "the daemon did not stop")


def resident(seconds):
    """Whether the running daemon outlives `seconds` with no clients."""
    before = pid()
    time.sleep(seconds)
    return before is not None and pid() == before


samples = {"self_spawn_ms": [], "login_first_ms": [], "login_warm_ms": []}
checks = {}
try:
    isolated = a("daemon", "autostart", "on", check=False, environment=dict(env, A_HOME=str(Path(tempfile.mkdtemp()))))
    assert isolated.returncode and "A_HOME" in isolated.stderr, isolated.stderr
    checks["isolated_homes_refused"] = True

    a("ls")
    assert not resident(IDLE + SETTLED + 2), "a daemon without autostart stayed up"
    checks["idle_exit_without_autostart"] = True

    a("daemon", "autostart", "on")
    assert registration() is not None
    a("ls")
    assert resident(IDLE + SETTLED + 2), "a self-spawned daemon with autostart on exited when idle"
    checks["resident_with_autostart"] = True
    running = pid()
    again = a("__daemon", "--login", check=False)
    assert again.returncode == 0 and pid() == running, again.stderr
    checks["login_entry_defers_to_a_running_daemon"] = True

    if system != "Windows":
        # A daemon brings an outdated registration up to date in the background.
        entry = registration()
        entry.write_text(entry.read_text().replace("--login", "--login-outdated"))
        logout()
        a("ls")
        wait(lambda: "--login-outdated" not in registration().read_text(), "the registration was not refreshed", SETTLED + 10)
        checks["registration_refreshed_by_a_daemon"] = True
    logout()

    for sample in range(args.samples):
        # Alternate order to reduce runner drift.
        for on in ([False, True] if sample % 2 == 0 else [True, False]):
            if on:
                a("daemon", "autostart", "on")
                login()
                wait(lambda: pid() is not None, "the login entry did not start a daemon")
                # A person's first command comes seconds after login.
                time.sleep(SETTLED + 1)
                samples["login_first_ms"].append(timed_ls())
                samples["login_warm_ms"].append(timed_ls())
                assert resident(IDLE + 1), "a login-started daemon exited when idle"
            else:
                a("daemon", "autostart", "off")
                assert registration() is None
                samples["self_spawn_ms"].append(timed_ls())
            logout()
    checks["login_started_daemons_stay_resident"] = True

    a("daemon", "autostart", "on")
    name = registration().name if system == "Linux" else None
    a("daemon", "autostart", "off")
    assert registration() is None
    if system == "Linux":
        enabled = subprocess.run(["systemctl", "--user", "is-enabled", name], env=env, capture_output=True, text=True)
        assert enabled.returncode, enabled.stdout
    checks["off_removes_the_registration"] = True

    medians = {key: statistics.median(values) for key, values in samples.items()}
    result = {"platform": platform.platform(), "binary_bytes": binary.stat().st_size, "samples": samples, "medians": medians,
              "login_first_minus_warm_ms": medians["login_first_ms"] - medians["login_warm_ms"],
              "login_first_minus_self_spawn_ms": medians["login_first_ms"] - medians["self_spawn_ms"], "checks": checks}
    print(json.dumps(result, indent=2))
    Path("autostart-result.json").write_text(json.dumps(result, indent=2) + "\n")
except BaseException:
    for log in state.rglob("daemon.jsonl"):
        print(f"Daemon log {log}:\n{log.read_text()}", flush=True)
    raise
finally:
    a("daemon", "autostart", "off", check=False)
    a("daemon", "stop", check=False)
