//! A session as every client follows it, without I/O: the link to the
//! daemon as a state machine, the shared view folded from what the daemon
//! sends, and a person's intents turned into numbered messages. A driver
//! (see [`crate::Driver`]) carries out the [`Effect`]s and hands back
//! [`Event`]s; the app renders the rest.
//!
//! What every client used to decide on its own is decided here once:
//! reconnecting after a drain (by id, from the last seq, never sending the
//! environment again), a detach never reading as a lost daemon, a wedged
//! daemon, input given while reconnecting, and which intents the session
//! takes now ([`Offer`]): an app cannot build a steer no turn is running
//! for, or an approval no call waits for. Turn completion comes only from
//! ordered kernel activity, never from the ledger at an entry prefix.

use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;

use protocol::view::{Change, SessionList, SessionView, Update};
use protocol::{
    Activity, CallId, ClientMsg, CommandInfo, DaemonMsg, Environment, Mode, Rejection,
    SessionSummary, Target,
};

use crate::{Cmd, Diagnosis, Error, Welcome};

/// A connection, numbered by the client that asked for it: events from
/// one it has since replaced are dropped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(pub u64);

/// Where this client's link to the daemon stands.
#[derive(Clone, Debug)]
pub enum Link {
    /// Opening a connection and attaching.
    Connecting,
    /// Attached; the replay of what was missed is coming in.
    Replaying {
        welcome: Welcome,
        /// The daemon is another host's.
        remote: bool,
    },
    /// Attached and synced: what arrives now is live.
    Live {
        welcome: Welcome,
        remote: bool,
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
    /// The daemon is gone, or would not serve this client.
    Lost(Error),
}

impl Link {
    /// Whether intents can still reach the daemon, now or once
    /// reconnected.
    pub const fn attached(&self) -> bool {
        !matches!(self, Self::Wedged(_) | Self::Ended(_))
    }

    /// Whether the connection is up: what is sent goes out now.
    pub const fn up(&self) -> bool {
        matches!(self, Self::Replaying { .. } | Self::Live { .. })
    }

    /// Whether the daemon is another host's.
    pub const fn remote(&self) -> bool {
        matches!(
            self,
            Self::Replaying { remote: true, .. } | Self::Live { remote: true, .. }
        )
    }
}

/// Which environment an attach gives the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Env {
    /// None: the session keeps the one it has, and nothing of this
    /// process's is captured.
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

/// What the driver tells the client, from connection `conn`.
#[derive(Debug)]
pub struct Event {
    pub conn: ConnId,
    pub kind: EventKind,
}

#[derive(Debug)]
pub enum EventKind {
    Connected {
        welcome: Welcome,
        remote: bool,
        commands: Vec<CommandInfo>,
    },
    Msg(DaemonMsg),
    /// A daemon draining for a newer build is waited out before the
    /// connection is made.
    Waiting,
    /// The connection closed, as `Error` says.
    Closed(Error),
    /// The daemon did not answer the hello.
    Wedged(Diagnosis),
    /// The connection could not be made.
    Failed(Error),
}

/// What the client asks of its driver ([`Io`]) and tells its app
/// ([`Show`]).
#[derive(Debug)]
pub enum Effect {
    Io(Io),
    Show(Show),
}

/// What the driver does for the client.
#[derive(Debug)]
pub enum Io {
    /// Open connection `conn` and make `attach` on it, replacing any
    /// other (`restart`: replacing the daemon first).
    Connect {
        conn: ConnId,
        attach: Attach,
        restart: bool,
    },
    Send(ClientMsg),
    /// Capture this client's environment again and send it.
    SendEnvironment,
    /// Close the connection.
    Hangup,
}

/// What the app renders.
#[derive(Debug)]
pub enum Show {
    /// The view changed so.
    View(Update),
    /// [`SessionClient::link`] changed.
    Link,
    /// [`SessionClient::sessions`] changed.
    Sessions,
    /// Something to tell the person.
    Notice(Notice),
}

/// Why an intent went nowhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    NotConnected,
    Backpressure,
    /// A steer or an interrupt while no turn runs.
    NothingRunning,
    /// An answer for a call that is not waiting for one.
    NotAsking(CallId),
    /// `/env refresh` on a remote daemon, whose environment is its
    /// bridge's.
    RemoteEnv,
    /// The driver could not send a message.
    NotSent(Error),
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConnected => f.write_str("not connected"),
            Self::Backpressure => {
                f.write_str("waiting for the daemon to acknowledge pending input")
            }
            Self::NothingRunning => f.write_str("nothing is running"),
            Self::NotAsking(call) => write!(f, "call {call} is not waiting for an answer"),
            Self::RemoteEnv => f.write_str(
                "the host's environment is captured when `a` connects; reattach to refresh it",
            ),
            Self::NotSent(e) => write!(f, "not sent: {e}"),
        }
    }
}

/// What a client's controls offer for the session's turn: computed here,
/// so no app derives it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TurnView {
    /// The kernel's activity at the current entry, unknown during a burst.
    pub activity: Option<Activity>,
    /// Working, including while the next complete burst is arriving.
    pub running: bool,
    /// The oldest call waiting for an approval.
    pub asking: Option<CallId>,
    pub asks: u32,
    /// An interrupt has something to stop, and a steer a turn to reach.
    pub can_interrupt: bool,
    /// Input reaches the daemon, now or once reconnected.
    pub can_send: bool,
}

/// How many mutations are kept for an answer.
const UNANSWERED: usize = 64;

/// One session's client, sans I/O.
pub struct SessionClient {
    link: Link,
    view: SessionView,
    view_current: bool,
    sessions: SessionList,
    /// The daemon's slash commands.
    commands: Vec<CommandInfo>,
    /// The connection events are taken from; older ones are replaced.
    conn: ConnId,
    /// The environment an [`Cmd::Open`] attaches with.
    env: Env,
    /// The attach last asked for: a restart makes it again.
    opened: Attach,
    /// The number of the last mutation sent.
    seq: u64,
    /// Messages given while the connection is down, sent once it is up.
    queued: Vec<ClientMsg>,
    /// Mutations sent and not yet answered: one refused because the daemon
    /// was draining is sent again, once, after the reattach.
    unanswered: VecDeque<ClientMsg>,
}

impl SessionClient {
    /// A client whose own opens attach with `env`.
    pub fn new(env: Env) -> Self {
        Self {
            link: Link::Connecting,
            view: SessionView::default(),
            view_current: false,
            sessions: SessionList::default(),
            commands: Vec::new(),
            conn: ConnId::default(),
            opened: Attach::new(Target::New, env.clone()),
            env,
            seq: 0,
            queued: Vec::new(),
            unanswered: VecDeque::new(),
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
    pub const fn connection(&self) -> ConnId {
        self.conn
    }

    pub fn turn(&self) -> TurnView {
        if !self.view_current {
            return TurnView {
                can_send: self.link.attached(),
                ..TurnView::default()
            };
        }
        let ledger = &self.view.ledger;
        let activity = self.view.activity;
        let running = activity != Some(Activity::Idle);
        let asking = ledger.inbox().next();
        let asks = u32::try_from(ledger.inbox().count()).unwrap_or(u32::MAX);
        TurnView {
            activity,
            running,
            asking,
            asks,
            can_interrupt: running && self.link.attached(),
            can_send: self.link.attached(),
        }
    }

    /// What this client may ask of the session now; nothing unless
    /// attached, now or once reconnected.
    pub fn offer(&mut self) -> Result<Offer<'_>, Notice> {
        if !self.link.attached() {
            Err(Notice::NotConnected)
        } else if self.unanswered.len() + self.queued.len() >= UNANSWERED {
            Err(Notice::Backpressure)
        } else {
            Ok(Offer(self))
        }
    }

    /// Attaches to `target` with this client's environment, on a new
    /// connection.
    pub fn open(&mut self, target: Target) -> Vec<Effect> {
        let env = if matches!(target, Target::Fork { .. }) {
            Env::Keep
        } else {
            self.env.clone()
        };
        let attach = Attach::new(target, env);
        self.attach(attach)
    }

    /// Makes `attach` on a new connection, leaving the current one.
    pub fn attach(&mut self, attach: Attach) -> Vec<Effect> {
        let mut out = self.leave();
        if !matches!((&attach.target, &self.opened.target), (Target::Id(a), Target::Id(b)) if a == b)
        {
            self.unanswered.clear();
            self.queued.clear();
            self.view_current = false;
        }
        self.opened = attach.clone();
        self.connect(attach, false, Link::Connecting, &mut out);
        out
    }

    /// Replaces a wedged daemon and repeats the current attachment,
    /// keeping a known session's environment.
    pub fn restart(&mut self) -> Vec<Effect> {
        let attach = self.reattach().unwrap_or_else(|| self.opened.clone());
        let mut out = vec![Effect::Io(Io::Hangup)];
        self.connect(attach, true, Link::Connecting, &mut out);
        out
    }

    /// Leaves the session, which keeps running; what was queued is
    /// dropped.
    pub fn detach(&mut self) -> Vec<Effect> {
        if matches!(self.link, Link::Ended(_)) {
            return Vec::new();
        }
        let mut out = self.leave();
        self.end(End::Detached, &mut out);
        out
    }

    /// The daemon is told when the connection is up; the connection goes
    /// either way.
    fn leave(&self) -> Vec<Effect> {
        let mut out = Vec::new();
        if self.link.up() {
            out.push(Effect::Io(Io::Send(ClientMsg::Detach)));
        }
        out.push(Effect::Io(Io::Hangup));
        out
    }

    /// Carries out `cmd`; what the session does not offer is a
    /// [`Notice`]. [`Cmd::Help`] is the app's to show.
    pub fn run(&mut self, cmd: Cmd) -> Vec<Effect> {
        let done = match cmd {
            Cmd::Input(text) => self.offer().map(|o| o.input(text)),
            Cmd::Steer(text) => self.offer().and_then(|o| o.steer(text)),
            Cmd::Approve { call, approved } => self.offer().and_then(|o| o.approve(call, approved)),
            Cmd::Interrupt => self.offer().and_then(Offer::interrupt),
            Cmd::RefreshEnv => self.offer().and_then(Offer::refresh_env),
            Cmd::Detach => Ok(self.detach()),
            Cmd::Sessions => self.offer().map(|o| o.0.send(ClientMsg::List)),
            Cmd::Open(target) => Ok(self.open(target)),
            Cmd::Fork(seq) => match self.view.session {
                Some(session) => self.offer().map(|offer| {
                    offer.0.send(ClientMsg::Attach {
                        target: Target::Fork {
                            session: session.to_string(),
                            seq,
                        },
                        workspace: PathBuf::new(),
                        since: 0,
                        env: None,
                        decl: protocol::ClientDecl::default(),
                    })
                }),
                None => Err(Notice::NotConnected),
            },
            Cmd::Help => Ok(Vec::new()),
        };
        done.unwrap_or_else(|notice| vec![Effect::Show(Show::Notice(notice))])
    }

    /// Sends `msg` once the connection is up, numbered if it is a
    /// mutation.
    fn send(&mut self, msg: ClientMsg) -> Vec<Effect> {
        let mut out = Vec::new();
        self.put(msg, &mut out);
        out
    }

    fn put(&mut self, mut msg: ClientMsg, out: &mut Vec<Effect>) {
        if !self.link.up() {
            self.queued.push(msg);
            return;
        }
        if let Some(seq @ None) = msg.client_seq_mut() {
            self.seq += 1;
            *seq = Some(self.seq);
            self.unanswered.push_back(msg.clone());
        }
        out.push(Effect::Io(Io::Send(msg)));
    }

    /// Takes in what the driver reports.
    pub fn step(&mut self, event: Event) -> Vec<Effect> {
        let mut out = Vec::new();
        // From a connection since replaced, or after the end.
        if event.conn != self.conn || matches!(self.link, Link::Ended(_)) {
            return out;
        }
        match event.kind {
            EventKind::Connected {
                welcome,
                remote,
                commands,
            } => {
                self.commands = commands;
                // Neither answered nor refused on the last connection: sent
                // again, and the daemon, which logs who sent what, does each
                // once.
                out.extend(
                    self.unanswered
                        .iter()
                        .cloned()
                        .map(|msg| Effect::Io(Io::Send(msg))),
                );
                self.go(Link::Replaying { welcome, remote }, &mut out);
                for msg in std::mem::take(&mut self.queued) {
                    self.put(msg, &mut out);
                }
            }
            EventKind::Msg(msg) => self.msg(&msg, &mut out),
            EventKind::Waiting => {
                if matches!(self.link, Link::Connecting | Link::Reconnecting { .. }) {
                    self.go(Link::Draining, &mut out);
                }
            }
            EventKind::Closed(why) => match self.reattach() {
                Some(attach) if matches!(self.link, Link::Draining) => {
                    let since = attach.since;
                    self.connect(attach, false, Link::Reconnecting { since }, &mut out);
                }
                _ => self.end(End::Lost(why), &mut out),
            },
            EventKind::Wedged(diagnosis) => self.go(Link::Wedged(diagnosis), &mut out),
            EventKind::Failed(why) => self.end(End::Lost(why), &mut out),
        }
        out
    }

    fn msg(&mut self, msg: &DaemonMsg, out: &mut Vec<Effect>) {
        match msg {
            DaemonMsg::Attached { session, .. } => {
                self.opened.target = Target::Id(*session);
                self.view_current = true;
            }
            DaemonMsg::Sessions { .. } => {
                self.sessions.fold(msg);
                out.push(Effect::Show(Show::Sessions));
                return;
            }
            DaemonMsg::Ack { client_seq } => {
                self.unanswered.retain(|m| !numbered(m, *client_seq));
                return;
            }
            DaemonMsg::Rejected {
                client_seq, reason, ..
            } => {
                let at = self
                    .unanswered
                    .iter()
                    .position(|m| numbered(m, *client_seq));
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
            Change::Synced => {
                if let Link::Replaying { welcome, remote } = self.link {
                    self.link = Link::Live { welcome, remote };
                }
                out.push(Effect::Show(Show::View(update)));
                out.push(Effect::Show(Show::Link));
                return;
            }
            Change::Draining => {
                self.link = Link::Draining;
                out.push(Effect::Show(Show::View(update)));
                out.push(Effect::Show(Show::Link));
                return;
            }
            _ => {}
        }
        out.push(Effect::Show(Show::View(update)));
    }

    /// Reattaches to the current target, resuming the view only when it
    /// belongs to that session.
    fn reattach(&self) -> Option<Attach> {
        let Target::Id(session) = self.opened.target else {
            return None;
        };
        Some(Attach {
            target: Target::Id(session),
            since: if self.view.session == Some(session) {
                self.view.seq
            } else {
                0
            },
            env: Env::Keep,
            workspace: self.opened.workspace.clone(),
        })
    }

    fn connect(&mut self, attach: Attach, restart: bool, link: Link, out: &mut Vec<Effect>) {
        self.conn.0 += 1;
        out.push(Effect::Io(Io::Connect {
            conn: self.conn,
            attach,
            restart,
        }));
        self.go(link, out);
    }

    fn end(&mut self, end: End, out: &mut Vec<Effect>) {
        self.queued.clear();
        self.go(Link::Ended(end), out);
    }

    /// Moves the link to `link`, which the app is told of.
    fn go(&mut self, link: Link, out: &mut Vec<Effect>) {
        self.link = link;
        out.push(Effect::Show(Show::Link));
    }
}

/// What the session takes from this client now: made by
/// [`SessionClient::offer`] only while attached, and each intent only
/// while the turn is in a state to take it.
pub struct Offer<'a>(&'a mut SessionClient);

impl Offer<'_> {
    /// A message, or a slash command the daemon runs.
    pub fn input(self, text: String) -> Vec<Effect> {
        self.0.send(ClientMsg::input(text))
    }

    /// Input for the running turn, before its next step.
    pub fn steer(self, text: String) -> Result<Vec<Effect>, Notice> {
        self.running()?;
        Ok(self.0.send(ClientMsg::Input {
            text,
            mode: Mode::Steer,
            client_seq: None,
        }))
    }

    /// Stops the running turn.
    pub fn interrupt(self) -> Result<Vec<Effect>, Notice> {
        self.running()?;
        Ok(self.0.send(ClientMsg::INTERRUPT))
    }

    /// Answers the approval call `call` waits for.
    pub fn approve(self, call: CallId, approved: bool) -> Result<Vec<Effect>, Notice> {
        self.view()?;
        if !self.0.view.ledger.inbox().any(|c| c == call) {
            return Err(Notice::NotAsking(call));
        }
        Ok(self.0.send(ClientMsg::approve(call, approved)))
    }

    /// Sends this client's environment again.
    pub fn refresh_env(self) -> Result<Vec<Effect>, Notice> {
        if self.0.link.remote() {
            return Err(Notice::RemoteEnv);
        }
        if !self.0.link.up() {
            return Err(Notice::NotConnected);
        }
        Ok(vec![Effect::Io(Io::SendEnvironment)])
    }

    fn view(&self) -> Result<(), Notice> {
        self.0
            .view_current
            .then_some(())
            .ok_or(Notice::NotConnected)
    }

    fn running(&self) -> Result<(), Notice> {
        self.view()?;
        if self.0.turn().running {
            Ok(())
        } else {
            Err(Notice::NothingRunning)
        }
    }
}

const fn numbered(msg: &ClientMsg, seq: u64) -> bool {
    matches!(msg.client_seq(), Some(s) if s == seq)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use protocol::{Audience, Body, Caps, Entry, Epoch, Json, Permission, Seq, SessionId, Verdict};

    use super::*;

    fn live(conn: u64, caps: Caps) -> Event {
        Event {
            conn: ConnId(conn),
            kind: EventKind::Connected {
                welcome: Welcome {
                    proto: protocol::PROTO,
                    caps,
                },
                remote: false,
                commands: Vec::new(),
            },
        }
    }

    fn msg(conn: u64, msg: DaemonMsg) -> Event {
        Event {
            conn: ConnId(conn),
            kind: EventKind::Msg(msg),
        }
    }

    fn closed(conn: u64) -> Event {
        Event {
            conn: ConnId(conn),
            kind: EventKind::Closed(Error::Closed),
        }
    }

    fn sends(effects: &[Effect]) -> Vec<&ClientMsg> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Io(Io::Send(m)) => Some(m),
                _ => None,
            })
            .collect()
    }

    fn connects(effects: &[Effect]) -> Vec<(ConnId, &Attach, bool)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Io(Io::Connect {
                    conn,
                    attach,
                    restart,
                }) => Some((*conn, attach, *restart)),
                _ => None,
            })
            .collect()
    }

    fn notices(effects: &[Effect]) -> Vec<&Notice> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Show(Show::Notice(n)) => Some(n),
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
        client.step(live(1, Caps::ALL));
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
                Seq::new(1).unwrap(),
                Epoch(1),
                Body::Input {
                    text: "hi".into(),
                    mode: Mode::Queue,
                    taken: true,
                    origin: None,
                },
            )),
        ));
        client.step(msg(
            1,
            DaemonMsg::Activity {
                seq: 1,
                activity: Activity::Idle,
            },
        ));
        client.step(msg(1, DaemonMsg::Synced { seq: 1 }));
        (client, session)
    }

    fn bash_call(seq: u64) -> DaemonMsg {
        DaemonMsg::Entry(Entry::new(
            Seq::new(seq).unwrap(),
            Epoch(1),
            Body::ToolCall {
                tool: "bash".into(),
                input: Json::encode(&serde_json::json!({ "command": "ls" })).unwrap(),
                id: None,
                card: None,
                by: Audience::Model,
            },
        ))
    }

    fn ask(seq: u64, call: u64) -> DaemonMsg {
        DaemonMsg::Entry(Entry::new(
            Seq::new(seq).unwrap(),
            Epoch(1),
            Body::Decision {
                by: "policy".into(),
                call: CallId::new(call),
                d: Verdict::Permission {
                    permission: Permission::Ask,
                },
            },
        ))
    }

    #[test]
    fn a_fork_refusal_keeps_the_source_attached_and_never_refreshes_its_environment() {
        let (mut client, source) = attached(Env::Origin);
        let connection = client.connection();
        let effects = client.run(Cmd::Fork(Seq::new(7)));
        assert_eq!(connects(&effects), []);
        assert!(matches!(sends(&effects)[..], [ClientMsg::Attach {
            target: Target::Fork { session, seq: Some(seq) }, env: None, since: 0, ..
        }] if session == &source.to_string() && seq.get() == 7));
        client.step(msg(
            connection.0,
            DaemonMsg::Error {
                message: "requires a rest point".into(),
            },
        ));
        assert!(matches!(client.link(), Link::Live { .. }));
        assert_eq!(client.view().session, Some(source));
        assert!(client.offer().is_ok());
    }

    #[test]
    fn a_reattach_after_drain_never_resends_the_environment() {
        let (mut client, session) = attached(Env::Origin);
        client.step(msg(1, DaemonMsg::Draining));
        assert!(matches!(client.link(), Link::Draining));
        let out = client.step(closed(1));
        let [(conn, attach, restart)] = connects(&out)[..] else {
            panic!("{out:?}");
        };
        assert_eq!((conn, restart), (ConnId(2), false));
        assert_eq!(attach.target, Target::Id(session));
        assert_eq!((attach.since, &attach.env), (1, &Env::Keep));
        assert!(matches!(client.link(), Link::Reconnecting { since: 1 }));

        // A restart of a wedged daemon picks the session up the same way.
        client.step(Event {
            conn: ConnId(2),
            kind: EventKind::Wedged(Diagnosis::default()),
        });
        let out = client.restart();
        assert!(
            matches!(connects(&out)[..], [(_, attach, true)] if attach.env == Env::Keep),
            "{out:?}"
        );
    }

    #[test]
    fn detach_is_never_reported_as_a_lost_daemon() {
        let (mut client, _) = attached(Env::Origin);
        let out = client.detach();
        assert!(matches!(sends(&out)[..], [ClientMsg::Detach]));
        // The daemon closing the connection is the answer, not a loss.
        let out = client.step(closed(1));
        assert!(out.is_empty(), "{out:?}");
        assert!(matches!(client.link(), Link::Ended(End::Detached)));
        assert!(!client.turn().can_send);

        // Without a detach, it is.
        let (mut client, _) = attached(Env::Origin);
        client.step(closed(1));
        assert!(matches!(
            client.link(),
            Link::Ended(End::Lost(Error::Closed))
        ));
    }

    #[test]
    fn events_from_a_replaced_connection_are_dropped() {
        let (mut client, _) = attached(Env::Origin);
        let out = client.open(Target::New);
        assert!(matches!(sends(&out)[..], [ClientMsg::Detach]));
        let out = client.step(closed(1));
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
        client.step(closed(1));
        let out = client.step(live(2, Caps::ALL));
        // Unanswered, "one" goes again under its number: a daemon that keeps
        // who sent what does it once.
        assert!(
            matches!(&sends(&out)[..], [
                ClientMsg::Input { text: one, client_seq: Some(1), .. },
                ClientMsg::Input { text: two, client_seq: Some(2), .. },
            ] if one == "one" && two == "two"),
            "{out:?}"
        );
    }

    #[test]
    fn unanswered_mutations_are_never_evicted_by_new_input() {
        for disconnected in [false, true] {
            let (mut client, _) = attached(Env::Keep);
            if disconnected {
                client.step(msg(1, DaemonMsg::Draining));
                client.step(closed(1));
            }
            for n in 0..UNANSWERED {
                let out = client.run(Cmd::Input(n.to_string()));
                assert_eq!(notices(&out), [] as [&Notice; 0]);
                assert_eq!(sends(&out).len(), usize::from(!disconnected));
            }
            let out = client.run(Cmd::Input("overflow".into()));
            assert!(sends(&out).is_empty(), "{out:?}");
            assert_eq!(notices(&out), vec![&Notice::Backpressure], "{out:?}");
            if !disconnected {
                client.step(msg(1, DaemonMsg::Draining));
                client.step(closed(1));
            }
            let out = client.step(live(2, Caps::ALL));
            let replay = sends(&out);
            assert_eq!(replay.len(), UNANSWERED);
            for (n, msg) in replay.iter().enumerate() {
                assert_eq!(msg.client_seq(), Some(n as u64 + 1));
                assert!(matches!(msg, ClientMsg::Input { text, .. } if text == &n.to_string()));
            }
            client.step(msg(2, DaemonMsg::Ack { client_seq: 1 }));
            let out = client.run(Cmd::Input("accepted".into()));
            assert_eq!(sends(&out)[0].client_seq(), Some(65));
        }
    }

    #[test]
    fn pending_input_follows_the_opened_target_before_it_attaches() {
        for reopen_same in [false, true] {
            for restart in [false, true] {
                let (mut client, a) = attached(Env::Keep);
                let b = SessionId::new();
                client.open(Target::Id(b));
                client.run(Cmd::Input("for B only".into()));
                if restart {
                    let out = client.restart();
                    assert_eq!(connects(&out)[0].1.target, Target::Id(b));
                } else {
                    client.open(Target::Id(if reopen_same { b } else { a }));
                }
                let conn = client.connection().0;
                let out = client.step(live(conn, Caps::ALL));
                assert_eq!(
                    sends(&out).len(),
                    usize::from(restart || reopen_same),
                    "{out:?}"
                );
            }
        }
    }

    #[test]
    fn restarting_a_known_pending_target_keeps_its_environment() {
        let mut client = SessionClient::new(Env::Origin);
        let session = SessionId::new();
        client.open(Target::Id(session));
        client.run(Cmd::Input("pending".into()));
        let out = client.restart();
        let attach = connects(&out)[0].1;
        assert_eq!(attach.target, Target::Id(session));
        assert_eq!(attach.env, Env::Keep);
        assert_eq!(attach.since, 0);
        let out = client.step(live(2, Caps::ALL));
        assert_eq!(sends(&out).len(), 1);
    }

    #[test]
    fn view_dependent_offers_never_cross_an_attachment_change() {
        let (mut client, _) = attached(Env::Keep);
        client.step(msg(1, bash_call(2)));
        client.step(msg(1, ask(3, 2)));
        let call = CallId::new(2).unwrap();
        assert_eq!(client.turn().asking, Some(call));
        let b = SessionId::new();
        client.open(Target::Id(b));
        for connected in [false, true] {
            if connected {
                client.step(live(2, Caps::ALL));
            }
            for cmd in [
                Cmd::Approve {
                    call,
                    approved: true,
                },
                Cmd::Steer("stale".into()),
                Cmd::Interrupt,
            ] {
                let out = client.run(cmd);
                assert_eq!(notices(&out), vec![&Notice::NotConnected], "{out:?}");
                assert!(sends(&out).is_empty(), "{out:?}");
            }
            assert!(!client.turn().running);
            assert_eq!(client.turn().asking, None);
            assert!(!client.turn().can_interrupt);
        }
        client.open(Target::Id(b));
        assert!(client.run(Cmd::Input("for B".into())).is_empty());
        let out = client.step(live(3, Caps::ALL));
        assert!(matches!(sends(&out)[..], [ClientMsg::Input { text, .. }] if text == "for B"));
        client.step(msg(
            3,
            DaemonMsg::Attached {
                session: b,
                workspace: PathBuf::from("/b"),
            },
        ));
        client.step(msg(3, bash_call(2)));
        client.step(msg(3, ask(3, 2)));
        let out = client.run(Cmd::Approve {
            call,
            approved: true,
        });
        assert!(
            matches!(sends(&out)[..], [ClientMsg::Approve { call: approved, .. }] if *approved == call)
        );
    }

    #[test]
    fn pending_input_never_crosses_an_explicit_session_change() {
        let (mut client, _) = attached(Env::Keep);
        client.run(Cmd::Input("unanswered".into()));
        client.step(msg(1, DaemonMsg::Draining));
        client.run(Cmd::Input("queued".into()));
        client.open(Target::New);
        let out = client.step(live(2, Caps::ALL));
        assert!(sends(&out).is_empty(), "{out:?}");
    }

    #[test]
    fn pending_input_survives_a_restart_of_the_same_session() {
        let (mut client, _) = attached(Env::Keep);
        client.run(Cmd::Input("unanswered".into()));
        client.step(msg(1, DaemonMsg::Draining));
        client.run(Cmd::Input("queued".into()));
        client.restart();
        let out = client.step(live(2, Caps::ALL));
        assert_eq!(sends(&out).len(), 2, "{out:?}");
    }

    #[test]
    fn a_resend_after_reconnect_runs_once() {
        let (mut client, _) = attached(Env::Origin);
        client.run(Cmd::Input("late".into()));
        // The daemon began draining as it read it: refused, so not done.
        let refused = client.step(msg(
            1,
            DaemonMsg::Rejected {
                client_seq: 1,
                reason: Rejection::Draining,
                message: "send that again".into(),
            },
        ));
        assert!(refused.is_empty(), "no error shown: {refused:?}");
        client.step(msg(1, DaemonMsg::Draining));
        client.step(closed(1));
        let out = client.step(live(2, Caps::ALL));
        assert_eq!(sends(&out).len(), 1, "{out:?}");
        // Taken this time: a second drain does not send it again.
        client.step(msg(2, DaemonMsg::Ack { client_seq: 2 }));
        client.step(msg(2, DaemonMsg::Draining));
        client.step(closed(2));
        assert!(sends(&client.step(live(3, Caps::ALL))).is_empty());
    }

    #[test]
    fn the_turn_says_what_runs_and_what_waits_on_this_client() {
        let (mut client, _) = attached(Env::Origin);
        assert!(!client.turn().running);
        client.step(msg(1, bash_call(2)));
        let turn = client.turn();
        assert!(turn.running && turn.can_interrupt);
        client.step(msg(1, ask(3, 2)));
        assert_eq!(client.turn().asking, CallId::new(2));
        assert_eq!(client.turn().asks, 1);
    }

    #[test]
    fn an_offer_is_made_only_while_attached_and_each_intent_only_when_the_turn_takes_it() {
        // Connecting counts as attached: input waits for the connection.
        let mut client = SessionClient::new(Env::Origin);
        client.open(Target::New);
        assert!(client.offer().unwrap().input("hi".into()).is_empty());
        client.step(Event {
            conn: ConnId(1),
            kind: EventKind::Wedged(Diagnosis::default()),
        });
        assert_eq!(client.offer().err(), Some(Notice::NotConnected));
        assert_eq!(
            notices(&client.run(Cmd::Input("hi".into()))),
            [&Notice::NotConnected]
        );

        let (mut client, _) = attached(Env::Origin);
        // Nothing runs: input yes, a steer or an interrupt no.
        assert_eq!(sends(&client.offer().unwrap().input("hi".into())).len(), 1);
        assert_eq!(
            client.offer().unwrap().steer("more".into()).err(),
            Some(Notice::NothingRunning)
        );
        let two = CallId::new(2).unwrap();
        assert_eq!(
            client.offer().unwrap().interrupt().err(),
            Some(Notice::NothingRunning)
        );
        assert_eq!(
            client.offer().unwrap().approve(two, true).err(),
            Some(Notice::NotAsking(two))
        );
        client.step(msg(1, bash_call(2)));
        assert!(client.offer().unwrap().interrupt().is_ok());
        assert_eq!(
            client.offer().unwrap().approve(two, true).err(),
            Some(Notice::NotAsking(two))
        );
        client.step(msg(1, ask(3, 2)));
        assert!(matches!(
            sends(&client.offer().unwrap().approve(two, false).unwrap())[..],
            [ClientMsg::Approve {
                call,
                approved: false,
                ..
            }] if *call == two
        ));
        // Attached but reconnecting: offered, and queued until the
        // connection is up.
        client.step(msg(1, DaemonMsg::Draining));
        client.step(closed(1));
        assert!(
            client
                .offer()
                .unwrap()
                .steer("go".into())
                .unwrap()
                .is_empty()
        );
        let out = client.step(live(2, Caps::ALL));
        // After the unanswered ones go again.
        assert!(
            matches!(
                sends(&out).last(),
                Some(ClientMsg::Input {
                    mode: Mode::Steer,
                    ..
                })
            ),
            "{out:?}"
        );

        // Ended: no offer.
        client.detach();
        assert_eq!(client.offer().err(), Some(Notice::NotConnected));
    }

    #[test]
    fn a_wedged_daemon_ends_nothing_until_restarted() {
        let mut client = SessionClient::new(Env::Origin);
        client.open(Target::New);
        client.step(Event {
            conn: ConnId(1),
            kind: EventKind::Wedged(Diagnosis::default()),
        });
        assert!(matches!(client.link(), Link::Wedged(_)));
        let out = client.restart();
        assert!(
            matches!(connects(&out)[..], [(ConnId(2), attach, true)] if attach.target == Target::New && attach.env == Env::Origin),
            "{out:?}"
        );
    }
}
