//! `bash`: one command per call, in a fresh non-interactive bash that
//! starts where the last command left off.
//!
//! A command runs in an owned native process scope with no stdin or
//! terminal. Cancellation stops that scope; `ExecutionFacts` describes the
//! platform's containment limits. Its timeout is how long the session waits:
//! past it the call goes on in the background ([`ext::Detach::Background`]),
//! the way a forced steer sends it, and its result reaches the agent on the
//! queue. A call the session must wait on (one another call invoked) is
//! stopped at its timeout instead. Its stdout and stderr share one pipe, in
//! the order written. The directory it ends in is reported through an exit trap and
//! kept as the `shell` extension's `cwd` state, so `cd` persists across
//! calls and restarts. Its calls are exclusive ([`ext::Concurrency`]): a
//! session runs one at a time, so commands never race for that directory.
//!
//! Output crosses a bounded capture pipe into an open blob stream.
//! Storage admission applies back-pressure; the supervisor follows the
//! stream, woken by its writes, keeping only what the model is shown, and
//! the stream is sealed into a blob as the whole output when that was cut;
//! while processes the command left running still write it, the result
//! holds the stream instead, cut or not. The result
//! holds the blob or the stream ([`Tool::held`]); the engine tells the
//! model where each is. A stream a group the session owns writes is sealed
//! as the group ends, held by the end's queued notice
//! ([`ext::ProcessLease::holding`]). A command whose output passes
//! [`OUTPUT_CAP`] is stopped. Cancellation before or during process launch
//! reports an aborted command, as cancellation during supervision does.

use ext::{LaunchSpec, OwnedProcess, ProcessOutput, ProcessSignal, Processes};
use std::collections::BTreeMap;
use std::fs::File;
use std::future::Future;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use protocol::{Card, Environment, Mark};
use serde::{Deserialize, Serialize};

pub use crate::output::OUTPUT_CAP;
use crate::output::{Ended, Live, Output, Rendered};
use crate::state::Cwd;
use ext::blob::{self, BlobOrigin, BlobRef, Held, Open, OutputStream, Stream};
use ext::shape::{self, Streams};
use ext::{Cx, Json, Partials, Schema, SpawnSpec as Spawn, Tool};

pub struct ShellTool<D> {
    facts: &'static sys::shell::ExecutionFacts,
    /// Seconds a call that does not say is waited for.
    timeout: u64,
    description: String,
    dialect: std::marker::PhantomData<D>,
}

pub type Bash = ShellTool<crate::dialect::Bash>;
pub type PowerShell = ShellTool<crate::dialect::PowerShell>;
pub type WindowsPowerShell = ShellTool<crate::dialect::WindowsPowerShell>;

impl<D: crate::dialect::Dialect> ShellTool<D> {
    pub(crate) fn new(facts: &'static sys::shell::ExecutionFacts, timeout: u64) -> Self {
        Self {
            facts,
            timeout,
            description: format!("{}\n{}", D::DESCRIPTION, facts.context()),
            dialect: std::marker::PhantomData,
        }
    }
}

/// How long a stopped command's group has between SIGTERM and SIGKILL.
pub const GRACE: Duration = Duration::from_secs(3);

/// The runtime the engine runs the call on: its clock and its timers.
fn runtime(cx: &Cx) -> io::Result<&dyn rt::Runtime> {
    cx.service::<dyn rt::Runtime>()
        .ok_or_else(|| io::Error::other("the engine provides no runtime"))
}

/// Seconds a command is waited for when the call does not say, unless
/// `[ext.shell] timeout` sets another.
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
    /// Seconds to wait before the command moves to the background, still running; its result arrives later.
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
    /// The limit, in seconds, that it ran into: it was stopped, as the
    /// session had to wait on it.
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
    /// The whole output, when it was cut. Held by the result
    /// ([`Tool::held`]), never logged with the output.
    #[serde(skip)]
    pub full: Option<BlobRef>,
    /// The whole output, still growing: a background job's, or one that
    /// processes the command left running still write, never a blob
    /// sealed under them; sealed as their group ends. Held by the result, never logged with the output.
    #[serde(skip)]
    pub growing: Option<OutputStream>,
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
}

impl<D: crate::dialect::Dialect> Tool for ShellTool<D> {
    type In = Command;
    type Out = Ran;

    const NAME: &'static str = D::NAME;
    const DESCRIPTION: &'static str = D::DESCRIPTION;
    const DETACH: ext::Detach = ext::Detach::Background;

    fn description(&self) -> &str {
        &self.description
    }

    /// Streams: its output as it comes is its progress.
    fn call(&self, input: &Command, cx: &Cx) -> impl shape::Output<Ran, Streams<String>> {
        ext::stream(|output| async move {
            run::<D>(self.facts, self.timeout, input, cx, output)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn to_model(output: &Json) -> llm::Output {
        output.parse().map_or_else(
            |_| llm::Output::Json(output.clone()),
            |ran| llm::Output::Text(model_text(&ran)),
        )
    }

    fn held(ran: &Ran) -> Vec<Held> {
        let full = ran.full.clone().map(Held::Blob);
        let growing = ran.growing.map(Held::Stream);
        full.into_iter().chain(growing).collect()
    }

    fn present(input: &Command) -> Option<Card> {
        Some(Card {
            title: title(input, D::PROMPT),
            ..Card::default()
        })
    }

    fn present_result(input: &Command, ran: &Ran) -> Option<Card> {
        Some(Card {
            title: title(input, D::PROMPT),
            body: ran.output.clone(),
            note: note(ran),
            mark: mark(ran),
        })
    }
}

/// What a command gone on in the background needs, beyond its input, its
/// stream and its group, to go on in the daemon's next build.
#[derive(Debug, Serialize, Deserialize)]
pub struct Detached {
    /// Where it started.
    dir: PathBuf,
    /// When, in milliseconds since the Unix epoch.
    started: u64,
}

/// A command gone on in the background goes on in the next build where it
/// is: its output followed from where its stream is, its result the one it
/// would have had. A group it left to the session is the session's there.
impl<D: crate::dialect::Dialect> ext::Resumable for ShellTool<D> {
    type State = Detached;

    fn resume(
        &self,
        _input: &Command,
        state: Detached,
        claimed: ext::Native,
        cx: &Cx,
    ) -> impl shape::Output<Ran, Streams<String>> {
        ext::stream(|output| async move {
            resume::<D>(state, claimed, cx, output)
                .await
                .map_err(|e| e.to_string())
        })
    }

    async fn rehold(&self, claimed: ext::Native, cx: &Cx) -> Result<ext::ProcessLease, String> {
        let rehold = async {
            let Stream { file, open: log } = cx.blobs.reopen(cx.call, OUTPUT_CAP)?;
            let process = adopt(cx, claimed, file).await?;
            let output = Written(Some(log));
            let call = cx.call;
            io::Result::Ok(process.process.holding(move || output.seal(call)))
        };
        rehold.await.map_err(|e| e.to_string())
    }
}

fn title(input: &Command, prompt: &str) -> String {
    let mut lines = input.command.trim().lines();
    let mut title = format!("{prompt}{}", lines.next().unwrap_or_default());
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
        (Some(job), ..) => note.push(format!("in the background, job {}", job.pgid)),
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
        trailer(format!("[running in the background: job {}]", job.pgid));
        return text;
    }
    if ran.output.is_empty() {
        trailer("(no output)".to_owned());
    }
    if let Some(pgid) = ran.lingering {
        trailer(format!("[background processes still running: job {pgid}]"));
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

async fn run<D: crate::dialect::Dialect>(
    facts: &sys::shell::ExecutionFacts,
    timeout: u64,
    input: &Command,
    cx: &Cx,
    progress: Partials<String>,
) -> io::Result<Ran> {
    match execute::<D>(facts, timeout, input, cx, progress).await {
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

async fn execute<D: crate::dialect::Dialect>(
    facts: &sys::shell::ExecutionFacts,
    timeout: u64,
    input: &Command,
    cx: &Cx,
    mut progress: Partials<String>,
) -> io::Result<Ran> {
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
    let env = environment(cx, std::env::vars_os());
    // Both streams share the one file, and its offset: in the order written.
    let Stream { file, open: log } = cx.blobs.stream(cx.call, OUTPUT_CAP)?;
    if input.background {
        return background(
            cx,
            spawn_with::<D>(facts, &input.command, &dir, env, None)?,
            file,
            log,
        )
        .await;
    }
    // Beside the stream, so it is as private, and swept with it.
    let report = log.path().with_extension("cwd");
    let spawn = spawn_with::<D>(facts, &input.command, &dir, env, Some(&report))?;
    // Watched before anything can write to it: a write before the watch
    // raises no edge, and would be read only once the command exits.
    let written = rt::Async::for_reading(&runtime(cx)?.reactor(), sys::write_watch(log.path())?)?;
    let child = launch(cx, spawn, file).await?;
    cx.spawned(child.id(), false);
    let limit = input.timeout.unwrap_or(timeout).clamp(1, MAX_TIMEOUT);
    let output = Followed {
        log: log.path(),
        written,
    };
    let detached = Detached {
        dir: dir.clone(),
        started: now_ms(),
    };
    let supervised = Supervised::Launched {
        limit: Duration::from_secs(limit),
        detached: &detached,
    };
    let done = supervise::<D>(child, output, supervised, cx, &mut progress).await?;
    let ran = finish(cx, done, log, report, dir, limit).await?;
    Ok(Ran {
        duration_ms: duration_ms(started),
        ..ran
    })
}

/// Goes on with a command an earlier build ran and handed over, gone on in
/// the background there: its group adopted, its stream followed from the
/// start, as only what comes after the handover is new progress.
async fn resume<D: crate::dialect::Dialect>(
    detached: Detached,
    claimed: ext::Native,
    cx: &Cx,
    mut progress: Partials<String>,
) -> io::Result<Ran> {
    let Stream { file, open: log } = cx.blobs.reopen(cx.call, OUTPUT_CAP)?;
    let report = log.path().with_extension("cwd");
    let written = rt::Async::for_reading(&runtime(cx)?.reactor(), sys::write_watch(log.path())?)?;
    let child = adopt(cx, claimed, file).await?;
    // Handed over again, should another upgrade come before it ends.
    cx.resumable::<ShellTool<D>>(&child, &detached);
    let output = Followed {
        log: log.path(),
        written,
    };
    let done = supervise::<D>(child, output, Supervised::Resumed, cx, &mut progress).await?;
    let ran = finish(cx, done, log, report, detached.dir, 0).await?;
    Ok(Ran {
        duration_ms: now_ms().saturating_sub(detached.started),
        ..ran
    })
}

/// What a command that ended shows and keeps, and the directory it left
/// the session in; `limit` is the timeout it had.
async fn finish(
    cx: &Cx,
    done: Finished,
    log: Open,
    report: PathBuf,
    dir: PathBuf,
    limit: u64,
) -> io::Result<Ran> {
    let pgid = done.process.id();
    let (Rendered { text, truncated }, kept) = keep(cx, log, done.output, done.lingering).await?;
    let old_dir = dir;
    let (cwd, moved) = rt::unblock(runtime(cx)?, move || {
        let ended = std::fs::read_to_string(&report).ok();
        let _ = std::fs::remove_file(&report);
        let cwd = ended
            .map(|dir| sys::shell_directory(&dir))
            .filter(|dir| dir.is_absolute() && dir.is_dir())
            .unwrap_or_else(|| old_dir.clone());
        let moved = std::fs::canonicalize(&old_dir).ok().as_ref() != Some(&cwd);
        (cwd, moved)
    })
    .await;
    let moved = moved.then(|| Cwd::change(&cwd, cx.env.as_ref().map(|env| env.cwd.local())));
    let growing = kept.growing.is_some().then_some(OutputStream {
        session: cx.session,
        call: cx.call,
    });
    if done.lingering {
        leave(cx, done.process, kept.growing);
    }
    let ran = Ran {
        exit: done.status.code(),
        signal: sys::exit_signal(done.status),
        timed_out: (done.stopped == Some(Stop::TimedOut)).then_some(limit),
        aborted: done.stopped == Some(Stop::Aborted),
        flooded: (done.stopped == Some(Stop::Flooded)).then_some(OUTPUT_CAP),
        cwd,
        duration_ms: 0,
        output: text,
        truncated,
        full: kept.blob,
        growing,
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

/// Milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Where the whole output is kept, when the model saw less of it.
#[derive(Default)]
struct Kept {
    blob: Option<BlobRef>,
    /// The stream, while processes the command left still write it.
    growing: Option<Open>,
}

/// What the model is shown of the ended output, and where the whole is
/// kept when it saw less: sealed into a blob, or, while processes the
/// command left behind still write it, left growing in its stream. A store
/// that fails costs the whole output, not the command's result.
async fn keep(cx: &Cx, log: Open, output: Ended, lingering: bool) -> io::Result<(Rendered, Kept)> {
    let call = cx.call;
    rt::unblock_with(runtime(cx)?, rt::BlockingClass::Storage, 0, move || {
        let cut = output.cut();
        let media_type = output.media_type();
        let rendered = output.render();
        // What processes the command left still write is kept, whether
        // or not the model saw all of it so far.
        if lingering {
            let kept = Kept {
                growing: Some(log),
                ..Kept::default()
            };
            return Ok((rendered, kept));
        }
        if !cut {
            let _ = log.discard();
            return Ok((rendered, Kept::default()));
        }
        let Ok(blob) = log.seal(media_type, BlobOrigin::ToolOutput(call)) else {
            return Ok((rendered, Kept::default()));
        };
        let kept = Kept {
            blob: Some(blob),
            ..Kept::default()
        };
        Ok((rendered, kept))
    })
    .await
}

/// Starts the command with its output going straight to its stream, and
/// leaves it running: the session owns it from here, and the stream grows
/// until the group ends.
async fn background(cx: &Cx, spawn: Spawn, file: File, log: Open) -> io::Result<Ran> {
    let started = runtime(cx)?.now();
    let cwd = spawn.cwd.clone();
    let child = launch(cx, spawn, file).await?;
    let pgid = child.id();
    leave(cx, child, Some(log));
    Ok(Ran {
        cwd,
        duration_ms: duration_ms(started),
        background: Some(Job { pgid }),
        growing: Some(OutputStream {
            session: cx.session,
            call: cx.call,
        }),
        ..Ran::default()
    })
}

/// Leaves `process` to the session, which owns it until its group ends:
/// then `output`, the stream it writes, is sealed into the blob the end
/// holds, or dropped if empty. A group the session stops first leaves no
/// stream behind.
fn leave(cx: &Cx, process: OwnedProcess, output: Option<Open>) {
    let Some(output) = output else {
        return process.detach(cx);
    };
    let output = Written(Some(output));
    let call = cx.call;
    cx.own(
        process.process.holding(move || output.seal(call)),
        ext::Role::Work,
    );
}

/// A stream nothing but a group the session owns writes: sealed once the
/// group has ended, discarded when dropped unsealed.
struct Written(Option<Open>);

impl Written {
    /// The blob it seals into, of the media type its start shows; nothing
    /// for a stream that is empty or cannot be read.
    fn seal(mut self, call: protocol::CallId) -> Vec<Held> {
        let Some(open) = &self.0 else {
            return Vec::new();
        };
        let Ok(file) = File::open(open.path()) else {
            return Vec::new();
        };
        let mut head = Vec::with_capacity(blob::SNIFFED);
        let Ok(1..) = file.take(blob::SNIFFED as u64).read_to_end(&mut head) else {
            // Dropped unsealed, an empty stream is discarded.
            return Vec::new();
        };
        let Some(open) = self.0.take() else {
            return Vec::new();
        };
        (open.seal(blob::media_type(&head), BlobOrigin::ToolOutput(call)))
            .map(Held::Blob)
            .into_iter()
            .collect()
    }
}

impl Drop for Written {
    fn drop(&mut self) {
        if let Some(open) = self.0.take() {
            let _ = open.discard();
        }
    }
}

/// The group `handed`, from the build before, owned again and writing
/// `file`, its stream.
async fn adopt(cx: &Cx, claimed: ext::Native, file: File) -> io::Result<OwnedProcess> {
    cx.service::<dyn Processes>()
        .ok_or_else(|| io::Error::other("no process provider"))?
        .adopt(cx, claimed, file)
        .await
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

fn spawn_with<D: crate::dialect::Dialect>(
    facts: &sys::shell::ExecutionFacts,
    command: &str,
    dir: &Path,
    mut env: BTreeMap<String, String>,
    report: Option<&Path>,
) -> io::Result<Spawn> {
    // Noninteractive startup files must not rewrite the wrapper's contract.
    facts.apply_environment(&mut env);
    env.remove("BASH_ENV");
    env.remove("ENV");
    let mut argv = vec![facts.executable.to_string_lossy().into_owned()];
    argv.extend(D::arguments(command, dir, report)?);
    sys::shell::validate_argv(&argv)?;
    Ok(Spawn {
        argv,
        env,
        cwd: dir.to_path_buf(),
    })
}

/// The session's environment (the client's, or else the daemon's without
/// its secrets), quieted, and told which session and model it serves.
fn environment(
    cx: &Cx,
    daemon: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> BTreeMap<String, String> {
    let mut env = cx.env.as_ref().map_or_else(
        || {
            Environment::new(protocol::HostPath::default(), daemon, false)
                .vars
                .into_iter()
                .filter(|(name, _)| !Environment::is_secret(name))
                .collect()
        },
        |env| env.vars.clone(),
    );
    for (name, value) in QUIET {
        env.insert(name.to_owned(), value.to_owned());
    }
    env.insert("A_SESSION_ID".to_owned(), cx.session.to_string());
    env.insert("A_MODEL".to_owned(), cx.model.to_string());
    env
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
    /// The shell runs, until `deadline`, if it is to be stopped at one.
    Running { deadline: Option<Instant> },
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
                } else if deadline.is_some_and(|deadline| now >= deadline) {
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
            Self::Running { deadline } => deadline,
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
    output: Ended,
    process: OwnedProcess,
}

/// Whose command the supervisor follows.
enum Supervised<'a> {
    /// One this build launched, waited for until `limit`, and how it goes
    /// on in the next build should it go on in the background.
    Launched {
        limit: Duration,
        detached: &'a Detached,
    },
    /// One an earlier build launched, gone on in the background there.
    Resumed,
}

impl<'a> Supervised<'a> {
    /// The stream opened for following, with what of it is followed
    /// already, and how the run starts at `start`.
    async fn begin(
        self,
        rt: &dyn rt::Runtime,
        log: &Path,
        start: Instant,
    ) -> io::Result<(File, Vec<u8>, Output, Run, Option<&'a Detached>)> {
        let path = log.to_owned();
        let mut file =
            rt::unblock_with(rt, rt::BlockingClass::Storage, 0, move || File::open(path)).await?;
        let mut buf = vec![0; 64 * 1024];
        let mut output = Output::default();
        match self {
            Self::Launched { limit, detached } => {
                let run = Run::Running {
                    deadline: Some(start + limit),
                };
                Ok((file, buf, output, run, Some(detached)))
            }
            Self::Resumed => {
                // What the build before followed was reported there.
                loop {
                    let read;
                    (file, buf, output, _, read) =
                        follow_chunk(rt, file, buf, output, Live::default()).await?;
                    if read < READ {
                        break;
                    }
                }
                Ok((file, buf, output, Run::Running { deadline: None }, None))
            }
        }
    }
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
async fn supervise<D: crate::dialect::Dialect>(
    child: OwnedProcess,
    Followed { log, written }: Followed<'_>,
    supervised: Supervised<'_>,
    cx: &Cx,
    progress: &mut Partials<String>,
) -> io::Result<Finished> {
    let rt = runtime(cx)?;
    let start = rt.now();
    let (mut file, mut buf, mut output, mut run, detached) =
        supervised.begin(rt, log, start).await?;
    let mut live = Live::default();
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
            let ended = when(exited, async {
                child.ended().await;
                Ok(())
            });
            let changed = rt::or(written.readable_result(), ended);
            let exits = when(!exited, async { child.wait().await.map(|_| ()) });
            let cancelled = when(matches!(run, Run::Running { .. }), async {
                cx.cancel.wait().await;
                Ok(())
            });
            let sleep = when(wait.is_some(), async {
                rt.sleep(wait.unwrap_or_default()).await;
                Ok(())
            });
            rt::or(changed, rt::or(exits, rt::or(cancelled, sleep))).await?;
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
                progress.send(text).await;
                reported = now;
            }
        }
        // Past its timeout, or no longer waited on, a call that may go on
        // in the background does, and its timeout no longer stops it: a
        // forced steer and the timeout take the one path.
        if let (Run::Running { deadline: Some(at) }, Some(detach)) = (run, &cx.detach)
            && (detach.is_set() || now >= at)
        {
            detach.set();
            run = Run::Running { deadline: None };
            if let Some(detached) = detached {
                cx.resumable::<ShellTool<D>>(&child, detached);
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
                    output: output.finish(),
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
