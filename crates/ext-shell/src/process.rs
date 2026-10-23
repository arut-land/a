//! Process ownership precedes every fallible readiness registration.

use std::io::{self, Read, Write};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ext::{
    BoxFuture, Cx, Flag, LaunchSpec, OwnedProcess, ProcessOutput, ProcessSignal, Processes,
    RunningProcess, SpawnError,
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
}

struct Reap(Option<sys::Child>, Arc<Live>);

impl Reap {
    fn complete(&mut self) {
        if let Some(mut child) = self.0.take() {
            *lock(&self.1.status) = Some(child.wait());
            self.1.exited.set();
        }
    }
}

impl Drop for Reap {
    fn drop(&mut self) {
        // Also covers failed registration and a runtime dropping its task.
        lock(&self.1.group).take();
        self.complete();
        lock(&self.1.captured).get_or_insert_with(|| Err(io::ErrorKind::Interrupted.into()));
        self.1.copied.set();
        lock(&self.1.permit).take();
        self.1.ended.set();
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
}

async fn launch(
    cx: &Cx,
    mut launch: LaunchSpec,
    watch: impl FnOnce(&mut sys::Child) -> io::Result<sys::ExitWatch>,
    register: impl FnOnce(
        &rt::Reactor,
        sys::ProcessPipe,
        u32,
    ) -> io::Result<rt::Async<sys::ProcessPipe>>,
) -> io::Result<OwnedProcess> {
    let permit = Permit::acquire()?;
    let rt = cx
        .service::<dyn rt::Runtime>()
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
                let live = Arc::new(Live {
                    id: child.id(),
                    group: Mutex::new(Some(group)),
                    status: Mutex::default(),
                    exited: Flag::default(),
                    copied: Flag::default(),
                    captured: Mutex::default(),
                    ended: Flag::default(),
                    permit: Mutex::new(Some(permit)),
                });
                Ok::<_, io::Error>((Reap(Some(child), live), pipes, capture))
            })());
        }),
    )
    .await?;
    let (mut owned, pipes, capture) = received
        .await
        .ok_or_else(|| io::Error::other("process launch stopped"))??;
    let live = Arc::clone(&owned.1);
    if cx.cancel.is_set() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let reactor = rt.reactor();
    let watch = match watch(owned.0.as_mut().expect("owned child")) {
        Ok(watch) => Some(rt::Async::for_reading(&reactor, watch)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Exit observation can precede waitable status; reaping must not block the reactor.
            owned = rt::unblock_with(rt, rt::BlockingClass::Storage, 0, move || {
                owned.complete();
                owned
            })
            .await;
            None
        }
        Err(e) => return Err(e),
    };
    let (input, output) = if let Some((input, output)) = pipes {
        (
            Some(Box::new(Pipe(register(&reactor, input, live.id)?)) as Box<dyn ext::ProcessPipe>),
            Some(Box::new(Pipe(rt::Async::read_pipe(&reactor, output)?))
                as Box<dyn ext::ProcessPipe>),
        )
    } else {
        (None, None)
    };
    rt.spawn(Box::pin(reap_output(
        owned,
        watch,
        reactor,
        capture,
        cx.service_arc::<dyn rt::Runtime>().expect("runtime"),
    )));
    Ok(OwnedProcess::new(live, input, output))
}

async fn reap_output(
    mut owned: Reap,
    watch: Option<rt::Async<sys::ExitWatch>>,
    reactor: rt::Reactor,
    capture: Option<sys::OutputCapture>,
    runtime: rt::Rt,
) {
    let capturing = Arc::clone(&owned.1);
    let copy_reactor = reactor.clone();
    let copy_runtime = Arc::clone(&runtime);
    let copying = async move {
        let result = if let Some(capture) = capture {
            let result = async {
                let input = rt::Async::read_pipe(&copy_reactor, capture.reader)?;
                capture_output(&*copy_runtime, input, capture.file, &capturing).await
            }
            .await;
            if result.is_err() {
                lock(&capturing.group).take();
            }
            result
        } else {
            Ok(())
        };
        *lock(&capturing.captured) = Some(result);
        capturing.copied.set();
    };
    let reaping = async {
        let result = async {
            if let Some(watch) = watch {
                watch.readable_result().await?;
            }
            owned.complete();
            descendants(&reactor, &owned.1, &*runtime).await
        }
        .await;
        if result.is_err() {
            lock(&owned.1.group).take();
        }
        result
    };
    // Process completion includes every byte already accepted from stdout.
    // Retain containment until both capture and descendant observation end.
    let _ = rt::zip(copying, reaping).await;
}

// One bounded chunk is admitted to disk at a time; a slow disk stops pipe reads.
async fn capture_output(
    runtime: &dyn rt::Runtime,
    input: rt::Async<sys::ProcessPipe>,
    mut file: std::fs::File,
    live: &Live,
) -> io::Result<()> {
    let mut buffer = vec![0; 64 << 10];
    let mut retained = 0u64;
    loop {
        let read = input.read_with(|mut input| input.read(&mut buffer)).await?;
        if read == 0 {
            return Ok(());
        }
        let keep = read.min(
            usize::try_from(crate::output::OUTPUT_CAP - retained).expect("capture cap fits usize"),
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
        if retained == crate::output::OUTPUT_CAP {
            live.signal(ProcessSignal::Kill)?;
            return Ok(());
        }
    }
}

// A leader can exit first. Failed descendant watches must revoke containment.
async fn descendants(
    reactor: &rt::Reactor,
    live: &Arc<Live>,
    runtime: &dyn rt::Runtime,
) -> io::Result<()> {
    loop {
        let owner = Arc::clone(live);
        let members = rt::unblock_with(runtime, rt::BlockingClass::Storage, 0, move || {
            lock(&owner.group)
                .as_ref()
                .map_or_else(|| Ok(Vec::new()), sys::ProcessLease::members)
        })
        .await?;
        let mut watches = Vec::new();
        for pid in members {
            match sys::exit_watch(pid).and_then(|w| rt::Async::for_reading(reactor, w)) {
                Ok(watch) => watches.push(watch),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
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
        // Stdin holds the child until the test releases status acquisition.
        let mut launching = std::pin::pin!(launch(
            &cx,
            spec(&cx, "read -r line"),
            |child| {
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
        assert!(sys::process_started(id).is_some());
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
}
