//! Incremental framing of a server-sent event stream. An event dispatches
//! at a blank line, so an event the body never terminates is never
//! delivered. No reply is resumed, so the `id` and `retry` fields are
//! ignored. Each event is handed over borrowed: its fields are assembled in
//! buffers the framer keeps, so a token costs no allocation here.

use crate::error::Error;
use crate::protocol::{Flow, Frame};

#[cfg(feature = "native-buffer")]
use sys::TransientBuffer;
#[cfg(not(feature = "native-buffer"))]
#[path = "sse_heap.rs"]
mod heap;
#[cfg(not(feature = "native-buffer"))]
use heap::TransientBuffer;

/// Per-reply ceilings, shared by framing and provider decoding.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DecodeLimits {
    pub frame_bytes: usize,
    pub reply_bytes: usize,
    pub argument_bytes: usize,
    pub parts: usize,
    pub depth: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            frame_bytes: 8 * 1024 * 1024,
            reply_bytes: 64 * 1024 * 1024,
            argument_bytes: 32 * 1024 * 1024,
            parts: 4096,
            depth: 64,
        }
    }
}

const BOM: [u8; 3] = [0xef, 0xbb, 0xbf];

#[derive(Debug)]
pub struct SseFramer {
    buffer: TransientBuffer,
    limits: DecodeLimits,
    frame_bytes: usize,
    bom_prefix: usize,
    bom_done: bool,
    /// The event being assembled from its field lines.
    event_type: TransientBuffer,
    data: TransientBuffer,
}

impl Default for SseFramer {
    fn default() -> Self {
        Self::with_limits(DecodeLimits::default())
    }
}

impl SseFramer {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Hands each event `chunk` completes to `each`, until it says the
    /// reply is done or fails.
    pub fn push(
        &mut self,
        chunk: &[u8],
        mut each: impl FnMut(Frame<'_>) -> Result<Flow, Error>,
    ) -> Result<Flow, Error> {
        let mut chunk = self.strip_bom(chunk)?;
        while let Some((&byte, rest)) = chunk.split_first() {
            if self.buffer.last() == Some(&b'\r') {
                if byte == b'\n' {
                    self.charge(1)?;
                }
                self.buffer.pop();
                let flow = self.line(&mut each)?;
                if flow == Flow::Done {
                    return Ok(flow);
                }
                if byte == b'\n' {
                    chunk = rest;
                    continue;
                }
            }
            let end = chunk
                .iter()
                .position(|&b| b == b'\r' || b == b'\n')
                .unwrap_or(chunk.len());
            if end > 0 {
                self.charge(end)?;
                self.buffer.extend_from_slice(&chunk[..end])?;
                chunk = &chunk[end..];
                continue;
            }
            self.charge(1)?;
            if byte == b'\n' {
                let flow = self.line(&mut each)?;
                if flow == Flow::Done {
                    return Ok(flow);
                }
            } else {
                self.buffer.push(byte)?;
            }
            chunk = rest;
        }
        Ok(Flow::More)
    }

    fn charge(&mut self, bytes: usize) -> Result<(), Error> {
        if bytes > self.limits.frame_bytes.saturating_sub(self.frame_bytes) {
            return Err(Error::limit("SSE frame bytes"));
        }
        self.frame_bytes += bytes;
        Ok(())
    }

    pub(crate) fn with_limits(limits: DecodeLimits) -> Self {
        Self {
            buffer: TransientBuffer::new(limits.frame_bytes),
            event_type: TransientBuffer::new(limits.frame_bytes),
            data: TransientBuffer::new(limits.frame_bytes),
            limits,
            frame_bytes: 0,
            bom_prefix: 0,
            bom_done: false,
        }
    }

    fn line(
        &mut self,
        each: &mut impl FnMut(Frame<'_>) -> Result<Flow, Error>,
    ) -> Result<Flow, Error> {
        if !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&self.buffer);
            let size = self
                .event_type
                .len()
                .saturating_add(self.data.len())
                .saturating_add(line.len())
                .saturating_add(1);
            if size > self.limits.frame_bytes {
                return Err(Error::limit("SSE frame bytes"));
            }
            line_field(&line, &mut self.event_type, &mut self.data)?;
            self.buffer.clear()?;
            return Ok(Flow::More);
        }
        self.frame_bytes = 0;
        let flow = if self.data.is_empty() {
            Ok(Flow::More)
        } else {
            self.data.pop();
            each(Frame {
                event: if self.event_type.is_empty() {
                    "message"
                } else {
                    self.event_type
                        .as_str()
                        .expect("event fields are typed text")
                },
                data: self.data.as_str().expect("data fields are typed text"),
            })
        };
        self.data.clear()?;
        self.event_type.clear()?;
        flow
    }

    fn strip_bom<'a>(&mut self, chunk: &'a [u8]) -> Result<&'a [u8], Error> {
        if self.bom_done {
            return Ok(chunk);
        }
        let mut rest = chunk;
        while let Some(&byte) = rest.first() {
            if BOM.get(self.bom_prefix) != Some(&byte) {
                // Not a BOM after all: replay the bytes that matched so far.
                let matched = std::mem::take(&mut self.bom_prefix);
                self.bom_done = true;
                self.charge(matched)?;
                self.buffer.extend_from_slice(&BOM[..matched])?;
                return Ok(rest);
            }
            self.bom_prefix += 1;
            rest = &rest[1..];
            if self.bom_prefix == BOM.len() {
                self.bom_prefix = 0;
                self.bom_done = true;
                return Ok(rest);
            }
        }
        Ok(rest)
    }
}

/// One non-blank field line into the event being assembled.
fn line_field(
    line: &str,
    event_type: &mut TransientBuffer,
    data: &mut TransientBuffer,
) -> Result<(), Error> {
    if line.starts_with(':') {
        return Ok(());
    }
    let (field, value) = match line.split_once(':') {
        Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
        None => (line, ""),
    };
    match field {
        "event" => {
            event_type.clear()?;
            event_type.push_str(value)?;
        }
        "data" => {
            data.push_str(value)?;
            data.push(b'\n')?;
        }
        _ => {}
    }
    Ok(())
}

/// Every event `chunk` completes, as `(event, data)`.
#[cfg(test)]
pub(crate) fn events(framer: &mut SseFramer, chunk: &[u8]) -> Vec<(String, String)> {
    let mut events = Vec::new();
    let _ = framer.push(chunk, |frame| {
        events.push((frame.event.to_owned(), frame.data.to_owned()));
        Ok(Flow::More)
    });
    events
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod storage_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn completed_large_events_leave_no_unconsumed_data() {
        let mut wire = Vec::new();
        write!(&mut wire, "data: ").unwrap();
        serde_json::to_writer(&mut wire, &"x".repeat(5 << 20)).unwrap();
        writeln!(&mut wire, "\n").unwrap();
        let mut framer = SseFramer::default();
        for _ in 0..3 {
            let mut seen = 0;
            for chunk in wire.chunks(16 << 10) {
                framer
                    .push(chunk, |frame| {
                        seen += 1;
                        assert_eq!(frame.data.len(), (5 << 20) + 2);
                        Ok(Flow::More)
                    })
                    .unwrap();
            }
            assert_eq!(seen, 1);
            assert!(framer.buffer.is_empty());
            assert!(framer.data.is_empty());
            #[cfg(feature = "native-buffer")]
            {
                assert!(framer.buffer.capacity() < wire.len());
                assert!(framer.data.capacity() < wire.len() / 2);
            }
        }
    }
}
