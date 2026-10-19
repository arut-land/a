use crate::{BoxFuture, Reactor, Runtime};

/// smol's global executor, blocking pool and timers, on async-io's reactor.
#[derive(Clone, Copy, Debug, Default)]
pub struct Smol;

impl Runtime for Smol {
    fn spawn(&self, task: BoxFuture<'static, ()>) {
        smol::spawn(task).detach();
    }

    fn spawn_blocking(&self, work: Box<dyn FnOnce() + Send>) {
        smol::spawn(smol::unblock(work)).detach();
    }

    fn reactor(&self) -> Reactor {
        Reactor::AsyncIo
    }
}
