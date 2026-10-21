//! `a` in a pipeline: one turn, on a prompt from `-p` and whatever stdin
//! holds, in a new session (or the one `resume`/`attach` names). What the
//! agent says goes to stdout as it streams, nothing else does, and the exit
//! status says how the turn ended (docs/cli.md).

use std::io::{self, IsTerminal, Read};
use std::process::ExitCode;
use std::thread;

use client::{Driver, End, Env, Event, Link, Renderer, SessionClient, Show};
use protocol::view::{Buttons, Change, Glyph, Item, Kind, Part, Shown, Status};
use protocol::{CallId, Target, Verdict};
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
    let (tx, rx) = client::inbox();
    let events = tx.clone();
    let runtime: rt::Rt = std::sync::Arc::new(rt::Threads::default());
    let driver = Driver::new(move |_, event| {
        let _ = events.send(Input::Client(event));
    })
    .start(runtime.clone(), daemon.clone());
    let stdin = tx.clone();
    thread::spawn(move || {
        let text = (!io::stdin().is_terminal()).then(|| {
            let mut bytes = Vec::new();
            let _ = io::stdin().lock().read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        });
        let _ = stdin.send(Input::Stdin(text));
    });
    // Ctrl-C interrupts the turn; where signals are not caught it ends `a`
    // as it would anything else.
    if let Ok(signals) = Signals::catch(&[Signal::Interrupt]) {
        thread::spawn(move || {
            while !signals.wait().is_empty() {
                if tx.send(Input::Interrupt).is_err() {
                    return;
                }
            }
        });
    }
    let mut once = Once {
        client: SessionClient::new(Env::Origin),
        prompt,
        mode,
        read: false,
        stdin: None,
        synced: false,
        turn: Turn::Unsent,
        streaming: None,
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
        let Some(input) = rx.recv() else {
            return Ok(Ended::Failed.into());
        };
        effects = match input {
            Input::Client(event) => once.client.step(event),
            Input::Stdin(text) => {
                (once.read, once.stdin) = (true, text);
                once.send()
            }
            Input::Interrupt => once.interrupt(),
        };
    }
}

/// Where the input stands.
#[derive(PartialEq, Eq)]
enum Turn {
    Unsent,
    Sent(String),
    /// The session took it up: the turn running is ours.
    Ours,
}

struct Once {
    client: SessionClient,
    /// `-p`'s prompt.
    prompt: Option<String>,
    /// `--mode`: set with the policy's `/mode` before the prompt goes.
    mode: Option<String>,
    /// Stdin is read (or a terminal, not to be read).
    read: bool,
    /// What it held.
    stdin: Option<String>,
    /// The replay of the attach is in.
    synced: bool,
    turn: Turn,
    /// The call whose reply is streaming.
    streaming: Option<CallId>,
    failed: bool,
    ended: Option<io::Result<ExitCode>>,
}

impl Once {
    /// Sends the input once attached and stdin is read.
    fn send(&mut self) -> Vec<client::Effect> {
        if !self.synced || !self.read || self.turn != Turn::Unsent {
            return Vec::new();
        }
        let text = input(self.prompt.as_deref(), self.stdin.as_deref());
        if text.is_empty() {
            eprintln!("a: nothing to send: give a prompt with -p, or on stdin");
            self.end(Ended::Usage);
            return Vec::new();
        }
        let mut effects = Vec::new();
        if let Some(mode) = &self.mode {
            effects = self.client.run(client::Cmd::Input(format!("/mode {mode}")));
        }
        self.turn = Turn::Sent(text.clone());
        effects.extend(self.client.run(client::Cmd::Input(text)));
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
        match self.client.link().clone() {
            Link::Reconnecting { .. } => eprintln!("[daemon upgraded; reconnecting]"),
            Link::Draining => eprintln!("[waiting for the previous daemon to finish its work]"),
            Link::Wedged(d) => {
                crate::report(&d);
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

    fn view(&mut self, change: &Change) {
        match change {
            Change::Synced => self.synced = true,
            Change::Error { message } => {
                // Nothing to attach to (no such session), or the input
                // refused.
                self.failed = true;
                self.fail(io::Error::other(message.clone()));
            }
            Change::Mismatch { supported } => {
                eprintln!("[{}]", protocol::view::mismatch_line(*supported));
            }
            Change::Added {
                item: Item::User { text, .. },
                ..
            } if matches!(&self.turn, Turn::Sent(sent) if sent == text) => self.turn = Turn::Ours,
            Change::Updated {
                index,
                part: Part::Outcome,
            } if self.turn != Turn::Ours => {
                let p = self.client.view().items[*index].present();
                if p.kind == Kind::Command && p.glyph == Glyph::Bad {
                    let why = p.body.text();
                    let why = why.lines().next().unwrap_or(&p.status);
                    self.fail(io::Error::other(format!("{}: {why}", p.title)));
                }
            }
            _ if self.turn != Turn::Ours => {}
            Change::Delta { call, text } => {
                self.streaming = Some(*call);
                crate::out(format_args!("{text}"));
            }
            Change::Added { item, .. } => match item {
                Item::Assistant { text, .. } => crate::out(format_args!("{text}\n")),
                // A retry streams its reply anew, from its start.
                Item::Hook {
                    call: Some(call),
                    decision: Verdict::Retry { .. },
                    ..
                } if self.streaming.take_if(|s| s == call).is_some() => {
                    crate::out(format_args!("\n"));
                }
                _ => {}
            },
            Change::Updated {
                index,
                part: Part::Outcome,
            } => self.settled(*index),
            _ => {}
        }
    }

    /// A reply streamed is ended; one that was not is written whole.
    fn settled(&mut self, index: usize) {
        let item = &self.client.view().items[index];
        let Item::Reply { status, .. } = item else {
            return;
        };
        let p = item.present();
        if p.kind != Kind::Reply {
            return;
        }
        if self.streaming.take_if(|s| s.0 == item.key()).is_some() {
            crate::out(format_args!("\n"));
        } else if let Shown::Markdown(text) = p.body
            && !text.is_empty()
        {
            crate::out(format_args!("{text}\n"));
        }
        match status {
            Status::Failed { .. } => {
                eprintln!("a: {}", p.status);
                self.failed = true;
            }
            Status::Interrupted => self.end(Ended::Interrupted),
            Status::Running | Status::Done => {}
        }
    }

    /// Only a complete kernel burst can certify that our turn ended.
    fn settle(&mut self) {
        if self.turn != Turn::Ours {
            return;
        }
        let view = self.client.view();
        let turn = self.client.turn();
        if turn
            .activity
            .is_some_and(protocol::Activity::requires_action)
        {
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
                    ))
                }
                _ => None,
            });
            match approval {
                Some((tool, title, explanation)) => eprintln!(
                    "a: {tool} ({title}) waits for approval in session {id}.\n{explanation}"
                ),
                None => eprintln!("a: the turn waits for an answer in session {id}: {answer}"),
            }
            self.end(Ended::Waiting);
        } else if !turn.running {
            self.end(if self.failed {
                Ended::Failed
            } else {
                Ended::Done
            });
        }
    }
}

fn approval_message(p: &protocol::view::Presented<'_>, id: &str) -> String {
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
                Show::Notice(notice) => eprintln!("[{notice}]"),
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
        Activity, Audience, Body, DaemonMsg, Entry, Epoch, Json, Mode, Outcome, Seq, SessionId,
    };
    use protocol::{Decode, view::SessionView};

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
    fn a_turn_ends_when_idle_arrives_after_its_last_entry() {
        let mut once = Once {
            client: SessionClient::new(Env::Keep),
            prompt: None,
            mode: None,
            read: true,
            stdin: None,
            synced: true,
            turn: Turn::Sent("sleep 0".into()),
            streaming: None,
            failed: false,
            ended: None,
        };
        let session = SessionId::new();
        receive(
            &mut once,
            DaemonMsg::Attached {
                session,
                workspace: "/w".into(),
            },
        );
        receive(
            &mut once,
            DaemonMsg::Activity {
                seq: 0,
                activity: Activity::Idle,
            },
        );
        let entries = [
            Body::Input {
                text: "sleep 0".into(),
                mode: Mode::Queue,
                taken: true,
                origin: None,
            },
            Body::ToolCall {
                invoke: None,
                tool: "sleep".into(),
                input: Json::encode(&0).unwrap(),
                id: None,
                card: None,
                by: Audience::Model,
            },
            Body::ToolResult {
                call: CallId::new(2).unwrap(),
                outcome: Outcome::Done {
                    output: Json::encode(&0).unwrap(),
                },
                card: None,
            },
        ];
        for (body, seq) in entries.into_iter().zip(1..) {
            receive(
                &mut once,
                DaemonMsg::Entry(Entry::new(Seq::new(seq).unwrap(), Epoch(0), body)),
            );
            assert!(once.ended.is_none(), "an entry prefix cannot end the turn");
        }
        assert_eq!(once.client.view().activity, None);
        receive(
            &mut once,
            DaemonMsg::Sessions {
                sessions: vec![protocol::SessionSummary {
                    id: session,
                    workspace: "/w".into(),
                    entries: 3,
                    clients: 1,
                    activity: Activity::Running,
                    residency: protocol::Residency::Loaded,
                    read_only: None,
                }],
            },
        );
        assert!(once.ended.is_none());
        receive(
            &mut once,
            DaemonMsg::Activity {
                seq: 3,
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
            read: true,
            stdin: None,
            synced: true,
            turn: Turn::Unsent,
            streaming: None,
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
