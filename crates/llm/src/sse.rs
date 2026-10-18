//! Incremental framing of response bytes. SSE dispatches an event at a
//! blank line, so an event the body never terminates is never delivered;
//! NDJSON flushes a final unterminated line. No reply is resumed, so SSE's
//! `id` and `retry` fields are ignored.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Cow<'static, str>,
    pub data: String,
}

use std::borrow::Cow;

const BOM: [u8; 3] = [0xef, 0xbb, 0xbf];

#[derive(Debug, Default)]
pub struct SseFramer {
    buffer: Vec<u8>,
    bom_prefix: usize,
    bom_done: bool,
    fields: Fields,
}

/// The event being assembled from its field lines.
#[derive(Debug, Default)]
struct Fields {
    ready: Vec<SseEvent>,
    event_type: String,
    data: String,
}

impl SseFramer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> std::vec::Drain<'_, SseEvent> {
        let chunk = self.strip_bom(chunk);
        self.buffer.extend_from_slice(chunk);
        let mut start = 0;
        while let Some((line_len, consumed)) = terminated_line(&self.buffer[start..]) {
            let line = String::from_utf8_lossy(&self.buffer[start..start + line_len]);
            self.fields.line(&line);
            start += consumed;
        }
        self.buffer.drain(..start);
        self.fields.ready.drain(..)
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

impl Fields {
    fn line(&mut self, line: &str) {
        if line.is_empty() {
            self.dispatch();
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => value.clone_into(&mut self.event_type),
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
            }
            _ => {}
        }
    }

    fn dispatch(&mut self) {
        if self.data.is_empty() {
            self.event_type.clear();
            return;
        }
        self.data.pop();
        let event = if self.event_type.is_empty() {
            Cow::Borrowed("message")
        } else {
            Cow::Owned(std::mem::take(&mut self.event_type))
        };
        self.ready.push(SseEvent {
            event,
            data: std::mem::take(&mut self.data),
        });
    }
}

#[derive(Debug, Default)]
pub struct NdjsonFramer {
    buffer: Vec<u8>,
    ready: Vec<Vec<u8>>,
}

impl NdjsonFramer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> std::vec::Drain<'_, Vec<u8>> {
        self.buffer.extend_from_slice(chunk);
        let mut start = 0;
        while let Some(pos) = self.buffer[start..].iter().position(|&b| b == b'\n') {
            let line = &self.buffer[start..start + pos];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if !line.is_empty() {
                self.ready.push(line.to_vec());
            }
            start += pos + 1;
        }
        self.buffer.drain(..start);
        self.ready.drain(..)
    }

    pub fn finish(&mut self) -> Option<Vec<u8>> {
        let line = std::mem::take(&mut self.buffer);
        (!line.is_empty()).then_some(line)
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

#[cfg(test)]
mod tests;
