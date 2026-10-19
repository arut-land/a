//! `bash`: one command per call, in a fresh non-interactive bash that
//! starts where the last command left off.
//!
//! A command runs in a session and process group of its own, with no stdin
//! and no terminal, so nothing it starts can wait for input or outlive a
//! timeout unnoticed. Its stdout and stderr share one pipe, in the order
//! written. The directory it ends in is reported through an exit trap and
//! kept as the `shell` extension's `cwd` state, so `cd` persists across
//! calls and restarts. A session runs its calls one at a time, so commands
//! never race each other for that directory.

use std::collections::BTreeMap;
use std::fmt::Write as _;
#[cfg(unix)]
use std::future::{Future, poll_fn};
use std::io;
#[cfg(unix)]
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::pin::{Pin, pin};
use std::process::{Child, Command as Process, ExitStatus, Stdio};
#[cfg(unix)]
use std::task::Poll;
use std::time::{Duration, Instant};

use protocol::{Card, Environment};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(unix)]
use super::output::Live;
use super::output::{Output, Rendered, Spill, create_private, private_dir};
#[cfg(unix)]
use crate::Kill;
use crate::state::Cwd;
use crate::{Cx, Spawn, Tool};

pub struct Bash;

/// Seconds a command may run when the call does not say.
pub const TIMEOUT: u64 = 120;
pub const MAX_TIMEOUT: u64 = 600;
/// After the shell exits, how long output may pause before the call ends
/// anyway: a background process may hold the pipe open indefinitely.
const IDLE: Duration = Duration::from_millis(100);
/// After the shell exits, the most the call waits for its output to end.
const LINGER: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(20);
/// How often output is reported as it arrives: each report is committed,
/// so this bounds how often a busy command syncs the log.
const REPORT: Duration = Duration::from_millis(100);

/// Set for every command: no colors, pagers or prompts, as no one is at a
/// terminal to see or answer them.
const QUIET: [(&str, &str); 5] = [
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

#[derive(Deserialize)]
pub struct Command {
    pub command: String,
    /// Seconds; [`TIMEOUT`] by default, at most [`MAX_TIMEOUT`].
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub background: bool,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ran {
    /// `None` when a signal ended it, or it runs in the background.
    pub exit: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    /// The limit, in seconds, that it ran into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub aborted: bool,
    /// Where the command left off.
    pub cwd: PathBuf,
    pub duration_ms: u64,
    /// What the model is shown of stdout and stderr, cleaned and cut.
    pub output: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// The whole output, when it was cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Job>,
    /// The process group, when processes the command started outlived it.
    /// The session owns them from then on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lingering: Option<u32>,
}

/// A command started in the background, owned by the session.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub pgid: u32,
    /// Where its output goes.
    pub log: PathBuf,
}

impl Tool for Bash {
    type In = Command;
    type Out = Ran;

    const NAME: &'static str = "bash";
    const DESCRIPTION: &'static str = "Runs a bash command in the session's working directory, which persists across calls, and returns its combined stdout and stderr with its exit status. There is no stdin or terminal.";

    fn schema() -> Value {
        super::schema(
            r#"{
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command line." },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 600,
                        "description": "Seconds before the command is stopped; 120 by default."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Start it in the background, for servers and watchers, and return at once with its process group and log file."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }"#,
        )
    }

    async fn call(&self, input: Command, cx: &Cx) -> Result<Ran, String> {
        run(input, cx).await.map_err(|e| e.to_string())
    }

    fn to_model(ran: &Ran) -> llm::Output {
        llm::Output::Text(model_text(ran))
    }

    fn present(input: &Command) -> Option<Card> {
        Some(Card {
            title: title(input),
            ..Card::default()
        })
    }

    fn present_result(input: &Command, ran: &Ran) -> Option<Card> {
        Some(Card {
            title: title(input),
            body: ran.output.clone(),
            note: note(ran),
        })
    }
}

fn title(input: &Command) -> String {
    let mut lines = input.command.trim().lines();
    let mut title = format!("$ {}", lines.next().unwrap_or_default());
    if lines.next().is_some() {
        title.push_str(" …");
    }
    if input.background {
        title.push_str(" &");
    }
    title
}

fn note(ran: &Ran) -> String {
    let mut note = Vec::new();
    if let Some(limit) = ran.timed_out {
        note.push(format!("timed out after {limit}s"));
    }
    if ran.aborted {
        note.push("aborted".to_owned());
    }
    match (&ran.background, &ran.signal, ran.exit) {
        (Some(job), ..) => note.push(format!("in the background, pgid {}", job.pgid)),
        (_, Some(signal), _) => note.push(format!("killed by {signal}")),
        (_, _, Some(code)) => note.push(format!("exit {code}")),
        _ => {}
    }
    note.join(" · ")
}

/// What the model reads: the output, then what became of the command,
/// its exit status last.
fn model_text(ran: &Ran) -> String {
    let mut text = ran.output.trim_end_matches('\n').to_owned();
    let mut trailer = |line: String| {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&line);
    };
    if let Some(job) = &ran.background {
        trailer(format!(
            "[running in the background: pgid {}; output: {}]",
            job.pgid,
            job.log.display()
        ));
        return text;
    }
    if ran.output.is_empty() {
        trailer("(no output)".to_owned());
    }
    if let Some(pgid) = ran.lingering {
        trailer(format!("[background processes still running: pgid {pgid}]"));
    }
    if ran.aborted {
        trailer("[aborted]".to_owned());
    }
    if let Some(limit) = ran.timed_out {
        trailer(format!("[timed out after {limit}s]"));
    }
    match (&ran.signal, ran.exit) {
        (Some(signal), _) => trailer(format!("[killed by signal: {signal}]")),
        (None, Some(code)) => trailer(format!("[exit code: {code}]")),
        (None, None) => {}
    }
    text
}

async fn run(input: Command, cx: &Cx) -> io::Result<Ran> {
    let started = Instant::now();
    let dir = cx.dir();
    // An interrupted turn's call starts nothing.
    if cx.cancel.is_cancelled() {
        return Ok(Ran {
            aborted: true,
            cwd: dir,
            ..Ran::default()
        });
    }
    let env = environment(cx, std::env::vars());
    let log = cx.spill.join(format!("{}.log", cx.call));
    if input.background {
        return background(cx, spawn(cx, &input.command, &dir, env, None)?, &log);
    }
    let report = cx.spill.join(format!("{}.cwd", cx.call));
    let spawn = spawn(cx, &input.command, &dir, env, Some(&report))?;
    let (reader, writer) = io::pipe()?;
    let child = {
        let mut cmd = process(&spawn);
        cmd.stdout(writer.try_clone()?).stderr(writer);
        // Dropping `cmd` closes our copies of the write end, so the pipe
        // ends when the command's processes close theirs.
        cmd.spawn()?
    };
    let pgid = child.id();
    cx.spawned(pgid, false);
    let limit = input.timeout.unwrap_or(TIMEOUT).clamp(1, MAX_TIMEOUT);
    let output = Output::new(Spill::new(log));
    let done = supervise(child, reader, Duration::from_secs(limit), cx, output).await?;

    let ended = std::fs::read_to_string(&report).ok();
    let _ = std::fs::remove_file(&report);
    let cwd = ended
        .map(|dir| PathBuf::from(dir.trim_end_matches('\n')))
        .filter(|dir| dir.is_absolute() && dir.is_dir())
        .unwrap_or_else(|| dir.clone());
    // `pwd -P` resolves symlinks the starting directory may have.
    if std::fs::canonicalize(&dir).ok().as_ref() != Some(&cwd) {
        let base = cx.env.as_ref().map(|env| env.cwd.as_path());
        cx.update::<Cwd>(Cwd::change(&cwd, base));
    }
    if done.lingering {
        cx.own(pgid, None);
    }
    let Rendered {
        text,
        truncated,
        full,
    } = done.output;
    Ok(Ran {
        exit: done.status.and_then(|s| s.code()),
        signal: done.status.and_then(signal_of),
        timed_out: (done.stopped == Some(Stop::TimedOut)).then_some(limit),
        aborted: done.stopped == Some(Stop::Aborted),
        cwd,
        duration_ms: duration_ms(started),
        output: text,
        truncated,
        full,
        background: None,
        lingering: done.lingering.then_some(pgid),
    })
}

/// Starts the command with its output going straight to its log, and
/// leaves it running: the session owns it from here.
fn background(cx: &Cx, spawn: Spawn, log: &Path) -> io::Result<Ran> {
    let started = Instant::now();
    let (file, log) = create_private(log)?;
    let mut cmd = process(&spawn);
    cmd.stdout(file.try_clone()?).stderr(file);
    let child = cmd.spawn()?;
    drop(cmd);
    let pgid = child.id();
    cx.own(pgid, Some(child));
    Ok(Ran {
        cwd: spawn.cwd,
        duration_ms: duration_ms(started),
        background: Some(Job { pgid, log }),
        ..Ran::default()
    })
}

/// The process for `command`, as the `spawn` hooks leave it: bash without
/// startup files, so the user's prompt and aliases stay out of it, given a
/// script that reports where it ended to `report`.
fn spawn(
    cx: &Cx,
    command: &str,
    dir: &Path,
    env: BTreeMap<String, String>,
    report: Option<&Path>,
) -> io::Result<Spawn> {
    let bash = find_bash(&env).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "bash not found: neither /bin/bash nor bash on PATH",
        )
    })?;
    if report.is_some() {
        private_dir(&cx.spill)?;
    }
    let mut script = String::new();
    if let Some(report) = report {
        let pwd = format!("pwd -P >{}", quote(&report.to_string_lossy()));
        let _ = writeln!(script, "trap {} EXIT", quote(&pwd));
    }
    // On its own line, so a command ending in a comment or a heredoc
    // still ends where it should.
    let _ = writeln!(
        script,
        "cd -- {} || exit\n{command}",
        quote(&dir.to_string_lossy())
    );
    let mut spawn = Spawn {
        argv: [bash.as_str(), "--noprofile", "--norc", "-c", &script]
            .map(str::to_owned)
            .into(),
        env,
        cwd: dir.to_path_buf(),
    };
    cx.spawn_hooks(&mut spawn);
    if spawn.argv.is_empty() {
        return Err(io::Error::other("a spawn hook left no program to run"));
    }
    Ok(spawn)
}

pub(crate) fn process(spawn: &Spawn) -> Process {
    let mut cmd = Process::new(&spawn.argv[0]);
    cmd.args(&spawn.argv[1..])
        .env_clear()
        .envs(&spawn.env)
        .current_dir(&spawn.cwd)
        .stdin(Stdio::null());
    sys::new_session(&mut cmd);
    cmd
}

/// The session's environment (the client's, or else the daemon's without
/// its secrets), quieted, and told which session and model it serves.
fn environment(
    cx: &Cx,
    daemon: impl Iterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    let mut env = cx.env.as_ref().map_or_default(|env| env.vars.clone());
    if cx.env.is_none() {
        // Inserted one by one: collecting would sort, in code of its own.
        for (name, value) in daemon.filter(|(name, _)| !Environment::is_secret(name)) {
            env.insert(name, value);
        }
    }
    for (name, value) in QUIET {
        env.insert(name.to_owned(), value.to_owned());
    }
    env.insert("A_SESSION_ID".to_owned(), cx.session.to_string());
    env.insert("A_MODEL".to_owned(), cx.model.to_string());
    env
}

fn find_bash(env: &BTreeMap<String, String>) -> Option<String> {
    let system = Path::new("/bin/bash");
    if system.is_file() {
        return Some(system.display().to_string());
    }
    let name = if cfg!(windows) { "bash.exe" } else { "bash" };
    std::env::split_paths(env.get("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .map(|path| path.display().to_string())
}

/// `text` as one shell word.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn duration_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(unix)]
fn signal_of(status: ExitStatus) -> Option<String> {
    std::os::unix::process::ExitStatusExt::signal(&status).map(sys::signal_name)
}

#[cfg(not(unix))]
fn signal_of(_status: ExitStatus) -> Option<String> {
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    TimedOut,
    Aborted,
}

struct Finished {
    status: Option<ExitStatus>,
    stopped: Option<Stop>,
    lingering: bool,
    output: Rendered,
}

/// Reads the command's output until it ends, reporting it as it comes and
/// stopping the command when it runs out of time or the call is cancelled.
/// It waits, as a task on the reactor, for whichever comes first of
/// output, the shell's exit, a cancel and the next deadline: a quiet
/// command costs no thread and no wake-up while it runs.
#[cfg(unix)]
async fn supervise(
    mut child: Child,
    reader: io::PipeReader,
    limit: Duration,
    cx: &Cx,
    mut output: Output,
) -> io::Result<Finished> {
    let pgid = child.id();
    let pipe = rt::Async::for_reading(reader)?;
    // Where the exit cannot be watched (a kernel without pidfds), it is
    // looked for every tick.
    let exit = sys::exit_watch(pgid).and_then(rt::Async::for_reading).ok();
    let mut buf = vec![0; 64 * 1024];
    let mut live = Live::default();
    let started = Instant::now();
    let mut kill = Kill::new(pgid);
    let mut killing: Option<Instant> = None;
    let (mut status, mut stopped, mut eof, mut exit_seen) = (None, None, false, false);
    let mut exited: Option<Instant> = None;
    let mut heard = Instant::now();
    let mut reported = Instant::now();
    let mut unreported = false;
    loop {
        let next = match exited {
            Some(at) => (heard.max(at) + IDLE).min(at + LINGER),
            None if exit.is_none() || exit_seen => Instant::now() + TICK,
            None => match stopped {
                Some(_) => killing.unwrap_or(started + Duration::from_secs(MAX_TIMEOUT * 2)),
                None => started + limit,
            },
        };
        let next = if unreported {
            next.min(reported + REPORT)
        } else {
            next
        };
        let wait = next.saturating_duration_since(Instant::now());
        if !wait.is_zero() {
            let readable = when(!eof, pipe.readable());
            let watched = exit.as_ref().filter(|_| status.is_none() && !exit_seen);
            let exits = when(watched.is_some(), async {
                if let Some(exit) = watched {
                    exit.readable().await;
                }
            });
            let cancelled = when(stopped.is_none(), cx.cancel.cancelled());
            let woke = first([
                pin!(readable),
                pin!(exits),
                pin!(cancelled),
                pin!(cx.rt.sleep(wait)),
            ])
            .await;
            exit_seen |= woke == 1;
        }
        eof |= drain(&pipe, &mut buf, |chunk| {
            output.push(chunk);
            live.push(chunk);
            heard = Instant::now();
            unreported = true;
        });
        if unreported && reported.elapsed() >= REPORT {
            unreported = false;
            if let Some(text) = live.take() {
                cx.progress(&text);
                reported = Instant::now();
            }
        }
        if status.is_none() {
            status = child.try_wait()?;
            if status.is_some() {
                exited = Some(Instant::now());
            }
        }
        if let Some(at) = exited {
            let quiet = heard.max(at).elapsed() >= IDLE;
            if eof || quiet || at.elapsed() >= LINGER {
                break;
            }
            continue;
        }
        if stopped.is_none() {
            stopped = if cx.cancel.is_cancelled() {
                Some(Stop::Aborted)
            } else if started.elapsed() >= limit {
                Some(Stop::TimedOut)
            } else {
                None
            };
        }
        if stopped.is_some() && killing.is_none_or(|at| Instant::now() >= at) {
            killing = kill.tick();
        }
    }
    drain(&pipe, &mut buf, |chunk| output.push(chunk));
    let mut lingering = sys::group_alive(pgid);
    // A command that was stopped leaves nothing behind.
    if lingering && stopped.is_some() {
        kill.now();
        lingering = false;
    }
    Ok(Finished {
        status,
        stopped,
        lingering,
        output: output.finish(),
    })
}

/// No daemon runs where processes have no groups to stop.
#[cfg(not(unix))]
#[allow(clippy::unused_async)]
async fn supervise(
    _child: Child,
    _reader: io::PipeReader,
    _limit: Duration,
    _cx: &Cx,
    _output: Output,
) -> io::Result<Finished> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Reads what the pipe has without waiting, handing each chunk to
/// `chunk`; whether it ended.
#[cfg(unix)]
fn drain(pipe: &rt::Async<io::PipeReader>, buf: &mut [u8], mut chunk: impl FnMut(&[u8])) -> bool {
    loop {
        match { pipe.get_ref() }.read(buf) {
            Ok(0) => return true,
            Ok(n) => chunk(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return e.kind() != io::ErrorKind::WouldBlock,
        }
    }
}

/// `future`, or one that never resolves when `cond` does not hold.
#[cfg(unix)]
async fn when(cond: bool, future: impl Future<Output = ()>) {
    if cond {
        future.await;
    } else {
        std::future::pending::<()>().await;
    }
}

/// Which of `futures` resolved first.
#[cfg(unix)]
fn first<const N: usize>(
    mut futures: [Pin<&mut (dyn Future<Output = ()> + Send)>; N],
) -> impl Future<Output = usize> {
    poll_fn(move |cx| {
        futures
            .iter_mut()
            .position(|f| f.as_mut().poll(cx).is_ready())
            .map_or(Poll::Pending, Poll::Ready)
    })
}

#[cfg(all(test, unix))]
mod tests;
