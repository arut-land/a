//! `bash`: one command per call, in a fresh non-interactive bash that
//! starts where the last command left off.
//!
//! A command runs in a session and process group of its own, with no stdin
//! and no terminal, so nothing it starts can wait for input or outlive a
//! timeout unnoticed. Its stdout and stderr share one pipe, in the order
//! written. The directory it ends in is reported through an exit trap and
//! kept as the `shell` extension's `cwd` state, so `cd` persists across
//! calls and restarts. Its calls are exclusive ([`ext::Concurrency`]): a
//! session runs one at a time, so commands never race for that directory.
//!
//! Output crosses a bounded capture pipe into the session's spill file.
//! Storage admission applies back-pressure; the supervisor follows the file,
//! woken by its writes, keeping only what the model is shown, and the file
//! is kept as the whole output when that was cut. A command whose output
//! passes [`OUTPUT_CAP`] is stopped. Cancellation before or during process
//! launch reports an aborted command, as cancellation during supervision does.

use ext::{LaunchSpec, OwnedProcess, ProcessOutput, ProcessSignal, Processes};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::future::Future;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use protocol::{Card, Environment, Mark};
use serde::{Deserialize, Serialize};

pub use crate::output::OUTPUT_CAP;
use crate::output::{Live, Output, Rendered, create_private, private_dir};
use crate::state::Cwd;
use ext::shape::{self, Streams};
use ext::{Cx, Json, Partials, Schema, SpawnSpec as Spawn, Tool};

pub struct Bash;

/// How long a stopped command's group has between SIGTERM and SIGKILL.
pub const GRACE: Duration = Duration::from_secs(3);

/// The runtime the engine runs the call on: its clock and its timers.
fn runtime(cx: &Cx) -> io::Result<&dyn rt::Runtime> {
    cx.service::<dyn rt::Runtime>()
        .ok_or_else(|| io::Error::other("the engine provides no runtime"))
}

/// Seconds a command may run when the call does not say.
pub const TIMEOUT: u64 = 120;
pub const MAX_TIMEOUT: u64 = 600;
/// After the shell exits, how long output may pause before the call ends
/// anyway: a background process may hold the pipe open indefinitely.
const IDLE: Duration = Duration::from_millis(100);
/// After the shell exits, the most the call waits for its output to end.
const LINGER: Duration = Duration::from_secs(2);
/// The most output read in one go, so a flood shares the reactor.
const READ: u64 = 1 << 20;
/// How often output is reported as it arrives: each report is a log entry,
/// written at once and made durable with the next commit that has to be.
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

#[derive(Deserialize, Schema)]
pub struct Command {
    /// The command line.
    pub command: String,
    /// Seconds before the command is stopped; 120 by default.
    #[serde(default)]
    #[schema(min = 1, max = 600)]
    pub timeout: Option<u64>,
    /// Start it in the background, for servers and watchers, and return at once with its process group and log file.
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
    /// The limit, in bytes, its output ran past: it was stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flooded: Option<u64>,
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

    /// Streams: its output as it comes is its progress.
    fn call(&self, input: &Command, cx: &Cx) -> impl shape::Output<Ran, Streams<String>> {
        ext::stream(|output| async move { run(input, cx, output).await.map_err(|e| e.to_string()) })
    }

    fn to_model(output: &Json) -> llm::Output {
        output.parse().map_or_else(
            |_| llm::Output::Json(output.clone()),
            |ran| llm::Output::Text(model_text(&ran)),
        )
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
            mark: mark(ran),
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
    if let Some(limit) = ran.flooded {
        note.push(format!("stopped past {} MiB of output", limit >> 20));
    }
    match (&ran.background, &ran.signal, ran.exit) {
        (Some(job), ..) => note.push(format!("in the background, pgid {}", job.pgid)),
        (_, Some(signal), _) => note.push(format!("killed by {signal}")),
        (_, _, Some(code)) => note.push(format!("exit {code}")),
        _ => {}
    }
    note.join(" · ")
}

/// How the command went, as only its exit status says: a nonzero exit, a
/// signal, a timeout or an abort is a failure; stopped for flooding, a
/// warning.
fn mark(ran: &Ran) -> Mark {
    let failed = ran.exit.is_some_and(|code| code != 0)
        || ran.signal.is_some()
        || ran.timed_out.is_some()
        || ran.aborted;
    if failed {
        Mark::Bad
    } else if ran.flooded.is_some() {
        Mark::Warn
    } else {
        Mark::Ok
    }
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
    if let Some(limit) = ran.flooded {
        trailer(format!("[stopped: its output passed {} MiB]", limit >> 20));
    }
    match (&ran.signal, ran.exit) {
        (Some(signal), _) => trailer(format!("[killed by signal: {signal}]")),
        (None, Some(code)) => trailer(format!("[exit code: {code}]")),
        (None, None) => {}
    }
    text
}

async fn run(input: &Command, cx: &Cx, progress: Partials<String>) -> io::Result<Ran> {
    match execute(input, cx, progress).await {
        // Launch can observe cancellation after the initial check, before
        // bash has a process to supervise and mark aborted.
        Err(error) if error.kind() == io::ErrorKind::Interrupted && cx.cancel.is_set() => Ok(Ran {
            aborted: true,
            cwd: cx.dir(),
            ..Ran::default()
        }),
        result => result,
    }
}

async fn execute(input: &Command, cx: &Cx, progress: Partials<String>) -> io::Result<Ran> {
    let started = runtime(cx)?.now();
    let dir = cx.dir();
    // An interrupted turn's call starts nothing.
    if cx.cancel.is_set() {
        return Ok(Ran {
            aborted: true,
            cwd: dir,
            ..Ran::default()
        });
    }
    let env = environment(cx, std::env::vars());
    let log = cx.spill.join(format!("{}.log", cx.call));
    if input.background {
        return background(cx, spawn(cx, &input.command, &dir, env, None)?, &log).await;
    }
    let report = cx.spill.join(format!("{}.cwd", cx.call));
    let spawn = spawn(cx, &input.command, &dir, env, Some(&report))?;
    // Both streams share the one file, and its offset: in the order written.
    let (file, log) = create_private(&log)?;
    // Watched before anything can write to it: a write before the watch
    // raises no edge, and would be read only once the command exits.
    let written = rt::Async::for_reading(&runtime(cx)?.reactor(), sys::write_watch(&log)?)?;
    let child = launch(cx, spawn, file).await?;
    let pgid = child.id();
    cx.spawned(pgid, false);
    let limit = input.timeout.unwrap_or(TIMEOUT).clamp(1, MAX_TIMEOUT);
    let output = Followed { log: &log, written };
    let done = supervise(child, output, Duration::from_secs(limit), cx, &progress).await?;
    // The file is kept as the whole output only when the model saw less.
    if done.output.full.is_none() {
        let _ = std::fs::remove_file(&log);
    }

    let ended = std::fs::read_to_string(&report).ok();
    let _ = std::fs::remove_file(&report);
    let cwd = ended
        .map(|dir| sys::shell_directory(&dir))
        .filter(|dir| dir.is_absolute() && dir.is_dir())
        .unwrap_or_else(|| dir.clone());
    // `pwd -P` resolves symlinks the starting directory may have.
    let moved = (std::fs::canonicalize(&dir).ok().as_ref() != Some(&cwd))
        .then(|| Cwd::change(&cwd, cx.env.as_ref().map(|env| env.cwd.as_path())));
    if done.lingering {
        done.process.detach(cx);
    }
    let Rendered {
        text,
        truncated,
        full,
    } = done.output;
    let ran = Ran {
        exit: done.status.code(),
        signal: sys::exit_signal(done.status),
        timed_out: (done.stopped == Some(Stop::TimedOut)).then_some(limit),
        aborted: done.stopped == Some(Stop::Aborted),
        flooded: (done.stopped == Some(Stop::Flooded)).then_some(OUTPUT_CAP),
        cwd,
        duration_ms: duration_ms(started),
        output: text,
        truncated,
        full,
        background: None,
        lingering: done.lingering.then_some(pgid),
    };
    match moved {
        Some(change) => cx
            .record_effect::<Cwd, _>(ran, &change)
            .await
            .map_err(io::Error::other),
        None => Ok(ran),
    }
}

/// Starts the command with its output going straight to its log, and
/// leaves it running: the session owns it from here.
async fn background(cx: &Cx, spawn: Spawn, log: &Path) -> io::Result<Ran> {
    let started = runtime(cx)?.now();
    let (file, log) = create_private(log)?;
    let cwd = spawn.cwd.clone();
    let child = launch(cx, spawn, file).await?;
    let pgid = child.id();
    child.detach(cx);
    Ok(Ran {
        cwd,
        duration_ms: duration_ms(started),
        background: Some(Job { pgid, log }),
        ..Ran::default()
    })
}

async fn launch(cx: &Cx, process: Spawn, file: File) -> io::Result<OwnedProcess> {
    cx.service::<dyn Processes>()
        .ok_or_else(|| io::Error::other("no process provider"))?
        .spawn(
            cx,
            LaunchSpec {
                process,
                output: ProcessOutput::File(file),
            },
        )
        .await
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
    let bash = sys::find_bash(env.get("PATH").map(String::as_str)).ok_or_else(|| {
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
        let pwd = format!("{} >{}", sys::shell_pwd(), quote(&sys::shell_path(report)));
        let _ = writeln!(script, "trap {} EXIT", quote(&pwd));
    }
    // On its own line, so a command ending in a comment or a heredoc
    // still ends where it should.
    let _ = writeln!(
        script,
        "cd -- {} || exit\n{command}",
        quote(&sys::shell_path(dir))
    );
    let spawn = Spawn {
        argv: [bash.as_str(), "--noprofile", "--norc", "-c", &script]
            .map(str::to_owned)
            .into(),
        env,
        cwd: dir.to_path_buf(),
    };
    Ok(spawn)
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

/// `text` as one shell word.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn duration_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Why a command was stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    TimedOut,
    Aborted,
    /// Its output passed [`OUTPUT_CAP`].
    Flooded,
}

/// Where a command stands, as its supervisor drives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    /// The shell runs, until `deadline`.
    Running { deadline: Instant },
    /// It is being stopped, for `why`.
    Stopping { why: Stop, kill: KillPhase },
    /// The shell exited `at`; what it left running may still write.
    Exited {
        status: ExitStatus,
        why: Option<Stop>,
        at: Instant,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KillPhase {
    /// SIGTERM went to the group `at`; SIGKILL follows a grace later.
    Term {
        at: Instant,
    },
    Killed,
}

/// What the supervisor saw since its last step.
#[derive(Clone, Copy, Debug, Default)]
struct Seen {
    exited: Option<ExitStatus>,
    cancelled: bool,
    flooded: bool,
}

impl Run {
    /// The next state at `now`, and the signal to send the group to get
    /// there. The shell's exit ends any state but its own; a stop starts
    /// only while it runs.
    fn step(self, now: Instant, seen: Seen) -> (Self, Option<ProcessSignal>) {
        let exited = |status, why| {
            (
                Self::Exited {
                    status,
                    why,
                    at: now,
                },
                None,
            )
        };
        match (self, seen.exited) {
            (Self::Running { .. }, Some(status)) => exited(status, None),
            (Self::Stopping { why, .. }, Some(status)) => exited(status, Some(why)),
            (Self::Running { deadline }, None) => {
                let why = if seen.cancelled {
                    Stop::Aborted
                } else if seen.flooded {
                    Stop::Flooded
                } else if now >= deadline {
                    Stop::TimedOut
                } else {
                    return (self, None);
                };
                let kill = KillPhase::Term { at: now };
                (Self::Stopping { why, kill }, Some(ProcessSignal::Terminate))
            }
            (
                Self::Stopping {
                    why,
                    kill: KillPhase::Term { at },
                },
                None,
            ) if now >= at + GRACE => {
                let kill = KillPhase::Killed;
                (Self::Stopping { why, kill }, Some(ProcessSignal::Kill))
            }
            (Self::Exited { .. }, _) | (Self::Stopping { .. }, None) => (self, None),
        }
    }

    /// When it must step next if nothing happens first, given that the
    /// output was last heard `heard`; `None` for never.
    fn deadline(self, heard: Instant) -> Option<Instant> {
        match self {
            Self::Running { deadline } => Some(deadline),
            Self::Stopping {
                kill: KillPhase::Term { at },
                ..
            } => Some(at + GRACE),
            Self::Stopping { .. } => None,
            Self::Exited { at, .. } => Some((heard.max(at) + IDLE).min(at + LINGER)),
        }
    }
}

struct Finished {
    status: ExitStatus,
    stopped: Option<Stop>,
    lingering: bool,
    output: Rendered,
    process: OwnedProcess,
}

/// Installed before launch so the first write cannot lose its wakeup.
struct Followed<'a> {
    log: &'a Path,
    written: rt::Async<sys::fs::WriteWatch>,
}

/// Follows the command's output in `log` until it ends, reporting it as it
/// comes and stopping the command when it runs out of time, writes too
/// much or the call is cancelled. It waits, as a task on the reactor, for
/// whichever comes first of a write, the shell's exit, a cancel and the
/// next deadline: a quiet command costs no thread and no wake-up while it
/// runs. Once the shell has exited, the output has ended when nothing of
/// its group is left to write, or it has paused [`IDLE`], or after
/// [`LINGER`].
async fn supervise(
    child: OwnedProcess,
    Followed { log, written }: Followed<'_>,
    limit: Duration,
    cx: &Cx,
    progress: &Partials<String>,
) -> io::Result<Finished> {
    let rt = runtime(cx)?;
    let path = log.to_owned();
    let mut file =
        rt::unblock_with(rt, rt::BlockingClass::Storage, 0, move || File::open(path)).await?;
    let mut buf = vec![0; 64 * 1024];
    let (mut output, mut live) = (Output::default(), Live::default());
    let start = rt.now();
    let mut run = Run::Running {
        deadline: start + limit,
    };
    let (mut heard, mut reported) = (start, start);
    let (mut unreported, mut behind) = (false, false);
    loop {
        let now = rt.now();
        let exited = matches!(run, Run::Exited { .. });
        let next = [run.deadline(heard), unreported.then_some(reported + REPORT)]
            .into_iter()
            .flatten()
            .min();
        let wait = next.map(|at| at.saturating_duration_since(now));
        if !behind && wait.is_none_or(|wait| !wait.is_zero()) {
            let writes = written.readable_result();
            let exits = when(!exited, async { child.wait().await.map(|_| ()) });
            let cancelled = when(matches!(run, Run::Running { .. }), async {
                cx.cancel.wait().await;
                Ok(())
            });
            let sleep = when(wait.is_some(), async {
                rt.sleep(wait.unwrap_or_default()).await;
                Ok(())
            });
            rt::or(writes, rt::or(exits, rt::or(cancelled, sleep))).await?;
        }
        written.get_ref().clear();
        let read;
        (file, buf, output, live, read) = follow_chunk(rt, file, buf, output, live).await?;
        behind = read == READ;
        if read > 0 {
            rt::yield_now().await;
        }
        let now = rt.now();
        if read > 0 {
            heard = now;
            unreported = true;
        }
        if unreported && now >= reported + REPORT {
            unreported = false;
            if let Some(text) = live.take() {
                progress.send(text);
                reported = now;
            }
        }
        let seen = Seen {
            exited: if exited { None } else { child.try_wait()? },
            cancelled: cx.cancel.is_set(),
            flooded: output.bytes() >= OUTPUT_CAP,
        };
        let signal;
        (run, signal) = run.step(now, seen);
        if let Some(signal) = signal {
            let _ = child.signal(signal);
        }
        if let Run::Exited { status, why, at } = run {
            let quiet = now >= heard.max(at) + IDLE;
            if !behind && (!child.alive() || quiet || now >= at + LINGER) {
                let mut lingering = child.alive();
                // A command that was stopped leaves nothing behind.
                if lingering && why.is_some() {
                    let _ = child.signal(ProcessSignal::Kill);
                    lingering = false;
                }
                if !lingering {
                    // Exit can precede the final captured bytes reaching the file.
                    child.output_drained().await?;
                }
                let rest = OUTPUT_CAP.saturating_sub(output.bytes()).min(READ);
                output = rt::unblock_with(rt, rt::BlockingClass::Storage, 256 << 10, move || {
                    follow(&mut file, &mut buf, rest, |chunk| output.push(chunk))?;
                    Ok::<_, io::Error>(output)
                })
                .await?;
                return Ok(Finished {
                    status,
                    stopped: why
                        .or_else(|| (output.bytes() >= OUTPUT_CAP).then_some(Stop::Flooded)),
                    lingering,
                    output: output.finish(log),
                    process: child,
                });
            }
        }
    }
}

async fn follow_chunk(
    rt: &dyn rt::Runtime,
    mut file: File,
    mut buf: Vec<u8>,
    mut output: Output,
    mut live: Live,
) -> io::Result<(File, Vec<u8>, Output, Live, u64)> {
    rt::unblock_with(rt, rt::BlockingClass::Storage, 256 << 10, move || {
        let read = follow(&mut file, &mut buf, READ, |chunk| {
            output.push(chunk);
            live.push(chunk);
        })?;
        Ok((file, buf, output, live, read))
    })
    .await
}

/// Reads what `file` has past where it was last read, up to `most` bytes,
/// handing each chunk to `chunk`; how many bytes it read.
fn follow(
    file: &mut File,
    buf: &mut [u8],
    most: u64,
    mut chunk: impl FnMut(&[u8]),
) -> io::Result<u64> {
    let mut read = 0;
    while read < most {
        let room = usize::try_from(most - read).map_or(buf.len(), |left| left.min(buf.len()));
        match file.read(&mut buf[..room]) {
            Ok(0) => break,
            Ok(n) => {
                chunk(&buf[..n]);
                read += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(read)
}

/// `future`, or one that never resolves when `cond` does not hold.
async fn when<T>(cond: bool, future: impl Future<Output = T>) -> T {
    if cond {
        future.await
    } else {
        std::future::pending().await
    }
}

#[cfg(all(test, unix))]
mod tests;
