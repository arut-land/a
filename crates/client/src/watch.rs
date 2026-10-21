//! The session list over a cancellable runtime-owned connection.

use crate::{Attach, ClientHandle, ConnId, Daemon, Driver, Env, Error, EventKind, Io};
use protocol::SessionSummary;

/// Dropping the watch revokes connection attempts, reads and reconnect backoff.
pub struct SessionWatch {
    _handle: ClientHandle,
}

impl SessionWatch {
    pub fn start(
        rt: rt::Rt,
        daemon: Daemon,
        sink: impl Fn(Result<&[SessionSummary], &Error>) + Send + Sync + 'static,
    ) -> Self {
        let mut driver = Driver::new(move |_, event| match event.kind {
            EventKind::Msg(protocol::DaemonMsg::Sessions { sessions }) => sink(Ok(&sessions)),
            EventKind::Failed(error) => sink(Err(&error)),
            EventKind::Wedged(_) => sink(Err(&Error::NotResponding)),
            _ => {}
        });
        driver.watch = true;
        let handle = driver.start(rt, daemon);
        let _ = handle.io(Io::Connect {
            conn: ConnId(1),
            attach: Attach::new(protocol::Target::New, Env::Keep),
            restart: false,
        });
        Self { _handle: handle }
    }
}
