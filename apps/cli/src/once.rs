//! `a` in a pipeline: one turn, on a prompt from `-p` and whatever stdin
//! holds, in a new session (or the one `resume`/`attach` names). What the
//! agent says goes to stdout as it streams, nothing else does, and the exit
//! status says how the turn ended (docs/cli.md).

use std::io::{self, IsTerminal, Read};
use std::process::ExitCode;
use std::thread;

use client::{Driver, End, Env, Event, Link, Renderer, SessionClient, Show};
use protocol::Target;
use protocol::view::{Buttons, Change, Glyph, Item, Kind, Part, Status};
use sys::{Signal, Signals};

/// How the turn ended, as the exit status says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// The turn ran to its end.
    Done = 0,
    /// A model call failed, the daemon refused the input or went away.
    Failed = 1,
    /// Nothing to send.
    Usage = 2,
    /// The turn waits on someone (a call's approval, a question), and no
    /// pipeline waits for another client: the session keeps the question
    /// open, for `a attach` or `a web` to answer.
    Waiting = 3,
    /// Interrupted: Ctrl-C here (130 is 128 + SIGINT), or by a client.
    Interrupted = 130,
}

impl From<Ended> for ExitCode {
    fn from(ended: Ended) -> Self {
        Self::from(ended as u8)
    }
}

/// What wakes the loop.
enum Input {
    Busy,
    /// All of stdin; none when it is a terminal.
    Stdin(Option<String>),
    Interrupt,
    Client(Event),
}

pub fn run(
    daemon: &client::Daemon,
    target: Target,
    prompt: Option<String>,
    mode: Option<String>,
) -> io::Result<ExitCode> {
    let (tx, rx) = client::inbox(client::QueueBudget::default(), |input| {
        size_of::<Input>()
            + match input {
                Input::Client(event) => event.retained_bytes(),
                Input::Stdin(Some(text)) => text.len(),
                _ => 0,
            }
    });
    let runtime: rt::Rt = std::sync::Arc::new(rt::Threads::default());
    let driver =
        Driver::with_inbox(tx.clone(), Input::Client).start(runtime.clone(), daemon.clone());
    let stdin = tx.clone();
    // thread-exception: pipeline-stdin
    thread::spawn(move || {
        read_stdin((!io::stdin().is_terminal()).then(io::stdin), &stdin);
    });
    // Ctrl-C interrupts the turn; where signals are not caught it ends `a`
    // as it would anything else.
    if let Ok(signals) = Signals::catch(&[Signal::Interrupt]) {
        let signals = rt::Async::for_reading(&runtime.reactor(), signals)?;
        runtime.spawn(Box::pin(async move {
            while signals
                .read_with(|signals| {
                    if signals.take().is_empty() {
                        Err(io::ErrorKind::WouldBlock.into())
                    } else {
                        Ok(())
                    }
                })
                .await
                .is_ok()
            {
                if tx.send_first(Input::Interrupt).is_err() {
                    break;
                }
            }
        }));
    }
    let mut once = Once {
        client: SessionClient::new(Env::Origin),
        prompt,
        mode,
        stdin: None,
        synced: false,
        turn: None,
        stream: client::ReplyStream::default(),
        failed: false,
        ended: None,
    };
    let mut effects = once.client.open(target);
    loop {
        driver.pump(&mut once, effects);
        if let Some(ended) = once.ended {
            // An interrupt must reach the transport before detaching revokes I/O.
            let _ = rt::block_on(rt::or(driver.flush(), async {
                runtime.sleep(client::HELLO_TIMEOUT).await;
                Err(client::Error::NotResponding)
            }));
            driver.pump(&mut (), once.client.detach());
            return ended;
        }
        let Some(event) = rx.recv() else {
            return Ok(Ended::Failed.into());
        };
        effects = match event {
            Input::Client(event) => once.client.step(event),
            Input::Busy => return Err(client::Error::Busy.into()),
            Input::Stdin(text) => {
                once.stdin = Some(input(once.prompt.take().as_deref(), text.as_deref()));
                once.send()
            }
            Input::Interrupt => once.interrupt(),
        };
    }
}

fn read_stdin(input: Option<impl Read>, post: &client::Post<Input>) {
    let text = input.map(|mut input| {
        let mut bytes = Vec::new();
        let _ = input.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    });
    if post.send_first(Input::Stdin(text)).is_err() {
        post.close_with(Input::Busy);
    }
}

struct Once {
    client: SessionClient,
    /// `-p`'s prompt.
    prompt: Option<String>,
    /// `--mode`: set with the policy's `/mode` before the prompt goes.
    mode: Option<String>,
    /// Complete input, once stdin has been read (or found to be a terminal).
    stdin: Option<String>,
    /// The replay of the attach is in.
    synced: bool,
    turn: Option<client::SubmissionId>,
    stream: client::ReplyStream,
    failed: bool,
    ended: Option<io::Result<ExitCode>>,
}

impl Once {
    /// Sends the input once attached and stdin is read.
    fn send(&mut self) -> Vec<client::Effect> {
        if !self.synced || self.turn.is_some() {
            return Vec::new();
        }
        let Some(text) = self.stdin.take() else {
            return Vec::new();
        };
        if text.is_empty() {
            errln!("a: nothing to send: give a prompt with -p, or on stdin");
            self.end(Ended::Usage);
            return Vec::new();
        }
        let mut effects = Vec::new();
        if let Some(mode) = &self.mode {
            effects = self
                .client
                .run(client::Cmd::Input(format!("/mode {mode}").into()));
        }
        match self.client.submit(text) {
            Ok((id, sent)) => {
                self.turn = Some(id);
                effects.extend(sent);
            }
            Err(notice) => self.fail(io::Error::other(notice.to_string())),
        }
        effects
    }

    fn interrupt(&mut self) -> Vec<client::Effect> {
        self.end(Ended::Interrupted);
        self.client.run(client::Cmd::Interrupt)
    }

    fn end(&mut self, ended: Ended) {
        self.ended.get_or_insert(Ok(ended.into()));
    }

    fn fail(&mut self, e: io::Error) {
        self.ended.get_or_insert(Err(e));
    }

    fn link(&mut self) {
        match self.client.link() {
            Link::Reconnecting { .. } => errln!("[daemon upgraded; reconnecting]"),
            Link::Draining => errln!("[waiting for the previous daemon to finish its work]"),
            Link::Wedged(d) => {
                crate::report(d);
                self.fail(io::Error::new(io::ErrorKind::TimedOut, crate::RESTART));
            }
            Link::Ended(End::Lost(why)) => {
                self.fail(io::Error::other(format!("daemon disconnected: {why}")));
            }
            Link::Ended(End::Detached)
            | Link::Connecting
            | Link::Replaying { .. }
            | Link::Live { .. } => {}
        }
    }

    fn taken(&self) -> bool {
        matches!(self.turn, Some(id) if matches!(self.client.submission(id), Some(client::SubmissionState::Taken | client::SubmissionState::Finished(_))))
    }

    fn view(&mut self, change: &Change) {
        if let Change::Batch { changes } = change {
            for change in changes {
                self.view(change);
            }
            return;
        }
        let events = self.stream.update(self.client.view(), change);
        if self.taken() {
            for event in events {
                match event {
                    client::ReplyEvent::Start => {}
                    client::ReplyEvent::Text(text) => crate::out(format_args!("{text}")),
                    client::ReplyEvent::End => crate::out(format_args!("\n")),
                }
            }
        }
        match change {
            Change::Synced => self.synced = true,
            Change::Error { message } => {
                // Nothing to attach to (no such session), or the input
                // refused.
                self.failed = true;
                self.fail(io::Error::other(message.clone()));
            }
            Change::Mismatch { supported } => {
                errln!("[{}]", protocol::view::mismatch_line(*supported));
            }
            Change::Updated {
                index,
                part: Part::Outcome,
            } if !self.taken() => {
                let p = self.client.view().items[*index].present();
                if p.kind == Kind::Command && p.glyph == Glyph::Bad {
                    let why = p.body.text();
                    let why = why.lines().next().unwrap_or(&p.status);
                    self.fail(io::Error::other(format!("{}: {why}", p.title)));
                }
            }
            _ if !self.taken() => {}
            Change::Updated {
                index,
                part: Part::Outcome,
            } => self.settled(*index),
            _ => {}
        }
    }

    /// A later successful attempt supersedes a retryable failure.
    fn settled(&mut self, index: usize) {
        let item = &self.client.view().items[index];
        let Item::Reply { status, .. } = item else {
            return;
        };
        let p = item.present();
        if p.kind != Kind::Reply {
            return;
        }
        match status {
            Status::Failed { .. } => {
                errln!("a: {}", p.status);
                self.failed = true;
            }
            Status::Interrupted => self.end(Ended::Interrupted),
            Status::Done => self.failed = false,
            Status::Running => {}
        }
    }

    /// Only a complete kernel burst can certify that our turn ended.
    fn settle(&mut self) {
        let Some(id) = self.turn else {
            return;
        };
        let Some(client::SubmissionState::Finished(activity)) = self.client.submission(id) else {
            return;
        };
        let view = self.client.view();
        if activity.requires_action() {
            let id = view.session.map_or_default(|s| s.short().to_string());
            let answer = format!("answer it with `a attach {id}` or in `a web`");
            let approval = view.items.iter().rev().find_map(|item| match item {
                Item::Tool { tool, .. }
                    if matches!(item.present().buttons, Buttons::Approve(_)) =>
                {
                    Some((
                        tool,
                        item.present().title,
                        approval_message(&view.present(item), &id),
                        if item.present().question.is_some() {
                            "an answer"
                        } else {
                            "approval"
                        },
                    ))
                }
                _ => None,
            });
            match approval {
                Some((tool, title, explanation, waiting)) => {
                    errln!(
                        "a: {tool} ({title}) waits for {waiting} in session {id}.\n{explanation}"
                    );
                }
                None => errln!("a: the turn waits for an answer in session {id}: {answer}"),
            }
            self.end(Ended::Waiting);
        } else {
            self.end(if self.failed {
                Ended::Failed
            } else {
                Ended::Done
            });
        }
    }
}

fn approval_message(p: &protocol::view::Presented<'_>, id: &str) -> String {
    if let Some(question) = p.question {
        return format!(
            "{}\nThe question stays open. Answer it with `a attach {id}` or in `a web`.",
            client::sanitize(&client::question_text(question))
        );
    }
    client::approval_parts(p).into_iter().map(|part| match part {
        client::ApprovalPart::Controls { .. } => format!("The session keeps the question open: answer it with `a attach {id}` or in `a web`, or run with `--mode auto` (A_MODE=auto) to let calls through unasked"),
        part => part.text().unwrap_or_default().to_owned(),
    }).collect::<Vec<_>>().join("\n")
}

impl Renderer for Once {
    fn show(&mut self, shown: Vec<Show>) -> Vec<client::Effect> {
        let mut effects = Vec::new();
        for show in shown {
            if self.ended.is_some() {
                break;
            }
            match show {
                Show::View(update) => {
                    self.view(&update.change);
                    if matches!(update.change, Change::Synced) {
                        effects.extend(self.send());
                    }
                    self.settle();
                }
                Show::Notice(notice) => errln!("[{notice}]"),
                Show::Link => self.link(),
                Show::Sessions => {}
            }
        }
        effects
    }
}

/// The input to send: `-p`'s prompt, then what stdin held, apart by a
/// blank line; either alone when the other is missing or blank.
fn input(prompt: Option<&str>, stdin: Option<&str>) -> String {
    let parts: Vec<&str> = [prompt, stdin]
        .into_iter()
        .flatten()
        .map(str::trim_end)
        .filter(|part| !part.trim().is_empty())
        .collect();
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use client::{ConnId, EventKind};
    use protocol::{
        Activity, Author, Body, ClientId, DaemonMsg, Entry, Epoch, Mode, Seq, SessionId,
    };
    use protocol::{Decode, view::SessionView};

    #[test]
    fn replay_cannot_reject_or_erase_piped_stdin() {
        use std::io::{Seek, Write};

        let (post, inbox) = client::inbox(
            client::QueueBudget {
                bytes: 8192,
                requests: 2,
                control_reserve: 1,
            },
            |_| 1,
        );
        post.send(Input::Client(Event {
            conn: ConnId(1),
            kind: EventKind::Waiting,
        }))
        .ok()
        .unwrap();
        let mut input = tempfile::tempfile().unwrap();
        input.write_all(b"piped input").unwrap();
        input.rewind().unwrap();
        read_stdin(Some(input), &post);
        assert!(
            matches!(inbox.try_recv(), Some(Input::Stdin(Some(text))) if text == "piped input")
        );
        assert!(matches!(
            inbox.try_recv(),
            Some(Input::Client(Event {
                kind: EventKind::Waiting,
                ..
            }))
        ));
        assert!(inbox.try_recv().is_none());
    }

    fn receive(once: &mut Once, msg: DaemonMsg) {
        let effects = once.client.step(Event {
            conn: ConnId(0),
            kind: EventKind::Msg(msg),
        });
        let shown = effects
            .into_iter()
            .filter_map(|e| match e {
                client::Effect::Show(show) => Some(show),
                client::Effect::Io(_) => None,
            })
            .collect();
        assert!(
            once.show(shown).is_empty(),
            "completion never queries the daemon"
        );
    }

    #[test]
    fn a_pipe_waits_for_its_own_input_and_the_certified_end() {
        let mut once = Once {
            client: SessionClient::new(Env::Keep),
            prompt: Some("same".into()),
            mode: None,
            stdin: Some("same".into()),
            synced: true,
            turn: None,
            stream: client::ReplyStream::default(),
            failed: false,
            ended: None,
        };
        receive(
            &mut once,
            DaemonMsg::Attached {
                session: SessionId::new(),
                workspace: "/w".into(),
            },
        );
        once.send();
        let Some(id) = once.turn else {
            panic!("input was not submitted")
        };
        for (seq, origin) in [
            (
                1,
                Author {
                    client: ClientId::new(),
                    ..id
                },
            ),
            (2, id),
        ] {
            receive(
                &mut once,
                DaemonMsg::Entry(Entry::new(
                    Seq::new(seq).unwrap(),
                    Epoch(0),
                    Body::Input {
                        segments: "same".into(),
                        mode: Mode::Queue,
                        taken: true,
                        origin: Some(origin),
                        force: false,
                    },
                )),
            );
            assert!(once.ended.is_none());
            receive(
                &mut once,
                DaemonMsg::Activity {
                    seq: seq - 1,
                    activity: Activity::Idle,
                },
            );
            assert!(once.ended.is_none());
            if seq == 1 {
                receive(
                    &mut once,
                    DaemonMsg::Activity {
                        seq,
                        activity: Activity::Idle,
                    },
                );
                assert!(
                    once.ended.is_none(),
                    "another client's identical input cannot finish this pipe"
                );
            }
        }
        receive(
            &mut once,
            DaemonMsg::Activity {
                seq: 2,
                activity: Activity::Idle,
            },
        );
        assert_eq!(once.ended.unwrap().unwrap(), ExitCode::SUCCESS);
    }

    #[test]
    fn sigint_sends_cancellation_before_detaching_from_unpublished_input_and_exits_130() {
        let mut once = Once {
            client: SessionClient::new(Env::Keep),
            prompt: Some("hello".into()),
            mode: None,
            stdin: Some("hello".into()),
            synced: true,
            turn: None,
            stream: client::ReplyStream::default(),
            failed: false,
            ended: None,
        };
        once.client.open(Target::New);
        let conn = once.client.connection();
        for kind in [
            EventKind::Connected {
                welcome: client::Welcome {
                    proto: protocol::PROTO,
                    caps: protocol::Caps::ALL,
                },
                remote: false,
                commands: Vec::new(),
                machine: protocol::MachineId([0; 16]),
                upgrade: None,
            },
            EventKind::Msg(DaemonMsg::Attached {
                session: SessionId::new(),
                workspace: "/w".into(),
            }),
            EventKind::Msg(DaemonMsg::Activity {
                seq: 0,
                activity: Activity::Idle,
            }),
        ] {
            once.client.step(Event { conn, kind });
        }
        let input = once.send();
        assert!(input.iter().any(|e| matches!(
            e,
            client::Effect::Io(client::Io::Send(protocol::ClientMsg::Input { .. }))
        )));
        assert!(!once.client.turn().running);
        let mut effects = once.interrupt();
        effects.extend(once.client.detach());
        let io: Vec<_> = effects
            .into_iter()
            .filter_map(|effect| match effect {
                client::Effect::Io(io) => Some(io),
                client::Effect::Show(_) => None,
            })
            .collect();
        assert!(
            matches!(
                io.as_slice(),
                [
                    client::Io::Send(protocol::ClientMsg::Interrupt { .. }),
                    client::Io::Send(protocol::ClientMsg::Detach),
                    client::Io::Hangup
                ]
            ),
            "{io:?}"
        );
        assert_eq!(once.ended.unwrap().unwrap(), ExitCode::from(130));
    }

    #[test]
    fn the_prompt_comes_first_and_stdin_after_a_blank_line() {
        assert_eq!(input(Some("sum up"), Some("a\nb\n")), "sum up\n\na\nb");
        assert_eq!(input(None, Some("hello\n")), "hello");
        assert_eq!(input(Some("hi"), None), "hi");
        assert_eq!(input(Some("hi"), Some("\n\n")), "hi");
        assert_eq!(input(None, Some("  \n")), "");
    }
    #[test]
    fn every_summary_follows_complete_effective_input_before_controls() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../../crates/protocol/tests/vectors/view.json"
        ))
        .unwrap();
        for vector in vectors
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["name"].as_str().unwrap().starts_with("approval reviews "))
        {
            let mut view = SessionView::default();
            for message in vector["messages"].as_array().unwrap() {
                view.fold(&DaemonMsg::from_json(message.to_string().as_bytes()).unwrap());
            }
            let p = view.present(&view.items[0]);
            let text = approval_message(&p, "id");
            let reason = text.find("Why:").unwrap();
            let summary = text.find("Model summary (unverified):").unwrap();
            assert!(reason < summary && summary < text.find("`a attach id`").unwrap());
            let markers: &[&str] = if vector["name"].as_str().unwrap().contains("multiline") {
                &[
                    "printf harmless\nrm -rf /tmp/approval-test-sentinel",
                    "60000",
                    "every argument",
                ]
            } else {
                &[
                    "/work/deep/config.ini",
                    "-allow = false",
                    "+allow = true",
                    "-delete = no",
                    "+delete = yes",
                    "flag",
                ]
            };
            for marker in markers {
                assert!(text.find(marker).unwrap() < reason, "{marker}: {text}");
            }
        }
    }
}
