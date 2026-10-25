//! Fetching a blob's bytes: what a client renders (an image, a tool's
//! whole output) by the [`BlobRef`](protocol::BlobRef) a session's log
//! holds. The bytes come as binary chunk frames over a connection of their
//! own, at the pace they are read: the daemon sends no faster than the
//! reader takes them, and hanging up cancels the fetch.

use std::io::{self, Read};
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;

use protocol::{BlobId, ClientMsg, DaemonMsg, Decode, Framed, Outcome, SessionId};

use crate::link::Stream;
use crate::query::{Canceller, ID};
use crate::{Connection, Daemon, Error};

/// A blob's bytes as they arrive, read in order. The read that ends them
/// fails if the daemon refused or stopped the fetch, or if what came is
/// not exactly what it says it sent.
pub struct Fetched {
    stream: Arc<dyn Stream>,
    buffered: Vec<u8>,
    blob: BlobId,
    /// The offset of the next byte due.
    next: u64,
    start: u64,
    /// The chunk being read, and how far.
    chunk: Vec<u8>,
    at: usize,
    done: bool,
}

impl Daemon {
    /// Fetches the bytes `range` of blob `blob` names, clamped to its end,
    /// as session `session`'s log references it: a blob no entry of the
    /// session references is refused. Over a connection of its own, sent
    /// right behind the hello.
    pub fn fetch(
        &self,
        session: SessionId,
        blob: BlobId,
        range: impl RangeBounds<u64>,
    ) -> Result<Fetched, Error> {
        let fetch = fetching(session, blob, &range);
        let conn = self.connect_as(None, &|| {}, &|_| Ok(vec![fetch.clone()]))?;
        Ok(Fetched::of(conn, blob, &range))
    }
}

impl Connection {
    /// [`Daemon::fetch`]; the connection is the fetch's from now on.
    pub fn fetch(
        mut self,
        session: SessionId,
        blob: BlobId,
        range: impl RangeBounds<u64>,
    ) -> io::Result<Fetched> {
        self.send(&fetching(session, blob, &range))?;
        Ok(Fetched::of(self, blob, &range))
    }
}

fn fetching(session: SessionId, blob: BlobId, range: &impl RangeBounds<u64>) -> ClientMsg {
    let offset = start(range);
    let len = match range.end_bound() {
        Bound::Included(&last) => Some(last.saturating_add(1).saturating_sub(offset)),
        Bound::Excluded(&end) => Some(end.saturating_sub(offset)),
        Bound::Unbounded => None,
    };
    ClientMsg::Fetch {
        id: ID,
        session,
        blob,
        offset,
        len,
    }
}

fn start(range: &impl RangeBounds<u64>) -> u64 {
    match range.start_bound() {
        Bound::Included(&first) => first,
        Bound::Excluded(&before) => before.saturating_add(1),
        Bound::Unbounded => 0,
    }
}

impl Fetched {
    fn of(conn: Connection, blob: BlobId, range: &impl RangeBounds<u64>) -> Self {
        Self {
            stream: Arc::from(conn.stream),
            buffered: conn.buffered,
            blob,
            next: start(range),
            start: start(range),
            chunk: Vec::new(),
            at: 0,
            done: false,
        }
    }

    pub fn canceller(&self) -> Canceller {
        Canceller(Arc::clone(&self.stream))
    }

    /// Takes frames up to the next chunk, into `chunk`, or the reply that
    /// ends the bytes: `false` then.
    async fn more(&mut self) -> io::Result<bool> {
        loop {
            let Some((len, framed)) = protocol::frame_in(&self.buffered)? else {
                if !self.stream.fill(&mut self.buffered).await? {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        Error::Closed.to_string(),
                    ));
                }
                continue;
            };
            let ended = match framed {
                Framed::Chunk(chunk) if chunk.blob == self.blob && chunk.offset == self.next => {
                    self.chunk.clear();
                    self.chunk.extend_from_slice(chunk.bytes);
                    self.at = 0;
                    self.next += chunk.bytes.len() as u64;
                    self.buffered.drain(..len);
                    return Ok(true);
                }
                Framed::Chunk(chunk) => Err(invalid(format!(
                    "bytes {} of blob {} where {} of {} were due",
                    chunk.offset, chunk.blob, self.next, self.blob
                ))),
                Framed::Json(json) => {
                    let DaemonMsg::Reply { id: ID, outcome } = DaemonMsg::from_json(json)? else {
                        self.buffered.drain(..len);
                        continue;
                    };
                    self.ended(outcome)
                }
            };
            self.buffered.drain(..len);
            self.done = true;
            return ended.map(|()| false);
        }
    }

    fn ended(&self, outcome: Outcome) -> io::Result<()> {
        match outcome {
            Outcome::Done { output } => {
                let sent: u64 = output.parse().map_err(invalid)?;
                if sent == self.next - self.start {
                    Ok(())
                } else {
                    Err(invalid(format!(
                        "{sent} bytes sent, {} received",
                        self.next - self.start
                    )))
                }
            }
            Outcome::Failed { error } => Err(io::Error::other(error)),
            Outcome::Interrupted { .. } => Err(io::Error::other("the fetch was cancelled")),
        }
    }
}

impl Read for Fetched {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.at == self.chunk.len()
            && (self.done || out.is_empty() || !rt::block_on(self.more())?)
        {
            return Ok(0);
        }
        let n = out.len().min(self.chunk.len() - self.at);
        out[..n].copy_from_slice(&self.chunk[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
