use crate::{BoxFuture, Reactor, Runtime};

/// A tokio runtime, by handle, on its own reactor and timers.
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

    fn reactor(&self) -> Reactor {
        Reactor::Tokio(self.0.clone())
    }
}
