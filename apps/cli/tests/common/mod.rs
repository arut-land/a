//! Drives the built `a` as a black box: each test gets its own `A_HOME`, its
//! own link to the binary (so its daemons can be told apart from everyone
//! else's), and a raw protocol connection to inspect what the daemon sends.

#![allow(dead_code)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::time::{Duration, Instant};

use protocol::{Body, ClientMsg, DaemonMsg, Entry, Outcome, PROTO, SessionId, Target};
use sys::{Home, Signal};

/// `A_STRICT_BUDGETS=1` holds latency to the design's budget; otherwise it
/// gets the 10x slack a shared CI runner or a dev build needs.
pub fn latency_budget(strict: Duration) -> Duration {
    if std::env::var_os("A_STRICT_BUDGETS").is_some_and(|v| v == "1") {
        strict
    } else {
        strict * 10
    }
}

/// Whether the tests drive the release build, given as `A_BIN` (absolute).
pub fn release() -> bool {
    std::env::var_os("A_BIN").is_some()
}

fn built() -> PathBuf {
    std::env::var_os("A_BIN").map_or_else(|| env!("CARGO_BIN_EXE_a").into(), PathBuf::from)
}

pub struct Env {
    pub root: PathBuf,
    pub exe: PathBuf,
    pub ws: PathBuf,
    pub home: Home,
}

impl Env {
    pub fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("a-acc-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (bin, ws) = (root.join("bin"), root.join("ws"));
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&ws).unwrap();
        let exe = bin.join("a");
        if fs::hard_link(built(), &exe).is_err() {
            fs::copy(built(), &exe).unwrap();
        }
        let env = Self {
            home: Home::from(root.join("home")),
            ws: fs::canonicalize(ws).unwrap(),
            root,
            exe,
        };
        // A binary's first exec can be slow (macOS scans new files).
        env.run(&["--version"]);
        env
    }

    pub fn cmd(&self, args: &[&str]) -> Command {
        self.cmd_under(&[], args)
    }

    /// `a` with `args`, run by the `wrapper` command line (such as strace).
    pub fn cmd_under(&self, wrapper: &[&str], args: &[&str]) -> Command {
        let mut cmd = match wrapper {
            [] => Command::new(&self.exe),
            [program, rest @ ..] => {
                let mut cmd = Command::new(program);
                cmd.args(rest).arg(&self.exe);
                cmd
            }
        };
        cmd.args(args)
            .current_dir(&self.ws)
            .env("A_HOME", self.root.join("home"))
            .env("A_AGENT", "echo")
            .env("A_IDLE_SECS", "15");
        cmd
    }

    pub fn run(&self, args: &[&str]) -> String {
        let Output {
            status,
            stdout,
            stderr,
        } = self.cmd(args).stdin(Stdio::null()).output().unwrap();
        let stderr = String::from_utf8_lossy(&stderr);
        assert!(status.success(), "a {args:?} failed: {stderr}");
        String::from_utf8(stdout).unwrap()
    }

    /// Starts the daemon (any client command spawns it) and returns its pid.
    pub fn start(&self) -> u32 {
        self.run(&["ls"]);
        self.pid().expect("no daemon after `a ls`")
    }

    /// The pid of the daemon holding this home's lock.
    pub fn pid(&self) -> Option<u32> {
        client::diagnose(&self.home).pid
    }

    /// Every daemon process of this test: daemons run the build staged
    /// under this test's home.
    pub fn daemons(&self) -> Vec<u32> {
        let runtime = self.root.join("home").join("runtime");
        let runtime = runtime.to_str().unwrap();
        processes("pid=")
            .into_iter()
            .filter(|(_, args)| args.starts_with(runtime) && args.contains(" __daemon "))
            .map(|(pid, _)| pid)
            .collect()
    }

    /// `kill -9` the daemon and waits for its lock to be released.
    pub fn kill_daemon(&self) {
        let pid = self.pid().expect("no daemon to kill");
        sys::signal_process(pid, Signal::Kill).unwrap();
        wait_for("the killed daemon to go", || self.pid().is_none());
    }

    pub fn connect(&self) -> Conn {
        let stream = UnixStream::connect(self.home.socket()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut conn = Conn {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
        };
        conn.send(&ClientMsg::Hello {
            proto: PROTO,
            // Any build the daemon's staged `current` does not outrank: it
            // answers with a welcome, never a drain.
            build: "acceptance".to_owned(),
        });
        assert!(matches!(conn.recv(), DaemonMsg::Welcome { .. }));
        conn
    }

    /// An interactive `a` with piped stdio, as a terminal would run it.
    pub fn repl(&self, args: &[&str]) -> Repl {
        let mut child = self
            .cmd(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        Repl {
            stdin: child.stdin.take(),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            child,
        }
    }

    /// A session's log as stored, asserting that every line is a complete
    /// entry.
    pub fn log(&self, id: SessionId) -> Vec<Entry> {
        let path = self.home.sessions_dir().join(format!("{id}.jsonl"));
        let text = fs::read_to_string(path).unwrap();
        assert!(text.ends_with('\n'), "torn final line in {text}");
        text.lines()
            .skip(1)
            .map(|line| {
                serde_json::from_str(line).unwrap_or_else(|e| panic!("torn entry {line}: {e}"))
            })
            .collect()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.cmd(&["daemon", "stop"]).output();
        for pid in self.daemons() {
            let _ = sys::signal_process(pid, Signal::Kill);
        }
        let _ = fs::remove_file(self.home.socket());
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub struct Conn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Conn {
    pub fn send(&mut self, msg: &ClientMsg) {
        protocol::write_frame(&mut self.writer, msg).unwrap();
    }

    pub fn recv(&mut self) -> DaemonMsg {
        protocol::read_frame(&mut self.reader)
            .unwrap()
            .expect("daemon closed the connection")
    }

    pub fn input(&mut self, text: &str) {
        self.send(&ClientMsg::Input { text: text.into() });
    }

    /// Attaches and returns the session and its replay, up to `Synced`.
    pub fn attach(&mut self, target: Target, ws: &Path) -> (SessionId, Vec<Entry>) {
        self.send(&ClientMsg::Attach {
            target,
            workspace: ws.into(),
            since: 0,
        });
        let DaemonMsg::Attached { session, .. } = self.recv() else {
            panic!("expected attached");
        };
        let mut replay = Vec::new();
        loop {
            match self.recv() {
                DaemonMsg::Entry(e) => replay.push(e),
                DaemonMsg::Synced { seq } => {
                    assert_eq!(seq, replay.last().map_or(0, |e| e.seq));
                    return (session, replay);
                }
                msg => panic!("unexpected during replay: {msg:?}"),
            }
        }
    }

    /// Live entries up to and including the first that `last` accepts.
    pub fn until(&mut self, last: impl Fn(&Entry) -> bool) -> Vec<Entry> {
        let mut got = Vec::new();
        loop {
            if let DaemonMsg::Entry(e) = self.recv() {
                let done = last(&e);
                got.push(e);
                if done {
                    return got;
                }
            }
        }
    }
}

pub struct Repl {
    pub stdin: Option<ChildStdin>,
    pub stdout: BufReader<ChildStdout>,
    pub child: Child,
}

impl Repl {
    pub fn line(&mut self) -> String {
        let mut line = String::new();
        assert_ne!(self.stdout.read_line(&mut line).unwrap(), 0, "a exited");
        line.trim_end().to_owned()
    }

    /// Lines up to and including the first that starts with `prefix`.
    pub fn lines_until(&mut self, prefix: &str) -> Vec<String> {
        let mut lines = Vec::new();
        loop {
            let line = self.line();
            let done = line.trim_start().starts_with(prefix);
            lines.push(line);
            if done {
                return lines;
            }
        }
    }

    pub fn type_line(&mut self, text: &str) {
        writeln!(self.stdin.as_mut().unwrap(), "{text}").unwrap();
    }

    /// Closes stdin, which detaches, and returns what `a` printed until it
    /// exited.
    pub fn finish(mut self) -> String {
        drop(self.stdin.take());
        let mut rest = String::new();
        self.stdout.read_to_string(&mut rest).unwrap();
        assert!(self.child.wait().unwrap().success());
        rest
    }
}

impl Drop for Repl {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Asserts the entries are numbered 1, 2, 3, … with nothing missing or
/// repeated.
pub fn assert_contiguous(entries: &[Entry]) {
    let seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
    let want: Vec<u64> = (1..=entries.len() as u64).collect();
    assert_eq!(seqs, want, "gaps or duplicates");
}

pub fn result_of(e: &Entry, call: u64) -> Option<&Outcome> {
    match &e.body {
        Body::ToolResult { call: c, outcome } if *c == call => Some(outcome),
        _ => None,
    }
}

pub fn is_tool_call(e: &Entry) -> bool {
    matches!(e.body, Body::ToolCall { .. })
}

/// Whether any live process is in group `pgid`.
pub fn group_alive(pgid: u32) -> bool {
    processes("pgid=").iter().any(|&(group, _)| group == pgid)
}

/// Every live (not zombie) process as `(column, args)`, where `column` is
/// the numeric `ps` column asked for.
fn processes(column: &str) -> Vec<(u32, String)> {
    let out = Command::new("ps")
        .args(["-A", "-ww", "-o", column, "-o", "stat=", "-o", "args="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let id = fields.next()?.parse().ok()?;
            let live = !fields.next()?.starts_with('Z');
            live.then(|| (id, fields.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}
