//! Blob bytes a client fetches ([`protocol::ClientMsg::Fetch`]): read
//! from the store a chunk at a time, each straight into its frame, and
//! sent as binary frames within a small window of the fetch's own. A
//! client fetches only what a session's log references, through that
//! session, so the bytes are as private as the log; a hash alone opens
//! nothing.

use std::io::{self, Seek, SeekFrom};
use std::sync::Arc;

use protocol::{BlobId, Frame, Json, SessionId};

use crate::Daemon;
use crate::outbox::Outbox;
use crate::query::Ended;

/// The most bytes one chunk frame carries.
const CHUNK: usize = 256 << 10;

/// The most of a fetch queued for its client at once: four chunks. More
/// buys no throughput over a socket that drains as fast as the reader,
/// and costs memory for every slow one.
const WINDOW: usize = 4 * (CHUNK + 64);

/// Sends the bytes `len` (or all) from `offset` of blob `blob`, clamped to
/// its end, to `outbox`, as session `session`'s log references it;
/// answers how many were sent.
pub(crate) async fn answer(
    daemon: Arc<Daemon>,
    outbox: Outbox,
    session: SessionId,
    blob: BlobId,
    offset: u64,
    len: Option<u64>,
) -> Result<Json, Ended> {
    let failed = |e: io::Error| Ended::Failed(format!("blob {blob}: {e}"));
    // Held only for the check: a stream does not keep the session loaded.
    {
        let Some(slot) = daemon.slot(session) else {
            return Err(Ended::NotHeld);
        };
        let live = daemon.live(&slot).await.map_err(failed)?;
        if !live.references(blob).await.map_err(failed)? {
            return Err(Ended::NotHeld);
        }
    }
    let rt = Arc::clone(&daemon.shared.rt);
    let store = Arc::clone(&daemon.shared.blobs);
    let opened = rt::unblock(&*rt, move || -> io::Result<_> {
        let mut reader = store.open(&blob)?;
        let end = reader.seek(SeekFrom::End(0))?;
        let start = reader.seek(SeekFrom::Start(offset.min(end)))?;
        Ok((reader, start, end))
    });
    let (mut reader, start, end) = opened.await.map_err(failed)?;
    let stop = len.map_or(end, |len| start.saturating_add(len).min(end));
    let mut at = start;
    while at < stop {
        let want = usize::try_from(stop - at).map_or(CHUNK, |left| left.min(CHUNK));
        let read = rt::unblock_with(&*rt, rt::BlockingClass::Storage, want, move || {
            Frame::read_chunk(blob, at, want, &mut reader).map(|frame| (reader, frame))
        });
        let (back, frame) = read.await.map_err(failed)?;
        reader = back;
        // Gone only with the connection, which cancels the fetch with it.
        if outbox.send_within(frame, WINDOW).await.is_err() {
            return Err(Ended::Failed("the client hung up".into()));
        }
        at += want as u64;
    }
    Json::encode(&(at - start)).map_err(|e| Ended::Failed(e.to_string()))
}
