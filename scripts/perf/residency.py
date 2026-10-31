"""Complete ownership waves over real fixture daemons, with alternating controls."""
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


# Applied only to the measured fixture checkout; the exact diff accompanies samples.
OVERLAP_HOOK = r'''
    #[cfg(feature = "fixtures")]
    fn overlap_release(release: impl FnOnce()) -> Option<bool> {
        use std::io::{Read, Write};
        let home = std::env::var_os("A_HOME")?;
        let directory = std::path::PathBuf::from(home).join("release-overlap");
        if !directory.is_dir() {
            return None;
        }
        if std::fs::remove_file(directory.join("arm")).is_err() {
            return Some(false);
        }
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        std::fs::write(directory.join("port"), listener.local_addr().unwrap().port().to_string()).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_nodelay(true).unwrap();
        let mut command = [0];
        stream.read_exact(&mut command).unwrap();
        stream.write_all(b"B").unwrap();
        let now = || u64::try_from(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()).unwrap();
        let begin_ns = now();
        let started = std::time::Instant::now();
        if command[0] == b'R' {
            release();
        }
        let native_ns = started.elapsed().as_nanos();
        let end_ns = now();
        writeln!(stream, "{}", serde_json::json!({"begin_ns": begin_ns, "end_ns": end_ns,
            "native_ns": native_ns, "released": command[0] == b'R'})).unwrap();
        Some(command[0] == b'R')
    }
'''


def instrument_overlap(source, output):
    path = source / "crates/sys/src/memory.rs"
    text = path.read_text()
    if "pub fn run(pending: &Pending)" not in text:
        raise ValueError("overlap requires a source revision with a native release policy; use the measured experiment revision")
    begin = text.index('        #[cfg(target_os = "linux")]\n        {\n            // Keep the main arena')
    end = text.index('        true\n', begin)
    release = text[begin:end]
    text = text[:begin] + ('        #[cfg(feature = "fixtures")]\n'
                           '        if let Some(result) = overlap_release(|| {\n' + release +
                           '        }) { return result; }\n') + text[begin:]
    text = text.replace('    #[cfg(target_os = "linux")]\n    struct Batch {',
                        OVERLAP_HOOK + '\n    #[cfg(target_os = "linux")]\n    struct Batch {', 1)
    path.write_text(text)
    (output / "overlap-instrumentation.patch").write_bytes(subprocess.check_output(
        ["git", "diff", "--", "crates/sys/src/memory.rs"], cwd=source))


def cargo_run(command, *, nextest_version="0.9.150", **kwargs):
    cargo = subprocess.check_output(["rustup", "which", "cargo"], cwd=kwargs.get("cwd"),
                                    env=kwargs.get("env"), text=True).strip()
    try:
        return subprocess.run(["mise", "exec", *([f"cargo:cargo-nextest@{nextest_version}"] if command[1] == "nextest" else []),
                               "--", cargo, *command[1:]], **kwargs)
    except subprocess.CalledProcessError as error:
        if error.stdout:
            print(error.stdout, file=sys.stderr)
        if error.stderr:
            print(error.stderr, file=sys.stderr)
        raise

# Generated only under CARGO_TARGET_DIR. Protocol encoding and native endpoints
# stay in their owning Rust crates, including Windows named pipes.
PROBE = r'''
use protocol::history::{Direction, Filter, Head, Heads, Located, Page, Request};
use protocol::view::{Item, SessionView};
use protocol::{Activity, ClientMsg, DaemonMsg, Decode, Entry, HostPath, SessionId, Target};
use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

fn unix_ns() -> u64 {
    u64::try_from(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()).unwrap()
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

fn replay_is_complete(expected: &SessionView, actual: &[Located], head: &Head) {
    assert_eq!(
        expected.seq, head.cut.seq,
        "the head must name a durable log cut"
    );
    assert_eq!(
        expected.items.len(),
        head.count,
        "the head must count every durable item"
    );
    assert_eq!(
        actual.len(),
        head.count,
        "demanded history must cover the complete head"
    );
    for (index, (actual, expected)) in actual.iter().zip(expected.items.iter()).enumerate() {
        assert_eq!(
            actual.index, index,
            "history must contain each index once, in order"
        );
        assert_eq!(
            &actual.item, expected,
            "history must preserve every durable item"
        );
    }
}

fn text(item: &mut Item) -> &mut String {
    match item {
        Item::User { text, .. } | Item::Assistant { text, .. } | Item::Reply { text, .. } => text,
        _ => panic!("only text items can be fragmented"),
    }
}

#[derive(Default)]
struct Replay {
    items: Vec<Located>,
    partial: Option<Item>,
    offset: Option<usize>,
}

impl Replay {
    fn append(&mut self, page: Page, head: &Head) {
        assert_eq!(page.cut, head.cut, "a page must belong to the demanded cut");
        assert_eq!(page.total, head.count);
        assert_eq!(page.start, self.items.len());
        assert!(!page.items.is_empty(), "a demanded page must advance");
        for mut located in page.items {
            assert_eq!(
                located.index,
                self.items.len(),
                "a range skipped or repeated an item"
            );
            if let Some(span) = page.fragment.as_ref().filter(|s| s.index == located.index) {
                assert_eq!(span.key, located.item.key());
                assert_eq!(span.start, self.offset.unwrap_or(0));
                assert!(span.end > span.start && span.end <= span.total);
                assert_eq!(text(&mut located.item).len(), span.end - span.start);
                if let Some(mut previous) = self.partial.take() {
                    let suffix = std::mem::take(text(&mut located.item));
                    let prefix = std::mem::take(text(&mut previous));
                    assert_eq!(
                        previous, located.item,
                        "fragment metadata changed within a cut"
                    );
                    *text(&mut located.item) = prefix + &suffix;
                }
                if span.end < span.total {
                    self.partial = Some(located.item);
                    self.offset = Some(span.end);
                    continue;
                }
                assert_eq!(text(&mut located.item).len(), span.total);
                self.offset = None;
            } else {
                assert!(self.partial.is_none(), "a fragment was left incomplete");
            }
            self.items.push(located);
        }
        assert_eq!(
            page.end,
            self.items.len() + usize::from(self.partial.is_some())
        );
    }
}

struct Client {
    stream: sys::OwnedConnection,
    reader: BufReader<sys::OwnedConnection>,
    session: Option<(SessionId, HostPath)>,
    heads: Heads,
    head: Option<Head>,
    activity: Option<(u64, Activity)>,
    request: u64,
}

impl Client {
    fn connect(home: &sys::Home) -> Self {
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
        Self {
            stream,
            reader,
            session: None,
            heads: Heads::default(),
            head: None,
            activity: None,
            request: 0,
        }
    }

    fn receive(&mut self) -> DaemonMsg {
        let msg = protocol::read_frame::<DaemonMsg>(&mut self.reader)
            .unwrap()
            .expect("daemon disconnected");
        match &msg {
            DaemonMsg::Attached { session, workspace } => {
                self.session = Some((*session, workspace.clone()))
            }
            DaemonMsg::Head { head } => self.head = Some((**head).clone()),
            DaemonMsg::HeadPart {
                seq,
                offset,
                text,
                done,
            } => {
                if let Some(head) = self.heads.part(*seq, *offset, text, *done).unwrap() {
                    self.head = Some(head);
                }
            }
            DaemonMsg::Activity { seq, activity } => self.activity = Some((*seq, *activity)),
            DaemonMsg::Error { message } => panic!("{message}"),
            DaemonMsg::Reply { outcome, .. } => assert!(
                matches!(outcome, protocol::Outcome::Done { .. }),
                "history request failed: {outcome:?}"
            ),
            _ => {}
        }
        msg
    }

    fn attach(home: &sys::Home, target: Target) -> (Self, f64, usize) {
        let committed = if let Target::Id(id) = &target {
            durable(home, *id).last().map_or(0, |e| e.seq.get())
        } else {
            0
        };
        let (client, ms, items, _) = Self::attach_on(home, target, Self::connect(home), committed);
        (client, ms, items)
    }

    fn attach_on(home: &sys::Home, target: Target, mut client: Self, committed: u64) -> (Self, f64, usize, u64) {
        let started = Instant::now();
        let message = ClientMsg::Attach {
                target,
                workspace: Some(std::env::current_dir().unwrap().try_into().unwrap()),
                since: 0,
                env: None,
                decl: Default::default(),
            };
        let sent_ns = unix_ns();
        protocol::write_frame(&mut client.stream, &message).unwrap();
        let seq = loop {
            if let DaemonMsg::Synced { seq } = client.receive() {
                break seq;
            }
        };
        let head = client
            .head
            .clone()
            .expect("attach must publish a head before Synced");
        assert_eq!(head.cut.seq, seq);
        assert!(
            seq >= committed,
            "attach omitted facts already durable before its head"
        );
        let items = client.replay(home, &head);
        let ms = started.elapsed().as_secs_f64() * 1000.;
        (client, ms, items.len(), sent_ns)
    }

    fn replay(&mut self, home: &sys::Home, head: &Head) -> Vec<Located> {
        let (id, workspace) = self.session.as_ref().unwrap();
        assert_eq!(head.cut.session, *id);
        let expected = SessionView::from_log(
            *id,
            workspace.clone(),
            durable(home, *id)
                .into_iter()
                .filter(|e| e.seq.get() <= head.cut.seq),
        );
        let mut replay = Replay::default();
        while replay.items.len() < head.count {
            self.request += 1;
            protocol::write_frame(
                &mut self.stream,
                &ClientMsg::History {
                    id: self.request,
                    range: Request {
                        cut: head.cut,
                        anchor: replay.items.len(),
                        text_offset: replay.offset,
                        direction: Direction::After,
                        bytes: 64 << 10,
                        filter: Filter::Transcript,
                    },
                },
            )
            .unwrap();
            let mut bytes = String::new();
            loop {
                if let DaemonMsg::HistoryPart {
                    id,
                    offset,
                    text,
                    done,
                } = self.receive()
                {
                    assert_eq!(id, self.request);
                    assert_eq!(
                        offset,
                        bytes.len() as u64,
                        "history chunks must be contiguous"
                    );
                    bytes.push_str(&text);
                    if done {
                        break;
                    }
                }
            }
            replay.append(Page::from_json(bytes.as_bytes()).unwrap(), head);
        }
        assert!(replay.partial.is_none(), "the final item must be complete");
        replay_is_complete(&expected, &replay.items, head);
        replay.items
    }

    fn reply(&mut self, home: &sys::Home, since: u64, last: &str) -> Vec<Located> {
        let mut checked = since;
        loop {
            if let Some((seq, Activity::Idle)) = self.activity
                && seq > checked
                && let Some(head) = self.head.clone().filter(|h| h.cut.seq == seq)
            {
                checked = seq;
                let items = self.replay(home, &head);
                if items.iter().rev().find_map(|i| match &i.item {
                    Item::Assistant { text, .. } => Some(text.as_str()),
                    _ => None,
                }) == Some(last)
                {
                    return items;
                }
                continue;
            }
            self.receive();
        }
    }

    fn detach(mut self) {
        protocol::write_frame(&mut self.stream, &ClientMsg::Detach).unwrap();
        while protocol::read_frame::<DaemonMsg>(&mut self.reader)
            .unwrap()
            .is_some()
        {}
    }
}

fn main() {
    let home = sys::Home::from(std::path::PathBuf::from(
        std::env::var_os("A_HOME").unwrap(),
    ));
    let mut held: Vec<Client> = Vec::new();
    let mut slow = None;
    let mut last = String::new();
    for line in std::io::stdin().lock().lines() {
        let value: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        let result = match value["op"].as_str().unwrap() {
            "catalog" => {
                let mut client = Client::connect(&home);
                protocol::write_frame(&mut client.stream, &ClientMsg::List).unwrap();
                let DaemonMsg::Sessions { sessions } = client.receive() else {
                    panic!("expected catalog")
                };
                serde_json::to_value(sessions).unwrap()
            }
            "new" => {
                let (client, _, _) = Client::attach(&home, Target::New);
                let id = client.session.as_ref().unwrap().0;
                client.detach();
                serde_json::json!(id)
            }
            "attach" | "slow" => {
                let id: SessionId = serde_json::from_value(value["id"].clone()).unwrap();
                let (mut client, ms, items) = Client::attach(&home, Target::Id(id));
                assert_eq!(client.session.as_ref().unwrap().0, id);
                if value["op"] == "slow" {
                    let head = client.head.clone().unwrap();
                    let before = client.replay(&home, &head);
                    slow = Some((client, head, before));
                } else {
                    held.push(client);
                }
                serde_json::json!({"ms": ms, "items": items})
            }
            "overlap" => {
                use std::io::Read;
                let id: SessionId = serde_json::from_value(value["id"].clone()).unwrap();
                let turn = value["action"] == "turn";
                let committed = durable(&home, id).last().map_or(0, |entry| entry.seq.get());
                let mut prepared = if turn {
                    Client::attach(&home, Target::Id(id)).0
                } else {
                    Client::connect(&home)
                };
                let input = "overlap: ".to_owned() + &"x".repeat(value["bytes"].as_u64().unwrap() as usize);
                let message = ClientMsg::Input {
                    segments: input.clone().into(), mode: Default::default(),
                    force: false, client_seq: None,
                };
                let mut frame = Vec::new();
                protocol::write_frame(&mut frame, &message).unwrap();
                let mut stream = std::net::TcpStream::connect(("127.0.0.1", value["port"].as_u64().unwrap() as u16)).unwrap();
                stream.set_nodelay(true).unwrap();
                stream.write_all(if value["release"] == true { b"R" } else { b"C" }).unwrap();
                let mut marker = [0];
                stream.read_exact(&mut marker).unwrap();
                assert_eq!(marker, *b"B");
                let started = Instant::now();
                let (client, begin_ns) = if turn {
                    let since = prepared.head.as_ref().unwrap().cut.seq;
                    let sent_ns = unix_ns();
                    prepared.stream.write_all(&frame).unwrap();
                    prepared.reply(&home, since, &input);
                    (prepared, sent_ns)
                } else {
                    let (client, _, _, sent_ns) = Client::attach_on(&home, Target::Id(id), prepared, committed);
                    (client, sent_ns)
                };
                let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                let end_ns = unix_ns();
                let mut native = String::new();
                BufReader::new(stream).read_line(&mut native).unwrap();
                let native: serde_json::Value = serde_json::from_str(&native).unwrap();
                client.detach();
                serde_json::json!({"action": value["action"], "begin_ns": begin_ns,
                    "end_ns": end_ns, "ms": elapsed_ms, "native": native})
            }
            "turns" => {
                let count = value["count"].as_u64().unwrap();
                let bytes = value["bytes"].as_u64().unwrap() as usize;
                let mut elapsed = Vec::new();
                for turn in 0..count {
                    last = format!("wave {} turn {turn}: {}", value["wave"], "x".repeat(bytes));
                    let since: Vec<_> = held
                        .iter()
                        .map(|c| c.head.as_ref().unwrap().cut.seq)
                        .collect();
                    let started = Instant::now();
                    for client in &mut held {
                        protocol::write_frame(
                            &mut client.stream,
                            &ClientMsg::Input {
                                segments: last.clone().into(),
                                mode: Default::default(),
                                force: false,
                                client_seq: None,
                            },
                        )
                        .unwrap();
                    }
                    for (client, since) in held.iter_mut().zip(since) {
                        client.reply(&home, since, &last);
                    }
                    elapsed.push(started.elapsed().as_secs_f64() * 1000.0);
                }
                serde_json::json!({"completed": count * held.len() as u64, "turn_batch_ms": elapsed})
            }
            "detach" => {
                for client in held.drain(..) {
                    client.detach();
                }
                serde_json::Value::Null
            }
            "drain" => {
                let (mut client, head, before) = slow.take().unwrap();
                assert_eq!(
                    client.replay(&home, &head),
                    before,
                    "a lagging follower's original cut must remain immutable"
                );
                let items = client.reply(&home, head.cut.seq, &last);
                assert!(
                    items.len() > before.len(),
                    "slow follower must have work to drain"
                );
                assert!(
                    items.starts_with(&before),
                    "slow follower must preserve its complete prior history"
                );
                client.detach();
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
    fn missing_duplicate_reordered_or_truncated_history_is_never_a_latency_sample() {
        let id = SessionId::new();
        let entries: Vec<_> = (1..=2)
            .map(|n| {
                Entry::new(
                    protocol::Seq::new(n).unwrap(),
                    protocol::Epoch(1),
                    protocol::Body::Assistant {
                        text: format!("durable item {n}"),
                    },
                )
            })
            .collect();
        let expected = SessionView::from_log(id, "/fixture".into(), &entries);
        let mut index = protocol::view::Index::new(id, expected.workspace.clone());
        for entry in &entries {
            index.apply(entry);
        }
        let head = index.head(protocol::Epoch(1));
        let actual: Vec<_> = expected
            .items
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, item)| Located { index, item })
            .collect();
        replay_is_complete(&expected, &actual, &head);
        let rejects = |items: &[Located], head: &Head| {
            assert!(
                std::panic::catch_unwind(|| replay_is_complete(&expected, items, head)).is_err()
            )
        };
        rejects(&actual[..1], &head);
        rejects(&[actual[0].clone(), actual[0].clone()], &head);
        rejects(&[actual[1].clone(), actual[0].clone()], &head);
        let mut truncated = actual.clone();
        text(&mut truncated[1].item).pop();
        rejects(&truncated, &head);
        let mut premature = head;
        premature.cut.seq = 0;
        rejects(&actual, &premature);
    }

    #[test]
    fn demanded_fragments_reconstruct_every_item_once_in_order() {
        let id = SessionId::new();
        let entries: Vec<_> = (1..=2)
            .map(|n| {
                Entry::new(
                    protocol::Seq::new(n).unwrap(),
                    protocol::Epoch(1),
                    protocol::Body::Assistant {
                        text: format!("item {n}: {}", "αβγδεζηθ".repeat(8)),
                    },
                )
            })
            .collect();
        let expected = SessionView::from_log(id, "/fixture".into(), &entries);
        let mut index = protocol::view::Index::new(id, expected.workspace.clone());
        for entry in &entries {
            index.apply(entry);
        }
        let head = index.head(protocol::Epoch(1));
        let mut replay = Replay::default();
        let mut pages = 0;
        while replay.items.len() < head.count {
            let request = Request {
                cut: head.cut,
                anchor: replay.items.len(),
                text_offset: replay.offset,
                direction: Direction::After,
                bytes: 9,
                filter: Filter::Transcript,
            };
            let page = index
                .page(&request, |seq| Ok(entries[seq.get() as usize - 1].clone()))
                .unwrap();
            replay.append(page, &head);
            pages += 1;
        }
        assert!(
            pages > head.count,
            "the fixture must exercise multiple fragments per item"
        );
        assert!(replay.partial.is_none());
        replay_is_complete(&expected, &replay.items, &head);
    }
}
'''


def build_probe(source, output, target):
    root = target / "residency-probe-source"
    root.mkdir(parents=True, exist_ok=True)
    (root / "main.rs").write_text(PROBE, encoding="utf-8")
    dependencies = tomllib.loads((source / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["dependencies"]
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
    """Wait for actor exit; zero clients alone still permits resident teardown."""
    started = time.monotonic()
    while time.monotonic() - started < 60:
        rows = probe.call("catalog")
        assert len(rows) == stored, rows
        if all(row["residency"] == "hibernated" and row["clients"] == 0 for row in rows):
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
    if output := getattr(active, "census_output", None):
        with (output / "census-native.jsonl").open("a", encoding="utf-8") as raw:
            raw.write(json.dumps(value) + "\n")
    if "error" in value:
        raise RuntimeError(value["error"])
    if value["allocator"] == "glibc":
        root = ET.fromstring(value["malloc_info"])
        arena = int(root.find("./system[@type='current']").attrib["size"])
        totals = {total.attrib["type"]: int(total.attrib["size"]) for total in root.findall("./total")}
        free = totals["rest"] + totals.get("fast", 0)
        mapped = totals["mmap"]
        value.update(allocated_bytes=arena - free + mapped, retained_free_bytes=free,
                     allocator_bytes=arena + mapped)
    elif value["allocator"] == "apple-malloc":
        stats = value["statistics"]
        value.update(allocated_bytes=stats["size_in_use"],
                     retained_free_bytes=stats["size_allocated"] - stats["size_in_use"],
                     allocator_bytes=stats["size_allocated"])
    else:
        allocated = sum(row["allocated_bytes"] for row in value["heaps"])
        committed = sum(row["committed_bytes"] for row in value["heaps"])
        value.update(allocated_bytes=allocated, retained_free_bytes=committed - allocated,
                     allocator_bytes=committed)
    assert value["pid"] == active.pid, value
    value["observed_at_ns"] = time.time_ns()
    value["maps"] = memory_maps(active.pid)
    if getattr(active, "vmmap", None):
        active.census_index += 1
        filename = f"vmmap-{active.pid}-{active.census_index:02}.csv"
        subprocess.run([str(active.vmmap), "-accepteula", "-p", str(active.pid),
                        str(active.census_output / filename)], check=True, timeout=60)
        value["vmmap"] = filename
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


def page_faults(pid):
    return {key: value for key, value in snapshot(pid).items()
            if key in ("minor_faults", "major_faults", "pfaults", "pageins", "num_page_faults")}


def sample(binary, probe_binary, output, stored, count, turns, size, idle_seconds, census=False, vmmap=None, overlap=None):
    with daemon(str(binary), output) as active:
        # lifecycle owns startup/teardown; restart applies this benchmark's
        # hibernation interval without changing the shared helper or product.
        active.client.stdin.close()
        active.client.wait(timeout=30)
        stop_daemon(binary, active.env, active.home)
        (active.home / "config.toml").write_text('[daemon]\nagent="echo"\nidle_secs=600\nhibernate_secs=1\n')
        if census:
            active.census = active.home / "alloc-census"
            active.census_output = output
            active.vmmap, active.census_index = vmmap, 0
            active.census.mkdir()
        if overlap:
            (active.home / "release-overlap").mkdir()
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
            if not overlap:
                time.sleep(12)  # existing blocking workers retire after ten seconds
            stages["catalog"] = observe(probe, active, idle_seconds)
            for wave in (1, 2):
                latencies = [probe.call("attach", id=id) for id in ids[:count]]
                if count:
                    probe.call("slow", id=ids[0])
                completed = probe.call("turns", count=turns, bytes=size, wave=wave)
                assert completed["completed"] == count * turns
                rows = catalog(probe, stored, count)
                assert sum(row["clients"] for row in rows) == count + bool(count), rows
                stages[f"wave{wave}_used"] = dict(observe(probe, active, idle_seconds), catalog=rows, attach=latencies, turns=completed)
                probe.call("detach")
                stages[f"wave{wave}_detached"] = observe(probe, active, idle_seconds)
                if count:
                    # Slow readers are expected owners, even after others leave.
                    time.sleep(1.1)
                    rows = catalog(probe, stored, 1)
                    assert next(row for row in rows if row["id"] == ids[0])["clients"] == 1
                    stages[f"wave{wave}_slow"] = dict(observe(probe, active, idle_seconds), catalog=rows)
                    assert probe.call("drain")["complete"]
                if overlap and wave == 2:
                    (active.home / "release-overlap/arm").touch()
                ack = hibernate(probe, stored)
                if not overlap:
                    time.sleep(12)
                stages[f"wave{wave}_hibernated"] = dict(observe(probe, active, idle_seconds), acknowledgement=ack)
            stages["wave2_hibernated"]["maps"] = memory_maps(active.pid)
            if overlap:
                port = active.home / "release-overlap/port"
                deadline = time.monotonic() + 30
                while not port.exists():
                    if time.monotonic() > deadline:
                        raise TimeoutError("native policy did not reach the armed release boundary")
                    time.sleep(.01)
                stages["overlap"] = probe.call("overlap", port=int(port.read_text()),
                    release=overlap[0], action=overlap[1], id=ids[0], bytes=size)
                return stages
            before = page_faults(active.pid)
            reopened = [probe.call("attach", id=id) for id in ids[:max(1, count)]]
            after = page_faults(active.pid)
            stages["reopen"] = dict(observe(probe, active, idle_seconds), attach=reopened,
                                    page_faults={key: after[key] - value for key, value in before.items()})
            probe.call("detach")
            hibernate(probe, stored)
            return stages


def timing_metrics(stage):
    values = {}
    if stage.get("attach"):
        values["reattach_ms"] = max(a["ms"] for a in stage["attach"])
    if stage.get("turns", {}).get("completed"):
        values["turn_batch_ms"] = max(stage["turns"]["turn_batch_ms"])
    return values


def summarize(records):
    groups = {}
    for r in records:
        for name, s in r["stages"].items():
            if name == "overlap":
                key = f'{r["label"]}/stored={r["stored"]}/active={r["active"]}/{name}'
                for metric, value in dict(request_ms=s["ms"], native_call_ms=s["native"]["native_ns"] / 1e6).items():
                    groups.setdefault(key, {}).setdefault(metric, []).append(value)
                continue
            values = dict(charged_kib=s["charged_bytes"] / 1024, rss_kib=s["before"]["rss_kib"],
                          sampled_peak_charged_kib=s["sampled_peak_charged_bytes"] / 1024,
                          cpu_ms=s["cpu_ms"], threads=s["after"]["thread_count"], switches=s["switches"])
            for key in ("peak_rss_kib", "peak_footprint_kib"):
                if key in s["after"]:
                    values[key] = s["after"][key]
            if "census" in s:
                values.update({metric.removesuffix("_bytes") + "_kib": s["census"][metric] / 1024 for metric in
                               ("allocated_bytes", "retained_free_bytes", "allocator_bytes")})
            values.update(timing_metrics(s))
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
            if stage == "overlap":
                group.setdefault("request_ms", []).append(value["ms"] - other["stages"][stage]["ms"])
                continue
            if "census" in value:
                for metric in ("allocated_bytes", "retained_free_bytes", "allocator_bytes"):
                    group.setdefault(metric.removesuffix("_bytes") + "_kib", []).append(
                        (value["census"][metric] - other["stages"][stage]["census"][metric]) / 1024)
            for metric, difference in dict(
                charged_kib=(value["charged_bytes"] - other["stages"][stage]["charged_bytes"]) / 1024,
                cpu_ms=value["cpu_ms"] - other["stages"][stage]["cpu_ms"],
            ).items():
                group.setdefault(metric, []).append(difference)
            reference = timing_metrics(other["stages"][stage])
            for metric, latency in timing_metrics(value).items():
                if metric in reference:
                    group.setdefault(metric, []).append(latency - reference[metric])
    return {key: {metric: distribution(values) for metric, values in group.items()} for key, group in groups.items()}


def table(summary, comparison=False):
    lines = ["Values are median [min, max]; " + ("A is reference; A2 is candidate." if comparison else "A and A2 are unchanged-source controls."), "",
             "| case/stage | charge KiB | RSS KiB | CPU ms | threads | reattach ms (slowest session/run) | slowest turn batch ms |",
             "| --- | ---: | ---: | ---: | ---: | ---: | ---: |"]
    for key, row in summary.items():
        if "minus" in key or key.endswith("/overlap"):
            continue
        cells = []
        for metric in ("charged_kib", "rss_kib", "cpu_ms", "threads", "reattach_ms", "turn_batch_ms"):
            value = row.get(metric)
            cells.append("" if value is None else "{median:.3f} [{min:.3f}, {max:.3f}]".format(**value))
        lines.append("| " + key + " | " + " | ".join(cells) + " |")
    overlap = {key: row for key, row in summary.items() if key.endswith("/overlap")}
    if overlap:
        lines.extend(["", "Call-entry overlap timings (A=no native release, A2=native release; one identical executable).",
                      "Raw timestamps must also confirm request/call overlap.", "",
                      "| case | request ms | native call ms |", "| --- | ---: | ---: |"])
        for key, row in overlap.items():
            cells = ["{median:.3f} [{min:.3f}, {max:.3f}]".format(**row[metric])
                     for metric in ("request_ms", "native_call_ms")]
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


DEPTH_TRACE = r'''
    for live in daemon.loaded() {
        live.barrier();
    }
    let started = Instant::now();
    let observations = std::sync::Mutex::new(Vec::new());
    let ready = || {
        let idle = daemon.idle();
        let mut graph: Vec<_> = daemon.loaded().iter()
            .map(|live| (live.id, live.has_work(), live.has_input(), live.has_jobs()))
            .collect();
        graph.sort_by_key(|entry| entry.0);
        let mut rows = crate::lock(&observations);
        if rows.last().is_none_or(|(_, previous, previous_idle)| previous != &graph || *previous_idle != idle) {
            rows.push((started.elapsed(), graph, idle));
        }
        idle
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        daemon.wait("settled", PATIENCE, &ready);
    }));
    eprintln!("depth-trace first-window elapsed={:?} result={}", started.elapsed(), result.is_ok());
    eprintln!("depth-trace observations={:#?}", crate::lock(&observations));
    if let Err(panic) = result {
        for header in daemon.shared.store.headers().unwrap() {
            eprintln!("depth-trace timeout-log {} {:#?}", header.id, bodies(&*daemon.shared.store, header.id));
        }
        rt::block_on(rt::or(daemon.pulse.until(&ready),
            rt::or(daemon.jobs.until(&ready), super::EVENTS.1.until(&ready))));
        eprintln!("depth-trace eventual-idle elapsed={:?}", started.elapsed());
        eprintln!("depth-trace observations={:#?}", crate::lock(&observations));
        for header in daemon.shared.store.headers().unwrap() {
            eprintln!("depth-trace final-log {} {:#?}", header.id, bodies(&*daemon.shared.store, header.id));
        }
        std::panic::resume_unwind(panic);
    }
'''


def engine_suites(source, reference, target, output, rounds, prepare, trial, scope, trace):
    env = environment(target)
    env["NEXTEST_PROFILE"] = "windows"
    env["A_TEST_POWERSHELL"] = ";".join([shutil.which("pwsh.exe"),
        str(Path(env["SYSTEMROOT"]) / "System32/WindowsPowerShell/v1.0/powershell.exe")])
    sources = {"main": reference, "branch": source} if reference else {"branch": source}
    saved = output / "engine-sources.json"
    identities = json.loads(saved.read_text()) if saved.exists() else {}
    for label, checkout in sources.items():
        if label in identities:
            assert identities[label]["revision"] == subprocess.check_output(
                ["git", "rev-parse", "HEAD"], cwd=checkout, text=True).strip()
            continue
        if trace:
            path = checkout / "crates/engine/src/tests.rs"
            text = path.read_text()
            begin = text.index("fn inherited_task_cannot_reset_an_exhausted_depth_budget()")
            end = text.index("\n#[test]", begin)
            body = text[begin:end]
            assert body.count("    settle(&daemon);\n") == 1
            path.write_text(text[:begin] + body.replace("    settle(&daemon);\n", DEPTH_TRACE) + text[end:])
            (output / f"depth-trace-{label}.patch").write_bytes(subprocess.check_output(
                ["git", "diff", "--", "crates/engine/src/tests.rs"], cwd=checkout))
        archive = output / f"engine-{label}.tar.zst"
        packages = ["-p", "engine"] if scope == "engine" else ["--workspace", "--features", "a/lua"]
        cargo_run(["cargo", "nextest", "archive", *packages, "--archive-file", str(archive)],
                  nextest_version="0.9.148", cwd=checkout, env=env, check=True)
        identities[label] = dict(source=str(checkout),
            revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=checkout, text=True).strip(),
            working_tree=subprocess.check_output(["git", "status", "--short"], cwd=checkout, text=True),
            archive=str(archive), sha256=hashlib.sha256(archive.read_bytes()).hexdigest())
        if reference and label == "main":
            cargo_run(["cargo", "clean"], cwd=checkout, env=env, check=True)
    (output / "engine-sources.json").write_text(json.dumps(identities, indent=2))
    if prepare:
        return
    (target / "engine-run").mkdir(parents=True, exist_ok=True)
    if trial is not None:
        assert not reference
        records = output / "engine-trials.jsonl"
        previous = [json.loads(line) for line in records.read_text().splitlines()] if records.exists() else []
        assert len(previous) == trial and all(row["returncode"] == 0 for row in previous)
    for round in ([trial] if trial is not None else range(rounds)):
        labels = list(sources) if round % 2 == 0 else list(reversed(sources))
        for label in labels:
            checkout = sources[label]
            command = ["cargo", "nextest", "run", "--archive-file", identities[label]["archive"],
                       "--workspace-remap", str(checkout), "--extract-to", str(target / "engine-run"),
                       "--extract-overwrite", "--no-fail-fast", "--retries", "0", "--no-tests=fail",
                       *(["--success-output", "final"] if trace else [])]
            result = cargo_run(command, nextest_version="0.9.148", cwd=checkout, env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            log = output / f"engine-{label}-{round:02}.txt"
            log.write_bytes(result.stdout)
            row = dict(round=round, label=label, returncode=result.returncode, log=log.name,
                       archive_sha256=identities[label]["sha256"])
            with (output / "engine-trials.jsonl").open("a") as stream:
                stream.write(json.dumps(row) + "\n")
            print(json.dumps(row), flush=True)
            print(result.stdout.decode("utf-8", errors="replace")[-5000:], flush=True)
            if result.returncode not in (0, 100) or b"tests run:" not in result.stdout:
                raise SystemExit(result.returncode or 1)
            if not reference and result.returncode:
                raise SystemExit(result.returncode)


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    # The workload only talks to fixture daemons and loopback diagnostics.
    keep = {
        "HOME", "USER", "LOGNAME", "PATH", "SHELL", "TMPDIR", "TMP", "TEMP",
        "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "USERPROFILE", "APPDATA",
        "LOCALAPPDATA", "PROGRAMFILES", "PROGRAMFILES(X86)", "INCLUDE", "LIB", "LIBPATH",
        "SYSTEMDRIVE", "PROGRAMDATA", "ALLUSERSPROFILE", "PROGRAMW6432",
        "COMMONPROGRAMFILES", "COMMONPROGRAMFILES(X86)", "COMMONPROGRAMW6432",
        "VSINSTALLDIR", "VCINSTALLDIR", "VCTOOLSINSTALLDIR", "VCTOOLSVERSION",
        "WINDOWSSDKDIR", "WINDOWSSDKVERSION", "UNIVERSALCRTSDKDIR", "UCRTVERSION",
        "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "CARGO_HOME", "CARGO_TARGET_DIR",
        "CARGO_PROFILE_DEV_CODEGEN_BACKEND", "RUSTFLAGS", "RUSTC_WRAPPER",
        "MISE_DATA_DIR", "MISE_CACHE_DIR", "MISE_CONFIG_DIR", "MISE_AUTO_INSTALL",
        "MISE_IGNORED_CONFIG_PATHS", "MISE_CARGO_BINSTALL_NATIVE",
        "RUNNER_TEMP", "RUNNER_OS", "RUNNER_ARCH", "GITHUB_STEP_SUMMARY",
    }
    clean = {key: value for key, value in os.environ.items() if key.upper() in keep}
    clean.update(MISE_NO_ENV="1", CARGO_BUILD_JOBS="2", PYTHONDONTWRITEBYTECODE="1",
                 OPENAI_API_KEY="loopback-dummy", OPENAI_BASE_URL="http://127.0.0.1:1/v1",
                 NO_PROXY="127.0.0.1,localhost")
    os.environ.clear()
    os.environ.update(clean)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=ROOT)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--target", type=Path, required=True)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--reference-binary", type=Path, help="A executable; --binary is A2 (otherwise A/A controls)")
    parser.add_argument("--reference-source", type=Path, help="build A from this checkout before building A2")
    parser.add_argument("--probe", type=Path)
    parser.add_argument("--build-only", action="store_true")
    parser.add_argument("--cases", help="explicit stored:active pairs for a focused diagnostic cohort")
    parser.add_argument("--vmmap", type=Path, help="native VMMap executable; census diagnostics only")
    parser.add_argument("--census", action="store_true", help="instrumented native allocator cohort; excluded from timing claims")
    parser.add_argument("--overlap", choices=["reopen", "turn"], help="fixture call-entry overlap, paired native release versus no-op")
    parser.add_argument("--engine-suites", action="store_true")
    parser.add_argument("--prepare-engine", action="store_true")
    parser.add_argument("--suite-trial", type=int)
    parser.add_argument("--suite-scope", choices=["engine", "workspace"], default="engine")
    parser.add_argument("--depth-trace", action="store_true")
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--stored", default="1,8,32")
    parser.add_argument("--active", default="1,4,8")
    parser.add_argument("--turns", type=int, default=4)
    parser.add_argument("--bytes", type=int, default=65536)
    parser.add_argument("--idle-seconds", type=float, default=1)
    args = parser.parse_args()
    if args.census and sys.platform not in ("linux", "win32", "darwin"):
        parser.error("native census cohort requires glibc Linux, macOS or Windows")
    if args.vmmap and (not args.census or sys.platform != "win32"):
        parser.error("VMMap requires a native Windows census cohort")
    if args.rounds < 5:
        parser.error("at least five interleaved rounds are required")
    if args.reference_binary and args.reference_source:
        parser.error("select a reference binary or source, not both")
    stored, counts = [int(n) for n in args.stored.split(",")], [int(n) for n in args.active.split(",")]
    if min(stored + counts) < 1 or max(counts) > max(stored) or args.turns < 1 or args.bytes < 1 or args.idle_seconds <= 0:
        parser.error("positive populations/work required, with active <= maximum stored")
    cases = list(dict.fromkeys([(1, 0), *((n, 1) for n in stored), *((max(stored), n) for n in counts)]))
    if args.cases:
        try:
            cases = [tuple(map(int, pair.split(":"))) for pair in args.cases.split(",")]
            if any(len(pair) != 2 or pair[0] < 1 or pair[1] < 0 or pair[1] > pair[0] for pair in cases):
                raise ValueError("invalid populations")
        except ValueError:
            parser.error("cases must be stored:active pairs with 0 <= active <= stored")
    output, target, source = args.output.resolve(), args.target.resolve(), args.source.resolve()
    if (output / "residency.jsonl").exists() or (output / "residency-failures.jsonl").exists():
        parser.error("select a fresh output directory; existing samples must not be mixed")
    output.mkdir(parents=True, exist_ok=True)
    os.environ["A_HOME"] = str(output / "test-home")
    if args.engine_suites:
        engine_suites(source, args.reference_source.resolve() if args.reference_source else None,
                      target, output, args.rounds, args.prepare_engine, args.suite_trial, args.suite_scope, args.depth_trace)
        return
    if args.overlap:
        if args.census or args.reference_source or args.reference_binary or args.binary:
            parser.error("overlap uses one identically instrumented executable for release/no-op pairs")
        instrument_overlap(source, output)
    if args.census:
        test_env = environment(target)
        if sys.platform == "linux" and platform.machine() == "aarch64":
            # The runner's mold aborts on range-extension thunks in the large
            # engine test image. Release images keep the normal linker recipe.
            # LLVM's RELR output lacks the glibc ABI marker on this runner.
            flags = tomllib.loads((source / ".cargo/config.toml").read_text())["target"]['cfg(target_os = "linux")']["rustflags"]
            test_env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(
                flag.replace("-fuse-ld=mold", "-fuse-ld=lld") for flag in flags
                if flag != "-Clink-arg=-Wl,--pack-dyn-relocs=relr")
        cargo_run(["cargo", "nextest", "run", "-p", "sys", "-p", "engine", "--features", "sys/fixtures"],
                  cwd=source, env=test_env, check=True)
        cargo_run(["cargo", "clippy", "-p", "sys", "-p", "engine", "--all-targets",
                   "--features", "sys/fixtures", "--", "-D", "warnings"],
                  cwd=source, env=test_env, check=True)
    reference = args.reference_binary.resolve() if args.reference_binary else None
    if args.reference_source:
        reference = stage(executable(args.reference_source.resolve(), environment(target), ["--features", "fixtures"], run=cargo_run), output / ("a-reference" + SUFFIX))
        # Cargo's local-package freshness can reuse the other checkout's image
        # in a shared target directory. Keep the staged control, not that cache.
        cargo_run(["cargo", "clean", "--release"], cwd=source, env=environment(target), check=True)
    binary = args.binary.resolve() if args.binary else stage(executable(source, environment(target), ["--features", "fixtures"], run=cargo_run), output / ("a-fixtures" + SUFFIX))
    reference = reference or binary
    if args.reference_source and reference.read_bytes() == binary.read_bytes():
        raise ValueError("paired sources produced identical images; use --reference-binary for intentional A/A controls")
    probe = args.probe.resolve() if args.probe else build_probe(source, output, target)
    from hosted import hardware
    metadata = dict(hardware=hardware(), platform=platform.platform(), machine=platform.machine(),
                    revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip(),
                    working_tree=subprocess.check_output(["git", "status", "--short"], cwd=source, text=True),
                    workload_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                    reference_revision=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=args.reference_source, text=True).strip() if args.reference_source else None,
                    reference_working_tree=subprocess.check_output(["git", "status", "--short"], cwd=args.reference_source, text=True) if args.reference_source else None,
                    census_module_sha256=hashlib.sha256((source / "crates/sys/src/census.rs").read_bytes()).hexdigest() if args.census else None,
                    binaries={str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in (binary, reference, probe)},
                    labels=dict(A=str(reference), A2=str(binary)),
                    parameters=dict(census=args.census, overlap=args.overlap, cases=cases, vmmap=str(args.vmmap) if args.vmmap else None, stored=stored, active=counts, turns=args.turns, bytes=args.bytes, idle_seconds=args.idle_seconds))
    (output / "residency-host.json").write_text(json.dumps(metadata, indent=2) + "\n")
    if args.build_only:
        return
    records = []
    for round in range(args.rounds):
        for total, active in (cases if round % 2 == 0 else list(reversed(cases))):
            for label in (("A", "A2") if round % 2 == 0 else ("A2", "A")):
                print(f"round={round} {label} stored={total} active={active}", flush=True)
                record = dict(round=round, label=label, stored=total, active=active)
                try:
                    record["stages"] = sample(reference if label == "A" else binary, probe, output, total, active, args.turns, args.bytes, args.idle_seconds, args.census, args.vmmap,
                                              (label == "A2", args.overlap) if args.overlap else None)
                except Exception as error:
                    record["failure"] = repr(error)
                    with (output / "residency-failures.jsonl").open("a") as stream:
                        stream.write(json.dumps(record) + "\n")
                    raise
                records.append(record)
                with (output / "residency.jsonl").open("a") as stream:
                    stream.write(json.dumps(record) + "\n")
                (output / "residency.json").write_text(json.dumps(dict(**metadata, summary=summarize(records), paired_A2_minus_A=paired(records)), indent=2) + "\n")

    report = table(summarize(records), reference != binary)
    (output / "residency.md").write_text(report)
    if summary_file := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary_file, "a") as stream:
            stream.write(report)


if __name__ == "__main__":
    main()
