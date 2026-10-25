//! Queries: what a client asks the daemon's extensions (the files of a
//! workspace, say) without touching any session's log. A query runs on a
//! connection of its own, so its answers never wait behind a session's
//! replay and hanging up cancels it.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use protocol::{ClientMsg, DaemonMsg, Json, Outcome, Scope};

use crate::{Connection, Daemon, Error, Reader, Writer};

/// The one query a connection of its own asks.
const ID: u64 = 1;

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
    reader: Reader,
    writer: Arc<Mutex<Writer>>,
    done: bool,
}

/// Cancels a query from another thread than the one reading its answers.
#[derive(Clone)]
pub struct Canceller(Arc<Mutex<Writer>>);

impl Daemon {
    /// Asks query `service` about `scope`, over a connection of its own.
    pub fn query(&self, service: &str, scope: Scope, body: Json) -> Result<Answers, Error> {
        Ok(self.connect(&|| {})?.query(service, scope, body)?)
    }
}

impl Connection {
    /// Asks query `service` about `scope`; the connection is the query's
    /// from now on.
    pub fn query(mut self, service: &str, scope: Scope, body: Json) -> io::Result<Answers> {
        self.writer.send(&ClientMsg::Query {
            id: ID,
            service: service.to_owned(),
            scope,
            body,
        })?;
        Ok(Answers {
            reader: self.reader,
            writer: Arc::new(Mutex::new(self.writer)),
            done: false,
        })
    }
}

impl Answers {
    pub fn canceller(&self) -> Canceller {
        Canceller(Arc::clone(&self.writer))
    }
}

impl Canceller {
    /// Asks the daemon to stop the query: its reply then says it was
    /// interrupted, unless it was on its way already.
    pub fn cancel(&self) -> io::Result<()> {
        (self.0.lock().unwrap_or_else(PoisonError::into_inner)).send(&ClientMsg::Cancel { id: ID })
    }
}

impl Iterator for Answers {
    type Item = io::Result<Answer>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let answer = self.reader.until(|msg| match msg {
            DaemonMsg::Update { id: ID, body } => Some(Answer::Update(body)),
            DaemonMsg::Reply { id: ID, outcome } => Some(Answer::Reply(outcome)),
            _ => None,
        });
        self.done = !matches!(answer, Ok(Answer::Update(_)));
        Some(answer)
    }
}
