//! One receive buffer, with reusable native pages for large frame accumulations.

use std::io;

use crate::Decode;

/// Ready frames share one read batch. Large accumulations retain their address
/// range after completion, while their backing pages return to the OS.
#[derive(Debug)]
pub struct FrameReader {
    buffered: sys::TransientBuffer,
    end: Option<usize>,
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameReader {
    pub const fn new() -> Self {
        Self {
            buffered: sys::TransientBuffer::new(crate::MAX_FRAME as usize + 4 + (64 << 10)),
            end: None,
        }
    }

    /// Whether the next frame needs more bytes. Its length is validated once,
    /// when the complete prefix arrives, before selecting storage for its body.
    #[inline]
    pub fn needs_more(&mut self) -> io::Result<bool> {
        Ok(self.complete_len()?.is_none())
    }

    #[inline]
    fn complete_len(&mut self) -> io::Result<Option<usize>> {
        let end = if let Some(end) = self.end {
            end
        } else {
            let Some(&[a, b, c, d]) = self.buffered.get(..4) else {
                return Ok(None);
            };
            crate::length(u32::from_le_bytes([a, b, c, d]))? + 4
        };
        if self.buffered.len() < end {
            self.end = Some(end);
            self.buffered.reserve_for(end)?;
            return Ok(None);
        }
        Ok(Some(end))
    }

    /// The next complete message and its wire length, if more bytes are not needed.
    #[inline]
    pub fn decode<T: Decode>(&mut self) -> io::Result<Option<(usize, T)>> {
        let Some(end) = self.complete_len()? else {
            return Ok(None);
        };
        let body = crate::Framed::read(
            crate::FrameKind::of(self.buffered[4])?,
            &self.buffered[5..end],
        )?;
        let msg = T::from_json(body.json()?)?;
        self.consume(end)?;
        Ok(Some((end, msg)))
    }
    #[inline]
    pub fn read_buffer(&mut self, len: usize) -> io::Result<sys::ReadBuffer<'_>> {
        self.buffered.read_buffer(len)
    }
    pub fn extend_from_slice(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.buffered.extend_from_slice(bytes)
    }
    #[inline]
    pub fn consume(&mut self, len: usize) -> io::Result<()> {
        self.buffered.consume(len)?;
        self.end = None;
        Ok(())
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.buffered.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buffered.is_empty()
    }
    #[inline]
    pub fn capacity(&self) -> usize {
        self.buffered.capacity()
    }
}
impl AsRef<[u8]> for FrameReader {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.buffered.as_ref()
    }
}
impl std::ops::Deref for FrameReader {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientMsg, DaemonMsg, Frame};

    #[test]
    fn large_frames_reuse_their_address_range_without_retaining_contents() {
        let large = Frame::new(&DaemonMsg::Error {
            message: "x".repeat(5 << 20),
        })
        .unwrap();
        let mut next = Vec::new();
        ClientMsg::Detach.write_frame(&mut next).unwrap();
        let mut reader = FrameReader::new();
        let mut address = None;
        for _ in 0..3 {
            reader.extend_from_slice(&large.as_bytes()[..4096]).unwrap();
            assert!(reader.decode::<DaemonMsg>().unwrap().is_none());
            if let Some(address) = address {
                assert_eq!(reader.as_ref().as_ptr(), address);
            }
            address = Some(reader.as_ref().as_ptr());
            reader.extend_from_slice(&large.as_bytes()[4096..]).unwrap();
            reader.extend_from_slice(&next[..3]).unwrap();
            assert!(
                matches!(reader.decode().unwrap(), Some((_,DaemonMsg::Error { message })) if message.len()==5<<20)
            );
            assert_eq!(reader.as_ref(), &next[..3]);
            assert!(reader.decode::<ClientMsg>().unwrap().is_none());
            reader.extend_from_slice(&next[3..]).unwrap();
            assert!(matches!(
                reader.decode().unwrap(),
                Some((_, ClientMsg::Detach))
            ));
            assert!(reader.is_empty());
        }
    }

    #[test]
    fn a_maximum_frame_keeps_room_for_the_next_read_batch() {
        use std::io::Read;

        let large = Frame::new(&DaemonMsg::Error {
            message: "x".repeat(crate::MAX_FRAME as usize - 128),
        })
        .unwrap();
        let split = large.as_bytes().len() - 1;
        let mut reader = FrameReader::new();
        reader
            .extend_from_slice(&large.as_bytes()[..split])
            .unwrap();
        assert!(reader.decode::<DaemonMsg>().unwrap().is_none());
        let mut remaining = large.as_bytes()[split..].to_vec();
        ClientMsg::Detach.write_frame(&mut remaining).unwrap();
        let mut read = reader.read_buffer(64 << 10).unwrap();
        let n = remaining.as_slice().read(read.as_mut()).unwrap();
        read.filled(n);
        assert!(matches!(
            reader.decode().unwrap(),
            Some((_, DaemonMsg::Error { .. }))
        ));
        assert!(matches!(
            reader.decode().unwrap(),
            Some((_, ClientMsg::Detach))
        ));
        assert!(reader.is_empty());
    }

    #[test]
    fn pending_reads_cannot_add_bytes_to_a_partial_frame() {
        use std::io::Read as _;
        let mut wire = Vec::new();
        ClientMsg::Detach.write_frame(&mut wire).unwrap();
        let mut reader = FrameReader::new();
        reader.extend_from_slice(&wire[..3]).unwrap();
        drop(reader.read_buffer(8192).unwrap());
        assert_eq!(reader.as_ref(), &wire[..3]);
        let mut read = reader.read_buffer(8192).unwrap();
        let n = (&wire[3..]).read(read.as_mut()).unwrap();
        read.filled(n);
        assert!(matches!(
            reader.decode().unwrap(),
            Some((_, ClientMsg::Detach))
        ));
    }

    #[test]
    fn small_frames_keep_their_read_batch_and_allocation() {
        let mut wire = Vec::new();
        for _ in 0..64 {
            ClientMsg::Detach.write_frame(&mut wire).unwrap();
        }
        let mut reader = FrameReader::new();
        reader.extend_from_slice(&wire).unwrap();
        let capacity = reader.capacity();
        for _ in 0..64 {
            assert!(matches!(
                reader.decode().unwrap(),
                Some((_, ClientMsg::Detach))
            ));
            assert_eq!(reader.capacity(), capacity);
        }
    }
}
