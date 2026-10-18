use std::time::Duration;

use crate::{BoxFuture, Runtime};

/// A tokio runtime, by handle.
#[derive(Clone, Debug)]
pub struct Tokio(pub tokio::runtime::Handle);

impl Tokio {
    /// The runtime the calling thread is inside.
    pub fn current() -> Self {
        Self(tokio::runtime::Handle::current())
    }
}

impl Runtime for Tokio {
    fn spawn(&self, task: BoxFuture<'static, ()>) {
        drop(self.0.spawn(task));
    }

    fn spawn_blocking(&self, work: Box<dyn FnOnce() + Send>) {
        drop(self.0.spawn_blocking(work));
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        let _entered = self.0.enter();
        Box::pin(tokio::time::sleep(duration))
    }
}
