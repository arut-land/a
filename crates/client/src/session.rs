//! A session as every client follows it, without I/O: the link to the
//! daemon as a state machine, the shared view folded from what the daemon
//! sends, and a person's intents turned into numbered messages. A driver
//! (see [`crate::Driver`]) carries out the [`Effect`]s and hands back
//! [`Event`]s; the app renders the rest.
//!
//! What every client used to decide on its own is decided here once:
//! reconnecting after a drain (by id, from the last seq, never sending the
//! environment again), a detach never reading as a lost daemon, a wedged
//! daemon, input given while reconnecting, and whether a turn runs, waits
//! on this client, or takes an interrupt or a steer.

use std::collections::VecDeque;
use std::path::PathBuf;

use protocol::view::{Change, SessionList, SessionView, Turn, Update};
use protocol::{ClientMsg, CommandInfo, DaemonMsg, Environment, Rejection, SessionSummary, Target};

use crate::{Cmd, Diagnosis, Welcome};

/// Where this client's link to the daemon stands.
#[derive(Clone, Debug)]
pub enum Link {
    /// Opening a connection and attaching.
    Connecting,
    /// Attached; `synced` once the replay of what was missed is in.
    Live {
        welcome: Welcome,
        /// The daemon is another host's.
        remote: bool,
        synced: bool,
    },
    /// The daemon finishes its work for a newer build, then closes the
    /// connection (or, while connecting, a draining daemon is waited out).
    Draining,
    /// Attaching again to the new daemon, from entry `since`.
    Reconnecting {
        since: u64,
    },
    /// The daemon took the connection but does not answer.
    Wedged(Diagnosis),
    Ended(End),
}

/// How a link ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    /// This client left; the session keeps running.
    Detached,
    /// The daemon is gone, or would not serve this client, as it says.
    Lost(String),
}

impl Link {
    /// Whether intents can still reach the daemon, now or once
    /// reconnected.
    pub const fn attached(&self) -> bool {
        !matches!(self, Self::Wedged(_) | Self::Ended(_))
    }
}

/// Which environment an attach gives the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Env {
    /// None: the session keeps the one it has.
    Keep,
    /// The origin's: this process's, or a remote daemon's bridge's.
    Origin,
    Given(Environment),
}

/// An attach to make on a new connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attach {
    pub target: Target,
    /// The entries after this one are replayed.
    pub since: u64,
    pub env: Env,
    /// The directory the daemon finds the workspace from; `None` is the
    /// environment's.
    pub workspace: Option<PathBuf>,
}

impl Attach {
    pub const fn new(target: Target, env: Env) -> Self {
        Self {
            target,
            since: 0,
            env,
            workspace: None,
        }
    }
}

/// What the driver tells the client, from connection `id`.
#[derive(Debug)]
pub enum Event {
    Connected {
        id: u64,
        welcome: Welcome,
        remote: bool,
        commands: Vec<CommandInfo>,
    },
    Msg {
        id: u64,
        msg: DaemonMsg,
    },
    /// A daemon draining for a newer build is waited out before the
    /// connection is made.
    Waiting {
        id: u64,
    },
    Closed {
        id: u64,
        why: String,
    },
    /// The daemon did not answer the hello.
    Wedged {
        id: u64,
        diagnosis: Diagnosis,
    },
    /// The connection could not be made.
    Failed {
        id: u64,
        why: String,
    },
}

/// What the client asks of its driver, and tells its app.
#[derive(Debug)]
pub enum Effect {
    /// Open connection `id` and make `attach` on it, replacing any other
    /// (`restart`: replacing the daemon first).
    Connect {
        id: u64,
        attach: Attach,
        restart: bool,
    },
    Send(ClientMsg),
    /// Capture this client's environment again and send it.
    SendEnvironment,
    /// Close the connection.
    Hangup,
    /// For the app: the view changed so.
    View(Update),
    /// For the app: [`SessionClient::link`] changed.
    Link,
    /// For the app: [`SessionClient::sessions`] changed.
    Sessions,
    /// For the app: something to tell the person.
    Notice(String),
}

impl Effect {
    /// Whether the driver carries it out, rather than the app.
    pub const fn is_io(&self) -> bool {
        matches!(
            self,
            Self::Connect { .. } | Self::Send(_) | Self::SendEnvironment | Self::Hangup
        )
    }
}

/// What a client's controls offer for the session's turn: computed here,
/// so no app derives it.
// Flags, as the apps' controls read them.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TurnView {
    /// A call is unsettled.
    pub running: bool,
    /// The oldest call waiting for an approval.
    pub asking: Option<u64>,
    pub asks: u32,
    /// An interrupt has something to stop.
    pub can_interrupt: bool,
    /// A steer reaches the running turn before its next step, rather than
    /// waiting for it as input.
    pub can_steer: bool,
    /// Input reaches the daemon, now or once reconnected.
    pub can_send: bool,
}

/// How many mutations are kept for an answer.
const UNANSWERED: usize = 64;

/// One session's client, sans I/O.
pub struct SessionClient {
    link: Link,
    view: SessionView,
    sessions: SessionList,
    /// The daemon's slash commands.
    commands: Vec<CommandInfo>,
    /// The connection events are taken from; older ones are replaced.
    id: u64,
    /// The environment an [`Cmd::Open`] attaches with.
    env: Env,
    /// The attach last asked for: a restart makes it again.
    opened: Attach,
    /// The number of the last mutation sent.
    seq: u64,
    /// Messages given while not live, sent once live.
    queued: Vec<ClientMsg>,
    /// Mutations sent and not yet answered: one refused because the daemon
    /// was draining is sent again, once, after the reattach.
    unanswered: VecDeque<ClientMsg>,
    turn: Turn,
}

const NOT_CONNECTED: &str = "not connected";
const REMOTE_ENV: &str =
    "the host's environment is captured when `a` connects; reattach to refresh it";

impl SessionClient {
    /// A client whose own opens attach with `env`.
    pub fn new(env: Env) -> Self {
        Self {
            link: Link::Connecting,
            view: SessionView::default(),
            sessions: SessionList::default(),
            commands: Vec::new(),
            id: 0,
            opened: Attach::new(Target::New, env.clone()),
            env,
            seq: 0,
            queued: Vec::new(),
            unanswered: VecDeque::new(),
            turn: Turn::default(),
        }
    }

    pub const fn link(&self) -> &Link {
        &self.link
    }

    pub const fn view(&self) -> &SessionView {
        &self.view
    }

    pub fn sessions(&self) -> &[SessionSummary] {
        &self.sessions.sessions
    }

    /// The daemon's slash commands, as its welcome advertised them.
    pub fn commands(&self) -> &[CommandInfo] {
        &self.commands
    }

    /// The connection events are taken from.
    pub const fn connection(&self) -> u64 {
        self.id
    }

    /// Whether the daemon is another host's.
    pub const fn remote(&self) -> bool {
        matches!(self.link, Link::Live { remote: true, .. })
    }

    pub const fn turn(&self) -> TurnView {
        let Turn {
            running,
            asking,
            asks,
        } = self.turn;
        TurnView {
            running,
            asking,
            asks,
            can_interrupt: running && self.link.attached(),
            can_steer: running && self.link.attached(),
            can_send: self.link.attached(),
        }
    }

    /// Attaches to `target` with this client's environment, on a new
    /// connection.
    pub fn open(&mut self, target: Target) -> Vec<Effect> {
        let attach = Attach::new(target, self.env.clone());
        self.attach(attach)
    }

    /// Makes `attach` on a new connection, leaving the current one.
    pub fn attach(&mut self, attach: Attach) -> Vec<Effect> {
        let mut out = Vec::new();
        if matches!(self.link, Link::Live { .. }) {
            out.push(Effect::Send(ClientMsg::Detach));
        }
        out.push(Effect::Hangup);
        self.opened = attach.clone();
        self.connect(attach, false, Link::Connecting, &mut out);
        out
    }

    /// Replaces a wedged daemon and attaches again: to the session shown,
    /// keeping its environment, or as first asked.
    pub fn restart(&mut self) -> Vec<Effect> {
        let attach = self.reattach().unwrap_or_else(|| self.opened.clone());
        let mut out = vec![Effect::Hangup];
        self.connect(attach, true, Link::Connecting, &mut out);
        out
    }

    /// Leaves the session, which keeps running; what was queued is
    /// dropped.
    pub fn detach(&mut self) -> Vec<Effect> {
        if matches!(self.link, Link::Ended(_)) {
            return Vec::new();
        }
        let mut out = Vec::new();
        if matches!(self.link, Link::Live { .. }) {
            out.push(Effect::Send(ClientMsg::Detach));
        }
        out.extend([Effect::Hangup, Effect::Link]);
        self.link = Link::Ended(End::Detached);
        self.queued.clear();
        out
    }

    /// Carries out `cmd`. [`Cmd::Help`] is the app's to show.
    pub fn run(&mut self, cmd: Cmd) -> Vec<Effect> {
        match cmd {
            Cmd::Input(text) => self.send(ClientMsg::input(text)),
            Cmd::Steer(text) => self.send(ClientMsg::Steer {
                text,
                client_seq: None,
            }),
            Cmd::Approve { call, approved } => self.send(ClientMsg::approve(call, approved)),
            Cmd::Interrupt => self.send(ClientMsg::INTERRUPT),
            Cmd::Detach => self.detach(),
            Cmd::RefreshEnv if self.remote() => vec![Effect::Notice(REMOTE_ENV.to_owned())],
            Cmd::RefreshEnv if matches!(self.link, Link::Live { .. }) => {
                vec![Effect::SendEnvironment]
            }
            Cmd::RefreshEnv => vec![Effect::Notice(NOT_CONNECTED.to_owned())],
            Cmd::Sessions => self.send(ClientMsg::List),
            Cmd::Open(target) => self.open(target),
            Cmd::Help => Vec::new(),
        }
    }

    /// Sends `msg` once live, numbered if it is a mutation.
    pub fn send(&mut self, msg: ClientMsg) -> Vec<Effect> {
        let mut out = Vec::new();
        self.put(msg, &mut out);
        out
    }

    fn put(&mut self, mut msg: ClientMsg, out: &mut Vec<Effect>) {
        if !matches!(self.link, Link::Live { .. }) {
            if self.link.attached() {
                self.queued.push(msg);
            } else {
                out.push(Effect::Notice(NOT_CONNECTED.to_owned()));
            }
            return;
        }
        if let Some(seq @ None) = msg.client_seq_mut() {
            self.seq += 1;
            *seq = Some(self.seq);
            if self.unanswered.len() == UNANSWERED {
                self.unanswered.pop_front();
            }
            self.unanswered.push_back(msg.clone());
        }
        out.push(Effect::Send(msg));
    }

    /// Takes in what the driver reports.
    pub fn step(&mut self, event: Event) -> Vec<Effect> {
        let mut out = Vec::new();
        let id = match &event {
            Event::Connected { id, .. }
            | Event::Msg { id, .. }
            | Event::Waiting { id }
            | Event::Closed { id, .. }
            | Event::Wedged { id, .. }
            | Event::Failed { id, .. } => *id,
        };
        // From a connection since replaced, or after the end.
        if id != self.id || matches!(self.link, Link::Ended(_)) {
            return out;
        }
        match event {
            Event::Connected {
                welcome,
                remote,
                commands,
                ..
            } => {
                self.link = Link::Live {
                    welcome,
                    remote,
                    synced: false,
                };
                self.commands = commands;
                // Neither answered nor refused on the last connection: sent
                // again, and the daemon, which logs who sent what, does each
                // once.
                out.extend(self.unanswered.iter().cloned().map(Effect::Send));
                out.push(Effect::Link);
                for msg in std::mem::take(&mut self.queued) {
                    self.put(msg, &mut out);
                }
            }
            Event::Msg { msg, .. } => self.msg(&msg, &mut out),
            Event::Waiting { .. } => {
                if matches!(self.link, Link::Connecting | Link::Reconnecting { .. }) {
                    self.link = Link::Draining;
                    out.push(Effect::Link);
                }
            }
            Event::Closed { why, .. } => match self.reattach() {
                Some(attach) if matches!(self.link, Link::Draining) => {
                    let since = attach.since;
                    self.connect(attach, false, Link::Reconnecting { since }, &mut out);
                }
                _ => self.end(End::Lost(why), &mut out),
            },
            Event::Wedged { diagnosis, .. } => {
                self.link = Link::Wedged(diagnosis);
                out.push(Effect::Link);
            }
            Event::Failed { why, .. } => self.end(End::Lost(why), &mut out),
        }
        out
    }

    fn msg(&mut self, msg: &DaemonMsg, out: &mut Vec<Effect>) {
        match msg {
            DaemonMsg::Sessions { .. } => {
                self.sessions.fold(msg);
                out.push(Effect::Sessions);
                return;
            }
            DaemonMsg::Ack { origin } => {
                self.unanswered.retain(|m| !numbered(m, origin.client_seq));
                return;
            }
            DaemonMsg::Rejected { origin, reason, .. } => {
                let at = self
                    .unanswered
                    .iter()
                    .position(|m| numbered(m, origin.client_seq));
                let refused = at.and_then(|at| self.unanswered.remove(at));
                if let (Some(mut again), Rejection::Draining) = (refused, reason) {
                    if let Some(seq) = again.client_seq_mut() {
                        *seq = None;
                    }
                    self.queued.push(again);
                    return;
                }
            }
            _ => {}
        }
        let Some(update) = self.view.fold(msg) else {
            return;
        };
        match &update.change {
            Change::Delta { .. } => {}
            Change::Synced => {
                if let Link::Live { synced, .. } = &mut self.link {
                    *synced = true;
                }
                self.turn = self.view.turn();
                out.push(Effect::View(update));
                out.push(Effect::Link);
                return;
            }
            Change::Draining => {
                self.link = Link::Draining;
                out.push(Effect::View(update));
                out.push(Effect::Link);
                return;
            }
            _ => self.turn = self.view.turn(),
        }
        out.push(Effect::View(update));
    }

    /// The attach that picks the session shown up again where the view
    /// left it, keeping the environment it has.
    fn reattach(&self) -> Option<Attach> {
        Some(Attach {
            target: Target::Id(self.view.session?),
            since: self.view.seq,
            env: Env::Keep,
            workspace: self.opened.workspace.clone(),
        })
    }

    fn connect(&mut self, attach: Attach, restart: bool, link: Link, out: &mut Vec<Effect>) {
        self.id += 1;
        self.link = link;
        out.push(Effect::Connect {
            id: self.id,
            attach,
            restart,
        });
        out.push(Effect::Link);
    }

    fn end(&mut self, end: End, out: &mut Vec<Effect>) {
        self.link = Link::Ended(end);
        self.queued.clear();
        out.extend([Effect::Hangup, Effect::Link]);
    }
}

const fn numbered(msg: &ClientMsg, seq: u64) -> bool {
    matches!(msg.client_seq(), Some(s) if s == seq)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use protocol::{
        Audience, Body, CallId, Caps, Entry, Mode, Origin, Permission, SessionId, Verdict,
    };

    use super::*;

    fn live(caps: Caps) -> Event {
        Event::Connected {
            id: 1,
            welcome: Welcome {
                proto: protocol::PROTO,
                caps,
                client: 1,
            },
            remote: false,
            commands: Vec::new(),
        }
    }

    fn msg(id: u64, msg: DaemonMsg) -> Event {
        Event::Msg { id, msg }
    }

    fn sends(effects: &[Effect]) -> Vec<&ClientMsg> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Send(m) => Some(m),
                _ => None,
            })
            .collect()
    }

    /// A client attached to a session that logged one message.
    fn attached(env: Env) -> (SessionClient, SessionId) {
        let mut client = SessionClient::new(env);
        let session = SessionId::new();
        client.open(Target::Latest {
            any_workspace: false,
        });
        client.step(live(Caps::ALL));
        client.step(msg(
            1,
            DaemonMsg::Attached {
                session,
                workspace: PathBuf::from("/w"),
            },
        ));
        client.step(msg(
            1,
            DaemonMsg::Entry(Entry::new(
                1,
                1,
                Body::Input {
                    text: "hi".into(),
                    mode: Mode::Queue,
                    taken: true,
                    origin: None,
                },
            )),
        ));
        client.step(msg(1, DaemonMsg::Synced { seq: 1 }));
        (client, session)
    }

    #[test]
    fn a_reattach_after_drain_never_resends_the_environment() {
        let (mut client, session) = attached(Env::Origin);
        client.step(msg(1, DaemonMsg::Draining));
        assert!(matches!(client.link(), Link::Draining));
        let out = client.step(Event::Closed {
            id: 1,
            why: "closed".into(),
        });
        let [
            Effect::Connect {
                id,
                attach,
                restart,
            },
            Effect::Link,
        ] = &out[..]
        else {
            panic!("{out:?}");
        };
        assert_eq!((*id, *restart), (2, false));
        assert_eq!(attach.target, Target::Id(session));
        assert_eq!((attach.since, &attach.env), (1, &Env::Keep));
        assert!(matches!(client.link(), Link::Reconnecting { since: 1 }));

        // A restart of a wedged daemon picks the session up the same way.
        client.step(Event::Wedged {
            id: 2,
            diagnosis: Diagnosis::default(),
        });
        let out = client.restart();
        assert!(
            matches!(&out[1], Effect::Connect { attach, restart: true, .. } if attach.env == Env::Keep),
            "{out:?}"
        );
    }

    #[test]
    fn detach_is_never_reported_as_a_lost_daemon() {
        let (mut client, _) = attached(Env::Origin);
        let out = client.detach();
        assert!(matches!(sends(&out)[..], [ClientMsg::Detach]));
        // The daemon closing the connection is the answer, not a loss.
        let out = client.step(Event::Closed {
            id: 1,
            why: "closed".into(),
        });
        assert!(out.is_empty(), "{out:?}");
        assert!(matches!(client.link(), Link::Ended(End::Detached)));
        assert!(!client.turn().can_send);

        // Without a detach, it is.
        let (mut client, _) = attached(Env::Origin);
        client.step(Event::Closed {
            id: 1,
            why: "gone".into(),
        });
        assert!(matches!(client.link(), Link::Ended(End::Lost(why)) if why == "gone"));
    }

    #[test]
    fn events_from_a_replaced_connection_are_dropped() {
        let (mut client, _) = attached(Env::Origin);
        let out = client.open(Target::New);
        assert!(matches!(sends(&out)[..], [ClientMsg::Detach]));
        let out = client.step(Event::Closed {
            id: 1,
            why: "detached".into(),
        });
        assert!(out.is_empty());
        assert!(matches!(client.link(), Link::Connecting));
    }

    #[test]
    fn input_while_reconnecting_is_sent_once_live_and_numbered_across_connections() {
        let (mut client, _) = attached(Env::Origin);
        let out = client.run(Cmd::Input("one".into()));
        assert!(matches!(
            sends(&out)[..],
            [ClientMsg::Input {
                client_seq: Some(1),
                ..
            }]
        ));
        client.step(msg(1, DaemonMsg::Draining));
        assert!(client.run(Cmd::Input("two".into())).is_empty());
        client.step(Event::Closed {
            id: 1,
            why: String::new(),
        });
        let mut reconnected = live(Caps::ALL);
        if let Event::Connected { id, .. } = &mut reconnected {
            *id = 2;
        }
        let out = client.step(reconnected);
        // Unanswered, "one" goes again under its number: a daemon that keeps
        // who sent what does it once.
        assert!(
            matches!(&sends(&out)[..], [
                ClientMsg::Input { text: one, client_seq: Some(1) },
                ClientMsg::Input { text: two, client_seq: Some(2) },
            ] if one == "one" && two == "two"),
            "{out:?}"
        );
    }

    #[test]
    fn a_resend_after_reconnect_runs_once() {
        let (mut client, _) = attached(Env::Origin);
        client.run(Cmd::Input("late".into()));
        // The daemon began draining as it read it: refused, so not done.
        let refused = client.step(msg(
            1,
            DaemonMsg::Rejected {
                origin: Origin {
                    client: 1,
                    client_seq: 1,
                },
                reason: Rejection::Draining,
                message: "send that again".into(),
            },
        ));
        assert!(refused.is_empty(), "no error shown: {refused:?}");
        client.step(msg(1, DaemonMsg::Draining));
        client.step(Event::Closed {
            id: 1,
            why: String::new(),
        });
        let mut reconnected = live(Caps::ALL);
        if let Event::Connected { id, .. } = &mut reconnected {
            *id = 2;
        }
        let out = client.step(reconnected);
        assert_eq!(sends(&out).len(), 1, "{out:?}");
        // Taken this time: a second drain does not send it again.
        client.step(msg(
            2,
            DaemonMsg::Ack {
                origin: Origin {
                    client: 2,
                    client_seq: 2,
                },
            },
        ));
        client.step(msg(2, DaemonMsg::Draining));
        client.step(Event::Closed {
            id: 2,
            why: String::new(),
        });
        let mut again = live(Caps::ALL);
        if let Event::Connected { id, .. } = &mut again {
            *id = 3;
        }
        assert!(sends(&client.step(again)).is_empty());
    }

    #[test]
    fn the_turn_says_what_runs_and_what_waits_on_this_client() {
        let (mut client, _) = attached(Env::Origin);
        assert!(!client.turn().running);
        client.step(msg(
            1,
            DaemonMsg::Entry(Entry::new(
                2,
                1,
                Body::ToolCall {
                    tool: "bash".into(),
                    input: serde_json::json!({ "command": "ls" }).into(),
                    id: None,
                    card: None,
                    by: Audience::Model,
                },
            )),
        ));
        let turn = client.turn();
        assert!(turn.running && turn.can_interrupt && turn.can_steer);
        client.step(msg(
            1,
            DaemonMsg::Entry(Entry::new(
                3,
                1,
                Body::Decision {
                    by: "policy".into(),
                    call: CallId::new(2),
                    d: Verdict::Permission {
                        permission: Permission::Ask,
                    },
                },
            )),
        ));
        assert_eq!(client.turn().asking, Some(2));
        assert_eq!(client.turn().asks, 1);
    }

    #[test]
    fn a_wedged_daemon_ends_nothing_until_restarted() {
        let mut client = SessionClient::new(Env::Origin);
        client.open(Target::New);
        client.step(Event::Wedged {
            id: 1,
            diagnosis: Diagnosis::default(),
        });
        assert!(matches!(client.link(), Link::Wedged(_)));
        let out = client.restart();
        assert!(
            matches!(&out[1], Effect::Connect { id: 2, attach, restart: true } if attach.target == Target::New && attach.env == Env::Origin),
            "{out:?}"
        );
    }
}
