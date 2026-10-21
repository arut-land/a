//! Discovery retains at most one response-sized aggregate. Output and its
//! escaped card therefore leave ample room in a 16 MiB protocol frame.
use std::io::{self, Write};

use crate::Failure;
use serde_json::Value;

pub(crate) struct Budget(usize);
impl Default for Budget {
    fn default() -> Self {
        Self(crate::rpc::LIMIT)
    }
}
impl Budget {
    pub(crate) fn include(&mut self, value: &Value) -> Result<(), Failure> {
        serde_json::to_writer(self, value).map_err(|_| Failure::Limit)
    }
}
impl Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or(io::ErrorKind::FileTooLarge)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn description(text: &str) -> String {
    const BYTES: usize = 512;
    if text.len() <= BYTES {
        text.to_owned()
    } else {
        format!(
            "{}…",
            &text[..text.floor_char_boundary(BYTES - '…'.len_utf8())]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summaries_are_deterministic_utf8_prefixes_with_a_byte_limit() {
        let text = "é".repeat(1000);
        let shortened = description(&text);
        assert!(shortened.len() <= 512);
        assert!(text.starts_with(shortened.strip_suffix('…').unwrap()));
        assert_eq!(shortened, description(&text));
    }
}
