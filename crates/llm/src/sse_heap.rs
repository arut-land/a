//! Vec storage for SSE decoding without native receive buffers.

use std::io;

#[derive(Debug)]
pub(super) struct TransientBuffer {
    bytes: Vec<u8>,
    maximum: usize,
}
impl TransientBuffer {
    pub const fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::new(),
            maximum,
        }
    }
    pub fn extend_from_slice(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > self.maximum.saturating_sub(self.bytes.len()) {
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    pub fn push(&mut self, byte: u8) -> io::Result<()> {
        self.extend_from_slice(&[byte])
    }
    pub fn push_str(&mut self, text: &str) -> io::Result<()> {
        self.extend_from_slice(text.as_bytes())
    }
    pub fn pop(&mut self) -> Option<u8> {
        self.bytes.pop()
    }
    #[expect(
        clippy::unnecessary_wraps,
        reason = "matches the fallible native storage interface"
    )]
    pub fn clear(&mut self) -> io::Result<()> {
        self.bytes.clear();
        Ok(())
    }
    pub fn as_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.bytes)
    }
}
impl std::ops::Deref for TransientBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}
