use protocol::view::{Change, Item, Kind, Part, SessionView, Shown};
use protocol::{CallId, Verdict};

#[derive(Debug, PartialEq, Eq)]
pub enum ReplyEvent<'a> {
    Start,
    Text(&'a str),
    End,
}

#[derive(Default)]
pub struct ReplyStream(Option<(CallId, usize)>);

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
                if self.0.is_none_or(|(id, _)| id != *call) {
                    if self.0.is_some() {
                        events.push(ReplyEvent::End);
                    }
                    self.0 = Some((*call, 0));
                    events.push(ReplyEvent::Start);
                }
                self.0.as_mut().unwrap().1 += text.len();
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
            } if self.0.take_if(|(id, _)| id == call).is_some() => events.push(ReplyEvent::End),
            Change::Updated {
                index,
                part: Part::Outcome,
            } => {
                let item = &view.items[*index];
                let p = item.present();
                if p.kind == Kind::Reply {
                    if let Some((_, emitted)) = self.0.take_if(|(id, _)| id.0 == item.key()) {
                        if let Shown::Markdown(text) = p.body
                            && let Some(rest) = text.get(emitted..)
                            && !rest.is_empty()
                        {
                            events.push(ReplyEvent::Text(rest));
                        }
                        events.push(ReplyEvent::End);
                    } else if let Shown::Markdown(text) = p.body
                        && !text.is_empty()
                    {
                        events.extend([ReplyEvent::Start, ReplyEvent::Text(text), ReplyEvent::End]);
                    }
                }
            }
            _ => {}
        }
        events
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
    use protocol::{Seq, view::Status};

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
}
