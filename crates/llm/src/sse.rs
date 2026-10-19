//! Incremental framing of a server-sent event stream. An event dispatches
//! at a blank line, so an event the body never terminates is never
//! delivered. No reply is resumed, so the `id` and `retry` fields are
//! ignored. Each event is handed over borrowed: its fields are assembled in
//! buffers the framer keeps, so a token costs no allocation here.

use crate::error::Error;
use crate::protocol::{Flow, Frame};

const BOM: [u8; 3] = [0xef, 0xbb, 0xbf];

#[derive(Debug, Default)]
pub struct SseFramer {
    buffer: Vec<u8>,
    bom_prefix: usize,
    bom_done: bool,
    /// The event being assembled from its field lines.
    event_type: String,
    data: String,
}

impl SseFramer {
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
        let chunk = self.strip_bom(chunk);
        self.buffer.extend_from_slice(chunk);
        let mut start = 0;
        let mut flow = Ok(Flow::More);
        while let Some((line_len, consumed)) = terminated_line(&self.buffer[start..]) {
            let line = String::from_utf8_lossy(&self.buffer[start..start + line_len]);
            start += consumed;
            if !line.is_empty() {
                line_field(&line, &mut self.event_type, &mut self.data);
                continue;
            }
            // A blank line dispatches the event, if it has data.
            if !self.data.is_empty() {
                self.data.pop();
                let event = if self.event_type.is_empty() {
                    "message"
                } else {
                    &self.event_type
                };
                flow = each(Frame {
                    event,
                    data: &self.data,
                });
                self.data.clear();
            }
            self.event_type.clear();
            if !matches!(flow, Ok(Flow::More)) {
                break;
            }
        }
        self.buffer.drain(..start);
        flow
    }

    fn strip_bom<'a>(&mut self, chunk: &'a [u8]) -> &'a [u8] {
        if self.bom_done {
            return chunk;
        }
        let mut rest = chunk;
        while let Some(&byte) = rest.first() {
            if BOM.get(self.bom_prefix) != Some(&byte) {
                // Not a BOM after all: replay the bytes that matched so far.
                let matched = std::mem::take(&mut self.bom_prefix);
                self.bom_done = true;
                self.buffer.extend_from_slice(&BOM[..matched]);
                return rest;
            }
            self.bom_prefix += 1;
            rest = &rest[1..];
            if self.bom_prefix == BOM.len() {
                self.bom_prefix = 0;
                self.bom_done = true;
                return rest;
            }
        }
        rest
    }
}

/// One non-blank field line into the event being assembled.
fn line_field(line: &str, event_type: &mut String, data: &mut String) {
    if line.starts_with(':') {
        return;
    }
    let (field, value) = match line.split_once(':') {
        Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
        None => (line, ""),
    };
    match field {
        "event" => value.clone_into(event_type),
        "data" => {
            data.push_str(value);
            data.push('\n');
        }
        _ => {}
    }
}

/// The length of the line `buffer` starts with, and the bytes it takes with
/// its terminator; `None` until the line is terminated.
fn terminated_line(buffer: &[u8]) -> Option<(usize, usize)> {
    let pos = buffer
        .iter()
        .position(|byte| *byte == b'\n' || *byte == b'\r')?;
    match buffer[pos] {
        b'\n' => Some((pos, pos + 1)),
        _ => match buffer.get(pos + 1) {
            Some(b'\n') => Some((pos, pos + 2)),
            Some(_) => Some((pos, pos + 1)),
            // The `\n` of a `\r\n` pair may still arrive.
            None => None,
        },
    }
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
