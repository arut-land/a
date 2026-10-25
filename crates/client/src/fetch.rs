//! Fetching a blob's bytes: what a client renders (an image, a tool's
//! whole output) by the [`BlobRef`](protocol::BlobRef) a session's log
//! holds. The bytes come as binary chunk frames over a connection of their
//! own, at the pace they are read: the daemon sends no faster than the
//! reader takes them, and hanging up cancels the fetch.

use std::fmt;
use std::io::{self, Read};
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;

use protocol::{BlobId, ClientMsg, DaemonMsg, Decode, Framed, Outcome, SessionId};

use crate::link::Stream;
use crate::query::{Canceller, ID};
use crate::{Connection, Daemon, Error};

/// A blob's bytes as they arrive, read in order. The read that ends them
/// fails if the daemon refused or stopped the fetch ([`Unfetched`]), or if
/// what came is not exactly what was asked for and it says it sent.
pub struct Fetched {
    stream: Arc<dyn Stream>,
    buffered: Vec<u8>,
    blob: BlobId,
    /// The offset of the next byte due.
    next: u64,
    start: u64,
    /// Where the bytes asked for end, if the ask said.
    end: Option<u64>,
    /// The chunk being read, and how far.
    chunk: Vec<u8>,
    at: usize,
    done: bool,
}

/// Why a fetch ended short of its bytes: the inner error of the
/// [`io::Error`] the read that ends them returns ([`Unfetched::of`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unfetched {
    /// No session it was fetched through references the blob
    /// ([`io::ErrorKind::NotFound`]).
    NotHeld,
    /// Its [`Canceller`] stopped it.
    Cancelled,
}

impl Unfetched {
    /// Why `error` ended a fetch, if a fetch's end it is.
    pub fn of(error: &io::Error) -> Option<Self> {
        error.get_ref()?.downcast_ref().copied()
    }
}

impl fmt::Display for Unfetched {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotHeld => "no session it was fetched through references the blob",
            Self::Cancelled => "the fetch was cancelled",
        })
    }
}

impl std::error::Error for Unfetched {}

impl From<Unfetched> for io::Error {
    fn from(why: Unfetched) -> Self {
        let kind = match why {
            Unfetched::NotHeld => io::ErrorKind::NotFound,
            Unfetched::Cancelled => io::ErrorKind::Other,
        };
        Self::new(kind, why)
    }
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
        rt::block_on(self.fetch_async(&crate::link::BLOCKING, session, blob, range))
    }

    /// Fetches bytes using the supplied runtime without blocking its worker.
    pub async fn fetch_async(
        &self,
        rt: &rt::Rt,
        session: SessionId,
        blob: BlobId,
        range: impl RangeBounds<u64>,
    ) -> Result<Fetched, Error> {
        let (offset, len) = bounds(&range);
        let fetch = message(session, blob, offset, len);
        let conn = self
            .connect_as_async(rt, None, &|| {}, &|_| Ok(vec![fetch.clone()]))
            .await?;
        Ok(Fetched::of(conn, blob, offset, len))
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
        let (offset, len) = bounds(&range);
        self.send(&message(session, blob, offset, len))?;
        Ok(Fetched::of(self, blob, offset, len))
    }
}

/// Where `range` starts, and how many bytes it holds if it ends.
fn bounds(range: &impl RangeBounds<u64>) -> (u64, Option<u64>) {
    let offset = match range.start_bound() {
        Bound::Included(&first) => first,
        Bound::Excluded(&before) => before.saturating_add(1),
        Bound::Unbounded => 0,
    };
    let len = match range.end_bound() {
        Bound::Included(&last) => Some(last.saturating_add(1).saturating_sub(offset)),
        Bound::Excluded(&end) => Some(end.saturating_sub(offset)),
        Bound::Unbounded => None,
    };
    (offset, len)
}

const fn message(session: SessionId, blob: BlobId, offset: u64, len: Option<u64>) -> ClientMsg {
    ClientMsg::Fetch {
        id: ID,
        session,
        blob,
        offset,
        len,
    }
}

impl Fetched {
    fn of(conn: Connection, blob: BlobId, offset: u64, len: Option<u64>) -> Self {
        Self {
            stream: Arc::from(conn.stream),
            buffered: conn.buffered,
            blob,
            next: offset,
            start: offset,
            end: len.map(|len| offset.saturating_add(len)),
            chunk: Vec::new(),
            at: 0,
            done: false,
        }
    }

    pub fn canceller(&self) -> Canceller {
        Canceller(Arc::clone(&self.stream))
    }

    /// Reads the next bytes, yielding while the daemon sends them.
    pub async fn read_async(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.at == self.chunk.len() && (self.done || out.is_empty() || !self.more().await?) {
            return Ok(0);
        }
        let n = out.len().min(self.chunk.len() - self.at);
        out[..n].copy_from_slice(&self.chunk[self.at..self.at + n]);
        self.at += n;
        Ok(n)
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
                // Matched by blob and offset: the next bytes due, and no
                // more than were asked for.
                Framed::Chunk(chunk)
                    if chunk.blob == self.blob
                        && chunk.offset == self.next
                        && self.end.is_none_or(|end| {
                            self.next.saturating_add(chunk.bytes.len() as u64) <= end
                        }) =>
                {
                    self.chunk.clear();
                    self.chunk.extend_from_slice(chunk.bytes);
                    self.at = 0;
                    self.next += chunk.bytes.len() as u64;
                    self.buffered.drain(..len);
                    return Ok(true);
                }
                Framed::Chunk(chunk) => Err(invalid(format!(
                    "{} bytes at {} of blob {} where {} of {} were due",
                    chunk.bytes.len(),
                    chunk.offset,
                    chunk.blob,
                    self.next,
                    self.blob
                ))),
                Framed::Json(json) => match DaemonMsg::from_json(json)? {
                    DaemonMsg::Reply { id: ID, outcome } => self.ended(outcome),
                    DaemonMsg::NotHeld { id: ID } => Err(Unfetched::NotHeld.into()),
                    _ => {
                        self.buffered.drain(..len);
                        continue;
                    }
                },
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
            Outcome::Interrupted { .. } => Err(Unfetched::Cancelled.into()),
        }
    }
}

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

impl Read for Fetched {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        rt::block_on(self.read_async(out))
    }
}
