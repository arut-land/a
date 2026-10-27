//! Receive storage belongs to a partial frame or a batch of ready frames.

use std::io;

use crate::Decode;

/// Decodes received bytes without keeping a completed frame's accumulation.
/// Ready frames share their read batch until it is consumed; only a partial
/// frame carries its storage across reads. Dropping the reader preserves all
/// unread bytes, so a cancelled receive can be resumed.
#[derive(Debug)]
pub struct FrameReader<'a> {
    buffered: &'a mut Vec<u8>,
    accumulation: Accumulation,
}

#[derive(Debug)]
enum Accumulation {
    Empty,
    Partial,
}

impl<'a> FrameReader<'a> {
    /// Resumes decoding the unread bytes of a connection.
    pub fn new(buffered: &'a mut Vec<u8>) -> Self {
        Self {
            buffered,
            accumulation: Accumulation::Empty,
        }
    }

    /// The next ready message and its wire length, or `None` if more bytes
    /// are needed. A completed accumulation gives up its allocation even
    /// when the read also brought the start of another frame.
    pub fn decode<T: Decode>(&mut self) -> io::Result<Option<(usize, T)>> {
        let Some((len, body)) = crate::frame_in(self.buffered)? else {
            self.accumulation = if self.buffered.is_empty() {
                Accumulation::Empty
            } else {
                Accumulation::Partial
            };
            return Ok(None);
        };
        let msg = T::from_json(body.json()?)?;
        match self.accumulation {
            Accumulation::Partial => {
                // Returning a growth allocation at its received size keeps native
                // allocators from reusing oversized blocks for decoded messages.
                let completed = std::mem::take(self.buffered).into_boxed_slice();
                *self.buffered = completed[len..].to_vec();
            }
            Accumulation::Empty => {
                self.buffered.drain(..len);
                if self.buffered.is_empty() {
                    *self.buffered = Vec::new();
                }
            }
        }
        self.accumulation = Accumulation::Empty;
        Ok(Some((len, msg)))
    }

    /// Reserves scratch for a read, committing only the bytes actually received.
    /// Dropping the scratch without committing leaves unread bytes intact.
    pub fn read_buffer(&mut self, len: usize) -> ReadBuffer<'_> {
        let kept = self.buffered.len();
        self.buffered.resize(kept + len, 0);
        ReadBuffer {
            buffered: self.buffered,
            kept,
        }
    }

    /// Adds bytes from one read. Call [`Self::decode`] until it needs more
    /// bytes before reading again, to preserve ready batching.
    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        self.buffered.extend_from_slice(bytes);
    }

    /// Whether the peer can close cleanly between frames.
    pub fn is_empty(&self) -> bool {
        self.buffered.is_empty()
    }
}

/// Scratch appended to a reader, removed on cancellation or a failed read.
#[derive(Debug)]
pub struct ReadBuffer<'a> {
    buffered: &'a mut Vec<u8>,
    kept: usize,
}

impl ReadBuffer<'_> {
    /// Commits the bytes a successful read wrote into the scratch.
    pub fn filled(mut self, len: usize) {
        self.kept += len;
    }
}

impl AsMut<[u8]> for ReadBuffer<'_> {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.buffered[self.kept..]
    }
}

impl Drop for ReadBuffer<'_> {
    fn drop(&mut self) {
        self.buffered.truncate(self.kept);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientMsg, Frame};

    #[test]
    fn a_completed_accumulation_cannot_own_the_next_partial_frame() {
        let large = Frame::new(&crate::DaemonMsg::Error {
            message: "x".repeat(5 << 20),
        })
        .unwrap();
        let mut next = Vec::new();
        ClientMsg::Detach.write_frame(&mut next).unwrap();
        let mut buffered = Vec::new();
        {
            let mut reader = FrameReader::new(&mut buffered);
            reader.extend_from_slice(&large.as_bytes()[..4096]);
            assert!(reader.decode::<crate::DaemonMsg>().unwrap().is_none());
            let allocation = reader.buffered.as_ptr();
            assert!(reader.decode::<crate::DaemonMsg>().unwrap().is_none());
            assert_eq!(reader.buffered.as_ptr(), allocation);
            reader.extend_from_slice(&large.as_bytes()[4096..]);
            reader.extend_from_slice(&next[..3]);
            assert!(matches!(
                reader.decode().unwrap(),
                Some((_, crate::DaemonMsg::Error { message })) if message.len() == 5 << 20
            ));
            assert_eq!(reader.buffered.capacity(), 3);
            assert!(reader.decode::<ClientMsg>().unwrap().is_none());
        }
        let mut reader = FrameReader::new(&mut buffered);
        assert!(reader.decode::<ClientMsg>().unwrap().is_none());
        reader.extend_from_slice(&next[3..]);
        assert!(matches!(
            reader.decode().unwrap(),
            Some((_, ClientMsg::Detach))
        ));
        assert_eq!(reader.buffered.capacity(), 0);
    }

    #[test]
    fn pending_reads_cannot_add_bytes_to_a_partial_frame() {
        use std::io::Read as _;

        let mut wire = Vec::new();
        ClientMsg::Detach.write_frame(&mut wire).unwrap();
        let mut buffered = wire[..3].to_vec();
        let mut reader = FrameReader::new(&mut buffered);
        assert!(reader.decode::<ClientMsg>().unwrap().is_none());
        drop(reader.read_buffer(8192));
        assert_eq!(reader.buffered.as_slice(), &wire[..3]);
        let mut read = reader.read_buffer(8192);
        let n = (&wire[3..]).read(read.as_mut()).unwrap();
        read.filled(n);
        assert!(matches!(
            reader.decode().unwrap(),
            Some((_, ClientMsg::Detach))
        ));
        assert_eq!(reader.buffered.capacity(), 0);
    }

    #[test]
    fn ready_frames_share_their_batch_until_it_is_consumed() {
        let mut buffered = Vec::new();
        for _ in 0..64 {
            ClientMsg::Detach.write_frame(&mut buffered).unwrap();
        }
        let capacity = buffered.capacity();
        let mut reader = FrameReader::new(&mut buffered);
        for _ in 0..63 {
            assert!(matches!(
                reader.decode().unwrap(),
                Some((_, ClientMsg::Detach))
            ));
            assert_eq!(reader.buffered.capacity(), capacity);
        }
        assert!(matches!(
            reader.decode().unwrap(),
            Some((_, ClientMsg::Detach))
        ));
        assert_eq!(reader.buffered.capacity(), 0);
    }
}
