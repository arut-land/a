use std::time::Duration;

use crate::{BoxFuture, Runtime};

/// smol's global executor, blocking pool and timers.
#[derive(Clone, Copy, Debug, Default)]
pub struct Smol;

impl Runtime for Smol {
    fn spawn(&self, task: BoxFuture<'static, ()>) {
        smol::spawn(task).detach();
    }

    fn spawn_blocking(&self, work: Box<dyn FnOnce() + Send>) {
        smol::spawn(smol::unblock(work)).detach();
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            smol::Timer::after(duration).await;
        })
    }
}
