use protocol::view::{Change, Item, Kind, Part, SessionView, Shown};
use protocol::{CallId, Verdict};

#[derive(Debug, PartialEq, Eq)]
pub enum ReplyEvent<'a> {
    Start,
    Text(&'a str),
    End,
}

#[derive(Default)]
pub struct ReplyStream {
    active: Option<(CallId, usize)>,
    finished: u64,
}

impl ReplyStream {
    pub fn project<'a>(
        &mut self,
        change: &'a Change,
        view: &'a SessionView,
    ) -> Vec<ReplyEvent<'a>> {
        let mut events = Vec::new();
        match change {
            Change::Batch { changes } => {
                for change in changes {
                    events.extend(self.project(change, view));
                }
            }
            Change::Delta { call, text }
                if view
                    .items
                    .iter()
                    .any(|item| item.key() == call.0 && item.present().kind == Kind::Reply) =>
            {
                if self.active.is_none_or(|(id, _)| id != *call) {
                    if self.active.is_some() {
                        events.push(ReplyEvent::End);
                    }
                    self.active = Some((*call, 0));
                    self.finished = self.finished.min(call.0.get() - 1);
                    events.push(ReplyEvent::Start);
                }
                self.active.as_mut().unwrap().1 += text.len();
                events.push(ReplyEvent::Text(text));
            }
            Change::Added {
                item: Item::Assistant { text, .. },
                ..
            } => {
                events.extend([ReplyEvent::Start, ReplyEvent::Text(text), ReplyEvent::End]);
            }
            Change::Added {
                item:
                    Item::Hook {
                        call: Some(call),
                        decision: Verdict::Retry { .. },
                        ..
                    },
                ..
            } if self.active.take_if(|(id, _)| id == call).is_some() => {
                events.push(ReplyEvent::End);
            }
            Change::Snapshot => {
                // Historical replies belong to transcript rendering; reconcile only
                // the reply this connection has already started emitting.
                if let Some((call, _)) = self.active {
                    if let Some(item) = view.items.iter().find(|item| item.key() == call.0) {
                        self.finish(item, &mut events);
                    } else {
                        self.active = None;
                        self.finished = self.finished.max(call.0.get());
                        events.push(ReplyEvent::End);
                    }
                }
            }
            Change::Updated {
                index,
                part: Part::Outcome,
            } => {
                self.finish(&view.items[*index], &mut events);
            }
            _ => {}
        }
        events
    }

    fn finish<'a>(&mut self, item: &'a Item, events: &mut Vec<ReplyEvent<'a>>) {
        if !item.settled()
            || item.key().get() <= self.finished
            || item.present().kind != Kind::Reply
        {
            return;
        }
        let emitted = if let Some((_, emitted)) = self.active.take_if(|(id, _)| id.0 == item.key())
        {
            emitted
        } else {
            events.push(ReplyEvent::Start);
            0
        };
        if let Shown::Markdown(text) = item.present().body
            && let Some(rest) = text.get(emitted..)
            && !rest.is_empty()
        {
            events.push(ReplyEvent::Text(rest));
        }
        events.push(ReplyEvent::End);
        self.finished = item.key().get();
    }
}

/// `text` without terminal escapes or control characters (a command's
/// colored output would otherwise drive the terminal), tabs as spaces.
pub fn sanitize(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.chars().any(|c| c.is_control() && c != '\n') {
        return text.into();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            // CSI: parameters and intermediates, then one final byte.
            '\x1b' if chars.peek() == Some(&'[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ST.
            '\x1b' if chars.peek() == Some(&']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Body, DaemonMsg, Entry, Epoch, Json, Outcome, Seq, SessionId, view::Status};

    fn entry(seq: u64, body: Body) -> Entry {
        Entry::new(Seq::new(seq).unwrap(), Epoch(0), body)
    }

    #[test]
    fn retry_text_is_emitted_once() {
        let call = CallId::new(1).unwrap();
        let mut stream = ReplyStream::default();
        let mut view = SessionView::default();
        view.items.push(Item::Reply {
            call,
            model: "m".into(),
            text: "reply".into(),
            status: Status::Done,
            usage: None,
            purpose: None,
            owner: None,
        });
        let delta = Change::Delta {
            call,
            text: "reply".into(),
        };
        assert_eq!(
            stream.project(&delta, &view),
            [ReplyEvent::Start, ReplyEvent::Text("reply")]
        );
        let retry = Change::Added {
            index: 1,
            item: Item::Hook {
                seq: Seq::new(2).unwrap(),
                hook: "retry".into(),
                call: Some(call),
                tool: String::new(),
                decision: Verdict::Retry {
                    attempt: 1,
                    error: "retry".into(),
                },
            },
        };
        assert_eq!(stream.project(&retry, &view), [ReplyEvent::End]);
        assert_eq!(
            stream.project(&delta, &view),
            [ReplyEvent::Start, ReplyEvent::Text("reply")]
        );
        let done = Change::Updated {
            index: 0,
            part: Part::Outcome,
        };
        assert_eq!(stream.project(&done, &view), [ReplyEvent::End]);
        assert!(stream.project(&done, &view).is_empty());
        assert_eq!(
            stream.project(&delta, &view),
            [ReplyEvent::Start, ReplyEvent::Text("reply")]
        );
        assert_eq!(stream.project(&done, &view), [ReplyEvent::End]);
        let mut resumed = ReplyStream::default();
        let partial = Change::Delta {
            call,
            text: "rep".into(),
        };
        assert_eq!(
            resumed.project(&partial, &view),
            [ReplyEvent::Start, ReplyEvent::Text("rep")]
        );
        assert_eq!(
            resumed.project(&done, &view),
            [ReplyEvent::Text("ly"), ReplyEvent::End]
        );
        let mut replay = ReplyStream::default();
        assert_eq!(
            replay.project(&done, &view),
            [
                ReplyEvent::Start,
                ReplyEvent::Text("reply"),
                ReplyEvent::End
            ]
        );
    }

    #[test]
    fn interactive_output_cannot_inject_terminal_controls() {
        assert_eq!(sanitize("\x1b[31mred\x1b[0m\tx\x07"), "red    x");
        assert_eq!(sanitize("\x1b]0;title\x07ok"), "ok");
        for chunk in ["\x1b", "[31m", "\x1b]title", "\x1b", "\\", "\r\x08\u{009b}"] {
            assert!(!sanitize(chunk).chars().any(|c| c.is_control() && c != '\n'));
        }
        assert!(matches!(
            sanitize("plain\ntext"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn reconnect_snapshot_finishes_a_partially_emitted_reply() {
        let call = CallId::new(1).unwrap();
        let mut view = SessionView::from_log(
            SessionId::new(),
            "/w".into(),
            [entry(
                1,
                Body::ModelCall {
                    model: "m".into(),
                    purpose: None,
                    r#for: None,
                    attempt: 0,
                    choice: protocol::ToolChoice::None,
                    request: None,
                },
            )],
        );
        let mut stream = ReplyStream::default();
        let update = view
            .fold(&DaemonMsg::Delta {
                call,
                text: "hel".into(),
                attempt: 0,
            })
            .unwrap();
        assert_eq!(
            stream.project(&update.change, &view),
            [ReplyEvent::Start, ReplyEvent::Text("hel")]
        );
        let running = view
            .fold(&DaemonMsg::Snapshot(Box::new(view.snapshot())))
            .unwrap();
        assert!(stream.project(&running.change, &view).is_empty());
        let mut daemon_view = view.clone();
        daemon_view.fold(&DaemonMsg::Entry(entry(
        2,
        Body::ModelResult {
            call,
            r#for: None,
            failure: None,
            outcome: Outcome::Done {
                output: Json::encode(
                    &serde_json::json!({"message":{"content":[{"type":"text","text":"hello"}]}}),
                )
                .unwrap(),
            },
        },
    )));
        assert!(
            matches!(&daemon_view.items[0], protocol::view::Item::Reply { text, .. } if text == "hello")
        );
        let update = view
            .fold(&DaemonMsg::Snapshot(Box::new(daemon_view.snapshot())))
            .unwrap();
        assert!(matches!(update.change, Change::Snapshot));
        assert_eq!(
            stream.project(&update.change, &view),
            [ReplyEvent::Text("lo"), ReplyEvent::End]
        );
        assert!(stream.project(&update.change, &view).is_empty());
        assert!(
            stream
                .project(
                    &Change::Updated {
                        index: 0,
                        part: Part::Outcome
                    },
                    &view
                )
                .is_empty()
        );
    }
}
