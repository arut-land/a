//! Blob bytes a client fetches ([`protocol::ClientMsg::Fetch`]): read
//! from the store a chunk at a time and sent as binary frames, each once
//! the client has room for it. A client fetches only what a session's log
//! references, through that session, so the bytes are as private as the
//! log; a hash alone opens nothing.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;

use protocol::{BlobId, Chunk, Frame, Json, SessionId};

use crate::Daemon;
use crate::outbox::Outbox;

/// The most bytes one chunk frame carries.
const CHUNK: usize = 256 << 10;

/// What a fetch asks for.
pub(crate) struct Fetch {
    pub session: SessionId,
    pub blob: BlobId,
    pub offset: u64,
    pub len: Option<u64>,
}

/// Sends the bytes `fetch` asks for, clamped to the blob's end, to
/// `outbox`; answers how many were sent.
pub(crate) async fn answer(
    daemon: Arc<Daemon>,
    outbox: Outbox,
    fetch: Fetch,
) -> Result<Json, String> {
    let Fetch {
        session,
        blob,
        offset,
        len,
    } = fetch;
    let slot = (daemon.slot(session)).ok_or_else(|| format!("no session {session}"))?;
    let live = daemon.live(&slot).await.map_err(|e| e.to_string())?;
    if !live.references(blob).await.map_err(|e| e.to_string())? {
        return Err(format!("session {session} references no blob {blob}"));
    }
    let rt = Arc::clone(&daemon.shared.rt);
    let store = Arc::clone(&daemon.shared.blobs);
    let opened = rt::unblock(&*rt, move || -> io::Result<_> {
        let mut reader = store.open(&blob)?;
        let end = reader.seek(SeekFrom::End(0))?;
        let start = reader.seek(SeekFrom::Start(offset.min(end)))?;
        Ok((reader, start, end))
    });
    let (mut reader, start, end) = opened.await.map_err(|e| format!("blob {blob}: {e}"))?;
    let stop = len.map_or(end, |len| start.saturating_add(len).min(end));
    let mut at = start;
    while at < stop {
        let want = usize::try_from(stop - at).map_or(CHUNK, |left| left.min(CHUNK));
        let read = rt::unblock_with(&*rt, rt::BlockingClass::Storage, want, move || {
            let mut bytes = vec![0; want];
            reader.read_exact(&mut bytes).map(|()| (reader, bytes))
        });
        let (back, bytes) = read.await.map_err(|e| format!("blob {blob}: {e}"))?;
        reader = back;
        let frame = Frame::chunk(Chunk {
            blob,
            offset: at,
            bytes: &bytes,
        })
        .map_err(|e| e.to_string())?;
        // Gone only with the connection, which cancels the fetch with it.
        if outbox.send_paced(frame).await.is_err() {
            return Err("the client hung up".into());
        }
        at += want as u64;
    }
    Json::encode(&(at - start)).map_err(|e| e.to_string())
}
