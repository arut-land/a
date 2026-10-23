//! Black-box cases use isolated homes and raw protocol connections.
//! Every case executes the same built binary: executable aliases can be
//! killed by the host before main. Staged home paths distinguish daemons.
//! Every process a test starts is in the test's process group, which ends
//! with the test however the test ends (see [`Group`]).

pub mod pty;

use std::fs;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::time::{Duration, Instant};

use protocol::view::SessionView;
use protocol::{Body, ClientMsg, DaemonMsg, Entry, Outcome, PROTO, Seq, SessionId, Target};
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

/// Runs `exe` once. A binary's first exec can be slow (macOS scans new
/// files), and an exec that waits on another's scan of the same file may be
/// refused, killed: the tests' first runs of a build go one at a time,
/// across test processes.
pub fn warm_up(exe: &Path) {
    let lock = fs::File::create(std::env::temp_dir().join("a-acc-first-exec.lock")).unwrap();
    lock.lock().unwrap();
    let out = Command::new(exe).arg("--version").output().unwrap();
    assert!(
        out.status.success(),
        "{} --version: {}",
        exe.display(),
        out.status
    );
}

/// A test's process group, and the watchdog that ends it. The group's
/// leader and the watchdog (in a group of its own, out of reach of
/// whatever signals the test's) each block reading a pipe only the test
/// holds. However the test ends (it returns, panics, or is killed and runs
/// no destructor), the pipe closes; the watchdog then kills the group,
/// which is every process the test started and whatever those started in
/// turn, and stops the daemon of every home the test listed (daemons leave
/// the group, as they leave their client's session).
struct Group {
    leader: Child,
    watchdog: Child,
    lifeline: Option<io::PipeWriter>,
    pgid: u32,
    /// Where the test lists its homes, one per line, for the watchdog.
    homes: PathBuf,
}

/// The watchdog: `$1` is the group, `$2` the `a` to stop daemons with,
/// `$3` the homes.
const WATCHDOG: &str = r#"read -r _
kill -KILL "-$1"
while read -r home; do A_HOME="$home" "$2" daemon stop >/dev/null 2>&1; done < "$3"
"#;

impl Group {
    fn new(exe: &Path, homes: PathBuf) -> Self {
        let (read, lifeline) = io::pipe().unwrap();
        let sh = |script: &str| {
            let mut sh = Command::new("/bin/sh");
            sh.args(["-c", script, "sh"])
                .env_remove("A_HOST")
                .process_group(0)
                .stdin(read.try_clone().unwrap())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            sh
        };
        let leader = sh("read -r _").spawn().unwrap();
        let pgid = leader.id();
        let watchdog = sh(WATCHDOG)
            .arg(pgid.to_string())
            .arg(exe)
            .arg(&homes)
            .spawn()
            .unwrap();
        Self {
            leader,
            watchdog,
            lifeline: Some(lifeline),
            pgid,
            homes,
        }
    }

    /// Lists `home` for the watchdog to stop the daemon of.
    fn add(&self, home: &Home) {
        let mut file = fs::File::options()
            .create(true)
            .append(true)
            .open(&self.homes)
            .unwrap();
        writeln!(file, "{}", home.root().display()).unwrap();
    }

    /// Ends the group as the test's end would, and waits until nothing in
    /// it is left. Never panics: it runs as a failed test unwinds.
    fn end(&mut self) {
        drop(self.lifeline.take());
        let _ = self.leader.wait();
        let _ = self.watchdog.wait();
        // Killed, so gone at once; a zombie a test never reaped is not live.
        let deadline = Instant::now() + Duration::from_secs(10);
        while group_alive(self.pgid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

pub struct Env {
    pub root: PathBuf,
    pub exe: PathBuf,
    pub ws: PathBuf,
    pub home: Home,
    group: Group,
    /// More environment for every `a` this test runs (never its daemons,
    /// which take theirs from [`Env::daemon`]).
    pub vars: Vec<(&'static str, &'static str)>,
}

impl Env {
    pub fn new(name: &str) -> Self {
        let root = crate::support::isolated_root(name);
        let (bin, ws) = (root.join("bin"), root.join("ws"));
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&ws).unwrap();
        let exe = built();
        let group = Group::new(&exe, root.join("homes"));
        let env = Self {
            home: Home::from(root.join("home")),
            ws,
            root,
            exe,
            group,
            vars: Vec::new(),
        };
        env.group.add(&env.home);
        warm_up(&env.exe);
        // The socket lives under /tmp by a hash of the home, which carries
        // this process's pid: a failed test of an earlier run, kept to look
        // at, may have left one that nothing listens on.
        env.home.endpoint().remove();
        env.daemon("agent = \"echo\"\nidle_secs = 15");
        env
    }

    /// Another home under this test's root, at `path`, whose daemon runs
    /// the echo agent and is stopped with the test's own.
    pub fn other_home(&mut self, path: PathBuf) -> Home {
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("config.toml"), "[daemon]\nagent = \"echo\"\n").unwrap();
        let home = Home::from(path);
        self.group.add(&home);
        home
    }

    /// An executable shell script `name` under this test's root.
    pub fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Adds `lines` to the `[daemon]` table of this home's `config.toml`,
    /// for the daemons started after.
    pub fn daemon(&self, lines: &str) {
        let path = self.home.config_file();
        let mut text = fs::read_to_string(&path).unwrap_or_else(|_| "[daemon]\n".into());
        text.push_str(lines);
        text.push('\n');
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Sets `lines` in extension `ext`'s table, `[ext.<ext>]`: after any
    /// `[daemon]` lines, as a table ends where the next begins.
    pub fn ext(&self, ext: &str, lines: &str) {
        let path = self.home.config_file();
        let text = fs::read_to_string(&path).unwrap_or_default();
        let text = text + &format!("\n[ext.{ext}]\n{lines}\n");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
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
        cmd.args(args);
        self.configure(cmd)
    }

    /// Another build of `a`, `exe`, with `args` and this test's home.
    pub fn cmd_as(&self, exe: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(exe);
        cmd.args(args);
        self.configure(cmd)
    }

    fn configure(&self, mut cmd: Command) -> Command {
        cmd.process_group(self.group.pgid.cast_signed())
            .current_dir(&self.ws)
            .env("A_HOME", self.home.root())
            // Never the developer's host.
            .env_remove("A_HOST")
            .envs(self.vars.iter().copied());
        cmd
    }

    /// A build of `a` other than the one under test, and older: the same
    /// program with a byte more, so its build id differs, built an hour
    /// earlier. As a CLI from another install would be next to the app's.
    pub fn older_build(&self) -> PathBuf {
        let exe = self.root.join("bin").join("a-old");
        let mut bytes = fs::read(&self.exe).unwrap();
        bytes.push(0);
        fs::write(&exe, bytes).unwrap();
        let file = fs::File::options().write(true).open(&exe).unwrap();
        let built = fs::metadata(&self.exe).unwrap().modified().unwrap();
        file.set_modified(built - Duration::from_secs(3600))
            .unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o755))
            .unwrap();
        drop(file);
        let out = Command::new(&exe).arg("--version").output().unwrap();
        assert!(out.status.success(), "the older build does not run");
        exe
    }

    pub fn run(&self, args: &[&str]) -> String {
        let Output {
            status,
            stdout,
            stderr,
        } = crate::support::TestProcess::spawn(self.cmd(args).stdin(Stdio::null()))
            .collect(crate::support::deadline())
            .unwrap();
        let stderr = String::from_utf8_lossy(&stderr);
        assert!(status.success(), "a {args:?} failed ({status}): {stderr}");
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

    /// The process group of everything this test starts.
    pub fn group(&self) -> u32 {
        self.group.pgid
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
        crate::support::signal_process(pid, Signal::Kill).unwrap();
        self.wait_exited("the killed daemon to go");
    }

    /// Waits on the daemon's lock, as a client does, until it is let go.
    pub fn wait_exited(&self, what: &str) {
        assert!(
            client::wait_exited(&self.home, Some(Duration::from_secs(10))),
            "timed out waiting for {what}"
        );
    }

    /// A connection the daemon welcomed.
    pub fn connect(&self) -> Conn {
        let mut conn = Conn::open(&self.home);
        assert!(matches!(conn.hello(), DaemonMsg::Welcome { .. }));
        conn
    }

    /// The line REPL over piped stdio, as a plain terminal would run it.
    pub fn repl(&self, args: &[&str]) -> Repl {
        self.repl_from(args, |_| {})
    }

    /// [`Env::repl`] from a shell that `shell` sets up.
    pub fn repl_from(&self, args: &[&str], shell: impl FnOnce(&mut Command)) -> Repl {
        let mut cmd = self.cmd(args);
        shell(&mut cmd);
        Repl::spawn(cmd)
    }

    /// A session's log as stored, asserting that every line is a complete
    /// entry.
    pub fn log(&self, id: SessionId) -> Vec<Entry> {
        let mut roots = vec![self.home.sessions_dir()];
        let path = loop {
            let root = roots.pop().expect("session log exists");
            let path = root.join(id.to_string()).join("log.jsonl");
            if path.is_file() {
                break path;
            }
            for entry in fs::read_dir(root).unwrap() {
                let children = entry.unwrap().path().join("children");
                if children.is_dir() {
                    roots.push(children);
                }
            }
        };
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

/// What the test started is gone before its home is: the watchdog stops
/// the daemons and kills the group, as it would had the test been killed.
impl Drop for Env {
    fn drop(&mut self) {
        self.group.end();
        // One that would not stop (wedged, or its home already gone).
        for pid in self.daemons() {
            let _ = crate::support::signal_process(pid, Signal::Kill);
        }
        let _ = fs::remove_file(self.home.endpoint().path());
        // A failed test's home, with the daemon's log, is kept to look at.
        if std::thread::panicking() {
            let _ = writeln!(io::stderr(), "kept {}", self.root.display());
        } else {
            crate::support::remove_home(&self.root);
        }
    }
}

/// How long a connection waits for the daemon's next frame.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Conn {
    reader: BufReader<sys::OwnedConnection>,
    writer: sys::OwnedConnection,
    /// Everything received, folded as a client's view.
    pub view: SessionView,
    /// The seq of the last entry received, and of its session.
    last: Option<(SessionId, u64)>,
}

pub fn hello() -> ClientMsg {
    ClientMsg::Hello {
        proto: PROTO,
        build: "acceptance".to_owned(),
        min: None,
        caps: protocol::Caps::NONE,
        client: None,
    }
}

impl Conn {
    /// A connection to `home`'s socket, before any hello.
    pub fn open(home: &Home) -> Self {
        Self::from(home.endpoint().connect().unwrap())
    }

    /// [`Conn::open`] to a daemon that is starting by itself: connects
    /// until something listens, as a socket path alone may be a stale
    /// socket's, or bound but not yet listened on. The connection that
    /// gets through is the one kept, so nothing else knocks first.
    pub fn open_when_listening(home: &Home) -> Self {
        let endpoint = home.endpoint();
        let mut stream = None;
        wait_for("the daemon to listen", || {
            stream = endpoint.connect().ok();
            stream.is_some()
        });
        Self::from(stream.unwrap())
    }

    fn from(stream: sys::OwnedConnection) -> Self {
        stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        Self {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            view: SessionView::default(),
            last: None,
        }
    }

    /// Says [`hello`] and returns the daemon's answer.
    pub fn hello(&mut self) -> DaemonMsg {
        self.send(&hello());
        self.recv()
    }

    pub fn send(&mut self, msg: &ClientMsg) {
        protocol::write_frame(&mut self.writer, msg).unwrap();
    }

    pub fn recv(&mut self) -> DaemonMsg {
        self.try_recv().expect("daemon closed the connection")
    }

    /// The next message, or none once the daemon has gone. A daemon that
    /// is up but silent for [`READ_TIMEOUT`] is a failure of its own.
    pub fn try_recv(&mut self) -> Option<DaemonMsg> {
        let msg = match protocol::read_frame(&mut self.reader) {
            Ok(msg) => msg?,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                panic!(
                    "the daemon sent nothing for {READ_TIMEOUT:?} after entry {:?}",
                    self.last
                )
            }
            Err(_) => return None,
        };
        match &msg {
            DaemonMsg::Attached { session, .. } => self.last = Some((*session, 0)),
            DaemonMsg::Entry(e) => {
                if let Some((_, seq)) = &mut self.last {
                    *seq = e.seq.get();
                }
            }
            _ => {}
        }
        self.view.fold(&msg);
        Some(msg)
    }

    pub fn input(&mut self, text: &str) {
        self.send(&ClientMsg::input(text));
    }

    /// Attaches and returns the session and its replay, up to `Synced`.
    /// Attaches without an environment, so the session keeps the one it has
    /// (none for a new session: commands inherit the daemon's).
    pub fn attach(&mut self, target: Target, ws: &Path) -> (SessionId, Vec<Entry>) {
        self.send(&ClientMsg::Attach {
            target,
            workspace: ws.into(),
            since: 0,
            env: None,
            decl: protocol::ClientDecl::default(),
        });
        let DaemonMsg::Attached { session, .. } = self.recv() else {
            panic!("expected attached");
        };
        let mut replay = Vec::new();
        loop {
            match self.recv() {
                DaemonMsg::Entry(e) => replay.push(e),
                DaemonMsg::Activity { .. } => {}
                DaemonMsg::Synced { seq } => {
                    assert_eq!(seq, replay.last().map_or(0, |e| e.seq.get()));
                    return (session, replay);
                }
                msg => panic!("unexpected during replay: {msg:?}"),
            }
        }
    }

    /// Runs `command` through the echo agent and returns its output.
    pub fn run_command(&mut self, command: &str) -> serde_json::Value {
        self.input(&format!("$ {command}"));
        let entries = self.until(|e| matches!(e.body, Body::ToolResult { .. }));
        match &entries.last().unwrap().body {
            Body::ToolResult {
                outcome: Outcome::Done { output },
                ..
            } => output.parse::<serde_json::Value>().unwrap(),
            other => panic!("{command} did not run: {other:?}"),
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
    /// Runs `cmd` with piped stdio, as the line REPL: `A_TUI=plain` asks
    /// for it where a pipe on stdin would run one turn.
    pub fn spawn(mut cmd: Command) -> Self {
        let mut child = cmd
            .env("A_TUI", "plain")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            stdin: child.stdin.take(),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            child,
        }
    }

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

pub fn wait_for(what: &str, done: impl FnMut() -> bool) {
    wait_within(what, Duration::from_secs(10), done);
}

pub fn wait_within(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Asserts the entries are numbered 1, 2, 3, … with nothing missing or
/// repeated.
pub fn assert_contiguous(entries: &[Entry]) {
    let seqs: Vec<u64> = entries.iter().map(|e| e.seq.get()).collect();
    let want: Vec<u64> = (1..=entries.len() as u64).collect();
    assert_eq!(seqs, want, "gaps or duplicates");
}

pub fn result_of(e: &Entry, call: Seq) -> Option<&Outcome> {
    match &e.body {
        Body::ToolResult {
            call: c, outcome, ..
        } if c.0 == call => Some(outcome),
        _ => None,
    }
}

pub fn is_tool_call(e: &Entry) -> bool {
    matches!(e.body, Body::ToolCall { .. })
}

/// Every live process whose command line names a path under `root`.
pub fn processes_under(root: &Path) -> Vec<(u32, String)> {
    let root = format!("{}/", root.display());
    processes("pid=")
        .into_iter()
        .filter(|(_, args)| args.contains(&root))
        .collect()
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

/// The names of process `pid`'s threads now.
pub fn thread_names(pid: u32) -> Vec<String> {
    thread_states(pid)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Every thread has reached a kernel wait, after the protocol fact the
/// caller awaits. An empty snapshot cannot certify a live process.
pub fn threads_parked(pid: u32) -> bool {
    let states = thread_states(pid);
    !states.is_empty() && states.iter().all(|(_, parked)| *parked)
}

fn thread_states(pid: u32) -> Vec<(String, bool)> {
    #[cfg(target_os = "linux")]
    {
        fs::read_dir(format!("/proc/{pid}/task"))
            .unwrap()
            .flatten()
            .filter_map(|task| {
                let status = fs::read_to_string(task.path().join("status")).ok()?;
                let name = status
                    .lines()
                    .find_map(|line| line.strip_prefix("Name:\t"))?;
                let state = status
                    .lines()
                    .find_map(|line| line.strip_prefix("State:\t"))?;
                Some((
                    name.to_owned(),
                    state.starts_with('S') || state.starts_with('I'),
                ))
            })
            .collect()
    }
    #[cfg(target_vendor = "apple")]
    #[allow(unsafe_code)]
    {
        use std::ffi::CStr;
        /// `<sys/proc_info.h>`: the flavor listing a task's thread handles.
        const PROC_PIDLISTTHREADS: i32 = 6;
        let mut handles = vec![0_u64; 4096];
        let size = i32::try_from(handles.len() * 8).unwrap();
        // SAFETY: `handles` is a writable buffer of `size` bytes; the call
        // writes at most that, returning how many it wrote.
        let written = unsafe {
            libc::proc_pidinfo(
                pid.cast_signed(),
                PROC_PIDLISTTHREADS,
                0,
                handles.as_mut_ptr().cast(),
                size,
            )
        };
        handles.truncate(usize::try_from(written).unwrap() / 8);
        let size = i32::try_from(size_of::<libc::proc_threadinfo>()).unwrap();
        handles
            .into_iter()
            .filter_map(|handle| {
                let mut info = std::mem::MaybeUninit::<libc::proc_threadinfo>::zeroed();
                // SAFETY: as above, for one `proc_threadinfo`, all written
                // when the call returns `size`; a thread that ended since
                // the listing returns less.
                let info = unsafe {
                    let written = libc::proc_pidinfo(
                        pid.cast_signed(),
                        libc::PROC_PIDTHREADINFO,
                        handle,
                        info.as_mut_ptr().cast(),
                        size,
                    );
                    (written == size).then(|| info.assume_init())?
                };
                // SAFETY: the kernel NUL-terminates the name.
                let name = unsafe { CStr::from_ptr(info.pth_name.as_ptr()) };
                // TH_STATE_WAITING from <mach/thread_info.h>.
                Some((name.to_string_lossy().into_owned(), info.pth_run_state == 3))
            })
            .collect()
    }
}

/// Process `pid`'s threads now, and its threads' context switches so far:
/// how often anything in it woke.
pub fn threads_and_switches(pid: u32) -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        let field = |status: &str, name: &str| -> u64 {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name)?.trim().parse().ok())
                .unwrap_or(0)
        };
        let (mut threads, mut switches) = (0, 0);
        for task in fs::read_dir(format!("/proc/{pid}/task")).unwrap().flatten() {
            let Ok(status) = fs::read_to_string(task.path().join("status")) else {
                continue;
            };
            threads += 1;
            switches += field(&status, "voluntary_ctxt_switches:")
                + field(&status, "nonvoluntary_ctxt_switches:");
        }
        (threads, switches)
    }
    #[cfg(target_vendor = "apple")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
        let size = i32::try_from(size_of::<libc::proc_taskinfo>()).unwrap();
        #[allow(unsafe_code)]
        // SAFETY: `info` is a writable buffer of exactly `size` bytes, as
        // the flavor requires; the call writes at most that, and all of it
        // when it returns `size`.
        let info = unsafe {
            let written = libc::proc_pidinfo(
                pid.cast_signed(),
                libc::PROC_PIDTASKINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            );
            assert_eq!(written, size, "no task info for {pid}");
            info.assume_init()
        };
        (
            u64::try_from(info.pti_threadnum).unwrap(),
            u64::try_from(info.pti_csw).unwrap(),
        )
    }
}

#[test]
fn every_case_executes_the_same_fixture_without_aliases() {
    let first = Env::new("fixture-first");
    let second = Env::new("fixture-second");
    assert_ne!(first.home.root(), second.home.root());
    assert_eq!(first.exe, built());
    assert_eq!(second.exe, first.exe);
}
