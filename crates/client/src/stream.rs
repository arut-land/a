use std::collections::HashMap;

use protocol::view::{Item, Kind, SessionView};
use protocol::{Seq, SessionId};

#[derive(Debug, PartialEq, Eq)]
pub enum ReplyEvent<'a> {
    Start,
    Text(&'a str),
    End,
}

#[derive(Default)]
pub struct ReplyStream(HashMap<(Option<SessionId>, Seq), (usize, bool)>);

impl ReplyStream {
    pub fn project<'a>(&mut self, view: &'a SessionView) -> Vec<ReplyEvent<'a>> {
        let mut events = Vec::new();
        for item in &view.items {
            if !matches!(item.present().kind, Kind::Reply | Kind::Assistant) {
                continue;
            }
            let text = match item {
                Item::Reply { call, text, .. } if !item.settled() => view
                    .streaming
                    .as_ref()
                    .filter(|s| s.call == *call)
                    .map_or(text.as_str(), |s| s.text.as_str()),
                Item::Reply { text, .. } | Item::Assistant { text, .. } => text,
                _ => continue,
            };
            if text.is_empty() && !item.settled() {
                continue;
            }
            let (emitted, ended) = self.0.entry((view.session, item.key())).or_insert_with(|| {
                events.push(ReplyEvent::Start);
                (0, false)
            });
            if *ended {
                continue;
            }
            if let Some(suffix) = text.get(*emitted..)
                && !suffix.is_empty()
            {
                events.push(ReplyEvent::Text(suffix));
                *emitted = text.len();
            }
            if item.settled() {
                events.push(ReplyEvent::End);
                *ended = true;
            }
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
    use crate::{Submission, SubmissionState};
    use protocol::{
        Activity, Author, Body, CallId, ClientId, ClientSeq, DaemonMsg, Entry, Epoch, Json, Mode,
        Outcome, Seq, SessionId,
        view::{Change, Status},
    };

    fn entry(seq: u64, body: Body) -> Entry {
        Entry::new(Seq::new(seq).unwrap(), Epoch(0), body)
    }

    #[test]
    fn retry_text_is_emitted_once() {
        let mut stream = ReplyStream::default();
        let mut view = reply(SessionId::new(), Status::Running, "");
        let call = CallId::new(1).unwrap();
        view.fold(&DaemonMsg::Delta {
            call,
            text: "rep".into(),
            attempt: 0,
        });
        assert_eq!(
            stream.project(&view),
            [ReplyEvent::Start, ReplyEvent::Text("rep")]
        );
        view.fold(&DaemonMsg::Delta {
            call,
            text: "reply".into(),
            attempt: 1,
        });
        assert_eq!(stream.project(&view), [ReplyEvent::Text("ly")]);
        let snapshot = reply(view.session.unwrap(), Status::Done, "reply").snapshot();
        view.fold(&DaemonMsg::Snapshot(Box::new(snapshot)));
        assert_eq!(stream.project(&view), [ReplyEvent::End]);
        assert_eq!(stream.project(&view), []);
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
        view.fold(&DaemonMsg::Delta {
            call,
            text: "hel".into(),
            attempt: 0,
        })
        .unwrap();
        assert_eq!(
            stream.project(&view),
            [ReplyEvent::Start, ReplyEvent::Text("hel")]
        );
        view.fold(&DaemonMsg::Snapshot(Box::new(view.snapshot())))
            .unwrap();
        assert_eq!(stream.project(&view), []);
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
        assert!(matches!(&daemon_view.items[0], Item::Reply { text, .. } if text == "hello"));
        let update = view
            .fold(&DaemonMsg::Snapshot(Box::new(daemon_view.snapshot())))
            .unwrap();
        assert!(matches!(update.change, Change::Snapshot));
        assert_eq!(
            stream.project(&view),
            [ReplyEvent::Text("lo"), ReplyEvent::End]
        );
        assert_eq!(stream.project(&view), []);
        assert_eq!(stream.project(&view), []);
    }
    #[test]
    fn a_submission_recovered_before_its_first_delta_still_emits_its_reply() {
        let id = Author {
            client: ClientId::new(),
            seq: ClientSeq::new(1).unwrap(),
        };
        let daemon_view = SessionView::from_log(
            SessionId::new(),
            "/w".into(),
            [
                Entry::new(
                    Seq::new(1).unwrap(),
                    Epoch(0),
                    Body::Input {
                        text: "hello".into(),
                        mode: Mode::Queue,
                        taken: true,
                        origin: Some(id),
                    },
                ),
                Entry::new(
                    Seq::new(2).unwrap(),
                    Epoch(0),
                    Body::Assistant {
                        text: "reply".into(),
                    },
                ),
            ],
        );
        let mut view = SessionView::default();
        view.fold(&DaemonMsg::Snapshot(Box::new(daemon_view.snapshot())))
            .unwrap();
        view.fold(&DaemonMsg::Activity {
            seq: 2,
            activity: Activity::Idle,
        });
        let mut submission = Submission::new(id);
        submission.fold(&view);
        assert_eq!(submission.state, SubmissionState::Finished(Activity::Idle));
        assert_eq!(
            ReplyStream::default().project(&view),
            [
                ReplyEvent::Start,
                ReplyEvent::Text("reply"),
                ReplyEvent::End
            ],
            "pipe mode must not finish successfully without emitting its recovered reply"
        );
    }

    fn reply(session: SessionId, status: Status, text: &str) -> SessionView {
        let mut view = SessionView::default();
        view.session = Some(session);
        view.items.push(Item::Reply {
            call: CallId::new(1).unwrap(),
            model: "m".into(),
            text: text.into(),
            status,
            usage: None,
            purpose: None,
            owner: None,
        });
        view
    }

    #[test]
    fn switching_sessions_does_not_suppress_replayed_replies_with_reused_call_numbers() {
        let mut stream = ReplyStream::default();
        let first = reply(SessionId::new(), Status::Done, "first session");
        let second = reply(SessionId::new(), Status::Done, "second session");
        assert_eq!(
            stream.project(&first),
            [
                ReplyEvent::Start,
                ReplyEvent::Text("first session"),
                ReplyEvent::End
            ]
        );
        assert_eq!(
            stream.project(&second),
            [
                ReplyEvent::Start,
                ReplyEvent::Text("second session"),
                ReplyEvent::End
            ]
        );
        assert_eq!(stream.project(&first), []);
    }

    #[test]
    fn a_delta_after_a_completing_snapshot_cannot_end_the_reply_twice() {
        let mut stream = ReplyStream::default();
        let mut view = reply(SessionId::new(), Status::Running, "");
        let call = CallId::new(1).unwrap();
        view.fold(&DaemonMsg::Delta {
            call,
            text: "hel".into(),
            attempt: 0,
        });
        assert_eq!(
            stream.project(&view),
            [ReplyEvent::Start, ReplyEvent::Text("hel")]
        );
        let snapshot = reply(view.session.unwrap(), Status::Done, "hello").snapshot();
        view.fold(&DaemonMsg::Snapshot(Box::new(snapshot.clone())));
        assert_eq!(
            stream.project(&view),
            [ReplyEvent::Text("lo"), ReplyEvent::End]
        );
        view.fold(&DaemonMsg::Delta {
            call,
            text: "late text".into(),
            attempt: 0,
        });
        assert_eq!(stream.project(&view), []);
        view.fold(&DaemonMsg::Snapshot(Box::new(snapshot)));
        assert_eq!(stream.project(&view), []);
    }
}
