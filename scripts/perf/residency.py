"""Complete ownership waves over real fixture daemons, with alternating A/A controls."""
import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import platform
import queue
import shutil
import socket
import subprocess
import sys
import threading
import time
import tomllib
import unittest
import xml.etree.ElementTree as ET

from build import SUFFIX, environment, executable, release_arguments, stage
from lifecycle import checked, daemon
from process import memory_maps, snapshot, stop_daemon
from stats import distribution

ROOT = Path(__file__).resolve().parents[2]

# Generated only under CARGO_TARGET_DIR. Protocol encoding and native endpoints
# stay in their owning Rust crates, including Windows named pipes.
PROBE = r'''
use protocol::{Activity, ClientMsg, DaemonMsg, Decode, Entry, SessionId, Target};
use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

fn replay_is_complete(expected: &[Entry], actual: &[Entry], seq: u64) {
    assert_eq!(actual, expected, "reopen must preserve every durable entry");
    assert_eq!(seq, expected.last().map_or(0, |e| e.seq.get()));
}
fn durable(home: &sys::Home, id: SessionId) -> Vec<Entry> {
    let text = std::fs::read_to_string(home.sessions_dir().join(id.to_string()).join("log.jsonl"))
        .unwrap();
    assert!(text.ends_with('\n'));
    text.lines()
        .skip(1)
        .map(|line| Entry::from_json(line.as_bytes()).unwrap())
        .collect()
}
fn main() {
    let home = sys::Home::from(std::path::PathBuf::from(
        std::env::var_os("A_HOME").unwrap(),
    ));
    let connect = || {
        let mut stream = home.endpoint().connect().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        protocol::write_frame(
            &mut stream,
            &ClientMsg::Hello {
                proto: protocol::PROTO,
                build: "residency".into(),
                caps: protocol::Caps::NONE,
                client: None,
                machine: None,
            },
        )
        .unwrap();
        assert!(matches!(
            protocol::read_frame(&mut reader).unwrap(),
            Some(DaemonMsg::Welcome { .. })
        ));
        (stream, reader)
    };
    let attach = |target| {
        let (mut stream, mut reader) = connect();
        let started = Instant::now();
        protocol::write_frame(
            &mut stream,
            &ClientMsg::Attach {
                target,
                workspace: Some(std::env::current_dir().unwrap().try_into().unwrap()),
                since: 0,
                env: None,
                decl: Default::default(),
            },
        )
        .unwrap();
        let mut id = None;
        let mut entries = Vec::new();
        let seq = loop {
            match protocol::read_frame::<DaemonMsg>(&mut reader)
                .unwrap()
                .unwrap()
            {
                DaemonMsg::Attached { session, .. } => id = Some(session),
                DaemonMsg::Entry(entry) => entries.push(entry),
                DaemonMsg::Synced { seq } => break seq,
                DaemonMsg::Error { message } => panic!("{message}"),
                _ => {}
            }
        };
        let ms = started.elapsed().as_secs_f64() * 1000.;
        let id = id.unwrap();
        let expected = durable(&home, id);
        replay_is_complete(&expected, &entries, seq);
        (stream, reader, id, ms, entries.len(), seq)
    };
    let mut held = Vec::new();
    let mut slow = None;
    let mut last = String::new();
    for line in std::io::stdin().lock().lines() {
        let value: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        let result = match value["op"].as_str().unwrap() {
            "catalog" => {
                let (mut stream, mut reader) = connect();
                protocol::write_frame(&mut stream, &ClientMsg::List).unwrap();
                let Some(DaemonMsg::Sessions { sessions }) =
                    protocol::read_frame(&mut reader).unwrap()
                else {
                    panic!("expected catalog")
                };
                serde_json::to_value(sessions).unwrap()
            }
            "new" => {
                let (_, _, id, _, _, _) = attach(Target::New);
                serde_json::json!(id)
            }
            "attach" | "slow" => {
                let id: SessionId = serde_json::from_value(value["id"].clone()).unwrap();
                let (stream, reader, actual, ms, entries, seq) = attach(Target::Id(id));
                assert_eq!(actual, id);
                if value["op"] == "slow" {
                    slow = Some((stream, reader, id, seq));
                } else {
                    held.push((stream, reader));
                }
                serde_json::json!({"ms": ms, "entries": entries})
            }
            "turns" => {
                let count = value["count"].as_u64().unwrap();
                let bytes = value["bytes"].as_u64().unwrap() as usize;
                // Send to every active session before awaiting any reply.
                for turn in 0..count {
                    last = format!("wave {} turn {turn}: {}", value["wave"], "x".repeat(bytes));
                    for (stream, _) in &mut held {
                        protocol::write_frame(
                            stream,
                            &ClientMsg::Input {
                                segments: last.clone().into(),
                                mode: Default::default(),
                                force: false,
                                client_seq: None,
                            },
                        )
                        .unwrap();
                    }
                    for (_, reader) in &mut held {
                        let mut answered = false;
                        loop {
                            match protocol::read_frame::<DaemonMsg>(reader).unwrap().unwrap() {
                                DaemonMsg::Entry(entry) => {
                                    if let protocol::Body::Assistant { text } = entry.body {
                                        answered |= text == last;
                                    }
                                }
                                DaemonMsg::Activity {
                                    activity: Activity::Idle,
                                    ..
                                } if answered => break,
                                DaemonMsg::Error { message } => panic!("{message}"),
                                _ => {}
                            }
                        }
                    }
                }
                serde_json::json!({"completed": count * held.len() as u64})
            }
            "detach" => {
                for (mut stream, mut reader) in held.drain(..) {
                    protocol::write_frame(&mut stream, &ClientMsg::Detach).unwrap();
                    while protocol::read_frame::<DaemonMsg>(&mut reader)
                        .unwrap()
                        .is_some()
                    {}
                }
                serde_json::Value::Null
            }
            "drain" => {
                let (mut stream, mut reader, id, since) = slow.take().unwrap();
                let mut entries = Vec::new();
                let mut answered = false;
                loop {
                    match protocol::read_frame::<DaemonMsg>(&mut reader)
                        .unwrap()
                        .unwrap()
                    {
                        DaemonMsg::Entry(entry) => {
                            if let protocol::Body::Assistant { text } = &entry.body {
                                answered |= text == &last;
                            }
                            entries.push(entry);
                        }
                        DaemonMsg::Activity {
                            activity: Activity::Idle,
                            ..
                        } if answered => break,
                        DaemonMsg::Error { message } => panic!("slow follower: {message}"),
                        _ => {}
                    }
                }
                let expected: Vec<_> = durable(&home, id)
                    .into_iter()
                    .filter(|entry| entry.seq.get() > since)
                    .collect();
                assert!(
                    !expected.is_empty(),
                    "slow follower must have work to drain"
                );
                replay_is_complete(&expected, &entries, entries.last().unwrap().seq.get());
                protocol::write_frame(&mut stream, &ClientMsg::Detach).unwrap();
                while protocol::read_frame::<DaemonMsg>(&mut reader)
                    .unwrap()
                    .is_some()
                {}
                serde_json::json!({"complete": true})
            }
            "memory" => serde_json::json!({
                "charged_bytes": sys::process_memory::charged_bytes(value["pid"].as_u64().unwrap() as u32).unwrap(),
                "metric": sys::process_memory::METRIC,
            }),
            op => panic!("unknown operation {op}"),
        };
        println!("{}", result);
        std::io::stdout().flush().unwrap();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_or_prematurely_synced_history_is_never_a_latency_sample() {
        let entry = Entry::new(
            protocol::Seq::new(1).unwrap(),
            protocol::Epoch(1),
            protocol::Body::Assistant {
                text: "durable history".into(),
            },
        );
        let expected = [entry];
        replay_is_complete(&expected, &expected, 1);
        assert!(std::panic::catch_unwind(|| replay_is_complete(&expected, &[], 1)).is_err());
        assert!(std::panic::catch_unwind(|| replay_is_complete(&expected, &expected, 0)).is_err());
    }
}
'''


def cargo_run(command, **kwargs):
    if command[0] == "cargo":
        toolchain = os.environ.get("RUSTUP_TOOLCHAIN")
        if not toolchain:
            with (Path(kwargs["cwd"]) / "rust-toolchain.toml").open("rb") as stream:
                toolchain = tomllib.load(stream)["toolchain"]["channel"]
        command = ["mise", "exec", *(["cargo:cargo-nextest@0.9.150"] if command[1] == "nextest" else []),
                   "--", "cargo", f"+{toolchain}", *command[1:]]
    return subprocess.run(command, **kwargs)


def build_probe(source, output, target):
    root = target / "residency-probe-source"
    root.mkdir(parents=True, exist_ok=True)
    (root / "main.rs").write_text(PROBE)
    dependencies = tomllib.loads((source / "Cargo.toml").read_text())["workspace"]["dependencies"]
    manifest = '[workspace]\n[package]\nname="residency-probe"\nversion="0.0.0"\nedition="2024"\n[[bin]]\nname="residency-probe"\npath="main.rs"\n[dependencies]\n'
    for name in ("protocol", "sys", "serde_json"):
        value = dependencies[name]
        if isinstance(value, dict):
            value = dict(value)
            if "path" in value:
                value["path"] = str(source / value["path"])
            encoded = "{ " + ", ".join(f"{key} = {json.dumps(item)}" for key, item in value.items()) + " }"
        else:
            encoded = json.dumps(value)
        manifest += f'{name} = {encoded}\n'
    manifest += '\n[profile.release]\nopt-level="z"\nlto="fat"\ncodegen-units=1\n'
    (root / "Cargo.toml").write_text(manifest)
    shutil.copy2(source / "Cargo.lock", root / "Cargo.lock")
    env = environment(target)
    for command in (["cargo", "fmt", "--manifest-path", str(root / "Cargo.toml")],
                    ["cargo", "nextest", "run", "--manifest-path", str(root / "Cargo.toml")],
                    ["cargo", "clippy", "--manifest-path", str(root / "Cargo.toml"), "--all-targets", "--", "-D", "warnings"]):
        cargo_run(command, cwd=source, env=env, check=True)
    result = cargo_run(["cargo", "build", *release_arguments(), "--manifest-path", str(root / "Cargo.toml"),
                             "--message-format=json"], cwd=source, env=env, check=True, capture_output=True, text=True)
    binary = next(Path(r["executable"]) for line in result.stdout.splitlines()
                  if (r := json.loads(line)).get("executable"))
    return stage(binary, output / ("residency-probe" + SUFFIX))


class Probe:
    def __init__(self, binary, active):
        self.child = subprocess.Popen([str(binary)], cwd=active.home, env=active.env,
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=None, text=True)
        self.lines = queue.Queue()
        def read():
            for line in self.child.stdout:
                self.lines.put(line)
            self.lines.put(None)
        self.reader = threading.Thread(target=read, daemon=True)
        self.reader.start()

    def call(self, op, **args):
        self.child.stdin.write(json.dumps(dict(op=op, **args)) + "\n")
        self.child.stdin.flush()
        line = self.lines.get(timeout=120)
        if line is None:
            raise RuntimeError(f"probe ended during {op}: {self.child.wait(timeout=10)}")
        return json.loads(line)

    def close(self):
        self.child.stdin.close()
        try:
            self.child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.child.kill()
            self.child.wait()
        self.child.stdout.close()
        self.reader.join(timeout=5)


def catalog(probe, stored, live=0):
    rows = probe.call("catalog")
    assert len(rows) == stored, rows
    assert sum(row["residency"] == "loaded" for row in rows) == live, rows
    assert all(row["activity"] == "idle" for row in rows), rows
    return rows


def hibernate(probe, stored):
    started = time.monotonic()
    while time.monotonic() - started < 60:
        rows = probe.call("catalog")
        if len(rows) == stored and all(row["residency"] == "hibernated" and row["clients"] == 0 for row in rows):
            return dict(ms=(time.monotonic() - started) * 1000, catalog=rows)
        time.sleep(.05)
    raise TimeoutError(f"hibernation not acknowledged: {rows}")


def allocation_census(active):
    port = active.census / f"{active.pid}.port"
    deadline = time.monotonic() + 30
    while not port.exists():
        if time.monotonic() >= deadline:
            raise TimeoutError("fixture daemon did not publish census endpoint")
        time.sleep(.01)
    with socket.create_connection(("127.0.0.1", int(port.read_text())), timeout=30) as stream:
        with stream.makefile("r") as reader:
            value = json.loads(reader.readline())
    if "error" in value:
        raise RuntimeError(value["error"])
    if value["allocator"] == "glibc":
        root = ET.fromstring(value["malloc_info"])
        arena = int(root.find("./system[@type='current']").attrib["size"])
        free = sum(int(root.find(f"./total[@type='{kind}']").attrib["size"]) for kind in ("fast", "rest"))
        mapped = int(root.find("./total[@type='mmap']").attrib["size"])
        value.update(allocated_bytes=arena - free + mapped, retained_free_bytes=free,
                     allocator_bytes=arena + mapped)
    else:
        allocated = sum(row["allocated_bytes"] for row in value["heaps"])
        committed = sum(row["committed_bytes"] for row in value["heaps"])
        value.update(allocated_bytes=allocated, retained_free_bytes=committed - allocated,
                     allocator_bytes=committed)
    assert value["pid"] == active.pid, value
    value["observed_at_ns"] = time.time_ns()
    value["maps"] = memory_maps(active.pid)
    return value


def observe(probe, active, idle_seconds):
    if active.monitor_errors:
        raise RuntimeError(active.monitor_errors)
    charge = probe.call("memory", pid=active.pid)
    before = snapshot(active.pid, threads=True)
    started = time.monotonic()
    time.sleep(idle_seconds)
    after = snapshot(active.pid, threads=True)
    elapsed = time.monotonic() - started
    census = dict(census=allocation_census(active)) if hasattr(active, "census") else {}
    return dict(**charge, **census, **active.peaks, before=before, after=after, seconds=elapsed,
                cpu_ms=(after["cpu_seconds"] - before["cpu_seconds"]) * 1000,
                switches=after["ctx_switches"] - before["ctx_switches"])


def sample(binary, probe_binary, output, stored, count, turns, size, idle_seconds, census=False):
    with daemon(str(binary), output) as active:
        # lifecycle owns startup/teardown; restart applies this benchmark's
        # hibernation interval without changing the shared helper or product.
        active.client.stdin.close()
        active.client.wait(timeout=30)
        stop_daemon(binary, active.env, active.home)
        (active.home / "config.toml").write_text('[daemon]\nagent="echo"\nidle_secs=600\nhibernate_secs=1\n')
        if census:
            active.census = active.home / "alloc-census"
            active.census.mkdir()
        checked([str(binary), "ls"], env=active.env, cwd=active.home, timeout=60)
        active.pid = int((active.home / "run/daemon.pid").read_text().splitlines()[0])
        if census:
            allocation_census(active)  # warm diagnostic buffers before catalog seeding
        probe = Probe(probe_binary, active)
        with contextlib.ExitStack() as stack:
            stack.enter_context(contextlib.closing(probe))
            observer = stack.enter_context(contextlib.closing(Probe(probe_binary, active)))
            active.peaks, active.monitor_errors = {}, []
            stop = threading.Event()
            def monitor():
                try:
                    while not stop.is_set():
                        charge = observer.call("memory", pid=active.pid)["charged_bytes"]
                        active.peaks["sampled_peak_charged_bytes"] = max(charge, active.peaks.get("sampled_peak_charged_bytes", 0))
                        stop.wait(.2)
                except Exception as error:
                    active.monitor_errors.append(repr(error))
            watcher = threading.Thread(target=monitor, daemon=True)
            watcher.start()
            def finish_monitor():
                stop.set()
                watcher.join(timeout=125)
                assert not watcher.is_alive(), "charge observer did not stop"
            stack.callback(finish_monitor)
            ids = [row["id"] for row in probe.call("catalog")]
            while len(ids) < stored:
                ids.append(probe.call("new"))
            # All catalog entries have real, equal-sized durable histories.
            for id in ids:
                probe.call("attach", id=id)
                assert probe.call("turns", count=turns, bytes=size, wave="seed")["completed"] == turns
                probe.call("detach")
            stages = {}
            hibernate(probe, stored)
            time.sleep(12)  # existing blocking workers retire after ten seconds
            stages["catalog"] = observe(probe, active, idle_seconds)
            for wave in (1, 2):
                latencies = [probe.call("attach", id=id) for id in ids[:count]]
                if count:
                    probe.call("slow", id=ids[0])
                assert probe.call("turns", count=turns, bytes=size, wave=wave)["completed"] == count * turns
                rows = catalog(probe, stored, count)
                assert sum(row["clients"] for row in rows) == count + bool(count), rows
                stages[f"wave{wave}_used"] = dict(observe(probe, active, idle_seconds), catalog=rows, attach=latencies)
                probe.call("detach")
                stages[f"wave{wave}_detached"] = observe(probe, active, idle_seconds)
                if count:
                    # Slow readers are expected owners, even after others leave.
                    time.sleep(1.1)
                    rows = catalog(probe, stored, 1)
                    assert next(row for row in rows if row["id"] == ids[0])["clients"] == 1
                    stages[f"wave{wave}_slow"] = dict(observe(probe, active, idle_seconds), catalog=rows)
                    assert probe.call("drain")["complete"]
                ack = hibernate(probe, stored)
                time.sleep(12)
                stages[f"wave{wave}_hibernated"] = dict(observe(probe, active, idle_seconds), acknowledgement=ack)
            stages["wave2_hibernated"]["maps"] = memory_maps(active.pid)
            reopened = [probe.call("attach", id=id) for id in ids[:max(1, count)]]
            stages["reopen"] = dict(observe(probe, active, idle_seconds), attach=reopened)
            probe.call("detach")
            hibernate(probe, stored)
            return stages


def summarize(records):
    groups = {}
    for r in records:
        for name, s in r["stages"].items():
            values = dict(charged_kib=s["charged_bytes"] / 1024, rss_kib=s["before"]["rss_kib"],
                          sampled_peak_charged_kib=s["sampled_peak_charged_bytes"] / 1024,
                          cpu_ms=s["cpu_ms"], threads=s["after"]["thread_count"], switches=s["switches"])
            for key in ("peak_rss_kib", "peak_footprint_kib"):
                if key in s["after"]:
                    values[key] = s["after"][key]
            if "census" in s:
                values.update({metric.removesuffix("_bytes") + "_kib": s["census"][metric] / 1024 for metric in
                               ("allocated_bytes", "retained_free_bytes", "allocator_bytes")})
            if s.get("attach"):
                values["reattach_ms"] = max(a["ms"] for a in s["attach"])
            key = f'{r["label"]}/stored={r["stored"]}/active={r["active"]}/{name}'
            for metric, value in values.items():
                groups.setdefault(key, {}).setdefault(metric, []).append(value)
        for later, earlier in (("wave1_used", "catalog"), ("wave1_hibernated", "catalog"),
                               ("wave2_hibernated", "wave1_hibernated")):
            key = f'{r["label"]}/stored={r["stored"]}/active={r["active"]}/{later}-minus-{earlier}'
            group = groups.setdefault(key, {})
            if "census" in r["stages"][later]:
                for metric in ("allocated_bytes", "retained_free_bytes", "allocator_bytes"):
                    group.setdefault(metric.removesuffix("_bytes") + "_kib", []).append((r["stages"][later]["census"][metric] -
                                                         r["stages"][earlier]["census"][metric]) / 1024)
            for metric, value in dict(
                charged_kib=(r["stages"][later]["charged_bytes"] - r["stages"][earlier]["charged_bytes"]) / 1024,
                rss_kib=r["stages"][later]["before"]["rss_kib"] - r["stages"][earlier]["before"]["rss_kib"],
            ).items():
                group.setdefault(metric, []).append(value)
    return {key: {metric: distribution(values) for metric, values in group.items()} for key, group in groups.items()}


class ResidencyTests(unittest.TestCase):
    def test_census_is_available_only_after_isolated_daemon_opt_in(self):
        if not (binary := os.environ.get("RESIDENCY_BINARY")):
            self.skipTest("set RESIDENCY_BINARY to a release fixture executable")
        with daemon(binary) as active:
            active.census = active.home / "alloc-census"
            assert not active.census.exists()
            active.client.stdin.close()
            active.client.wait(timeout=30)
            stop_daemon(binary, active.env, active.home)
            active.census.mkdir()
            checked([binary, "ls"], env=active.env, cwd=active.home, timeout=60)
            active.pid = int((active.home / "run/daemon.pid").read_text().splitlines()[0])
            if sys.platform == "darwin":
                with self.assertRaisesRegex(RuntimeError, "native census requires glibc or Windows"):
                    allocation_census(active)
            else:
                value = allocation_census(active)
                self.assertGreater(value["allocated_bytes"], 0)
                self.assertGreaterEqual(value["allocator_bytes"], value["allocated_bytes"])

    def test_full_lifecycle_preserves_history_and_slow_followers_own_only_their_session(self):
        binary, probe = os.environ.get("RESIDENCY_BINARY"), os.environ.get("RESIDENCY_PROBE")
        if not binary or not probe:
            self.skipTest("set RESIDENCY_BINARY and RESIDENCY_PROBE to release executables")
        sample(Path(binary), Path(probe), None, 2, 1, 2, 65536, .05)


def paired(records):
    indexed = {(r["round"], r["stored"], r["active"], r["label"]): r for r in records}
    groups = {}
    for (round, stored, active, label), r in indexed.items():
        other = indexed.get((round, stored, active, "A"))
        if label != "A2" or other is None:
            continue
        for stage, value in r["stages"].items():
            key = f"stored={stored}/active={active}/{stage}"
            group = groups.setdefault(key, {})
            if "census" in value:
                for metric in ("allocated_bytes", "retained_free_bytes", "allocator_bytes"):
                    group.setdefault(metric.removesuffix("_bytes") + "_kib", []).append(
                        (value["census"][metric] - other["stages"][stage]["census"][metric]) / 1024)
            for metric, difference in dict(
                charged_kib=(value["charged_bytes"] - other["stages"][stage]["charged_bytes"]) / 1024,
                cpu_ms=value["cpu_ms"] - other["stages"][stage]["cpu_ms"],
            ).items():
                group.setdefault(metric, []).append(difference)
    return {key: {metric: distribution(values) for metric, values in group.items()} for key, group in groups.items()}


def table(summary):
    lines = ["Values are median [min, max]; A and A2 are unchanged-source controls.", "",
             "| case/stage | charge KiB | RSS KiB | CPU ms | threads | reattach ms (slowest session/run) |",
             "| --- | ---: | ---: | ---: | ---: | ---: |"]
    for key, row in summary.items():
        if "minus" in key:
            continue
        cells = []
        for metric in ("charged_kib", "rss_kib", "cpu_ms", "threads", "reattach_ms"):
            value = row.get(metric)
            cells.append("" if value is None else "{median:.3f} [{min:.3f}, {max:.3f}]".format(**value))
        lines.append("| " + key + " | " + " | ".join(cells) + " |")
    census = {key: row for key, row in summary.items() if "allocated_kib" in row}
    if census:
        lines.extend(["", "Instrumented native allocator observations; timings above are diagnostic only.", "",
                      "| case/stage | allocator allocated KiB | allocator free capacity KiB | allocator total KiB |",
                      "| --- | ---: | ---: | ---: |"])
        for key, row in census.items():
            cells = [f"{row[metric]['median']:.3f} [{row[metric]['min']:.3f}, {row[metric]['max']:.3f}]"
                     for metric in ("allocated_kib", "retained_free_kib", "allocator_kib")]
            lines.append("| " + " | ".join([key, *cells]) + " |")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=ROOT)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--target", type=Path, required=True)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--probe", type=Path)
    parser.add_argument("--build-only", action="store_true")
    parser.add_argument("--census", action="store_true", help="instrumented native allocator cohort; excluded from timing claims")
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--stored", default="1,8,32")
    parser.add_argument("--active", default="1,4,8")
    parser.add_argument("--turns", type=int, default=4)
    parser.add_argument("--bytes", type=int, default=65536)
    parser.add_argument("--idle-seconds", type=float, default=1)
    args = parser.parse_args()
    # Do not inherit provider credentials or proxy routing into fixture processes.
    keep = {"PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "TMP", "TEMP", "LANG",
            "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "USERPROFILE", "APPDATA", "LOCALAPPDATA",
            "PROGRAMFILES", "PROGRAMFILES(X86)", "PROGRAMDATA", "NUMBER_OF_PROCESSORS",
            "RUNNER_TEMP", "GITHUB_STEP_SUMMARY"}
    clean = {key: value for key, value in os.environ.items()
             if key.upper() in keep or key.startswith(("CARGO_", "RUST", "MISE_", "MBX_"))}
    os.environ.clear()
    os.environ.update(clean, OPENAI_API_KEY="loopback-dummy", OPENAI_BASE_URL="http://127.0.0.1:1/v1",
                      NO_PROXY="127.0.0.1,localhost", CARGO_BUILD_JOBS="2")
    if args.census and sys.platform not in ("linux", "win32"):
        parser.error("native census cohort requires glibc Linux or Windows")
    if args.rounds < 5:
        parser.error("at least five interleaved rounds are required")
    stored, counts = [int(n) for n in args.stored.split(",")], [int(n) for n in args.active.split(",")]
    if min(stored + counts) < 1 or max(counts) > max(stored) or args.turns < 1 or args.bytes < 1 or args.idle_seconds <= 0:
        parser.error("positive populations/work required, with active <= maximum stored")
    output, target, source = args.output.resolve(), args.target.resolve(), args.source.resolve()
    if (output / "residency.jsonl").exists() or (output / "residency-failures.jsonl").exists():
        parser.error("select a fresh output directory; existing samples must not be mixed")
    output.mkdir(parents=True, exist_ok=True)
    if args.census:
        cargo_run(["cargo", "nextest", "run", "-p", "sys", "--features", "fixtures"],
                  cwd=source, env=environment(target), check=True)
    binary = args.binary.resolve() if args.binary else stage(executable(source, environment(target), ["--features", "fixtures"], run=cargo_run), output / ("a-fixtures" + SUFFIX))
    probe = args.probe.resolve() if args.probe else build_probe(source, output, target)
    from hosted import hardware
    metadata = dict(hardware=hardware(), platform=platform.platform(), machine=platform.machine(),
                    revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip(),
                    working_tree=subprocess.check_output(["git", "status", "--short"], cwd=source, text=True),
                    workload_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                    binaries={str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in (binary, probe)},
                    parameters=dict(census=args.census, stored=stored, active=counts, turns=args.turns, bytes=args.bytes, idle_seconds=args.idle_seconds))
    (output / "residency-host.json").write_text(json.dumps(metadata, indent=2) + "\n")
    if args.build_only:
        return
    cases = list(dict.fromkeys([(1, 0), *((n, 1) for n in stored), *((max(stored), n) for n in counts)]))
    records = []
    for round in range(args.rounds):
        for total, active in (cases if round % 2 == 0 else list(reversed(cases))):
            for label in (("A", "A2") if round % 2 == 0 else ("A2", "A")):
                print(f"round={round} {label} stored={total} active={active}", flush=True)
                record = dict(round=round, label=label, stored=total, active=active)
                try:
                    record["stages"] = sample(binary, probe, output, total, active, args.turns, args.bytes, args.idle_seconds, args.census)
                except Exception as error:
                    record["failure"] = repr(error)
                    with (output / "residency-failures.jsonl").open("a") as stream:
                        stream.write(json.dumps(record) + "\n")
                    raise
                records.append(record)
                with (output / "residency.jsonl").open("a") as stream:
                    stream.write(json.dumps(record) + "\n")
                (output / "residency.json").write_text(json.dumps(dict(**metadata, summary=summarize(records), paired_A2_minus_A=paired(records)), indent=2) + "\n")

    report = table(summarize(records))
    (output / "residency.md").write_text(report)
    if summary_file := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary_file, "a") as stream:
            stream.write(report)


if __name__ == "__main__":
    main()
