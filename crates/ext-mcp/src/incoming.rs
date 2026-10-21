//! One request owns both its ingress budget and its publication slots.
//! A slow engine never causes a queue of intermediate server progress.
use std::time::{Duration, Instant};

use ext::Cx;
use serde_json::Value;

use crate::{Failure, rpc};

const NOTIFICATIONS: usize = 256;
const NOTIFICATION_BYTES: usize = 64 * 1024;
const PROGRESS_BYTES: usize = 1024;
const PUBLICATIONS: usize = 8;
const INTERVAL: Duration = Duration::from_millis(100);

pub(crate) struct Incoming {
    id: u64,
    remaining: usize,
    bytes: usize,
    progress: Progress,
}

impl Incoming {
    pub(crate) fn new(id: u64) -> Self {
        Self {
            id,
            remaining: NOTIFICATIONS,
            bytes: NOTIFICATION_BYTES,
            progress: Progress::default(),
        }
    }

    pub(crate) fn decode(
        &mut self,
        message: &Value,
        bytes: usize,
    ) -> Result<Option<Value>, Failure> {
        let result = rpc::decode(message, self.id)?;
        if result.is_none() {
            self.remaining = self.remaining.checked_sub(1).ok_or(Failure::Limit)?;
            self.bytes = self.bytes.checked_sub(bytes).ok_or(Failure::Limit)?;
            if message["method"] == "notifications/progress"
                && message["params"]["progressToken"] == self.id
            {
                if let Some(text) = message["params"]["message"].as_str() {
                    self.progress.pending = Some(
                        text[..text.floor_char_boundary(PROGRESS_BYTES.min(text.len()))].to_owned(),
                    );
                }
            }
        }
        Ok(result)
    }

    pub(crate) fn publish(&mut self, cx: &Cx, finished: bool) {
        let now = cx
            .service::<dyn rt::Runtime>()
            .expect("engine runtime")
            .now();
        if let Some(text) = self.progress.take(now, finished) {
            cx.progress(&text);
        }
    }
}

#[derive(Default)]
struct Progress {
    pending: Option<String>,
    sent: usize,
    last: Option<Instant>,
}

impl Progress {
    fn take(&mut self, now: Instant, finished: bool) -> Option<String> {
        // Reserve a slot for the latest update when the response arrives.
        let limit = if finished {
            PUBLICATIONS
        } else {
            PUBLICATIONS - 1
        };
        if self.sent >= limit
            || (!finished
                && self
                    .last
                    .is_some_and(|last| now.saturating_duration_since(last) < INTERVAL))
        {
            return None;
        }
        let text = self.pending.take()?;
        self.sent += 1;
        self.last = Some(now);
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_notifications_spend_the_same_count_and_byte_budget() {
        let unknown = json!({"jsonrpc":"2.0","method":"notifications/unknown"});
        let mut count = Incoming::new(1);
        for _ in 0..NOTIFICATIONS {
            assert_eq!(count.decode(&unknown, 1), Ok(None));
        }
        assert_eq!(count.decode(&unknown, 1), Err(Failure::Limit));
        let mut bytes = Incoming::new(1);
        assert_eq!(bytes.decode(&unknown, NOTIFICATION_BYTES), Ok(None));
        assert_eq!(bytes.decode(&unknown, 1), Err(Failure::Limit));
    }

    #[test]
    fn publication_is_bounded_and_pending_progress_is_latest_wins() {
        let start = Instant::now();
        let mut progress = Progress::default();
        for i in 0..1000 {
            progress.pending = Some(i.to_string());
            if let Some(text) = progress.take(start, false) {
                assert_eq!(text, "0");
            }
        }
        assert_eq!(
            progress.take(start + INTERVAL, false).as_deref(),
            Some("999")
        );
        for i in 2..1000 {
            progress.pending = Some(i.to_string());
            progress.take(start + INTERVAL * i, false);
        }
        assert_eq!(progress.sent, PUBLICATIONS - 1);
        assert_eq!(progress.take(start, true).as_deref(), Some("999"));
        assert_eq!(progress.sent, PUBLICATIONS);
        progress.pending = Some("ignored".into());
        assert_eq!(progress.take(start, true), None);
    }
}
