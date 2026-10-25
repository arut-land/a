//! Queries: what a client asks the daemon's extensions (the files of a
//! workspace, say) without touching any session's log. A query runs on a
//! connection of its own, so its answers never wait behind a session's
//! replay and hanging up cancels it.

use std::io;
use std::sync::Arc;

use protocol::{ClientMsg, DaemonMsg, Json, Outcome, Scope};

use crate::link::Stream;
use crate::{Connection, Daemon, Error};

/// The one query a connection of its own asks.
pub(crate) const ID: u64 = 1;

/// One of a query's answers, as the daemon sends them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// A streamed partial answer.
    Update(Json),
    /// How it ended; nothing follows.
    Reply(Outcome),
}

/// A query's answers as they come: its updates, then its reply. Dropping
/// it, and every [`Canceller`] of it, hangs up, which cancels the query.
pub struct Answers {
    stream: Arc<dyn Stream>,
    buffered: Vec<u8>,
    done: bool,
}

/// Cancels a query, or a fetch, from another thread than the one reading
/// its answers.
#[derive(Clone)]
pub struct Canceller(pub(crate) Arc<dyn Stream>);

impl Daemon {
    /// Asks query `service` about `scope`, over a connection of its own:
    /// sent right behind the hello.
    pub fn query(&self, service: &str, scope: Scope, body: Json) -> Result<Answers, Error> {
        let query = ClientMsg::Query {
            id: ID,
            service: service.to_owned(),
            scope,
            body,
        };
        let conn = self.connect_as(None, &|| {}, &|_| Ok(vec![query.clone()]))?;
        Ok(Answers::of(conn))
    }
}

impl Connection {
    /// Asks query `service` about `scope`; the connection is the query's
    /// from now on.
    pub fn query(mut self, service: &str, scope: Scope, body: Json) -> io::Result<Answers> {
        self.send(&ClientMsg::Query {
            id: ID,
            service: service.to_owned(),
            scope,
            body,
        })?;
        Ok(Answers::of(self))
    }
}

impl Answers {
    fn of(conn: Connection) -> Self {
        Self {
            stream: Arc::from(conn.stream),
            buffered: conn.buffered,
            done: false,
        }
    }

    pub fn canceller(&self) -> Canceller {
        Canceller(Arc::clone(&self.stream))
    }
}

impl Canceller {
    /// Asks the daemon to stop the query: its reply then says it was
    /// interrupted, unless it was on its way already.
    pub fn cancel(&self) -> io::Result<()> {
        rt::block_on(self.0.send(&ClientMsg::Cancel { id: ID }))
    }
}

impl Iterator for Answers {
    type Item = io::Result<Answer>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let answer = rt::block_on(crate::until(
            &*self.stream,
            &mut self.buffered,
            &mut |msg| match msg {
                DaemonMsg::Update { id: ID, body } => Some(Answer::Update(body)),
                DaemonMsg::Reply { id: ID, outcome } => Some(Answer::Reply(outcome)),
                _ => None,
            },
        ));
        self.done = !matches!(answer, Ok(Answer::Update(_)));
        Some(answer)
    }
}
