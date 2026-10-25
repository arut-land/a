//! Process ownership precedes every fallible readiness registration.
//!
//! A captured group can be handed to the daemon's next build
//! ([`RunningProcess::hand_over`]): its supervision stops between two
//! chunks of output, leaving its leader, lease and pipe parked. Committed,
//! they go to `sys`, which passes them on live, and the next build adopts
//! the group ([`Processes::adopt`]) and supervises it from there; called
//! off, supervision goes on here as before.

use std::fs::File;
use std::io::{self, Read, Write};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use ext::{
    BoxFuture, Cx, Flag, LaunchSpec, OwnedProcess, Parting, ProcessOutput, ProcessSignal,
    Processes, RunningProcess, SpawnError,
};

pub(crate) struct Provider;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

const MAX_PROCESSES: usize = 32;
static PROCESSES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct Permit;
impl Permit {
    fn acquire() -> io::Result<Self> {
        use std::sync::atomic::Ordering::AcqRel;
        PROCESSES
            .try_update(AcqRel, std::sync::atomic::Ordering::Acquire, |n| {
                (n < MAX_PROCESSES).then_some(n + 1)
            })
            .map(|_| Self)
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "process capacity exhausted"))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        PROCESSES.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

struct Live {
    id: u32,
    group: Mutex<Option<sys::ProcessLease>>,
    status: Mutex<Option<io::Result<ExitStatus>>>,
    exited: Flag,
    copied: Flag,
    captured: Mutex<Option<io::Result<()>>>,
    ended: Flag,
    permit: Mutex<Option<Permit>>,
    runtime: rt::Rt,
    /// The supervision running now, which a handover stops.
    watch: Mutex<Watch>,
    /// What supervision leaves of a group it stopped for a handover.
    parted: Mutex<Parted>,
    this: Weak<Self>,
}

/// One supervision of a group: `handing` stops it between two chunks of
/// output, and `parked` says it stopped.
#[derive(Clone, Default)]
struct Watch {
    handing: Flag,
    parked: Flag,
}

#[derive(Default)]
struct Parted {
    child: Option<sys::Child>,
    lease: Option<sys::ProcessLease>,
    /// The pipe, unless nothing of the group writes it any more, and the
    /// stream its output goes to.
    output: Option<(sys::ProcessPipe, File)>,
}

impl Live {
    fn new(
        child: &sys::Child,
        group: sys::ProcessLease,
        permit: Permit,
        runtime: rt::Rt,
    ) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            id: child.id(),
            group: Mutex::new(Some(group)),
            status: Mutex::default(),
            exited: Flag::default(),
            copied: Flag::default(),
            captured: Mutex::default(),
            ended: Flag::default(),
            permit: Mutex::new(Some(permit)),
            runtime,
            watch: Mutex::default(),
            parted: Mutex::default(),
            this: this.clone(),
        })
    }

    /// What a handover stopped, for it to give up or call off.
    async fn stopped(&self) -> io::Result<Stopped> {
        let watch = lock(&self.watch).clone();
        watch.handing.set();
        let parked = rt::or(
            async {
                watch.parked.wait().await;
                true
            },
            async {
                self.ended.wait().await;
                false
            },
        )
        .await;
        let live = self
            .this
            .upgrade()
            .filter(|_| parked)
            .ok_or_else(|| io::Error::other("the group ended before it was handed over"))?;
        let Parted {
            child,
            lease,
            output,
        } = std::mem::take(&mut *lock(&self.parted));
        let mut stopped = Stopped {
            live,
            child,
            lease,
            output: None,
        };
        if let Some((pipe, mut file)) = output {
            let (pipe, unread) = sys::handover::park(pipe)?;
            stopped.output = Some((pipe, file.try_clone()?));
            file.write_all(&unread)?;
        }
        Ok(stopped)
    }
}

impl RunningProcess for Live {
    fn id(&self) -> u32 {
        self.id
    }
    fn signal(&self, signal: ProcessSignal) -> io::Result<()> {
        if let Some(group) = &*lock(&self.group) {
            group.signal(match signal {
                ProcessSignal::Terminate => sys::Signal::Terminate,
                ProcessSignal::Kill => sys::Signal::Kill,
            })?;
        }
        Ok(())
    }
    fn alive(&self) -> bool {
        lock(&self.group).is_some()
    }
    fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        lock(&self.status).as_ref().map_or(Ok(None), |status| {
            status
                .as_ref()
                .copied()
                .map(Some)
                .map_err(|e| io::Error::new(e.kind(), e.to_string()))
        })
    }
    fn wait(&self) -> BoxFuture<'_, io::Result<ExitStatus>> {
        Box::pin(async {
            self.exited.wait().await;
            self.try_wait()?
                .ok_or_else(|| io::Error::other("missing process status"))
        })
    }
    fn output_drained(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async {
            self.copied.wait().await;
            lock(&self.captured)
                .as_ref()
                .expect("capture completed")
                .as_ref()
                .copied()
                .map_err(|e| io::Error::new(e.kind(), e.to_string()))
        })
    }
    fn ended(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.ended.wait())
    }
    fn hand_over(&self) -> BoxFuture<'_, io::Result<Box<dyn Parting>>> {
        Box::pin(async { Ok(Box::new(self.stopped().await?) as Box<dyn Parting>) })
    }
}

/// A group whose supervision a handover stopped: given up as it commits,
/// supervised again from where it was as it drops.
struct Stopped {
    live: Arc<Live>,
    child: Option<sys::Child>,
    lease: Option<sys::ProcessLease>,
    output: Option<(sys::handover::Parked, File)>,
}

impl Parting for Stopped {
    fn commit(mut self: Box<Self>) -> ext::Native {
        let (Some(child), Some(lease)) = (self.child.take(), self.lease.take()) else {
            unreachable!("a stopped group has its leader and lease")
        };
        let given = sys::handover::give(child, lease, self.output.take().map(|(pipe, _)| pipe));
        // It ends here: what waits on it here waits for good.
        let live = &self.live;
        lock(&live.captured).get_or_insert(Ok(()));
        live.copied.set();
        lock(&live.permit).take();
        live.ended.set();
        ext::Native::new(given)
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        let (Some(child), Some(lease)) = (self.child.take(), self.lease.take()) else {
            return;
        };
        let live = Arc::clone(&self.live);
        *lock(&live.group) = Some(lease);
        *lock(&live.watch) = Watch::default();
        let capture = self.output.take().and_then(|(pipe, file)| {
            let reader = pipe.unpark().ok()?;
            Some(sys::OutputCapture { reader, file })
        });
        let runtime = Arc::clone(&live.runtime);
        runtime.spawn(Box::pin(async move {
            let rt = Arc::clone(&live.runtime);
            let _ = watch_over(&rt, Reap::new(child, live), capture, |child| {
                child.exit_watch()
            })
            .await;
        }));
    }
}

struct Reap {
    child: Option<sys::Child>,
    live: Arc<Live>,
    watch: Watch,
    /// Stopped for a handover: nothing of the group is finished.
    parked: bool,
}

impl Reap {
    fn new(child: sys::Child, live: Arc<Live>) -> Self {
        let watch = lock(&live.watch).clone();
        Self {
            child: Some(child),
            live,
            watch,
            parked: false,
        }
    }

    /// Waits for the leader's status, once; the child is kept, as a
    /// handover gives it up with the group even once its leader exited.
    fn complete(&mut self) {
        if let Some(child) = self.child.as_mut()
            && !self.live.exited.is_set()
        {
            *lock(&self.live.status) = Some(child.wait());
            self.live.exited.set();
        }
    }
}

impl Drop for Reap {
    fn drop(&mut self) {
        if self.parked {
            self.watch.parked.set();
            return;
        }
        // Also covers failed registration and a runtime dropping its task.
        lock(&self.live.group).take();
        self.complete();
        lock(&self.live.captured).get_or_insert_with(|| Err(io::ErrorKind::Interrupted.into()));
        self.live.copied.set();
        lock(&self.live.permit).take();
        self.live.ended.set();
    }
}

impl Processes for Provider {
    fn spawn<'a>(
        &'a self,
        cx: &'a Cx,
        spec: LaunchSpec,
    ) -> BoxFuture<'a, Result<OwnedProcess, SpawnError>> {
        Box::pin(launch(
            cx,
            spec,
            |child| child.exit_watch(),
            |reactor, input, _| rt::Async::new(reactor, input),
        ))
    }

    fn adopt<'a>(
        &'a self,
        cx: &'a Cx,
        claimed: ext::Native,
        output: File,
    ) -> BoxFuture<'a, Result<OwnedProcess, SpawnError>> {
        Box::pin(async move {
            let claimed: sys::handover::Claimed = claimed
                .take()
                .ok_or_else(|| io::Error::other("not a group this build claimed"))?;
            let permit = Permit::acquire()?;
            let rt = cx
                .service_arc::<dyn rt::Runtime>()
                .ok_or_else(|| io::Error::other("no runtime"))?;
            let sys::ContainedChild {
                child,
                group,
                capture,
            } = claimed.adopt(output);
            let live = Live::new(&child, group, permit, Arc::clone(&rt));
            supervise(
                &rt,
                (Reap::new(child, live), None, capture),
                |child| child.exit_watch(),
                |reactor, input, _| rt::Async::new(reactor, input),
            )
            .await
        })
    }
}

fn launch(
    cx: &Cx,
    launch: LaunchSpec,
    watch: impl FnOnce(&mut sys::Child) -> io::Result<sys::ExitWatch>,
    register: impl FnOnce(
        &rt::Reactor,
        sys::ProcessPipe,
        u32,
    ) -> io::Result<rt::Async<sys::ProcessPipe>>,
) -> impl Future<Output = io::Result<OwnedProcess>> {
    let admitted = match Permit::acquire() {
        Ok(permit) => Ok((permit, launch)),
        Err(error) => {
            drop(launch);
            Err(error)
        }
    };
    launch_admitted(cx, admitted, watch, register)
}

async fn launch_admitted(
    cx: &Cx,
    admitted: io::Result<(Permit, LaunchSpec)>,
    watch: impl FnOnce(&mut sys::Child) -> io::Result<sys::ExitWatch>,
    register: impl FnOnce(
        &rt::Reactor,
        sys::ProcessPipe,
        u32,
    ) -> io::Result<rt::Async<sys::ProcessPipe>>,
) -> io::Result<OwnedProcess> {
    let (permit, mut launch) = admitted?;
    let rt = cx
        .service_arc::<dyn rt::Runtime>()
        .ok_or_else(|| io::Error::other("no runtime"))?;
    if cx.cancel.is_set() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    cx.spawn_hooks(&mut launch.process)
        .map_err(io::Error::other)?;
    if launch.process.argv.is_empty() {
        return Err(io::Error::other("a spawn hook left no program to run"));
    }
    let cancel = cx.cancel.clone();
    let (sent, received) = rt::oneshot::channel();
    let runtime = Arc::clone(&rt);
    rt.spawn_blocking(
        rt::BlockingClass::Storage,
        0,
        Box::new(move || {
            sent.send((|| {
                if cancel.is_set() {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let mut cmd = std::process::Command::new(&launch.process.argv[0]);
                cmd.args(&launch.process.argv[1..])
                    .env_clear()
                    .envs(&launch.process.env)
                    .current_dir(&launch.process.cwd);
                let (contained, pipes) = match launch.output {
                    ProcessOutput::File(file) => (sys::spawn_group_captured(&mut cmd, file)?, None),
                    ProcessOutput::Stdio => {
                        let (child, input, output) = sys::spawn_stdio(&mut cmd)?;
                        (child, Some((input, output)))
                    }
                };
                let sys::ContainedChild {
                    child,
                    group,
                    capture,
                } = contained;
                let live = Live::new(&child, group, permit, runtime);
                Ok::<_, io::Error>((Reap::new(child, live), pipes, capture))
            })());
        }),
    )
    .await?;
    let launched = received
        .await
        .ok_or_else(|| io::Error::other("process launch stopped"))??;
    if cx.cancel.is_set() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    supervise(&rt, launched, watch, register).await
}

/// The pipes of a process with stdio of its own.
type Stdio = Option<(sys::ProcessPipe, sys::ProcessPipe)>;

/// Watches a contained process, launched or adopted, to its end, copying
/// what it captures into its stream.
async fn supervise(
    rt: &rt::Rt,
    (owned, pipes, capture): (Reap, Stdio, Option<sys::OutputCapture>),
    watch: impl FnOnce(&mut sys::Child) -> io::Result<sys::ExitWatch>,
    register: impl FnOnce(
        &rt::Reactor,
        sys::ProcessPipe,
        u32,
    ) -> io::Result<rt::Async<sys::ProcessPipe>>,
) -> io::Result<OwnedProcess> {
    let live = Arc::clone(&owned.live);
    let reactor = rt.reactor();
    let (input, output) = if let Some((input, output)) = pipes {
        (
            Some(Box::new(Pipe(register(&reactor, input, live.id)?)) as Box<dyn ext::ProcessPipe>),
            Some(Box::new(Pipe(rt::Async::read_pipe(&reactor, output)?))
                as Box<dyn ext::ProcessPipe>),
        )
    } else {
        (None, None)
    };
    watch_over(rt, owned, capture, watch).await?;
    Ok(OwnedProcess::new(live, input, output))
}

/// Starts the task that watches `owned` to its end: launched, adopted, or
/// stopped for a handover that was called off.
async fn watch_over(
    rt: &rt::Rt,
    mut owned: Reap,
    capture: Option<sys::OutputCapture>,
    watch: impl FnOnce(&mut sys::Child) -> io::Result<sys::ExitWatch>,
) -> io::Result<()> {
    let reactor = rt.reactor();
    let watch = match watch(owned.child.as_mut().expect("owned child")) {
        Ok(watch) => Some(rt::Async::for_reading(&reactor, watch)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Exit observation can precede waitable status; reaping must not block the reactor.
            owned = rt::unblock_with(&**rt, rt::BlockingClass::Storage, 0, move || {
                owned.complete();
                owned
            })
            .await;
            None
        }
        Err(e) => return Err(e),
    };
    rt.spawn(Box::pin(reap_output(
        owned,
        watch,
        reactor,
        capture,
        Arc::clone(rt),
    )));
    Ok(())
}

async fn reap_output(
    mut owned: Reap,
    watch: Option<rt::Async<sys::ExitWatch>>,
    reactor: rt::Reactor,
    capture: Option<sys::OutputCapture>,
    runtime: rt::Rt,
) {
    let scope_done = Flag::default();
    let capture_done = scope_done.clone();
    let capturing = Arc::clone(&owned.live);
    let handing = owned.watch.handing.clone();
    let copy_reactor = reactor.clone();
    let copy_runtime = Arc::clone(&runtime);
    let copying = async move {
        let result = if let Some(capture) = capture {
            let result = async {
                let input = rt::Async::read_pipe(&copy_reactor, capture.reader)?;
                capture_output(
                    &*copy_runtime,
                    input,
                    capture.file,
                    &capturing,
                    (&capture_done, &handing),
                )
                .await
            }
            .await;
            match result {
                Ok(Captured::Handing(pipe, file)) => {
                    // Not finished: the handover takes it on, or this
                    // build again.
                    lock(&capturing.parted).output = Some((pipe, file));
                    return;
                }
                Ok(Captured::Ended) => Ok(()),
                Err(error) => {
                    lock(&capturing.group).take();
                    Err(error)
                }
            }
        } else {
            Ok(())
        };
        *lock(&capturing.captured) = Some(result);
        capturing.copied.set();
    };
    let handing = owned.watch.handing.clone();
    let reaping = async {
        let ended = rt::or(
            async {
                let result = async {
                    if let Some(watch) = watch {
                        watch.readable_result().await?;
                    }
                    owned.complete();
                    descendants(&reactor, &owned.live, &*runtime).await
                }
                .await;
                Some(result)
            },
            async {
                handing.wait().await;
                None
            },
        )
        .await;
        let Some(result) = ended else {
            // Left for a handover, which gives it up or supervises it again.
            let mut parted = lock(&owned.live.parted);
            parted.child = owned.child.take();
            parted.lease = lock(&owned.live.group).take();
            owned.parked = true;
            return Ok(());
        };
        lock(&owned.live.group).take();
        // Nothing of the group is left to hand over: the last owner reaps it.
        owned.child.take();
        scope_done.set();
        result
    };
    // Escaped writers cannot extend ownership. Finish accepted writes, then
    // give buffered pipe completions a bounded byte budget and grace period.
    let _ = rt::zip(copying, reaping).await;
}

/// How capture ended.
enum Captured {
    /// Nothing more is kept: nothing of the group writes the pipe any
    /// more, the group ended, or it wrote past the cap.
    Ended,
    /// Stopped between two chunks for a handover: the pipe, and the stream.
    Handing(sys::ProcessPipe, File),
}

// One bounded chunk is admitted to disk at a time; a slow disk stops pipe reads.
async fn capture_output(
    runtime: &dyn rt::Runtime,
    input: rt::Async<sys::ProcessPipe>,
    mut file: File,
    live: &Live,
    (scope_done, handing): (&Flag, &Flag),
) -> io::Result<Captured> {
    let mut buffer = vec![0; 64 << 10];
    // An adopted group's stream holds what the build before kept of it.
    let mut retained = file.metadata().map_or(0, |meta| meta.len());
    let mut drain_left = 1 << 20;
    let mut drain_until = None;
    loop {
        if retained >= crate::output::OUTPUT_CAP {
            live.signal(ProcessSignal::Kill)?;
            return Ok(Captured::Ended);
        }
        let read = if scope_done.is_set() {
            if drain_left == 0 {
                return Ok(Captured::Ended);
            }
            let deadline = *drain_until
                .get_or_insert_with(|| runtime.now() + std::time::Duration::from_millis(100));
            let remaining = deadline.saturating_duration_since(runtime.now());
            if remaining.is_zero() {
                return Ok(Captured::Ended);
            }
            let size = buffer.len().min(drain_left);
            match rt::or(
                async {
                    input
                        .read_with(|mut input| input.read(&mut buffer[..size]))
                        .await
                        .map(Some)
                },
                async {
                    runtime.sleep(remaining).await;
                    Ok(None)
                },
            )
            .await?
            {
                Some(read) => {
                    drain_left -= read;
                    read
                }
                None => return Ok(Captured::Ended),
            }
        } else {
            match rt::or(
                async {
                    scope_done.wait().await;
                    Ok(Next::Ended)
                },
                rt::or(
                    async {
                        handing.wait().await;
                        Ok(Next::Handing)
                    },
                    async {
                        input
                            .read_with(|mut input| input.read(&mut buffer))
                            .await
                            .map(Next::Read)
                    },
                ),
            )
            .await?
            {
                Next::Read(read) => read,
                Next::Ended => continue,
                Next::Handing => return Ok(Captured::Handing(input.into_inner()?, file)),
            }
        };
        if read == 0 {
            return Ok(Captured::Ended);
        }
        let keep = read.min(
            usize::try_from(crate::output::OUTPUT_CAP.saturating_sub(retained))
                .expect("capture cap fits usize"),
        );
        let (state, result) = rt::unblock_with(
            runtime,
            rt::BlockingClass::Storage,
            buffer.len(),
            move || {
                let result = file
                    .write_all(&buffer[..keep])
                    .and_then(|()| sys::fs::output_written(&file));
                ((file, buffer), result)
            },
        )
        .await;
        (file, buffer) = state;
        result?;
        retained += keep as u64;
    }
}

/// What woke capture.
enum Next {
    Read(usize),
    Ended,
    Handing,
}

// A leader can exit first. Failed descendant watches must revoke containment.
async fn descendants(
    reactor: &rt::Reactor,
    live: &Arc<Live>,
    runtime: &dyn rt::Runtime,
) -> io::Result<()> {
    loop {
        let owner = Arc::clone(live);
        let watches = rt::unblock_with(runtime, rt::BlockingClass::Storage, 0, move || {
            lock(&owner.group)
                .as_ref()
                .map_or_else(|| Ok(Vec::new()), sys::ProcessLease::exit_watches)
        })
        .await?;
        let watches = watches
            .into_iter()
            .map(|watch| rt::Async::for_reading(reactor, watch))
            .collect::<io::Result<Vec<_>>>()?;
        if watches.is_empty() {
            return Ok(());
        }
        let mut exits: Vec<_> = watches
            .iter()
            .map(|w| Box::pin(w.readable_result()))
            .collect();
        std::future::poll_fn(|cx| {
            for exit in &mut exits {
                if let std::task::Poll::Ready(result) = exit.as_mut().poll(cx) {
                    return std::task::Poll::Ready(result);
                }
            }
            std::task::Poll::Pending
        })
        .await?;
    }
}

struct Pipe(rt::Async<sys::ProcessPipe>);
impl ext::ProcessPipe for Pipe {
    fn read<'a>(&'a self, bytes: &'a mut [u8]) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(self.0.read_with(move |mut pipe| pipe.read(bytes)))
    }
    fn write<'a>(&'a self, bytes: &'a [u8]) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(self.0.write_with(move |mut pipe| pipe.write(bytes)))
    }
    fn try_write(&self, bytes: &[u8]) -> io::Result<usize> {
        self.0.get_ref().write(bytes)
    }
    fn flush(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(self.0.flush())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ext::host::Registry;
    use ext::testing::Rig;
    use ext::{Extension, Registrar, SpawnSpec};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Waker};

    fn spec(cx: &Cx, command: &str) -> LaunchSpec {
        LaunchSpec {
            process: SpawnSpec {
                argv: vec![
                    sys::find_bash(std::env::var("PATH").ok().as_deref()).unwrap(),
                    "--noprofile".into(),
                    "--norc".into(),
                    "-c".into(),
                    command.into(),
                ],
                cwd: cx.dir(),
                env: std::env::vars().collect(),
            },
            output: ProcessOutput::Stdio,
        }
    }

    #[test]
    #[cfg(unix)]
    fn escaped_stdout_cannot_hold_a_timeout_or_its_capacity() {
        escaped_stdout_releases_capacity(false);
    }

    #[test]
    #[cfg(unix)]
    fn escaped_stdout_cannot_hold_cancellation_or_its_capacity() {
        escaped_stdout_releases_capacity(true);
    }

    #[cfg(unix)]
    fn escaped_holder(directory: &std::path::Path) -> std::path::PathBuf {
        let source = directory.join("holder.c");
        let executable = directory.join("holder");
        std::fs::write(
            &source,
            r#"
#include <unistd.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <string.h>
int main(int argc, char **argv) {
    (void)argc;
    if (setsid() < 0) return 1;
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un address = {0};
    address.sun_family = AF_UNIX;
    if (strlen(argv[1]) >= sizeof(address.sun_path)) return 2;
    strcpy(address.sun_path, argv[1]);
    if (connect(fd, (struct sockaddr *)&address, sizeof(address))) return 3;
    if (write(1, "escaped\n", 8) != 8) return 4;
    if (write(fd, "r", 1) != 1) return 5;
    char release;
    read(fd, &release, 1);
    return 0;
}
"#,
        )
        .unwrap();
        assert!(
            std::process::Command::new("cc")
                .arg(&source)
                .arg("-o")
                .arg(&executable)
                .status()
                .unwrap()
                .success()
        );
        executable
    }

    #[cfg(unix)]
    fn escaped_stdout_releases_capacity(cancel: bool) {
        use std::os::unix::net::UnixListener;
        use std::time::{Duration, Instant};
        let rig = Rig::new(
            if cancel {
                "escaped-capture-cancel"
            } else {
                "escaped-capture-timeout"
            },
            Registry::default().with(&crate::Shell::default()).unwrap(),
        );
        let executable = escaped_holder(&rig.dir);
        {
            let rig = Rig::new(
                if cancel {
                    "escaped-cancel"
                } else {
                    "escaped-timeout"
                },
                Registry::default().with(&crate::Shell::default()).unwrap(),
            );
            let socket = rig.dir.join("release.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let cx = rig.cx();
            // Leave only one slot for the command whose cleanup is under test.
            let queued: Vec<_> = (0..MAX_PROCESSES - 1)
                .map(|_| Provider.spawn(&cx, spec(&cx, "true")))
                .collect();
            let command = format!("'{}' '{}' & wait", executable.display(), socket.display());
            std::thread::scope(|scope| {
                let (sent, received) = std::sync::mpsc::sync_channel(1);
                let rig = &rig;
                scope.spawn(move || {
                    sent.send(rig.done(
                        "bash",
                        &serde_json::json!({"command": command, "timeout": 1}),
                    ))
                    .unwrap();
                });
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut holder = loop {
                    match listener.accept() {
                        Ok((holder, _)) => break holder,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "holder did not escape");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                holder.set_nonblocking(false).unwrap();
                holder
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                holder.read_exact(&mut [0]).unwrap();
                if cancel {
                    rig.cancel.set();
                }
                let result = received.recv_timeout(Duration::from_secs(4));
                // Release our escaped process even if the regression fails.
                if let Ok(result) = &result {
                    assert_eq!(result["aborted"].as_bool().unwrap_or(false), cancel);
                    if !cancel {
                        assert_eq!(result["timed_out"], 1);
                    }
                    assert!(result["output"].as_str().unwrap().contains("escaped"));
                    let replacement = Rig::new("escaped-replacement", Registry::default());
                    let replacement_cx = replacement.cx();
                    let released_by = Instant::now() + Duration::from_secs(2);
                    let next = loop {
                        match rt::block_on(
                            Provider.spawn(&replacement_cx, spec(&replacement_cx, "true")),
                        ) {
                            Ok(next) => break next,
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                assert!(
                                    Instant::now() < released_by,
                                    "completion retained its permit"
                                );
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(error) => panic!("{error}"),
                        }
                    };
                    rt::block_on(next.ended());
                }
                holder.shutdown(std::net::Shutdown::Write).unwrap();
                assert_eq!(holder.read(&mut [0]).unwrap(), 0);
                drop(holder);
                assert!(
                    result.is_ok(),
                    "escaped stdout retained completion: {result:?}"
                );
            });
            drop(queued);
        }
    }

    #[test]
    fn outstanding_launches_are_bounded_and_completion_releases_capacity() {
        let rig = Rig::new("process-capacity", Registry::default());
        let cx = rig.cx();
        rt::block_on(async {
            let mut queued: Vec<_> = (0..MAX_PROCESSES)
                .map(|_| Provider.spawn(&cx, spec(&cx, "read -r line")))
                .collect();
            assert_eq!(
                Provider
                    .spawn(&cx, spec(&cx, "read -r line"))
                    .await
                    .err()
                    .expect("queued work exceeded capacity")
                    .kind(),
                io::ErrorKind::WouldBlock
            );
            drop(queued.pop());
            queued.push(Provider.spawn(&cx, spec(&cx, "read -r line")));
            let mut children = Vec::new();
            for launching in queued {
                children.push(launching.await.unwrap());
            }
            let refused = Provider
                .spawn(&cx, spec(&cx, "read -r line"))
                .await
                .err()
                .expect("capacity was exceeded");
            assert_eq!(refused.kind(), io::ErrorKind::WouldBlock);
            let child = children.pop().unwrap();
            let completion = Arc::clone(&child.process);
            drop(child);
            completion.ended().await;
            let replacement = Provider
                .spawn(&cx, spec(&cx, "read -r line"))
                .await
                .unwrap();
            drop(replacement);
            for child in children {
                let completion = Arc::clone(&child.process);
                drop(child);
                completion.ended().await;
            }
        });
    }

    #[test]
    fn failed_pipe_registration_still_kills_and_reaps_the_child() {
        let rig = Rig::new("registration-failure", Registry::default());
        let cx = rig.cx();
        let mut pid = None;
        let result = rt::block_on(launch(
            &cx,
            spec(&cx, "read -r line"),
            |child| child.exit_watch(),
            |_, _, id| {
                pid = Some(id);
                Err(io::Error::other("registration refused"))
            },
        ));
        assert!(result.is_err());
        assert!(
            sys::process_started(pid.unwrap()).is_none(),
            "child was not reaped"
        );
    }

    #[test]
    fn cancellation_reaps_each_owned_group_once() {
        let rig = Rig::new("process-cancellation", Registry::default());
        let cx = rig.cx();
        rt::block_on(async {
            let first = Provider
                .spawn(&cx, spec(&cx, "read -r line"))
                .await
                .unwrap();
            let second = Provider
                .spawn(&cx, spec(&cx, "read -r line"))
                .await
                .unwrap();
            let handles = [Arc::clone(&first.process), Arc::clone(&second.process)];
            drop(first);
            drop(second);
            for process in handles {
                process.ended().await;
                let status = process.wait().await.unwrap();
                // Multiple consumers read the cached result, never wait/reap again.
                assert_eq!(process.wait().await.unwrap(), status);
                assert!(!process.alive());
                assert!(sys::process_started(process.id()).is_none());
            }
        });
    }

    #[derive(Default)]
    struct Manual {
        tasks: Mutex<Vec<rt::BoxFuture<'static, ()>>>,
        blocking: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
        defer: AtomicBool,
    }
    impl rt::Runtime for Manual {
        fn spawn(&self, task: rt::BoxFuture<'static, ()>) {
            lock(&self.tasks).push(task);
        }
        fn spawn_blocking(
            &self,
            _: rt::BlockingClass,
            _: usize,
            work: Box<dyn FnOnce() + Send>,
        ) -> rt::BoxFuture<'static, io::Result<()>> {
            if self.defer.load(Ordering::Acquire) {
                lock(&self.blocking).push(work);
            } else {
                work();
            }
            Box::pin(std::future::ready(Ok(())))
        }
        fn spawn_flush(
            &self,
            _: Box<dyn FnOnce() + Send>,
        ) -> rt::BoxFuture<'static, io::Result<()>> {
            unreachable!()
        }
        fn reactor(&self) -> rt::Reactor {
            rt::Reactor::AsyncIo
        }
        fn sleep(&self, _: std::time::Duration) -> rt::BoxFuture<'static, ()> {
            panic!("quiet process requested a timer")
        }
    }
    struct ManualService(Arc<Manual>);
    impl Extension for ManualService {
        const NAME: &'static str = "manual";
        const VERSION: u32 = 1;
        fn contribute(&self, r: &mut Registrar) {
            r.provide::<dyn rt::Runtime>(self.0.clone());
        }
    }

    #[test]
    fn an_exited_watch_waits_for_reapable_status() {
        let runtime = Arc::new(Manual::default());
        let rig = Rig::new(
            "watch-before-status",
            Registry::default()
                .with(&ManualService(Arc::clone(&runtime)))
                .unwrap(),
        );
        let cx = rig.cx();
        let pid = Mutex::new(None);
        // Check identity while stdin holds the child alive; exit can hide it before reaping.
        let mut launching = std::pin::pin!(launch(
            &cx,
            spec(&cx, "read -r line"),
            |child| {
                assert!(sys::process_started(child.id()).is_some());
                *lock(&pid) = Some(child.id());
                child.kill().unwrap();
                runtime.defer.store(true, Ordering::Release);
                Err(io::ErrorKind::NotFound.into())
            },
            |reactor, input, _| rt::Async::new(reactor, input),
        ));
        let mut context = Context::from_waker(Waker::noop());
        assert!(launching.as_mut().poll(&mut context).is_pending());
        let id = lock(&pid).unwrap();
        let reaping = lock(&runtime.blocking)
            .pop()
            .expect("status acquisition was admitted");
        reaping();
        let Poll::Ready(Ok(process)) = launching.as_mut().poll(&mut context) else {
            panic!("the reaped child must still be a successful launch");
        };
        runtime.defer.store(false, Ordering::Release);
        rt::block_on(lock(&runtime.tasks).pop().unwrap());
        let status = rt::block_on(process.process.wait()).unwrap();
        assert!(!status.success());
        assert_eq!(process.process.try_wait().unwrap(), Some(status));
        assert_eq!(rt::block_on(process.process.wait()).unwrap(), status);
        assert!(sys::process_started(id).is_none());
        assert!(lock(&runtime.blocking).is_empty());
    }

    #[test]
    fn quiet_processes_do_not_require_polling() {
        let runtime = Arc::new(Manual::default());
        let rig = Rig::new(
            "quiet-process",
            Registry::default()
                .with(&ManualService(Arc::clone(&runtime)))
                .unwrap(),
        );
        let cx = rig.cx();
        let process = rt::block_on(Provider.spawn(&cx, spec(&cx, "read -r line"))).unwrap();
        let control = Arc::clone(&process.process);
        let mut task = lock(&runtime.tasks).pop().unwrap();
        let mut context = Context::from_waker(Waker::noop());
        assert!(task.as_mut().poll(&mut context).is_pending());
        assert!(control.try_wait().unwrap().is_none());
        assert!(
            lock(&runtime.tasks).is_empty(),
            "a quiet child scheduled extra work"
        );
        drop(process);
        rt::block_on(task);
        assert!(rt::block_on(control.wait()).is_ok());
        assert!(!control.alive());
        let mut pid = None;
        let failed = rt::block_on(launch(
            &cx,
            spec(&cx, "read -r line"),
            |child| {
                pid = Some(child.id());
                Err(io::ErrorKind::Unsupported.into())
            },
            |reactor, input, _| rt::Async::new(reactor, input),
        ));
        assert!(
            failed.is_err(),
            "unsupported watches must clean up instead of scheduling polls"
        );
        assert!(lock(&runtime.tasks).is_empty());
        assert!(sys::process_started(pid.unwrap()).is_none());
    }

    /// What `given` names, claimed as the next image of this process
    /// claims it from the record it reads.
    #[cfg(unix)]
    fn claimed(given: ext::Native) -> ext::Native {
        use std::os::fd::IntoRawFd;
        let given: sys::handover::Given = given.take().unwrap();
        // As an exec passes them on: owned by nothing until claimed.
        for handle in given.handles {
            let _ = handle.into_raw_fd();
        }
        let owner = serde_json::Value::Null;
        let bytes = sys::handover::encode([(&given.group, &owner)]);
        let path = std::env::temp_dir().join(format!("a-claim-{}", given.group.pid));
        std::fs::write(&path, bytes).unwrap();
        let record = File::open(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        let number = sys::InheritedNumber::from(record.into_raw_fd());
        ext::Native::new(sys::handover::read_record(number).unwrap().pop().unwrap().0)
    }

    /// Handed over, a group runs on: the owner it leaves neither signals
    /// nor reaps it, and the owner that adopts it (here the same process,
    /// as the next image of one is) gets its exit status and what it writes
    /// after, its stream whole.
    #[test]
    #[cfg(unix)]
    fn a_group_handed_over_runs_on_unsignalled_and_is_adopted_whole() {
        let rig = Rig::new(
            "handed-group",
            Registry::default().with(&crate::Shell::default()).unwrap(),
        );
        let cx = rig.cx();
        let path = rig.dir.join("stream");
        let file = File::create(&path).unwrap();
        rt::block_on(async {
            let launch = LaunchSpec {
                output: ProcessOutput::File(file),
                ..spec(&cx, "echo before; sleep 1; echo after; exit 7")
            };
            let process = Provider.spawn(&cx, launch).await.unwrap();
            let given = process.hand_over().await.unwrap().commit();
            drop(process);
            let output = File::options().append(true).open(&path).unwrap();
            let adopted = Provider.adopt(&cx, claimed(given), output).await.unwrap();
            assert_eq!(adopted.wait().await.unwrap().code(), Some(7));
            adopted.output_drained().await.unwrap();
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before\nafter\n");
    }

    /// A handover called off leaves the group as it was: supervised here,
    /// its status and its whole output still to come.
    #[test]
    fn a_handover_called_off_leaves_the_group_supervised() {
        let rig = Rig::new(
            "called-off",
            Registry::default().with(&crate::Shell::default()).unwrap(),
        );
        let cx = rig.cx();
        let path = rig.dir.join("stream");
        let file = File::create(&path).unwrap();
        rt::block_on(async {
            let launch = LaunchSpec {
                output: ProcessOutput::File(file),
                ..spec(&cx, "echo before; sleep 1; echo after; exit 7")
            };
            let process = Provider.spawn(&cx, launch).await.unwrap();
            drop(process.hand_over().await.unwrap());
            assert_eq!(process.wait().await.unwrap().code(), Some(7));
            process.output_drained().await.unwrap();
            process.ended().await;
        });
        let output = std::fs::read_to_string(&path).unwrap();
        assert_eq!(output.replace('\r', ""), "before\nafter\n");
    }
}
