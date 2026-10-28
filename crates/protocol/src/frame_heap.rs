//! Vec storage for wire consumers that do not use native receive buffers.

use std::io;

#[derive(Debug)]
pub struct TransientBuffer {
    bytes: Vec<u8>,
    start: usize,
    end: usize,
    maximum: usize,
}

impl TransientBuffer {
    pub const fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::new(),
            start: 0,
            end: 0,
            maximum,
        }
    }
    pub fn reserve_for(&mut self, len: usize) -> io::Result<()> {
        if len > self.maximum {
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        Ok(())
    }
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }
    #[expect(
        clippy::unnecessary_wraps,
        reason = "matches the fallible native storage interface"
    )]
    pub fn consume(&mut self, len: usize) -> io::Result<()> {
        self.start += len.min(self.end - self.start);
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        }
        Ok(())
    }
    pub fn read_buffer(&mut self, len: usize) -> io::Result<ReadBuffer<'_>> {
        let kept = self.end - self.start;
        let end = kept.checked_add(len).ok_or(io::ErrorKind::OutOfMemory)?;
        self.reserve_for(end)?;
        self.bytes.copy_within(self.start..self.end, 0);
        self.start = 0;
        self.end = end;
        if end > self.bytes.len() {
            self.bytes.resize(end, 0);
        }
        Ok(ReadBuffer {
            buffer: self,
            kept,
            end,
        })
    }
    pub fn extend_from_slice(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut read = self.read_buffer(bytes.len())?;
        read.as_mut().copy_from_slice(bytes);
        read.filled(bytes.len());
        Ok(())
    }
}
impl AsRef<[u8]> for TransientBuffer {
    fn as_ref(&self) -> &[u8] {
        &self.bytes[self.start..self.end]
    }
}
impl std::ops::Deref for TransientBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

/// Initialized scratch whose bytes become input only after a successful read.
#[derive(Debug)]
pub struct ReadBuffer<'a> {
    buffer: &'a mut TransientBuffer,
    kept: usize,
    end: usize,
}
impl ReadBuffer<'_> {
    pub fn filled(mut self, len: usize) {
        assert!(len <= self.end - self.kept);
        self.kept += len;
    }
}
impl AsMut<[u8]> for ReadBuffer<'_> {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.buffer.bytes[self.kept..self.end]
    }
}

impl Drop for ReadBuffer<'_> {
    fn drop(&mut self) {
        self.buffer.end = self.kept;
    }
}
