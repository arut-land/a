use protocol::view::{Item, Kind, SessionView};
use protocol::{Seq, SessionId};

#[derive(Debug, PartialEq, Eq)]
pub enum ReplyEvent<'a> {
    Start,
    Text(&'a str),
    End,
}

#[derive(Default)]
pub struct ReplyStream {
    session: Option<SessionId>,
    settled: u64,
    open: Option<(Seq, String)>,
}

impl ReplyStream {
    pub fn project<'a>(&mut self, view: &'a SessionView) -> Vec<ReplyEvent<'a>> {
        let mut events = Vec::new();
        if self.session != view.session {
            events.extend(self.open.take().map(|_| ReplyEvent::End));
            self.session = view.session;
            self.settled = 0;
        }
        if let Some((seq, _)) = &self.open
            && !view.items.iter().any(|item| item.key() == *seq)
        {
            events.push(ReplyEvent::End);
            self.settled = self.settled.max(seq.get());
            self.open = None;
        }
        for item in &view.items {
            if item.key().get() <= self.settled
                || !matches!(item.present().kind, Kind::Reply | Kind::Assistant)
            {
                continue;
            }
            let live = view
                .streaming
                .as_ref()
                .filter(|s| s.call.0 == item.key() && !item.settled());
            let text = match item {
                Item::Reply { text, .. } | Item::Assistant { text, .. } => {
                    live.map_or(text.as_str(), |s| s.text.as_str())
                }
                _ => continue,
            };
            if text.is_empty() && !item.settled() {
                break;
            }
            let (_, mut printed) = self.open.take().unwrap_or_else(|| {
                events.push(ReplyEvent::Start);
                (item.key(), String::new())
            });
            if let Some(suffix) = text.strip_prefix(printed.as_str()) {
                if !suffix.is_empty() {
                    events.push(ReplyEvent::Text(suffix));
                    printed.push_str(suffix);
                }
            } else if !printed.starts_with(text) {
                events.extend([ReplyEvent::End, ReplyEvent::Start, ReplyEvent::Text(text)]);
                printed.clear();
                printed.push_str(text);
            }
            if item.settled() {
                events.push(ReplyEvent::End);
                self.settled = item.key().get();
            } else {
                self.open = Some((item.key(), printed));
                // Preserve transcript order: an ended frontier cannot pass an open reply.
                break;
            }
        }
        events
    }
}

#[derive(Default)]
enum Escape {
    #[default]
    Text,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

#[derive(Default)]
pub struct Sanitizer {
    escape: Escape,
    pending: Vec<u8>,
}

impl Sanitizer {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        let mut pending = std::mem::take(&mut self.pending);
        pending.extend_from_slice(bytes);
        let mut consumed = 0;
        let mut out = String::with_capacity(pending.len());
        while consumed < pending.len() {
            let (text, invalid) = match std::str::from_utf8(&pending[consumed..]) {
                Ok(text) => (text, None),
                Err(error) => (
                    std::str::from_utf8(&pending[consumed..consumed + error.valid_up_to()])
                        .unwrap(),
                    error.error_len(),
                ),
            };
            for c in text.chars().chain(invalid.map(|_| '�')) {
                self.escape = match (&self.escape, c) {
                    (Escape::Text, '\x1b') => Escape::Esc,
                    (Escape::Text | Escape::Esc, '\u{009b}') | (Escape::Esc, '[') => Escape::Csi,
                    (Escape::Text | Escape::Esc, '\u{009d}') | (Escape::Esc, ']') => Escape::Osc,
                    (Escape::Text, '\t') => {
                        out.push_str("    ");
                        Escape::Text
                    }
                    (Escape::Text, c) => {
                        if c == '\n' || !c.is_control() {
                            out.push(c);
                        }
                        Escape::Text
                    }
                    (Escape::Esc, _) => Escape::Text,
                    (Escape::Csi, '@'..='~') | (Escape::Osc, '\x07') | (Escape::OscEsc, '\\') => {
                        Escape::Text
                    }
                    (Escape::Osc | Escape::OscEsc, '\x1b') => Escape::OscEsc,
                    (Escape::OscEsc | Escape::Osc, _) => Escape::Osc,
                    (Escape::Csi, _) => Escape::Csi,
                };
            }
            consumed += text.len() + invalid.unwrap_or(0);
            if invalid.is_none() {
                break;
            }
        }
        self.pending.extend_from_slice(&pending[consumed..]);
        out
    }
}

/// One complete text value; terminal writers retain a [`Sanitizer`] across writes.
pub fn sanitize(text: &str) -> std::borrow::Cow<'_, str> {
    if text.chars().any(|c| c.is_control() && c != '\n') {
        Sanitizer::default().push(text.as_bytes()).into()
    } else {
        text.into()
    }
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
                .unwrap().into(),
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
                        segments: "hello".into(),
                        mode: Mode::Queue,
                        taken: true,
                        origin: Some(id),
                        force: false,
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
        assert_eq!(
            stream.project(&first),
            [
                ReplyEvent::Start,
                ReplyEvent::Text("first session"),
                ReplyEvent::End
            ]
        );
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

    #[test]
    fn retry_replacement_cannot_drop_a_unicode_reply_at_an_old_byte_offset() {
        for (first, replacement) in [("a", "é"), ("hello", "world"), ("é", "a")] {
            let call = CallId::new(1).unwrap();
            let mut view = reply(SessionId::new(), Status::Running, "");
            let mut stream = ReplyStream::default();
            view.fold(&DaemonMsg::Delta {
                call,
                text: first.into(),
                attempt: 0,
            });
            assert_eq!(
                stream.project(&view),
                [ReplyEvent::Start, ReplyEvent::Text(first)]
            );
            view.fold(&DaemonMsg::Delta {
                call,
                text: replacement.into(),
                attempt: 1,
            });
            assert_eq!(
                stream.project(&view),
                [
                    ReplyEvent::End,
                    ReplyEvent::Start,
                    ReplyEvent::Text(replacement)
                ]
            );
            assert_eq!(stream.project(&view), []);
            assert_eq!(stream.open.as_ref().map(|(seq, _)| *seq), Some(call.0));
            view.fold(&DaemonMsg::Snapshot(Box::new(
                reply(view.session.unwrap(), Status::Done, replacement).snapshot(),
            )));
            assert_eq!(stream.project(&view), [ReplyEvent::End]);
            assert_eq!(stream.project(&view), []);
        }
    }

    #[test]
    fn an_identical_retry_can_rebuild_its_prefix_without_duplicate_output() {
        let call = CallId::new(1).unwrap();
        let mut view = reply(SessionId::new(), Status::Running, "");
        let mut stream = ReplyStream::default();
        view.fold(&DaemonMsg::Delta {
            call,
            text: "héllo".into(),
            attempt: 0,
        });
        assert_eq!(
            stream.project(&view),
            [ReplyEvent::Start, ReplyEvent::Text("héllo")]
        );
        for text in ["h", "é", "llo"] {
            view.fold(&DaemonMsg::Delta {
                call,
                text: text.into(),
                attempt: 1,
            });
            assert_eq!(stream.project(&view), []);
        }
        view.fold(&DaemonMsg::Snapshot(Box::new(
            reply(view.session.unwrap(), Status::Done, "héllo").snapshot(),
        )));
        assert_eq!(stream.project(&view), [ReplyEvent::End]);
    }

    #[test]
    fn split_escape_sequences_have_the_same_visible_output_as_unsplit() {
        for raw in [
            "\x1b[31mhéllo\x1b[0m",
            "\x1b]0;hidden\x07héllo",
            "\x1b]hidden\x1b\\世界\t!\n",
        ] {
            let expected = sanitize(raw);
            for boundary in 0..=raw.len() {
                let mut sanitizer = Sanitizer::default();
                let actual = sanitizer.push(&raw.as_bytes()[..boundary])
                    + &sanitizer.push(&raw.as_bytes()[boundary..]);
                assert_eq!(actual, expected, "split at byte {boundary}");
                assert_eq!(sanitizer.pending.len(), 0);
            }
            let mut sanitizer = Sanitizer::default();
            let actual: String = raw
                .as_bytes()
                .iter()
                .map(|byte| sanitizer.push(&[*byte]))
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn ended_replies_leave_only_a_frontier_and_departed_sessions_leave_no_state() {
        let session = SessionId::new();
        let mut stream = ReplyStream::default();
        let mut view = reply(session, Status::Done, "done");
        for seq in 1..=1000 {
            let Item::Reply { call, .. } = &mut view.items[0] else {
                unreachable!()
            };
            *call = CallId::new(seq).unwrap();
            assert_eq!(
                stream.project(&view),
                [ReplyEvent::Start, ReplyEvent::Text("done"), ReplyEvent::End]
            );
            assert!(stream.open.is_none());
            assert_eq!(stream.settled, seq);
        }
        let Item::Reply { call, status, .. } = &mut view.items[0] else {
            unreachable!()
        };
        *call = CallId::new(1001).unwrap();
        *status = Status::Running;
        assert_eq!(
            stream.project(&view),
            [ReplyEvent::Start, ReplyEvent::Text("done")]
        );
        assert!(stream.open.is_some());
        let mut departed = SessionView::default();
        departed.session = Some(SessionId::new());
        assert_eq!(stream.project(&departed), [ReplyEvent::End]);
        assert!(stream.open.is_none());
        assert_eq!(stream.settled, 0);
    }

    #[test]
    fn the_settled_frontier_never_passes_an_open_reply() {
        let mut view = reply(SessionId::new(), Status::Running, "first");
        view.items.push(Item::Assistant {
            seq: Seq::new(2).unwrap(),
            text: "second".into(),
        });
        let mut stream = ReplyStream::default();
        assert_eq!(
            stream.project(&view),
            [ReplyEvent::Start, ReplyEvent::Text("first")]
        );
        assert_eq!(stream.settled, 0);
        let Item::Reply { status, .. } = &mut view.items[0] else {
            unreachable!()
        };
        *status = Status::Done;
        assert_eq!(
            stream.project(&view),
            [
                ReplyEvent::End,
                ReplyEvent::Start,
                ReplyEvent::Text("second"),
                ReplyEvent::End
            ]
        );
        assert!(stream.open.is_none());
        assert_eq!(stream.settled, 2);
    }
}
