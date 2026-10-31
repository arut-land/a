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

use protocol::view::{Change, SessionList, SessionView, Update};
use protocol::{
    Activity, Author, CallId, ClientMsg, CommandInfo, DaemonMsg, Environment, Mode, Rejection,
    Segments, SessionSummary, Target,
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
    pub client: protocol::ClientId,
    pub target: Target,
    /// The entries after this one are replayed.
    pub since: u64,
    pub env: Env,
    /// The directory the daemon finds the workspace from; `None` is the
    /// environment's.
    pub workspace: Option<protocol::HostPath>,
    /// Renderer declarations sent when this connection attaches.
    pub decl: protocol::ClientDecl,
}

impl Attach {
    pub fn new(target: Target, env: Env) -> Self {
        Self {
            client: protocol::ClientId::new(),
            target,
            since: 0,
            env,
            workspace: None,
            decl: protocol::ClientDecl::default(),
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
        /// The daemon's machine, as its welcome named it.
        machine: protocol::MachineId,
        /// What holds the upgrade to a newer build the daemon waits for,
        /// serving meanwhile.
        upgrade: Option<String>,
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
    /// An independently requested range, released when its consumer drops it.
    History {
        reader: u64,
        page: crate::HistoryPage,
    },
    /// Something to tell the person.
    Notice(Notice),
}

/// Why an intent went nowhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    NotConnected,
    /// An independent scan must restart after the session history changes generation.
    HistoryChanged {
        reader: u64,
    },
    Backpressure,
    /// A steer or an interrupt while no turn runs.
    NothingRunning,
    /// An answer for a call that is not waiting for one.
    NotAsking(CallId),
    /// A withdrawal of input that is not the user's queued input.
    NotQueued(protocol::Seq),
    /// `/env refresh` on a remote daemon, whose environment is its
    /// bridge's.
    RemoteEnv,
    /// The driver could not send a message.
    NotSent(Error),
    ForkRejected(protocol::ForkError),
    /// The daemon waits to upgrade to a newer build, held by this, and
    /// serves this client meanwhile.
    UpgradeWaits(String),
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConnected => f.write_str("not connected"),
            Self::HistoryChanged { .. } => f.write_str("history changed; request the range again"),
            Self::Backpressure => {
                f.write_str("waiting for the daemon to acknowledge pending input")
            }
            Self::NothingRunning => f.write_str("nothing is running"),
            Self::NotAsking(call) => write!(f, "call {call} is not waiting for an answer"),
            Self::NotQueued(input) => write!(f, "input {input} is not queued"),
            Self::RemoteEnv => f.write_str(
                "the host's environment is captured when `a` connects; reattach to refresh it",
            ),
            Self::ForkRejected(reason) => reason.fmt(f),
            Self::UpgradeWaits(held) => write!(
                f,
                "the upgrade to a newer build waits: {held}; `a daemon stop` ends what holds it"
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
    /// Published work or a submitted mutation can still need cancellation.
    pub can_interrupt: bool,
    /// Input reaches the daemon, now or once reconnected.
    pub can_send: bool,
}

/// The normal send window; cancellation can reserve one further slot.
pub(crate) const UNANSWERED: usize = 64;
pub(crate) const MUTATION_BUDGET: crate::QueueBudget = crate::QueueBudget {
    bytes: 16 << 20,
    requests: UNANSWERED + 1,
    control_reserve: 1,
};

/// One session's client, sans I/O.
pub struct SessionClient {
    link: Link,
    view: SessionView,
    history: crate::history::History,
    heads: protocol::history::Heads,
    conversation: Option<crate::ConversationHead>,
    readers: std::collections::BTreeMap<u64, crate::history::History>,
    sides: std::collections::BTreeMap<String, crate::history::Side>,
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
    submission: Option<crate::Submission>,
}

impl SessionClient {
    /// A client whose own opens attach with `env`.
    pub fn new(env: Env) -> Self {
        Self {
            link: Link::Connecting,
            view: SessionView::default(),
            history: crate::history::History::default(),
            heads: protocol::history::Heads::default(),
            conversation: None,
            readers: std::collections::BTreeMap::default(),
            sides: std::collections::BTreeMap::default(),
            view_current: false,
            sessions: SessionList::default(),
            commands: Vec::new(),
            conn: ConnId::default(),
            opened: Attach::new(Target::New, env.clone()),
            env,
            seq: 0,
            queued: Vec::new(),
            unanswered: VecDeque::new(),
            submission: None,
        }
    }

    pub const fn link(&self) -> &Link {
        &self.link
    }

    pub fn conversation(&self) -> Option<&crate::ConversationHead> {
        self.conversation.as_ref()
    }

    pub const fn view(&self) -> &SessionView {
        &self.view
    }

    /// Declares the side purposes whose completed values this UI needs.
    pub fn demand_sides(&mut self, purposes: impl IntoIterator<Item = String>) -> Vec<Effect> {
        let wanted: std::collections::BTreeSet<_> = purposes.into_iter().collect();
        if self.sides.keys().eq(wanted.iter()) {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.sides.retain(|purpose, side| {
            if wanted.contains(purpose) {
                return true;
            }
            if let Some(cancel) = side.history.close() {
                out.push(Effect::Io(Io::Send(cancel)));
            }
            false
        });
        let removed: Vec<_> = self
            .view
            .sides
            .keys()
            .filter(|purpose| !wanted.contains(*purpose))
            .cloned()
            .collect();
        for purpose in removed {
            out.push(Effect::Show(Show::View(
                self.view
                    .side_range(purpose, protocol::view::Items::default()),
            )));
        }
        for purpose in &wanted {
            self.sides.entry(purpose.clone()).or_default();
        }
        self.opened.decl.sides = wanted.into_iter().collect();
        if self.link.up() {
            out.extend(self.send(ClientMsg::Declare(self.opened.decl.clone())));
        }
        out
    }

    /// Changes this renderer's demanded history range, preserving drafts and controls.
    pub fn demand(&mut self, demand: crate::HistoryDemand) -> Vec<Effect> {
        self.history
            .demand(demand)
            .into_iter()
            .map(|msg| Effect::Io(Io::Send(msg)))
            .collect()
    }

    /// Reads history independently of the presentation window (recall, search, export).
    pub fn read_history(&mut self, reader: u64, demand: crate::HistoryDemand) -> Vec<Effect> {
        let Some(position) = &self.view.history else {
            return Vec::new();
        };
        self.read_history_at(reader, position.cut, demand)
    }

    /// Continues a scan at its original immutable cut, even while new turns commit.
    pub fn read_history_at(
        &mut self,
        reader: u64,
        cut: protocol::history::Cut,
        demand: crate::HistoryDemand,
    ) -> Vec<Effect> {
        let Some(position) = &self.view.history else {
            return Vec::new();
        };
        if cut.session != position.cut.session || cut.generation != position.cut.generation {
            return vec![Effect::Show(Show::Notice(Notice::HistoryChanged {
                reader,
            }))];
        }
        self.readers
            .entry(reader)
            .or_default()
            .at(cut, position.count, demand)
            .into_iter()
            .map(|msg| Effect::Io(Io::Send(msg)))
            .collect()
    }

    pub fn close_history(&mut self, reader: u64) -> Vec<Effect> {
        self.readers
            .remove(&reader)
            .and_then(|mut history| history.close())
            .into_iter()
            .map(|msg| Effect::Io(Io::Send(msg)))
            .collect()
    }

    pub fn history_pending(&self) -> bool {
        self.history.pending()
    }

    /// Withdraws the range demand and releases data already transferred to a renderer.
    pub fn release_history(&mut self) -> Vec<Effect> {
        self.view.items = protocol::view::Items::default();
        self.history
            .release()
            .into_iter()
            .map(|msg| Effect::Io(Io::Send(msg)))
            .collect()
    }

    /// Inputs still owned by this client, oldest first. Recorded inputs are
    /// recalled from the session view instead, including its visible queue.
    pub fn pending_inputs(&self) -> impl DoubleEndedIterator<Item = &Segments> {
        let mut inputs: Vec<_> = self
            .unanswered
            .iter()
            .chain(&self.queued)
            .filter_map(|msg| match msg {
                ClientMsg::Input { segments, .. } => Some((msg.client_seq(), segments)),
                _ => None,
            })
            .collect();
        // A draining rejection can move an older offer behind newer queued input.
        inputs.sort_unstable_by_key(|(seq, _)| *seq);
        inputs.into_iter().map(|(_, segments)| segments)
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
        let pending = !self.unanswered.is_empty() || self.queued.iter().any(is_mutation);
        if !self.view_current {
            return TurnView {
                can_interrupt: pending && self.link.attached(),
                can_send: self.link.attached(),
                ..TurnView::default()
            };
        }
        let state = &self.view.state;
        let activity = self.view.activity;
        let running = !matches!(
            activity,
            Some(Activity::Idle | Activity::Done | Activity::Failed)
        );
        let asking = state.inbox().next();
        let asks = u32::try_from(state.inbox().count()).unwrap_or(u32::MAX);
        TurnView {
            activity,
            running,
            asking,
            asks,
            can_interrupt: (running || pending) && self.link.attached(),
            can_send: self.link.attached(),
        }
    }

    /// What this client may ask of the session now; nothing unless
    /// attached, now or once reconnected.
    pub fn offer(&mut self) -> Result<Offer<'_>, Notice> {
        if self.link.attached() {
            Ok(Offer(self))
        } else {
            Err(Notice::NotConnected)
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
            self.conversation = None;
            self.submission = None;
        }
        let mut attach = attach;
        attach.decl.sides = self.sides.keys().cloned().collect();
        attach.client = self.opened.client;
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
            Cmd::Input(text) => self.offer().and_then(|o| o.input(text)),
            Cmd::Steer(text) => self.offer().and_then(|o| o.steer(text)),
            Cmd::Approve { call, approved } => self.offer().and_then(|o| o.approve(call, approved)),
            Cmd::Answer { call, answer } => self.offer().and_then(|o| o.answer(call, answer)),
            Cmd::Interrupt => self.offer().and_then(Offer::interrupt),
            Cmd::RefreshEnv => self.offer().and_then(Offer::refresh_env),
            Cmd::Detach => Ok(self.detach()),
            Cmd::Sessions => self.offer().and_then(|o| o.send(ClientMsg::List)),
            Cmd::Open(target) => Ok(self.open(target)),
            Cmd::Fork(seq) => match self.view.session {
                Some(session) => self.offer().map(|offer| {
                    offer
                        .0
                        .send(ClientMsg::Attach {
                            target: Target::Fork {
                                session: session.to_string(),
                                seq,
                            },
                            workspace: None,
                            since: 0,
                            env: None,
                            decl: offer.0.opened.decl.clone(),
                        })
                        .into_iter()
                        .collect()
                }),
                None => Err(Notice::NotConnected),
            },
            Cmd::Help => Ok(Vec::new()),
        };
        done.unwrap_or_else(|notice| vec![Effect::Show(Show::Notice(notice))])
    }

    /// Tracks one submitted input by its durable mutation origin.
    pub fn submit(&mut self, text: String) -> Result<(crate::SubmissionId, Vec<Effect>), Notice> {
        let effects = self.offer()?.input(text)?;
        let id = Author {
            client: self.opened.client,
            seq: protocol::ClientSeq::new(self.seq).unwrap(),
        };
        self.submission = Some(crate::Submission::new(id));
        Ok((id, effects))
    }

    pub fn submission(&self, id: crate::SubmissionId) -> Option<crate::SubmissionState> {
        self.submission
            .as_ref()
            .filter(|s| s.id == id)
            .map(|s| s.state)
    }

    /// Sends `msg` once the connection is up, numbered if it is a
    /// mutation.
    fn send(&mut self, mut msg: ClientMsg) -> Option<Effect> {
        if let Some(seq @ None) = msg.client_seq_mut() {
            self.seq += 1;
            *seq = Some(self.seq);
        }
        if self.link.up() {
            if is_mutation(&msg) {
                self.unanswered.push_back(msg.clone());
            }
            Some(Effect::Io(Io::Send(msg)))
        } else {
            self.queued.push(msg);
            None
        }
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
                upgrade,
                ..
            } => {
                if let Some(held) = upgrade {
                    out.push(Effect::Show(Show::Notice(Notice::UpgradeWaits(held))));
                }
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
                    out.extend(self.send(msg));
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

    fn history_part(
        &mut self,
        id: u64,
        offset: u64,
        text: &str,
        done: bool,
        out: &mut Vec<Effect>,
    ) {
        if let Some((purpose, side)) = self
            .sides
            .iter_mut()
            .find(|(_, side)| side.history.owns(id))
        {
            match side.history.part(id, offset, text, done) {
                Ok((loaded, messages)) => {
                    out.extend(
                        messages
                            .into_iter()
                            .map(|message| Effect::Io(Io::Send(message))),
                    );
                    if let Some(loaded) = loaded {
                        out.push(Effect::Show(Show::View(
                            self.view.side_range(purpose.clone(), loaded.items),
                        )));
                    }
                }
                Err(error) => out.push(Effect::Show(Show::Notice(Notice::NotSent(error.into())))),
            }
            return;
        }
        if let Some((&reader, history)) = self
            .readers
            .iter_mut()
            .find(|(_, history)| history.owns(id))
        {
            match history.part(id, offset, text, done) {
                Ok((loaded, messages)) => {
                    out.extend(messages.into_iter().map(|msg| Effect::Io(Io::Send(msg))));
                    if let Some(loaded) = loaded {
                        out.push(Effect::Show(Show::History {
                            reader,
                            page: loaded,
                        }));
                    }
                }
                Err(error) => {
                    out.push(Effect::Show(Show::Notice(Notice::NotSent(error.into()))));
                }
            }
            return;
        }
        match self.history.part(id, offset, text, done) {
            Ok((loaded, messages)) => {
                out.extend(messages.into_iter().map(|msg| Effect::Io(Io::Send(msg))));
                if let Some(loaded) = loaded {
                    let update = self.view.range(loaded.items, loaded.range, loaded.fragment);
                    out.push(Effect::Show(Show::View(update)));
                }
            }
            Err(error) => {
                out.push(Effect::Show(Show::Notice(Notice::NotSent(error.into()))));
            }
        }
    }

    fn msg(&mut self, msg: &DaemonMsg, out: &mut Vec<Effect>) {
        match msg {
            DaemonMsg::HeadPart {
                seq,
                offset,
                text,
                done,
            } => {
                match self.heads.part(*seq, *offset, text, *done) {
                    Ok(Some(head)) => self.msg(
                        &DaemonMsg::Head {
                            head: Box::new(head),
                        },
                        out,
                    ),
                    Ok(None) => {}
                    Err(error) => self.end(End::Lost(error.into()), out),
                }
                return;
            }
            DaemonMsg::Head { head } => self.accept_head(head, out),
            DaemonMsg::HistoryPart {
                id,
                offset,
                text,
                done,
            } => {
                self.history_part(*id, *offset, text, *done, out);
                return;
            }
            DaemonMsg::Reply {
                id,
                outcome: protocol::Outcome::Failed { error },
            } => {
                if self.failed_side(*id)
                    || self.history.failed(*id)
                    || self.readers.values_mut().any(|history| history.failed(*id))
                {
                    out.push(Effect::Show(Show::Notice(Notice::NotSent(
                        std::io::Error::other(error.clone()).into(),
                    ))));
                    return;
                }
            }
            DaemonMsg::ForkRejected { reason } => {
                out.push(Effect::Show(Show::Notice(Notice::ForkRejected(
                    reason.clone(),
                ))));
                return;
            }
            DaemonMsg::Attached { session, .. } => {
                if self.view.session != Some(*session) {
                    for side in self.sides.values_mut() {
                        side.reference = None;
                        if let Some(cancel) = side.history.close() {
                            out.push(Effect::Io(Io::Send(cancel)));
                        }
                    }
                }
                self.opened.target = Target::Id(*session);
                self.view_current = true;
            }
            // Logged is done: the ack is queued once the mutation commits,
            // behind whatever the session has published by then, so the
            // turn it started can be seen to end before it arrives. One
            // connection's mutations run in order, so those numbered before
            // it are answered too (the kernel's resend rule).
            DaemonMsg::Entry(entry) => {
                if let Some(author) = entry.body.origin()
                    && author.client == self.opened.client
                {
                    self.unanswered
                        .retain(|m| m.client_seq().is_none_or(|s| s > author.seq.get()));
                }
            }
            DaemonMsg::Sessions { .. } => {
                self.sessions.fold(msg);
                out.push(Effect::Show(Show::Sessions));
                return;
            }
            DaemonMsg::Ack { client_seq } => {
                self.unanswered
                    .retain(|m| m.client_seq() != Some(*client_seq));
                return;
            }
            DaemonMsg::Rejected {
                client_seq, reason, ..
            } => {
                let at = self
                    .unanswered
                    .iter()
                    .position(|m| m.client_seq() == Some(*client_seq));
                let refused = at.and_then(|at| self.unanswered.remove(at));
                if let (Some(again), Rejection::Draining) = (refused, reason) {
                    self.queued.push(again);
                    return;
                }
            }
            _ => {}
        }
        self.publish(msg, out);
    }

    fn failed_side(&mut self, id: u64) -> bool {
        self.sides.values_mut().any(|side| {
            if !side.history.failed(id) {
                return false;
            }
            side.reference = None;
            true
        })
    }

    fn accept_head(&mut self, head: &protocol::history::Head, out: &mut Vec<Effect>) {
        self.conversation = Some(crate::ConversationHead {
            session: head.cut.session,
            seq: head.cut.seq,
            identity: head.conversation.clone(),
            usage: head.usage,
        });
        for (purpose, side) in &mut self.sides {
            let reference = head.sides.get(purpose).map(|reference| {
                (
                    protocol::history::Cut {
                        seq: reference.revision,
                        ..head.cut
                    },
                    reference.index,
                )
            });
            if !side.history.same_generation(head.cut)
                || (reference.is_none() && side.reference.is_some())
            {
                out.push(Effect::Show(Show::View(
                    self.view
                        .side_range(purpose.clone(), protocol::view::Items::default()),
                )));
            }
            if side.reference == reference {
                continue;
            }
            side.reference = reference;
            let messages = if let Some((cut, index)) = reference {
                side.history.at(
                    cut,
                    head.count,
                    crate::HistoryDemand {
                        anchor: crate::HistoryAnchor::TextAfter {
                            item: index,
                            offset: 0,
                        },
                        bytes: usize::MAX,
                        ..Default::default()
                    },
                )
            } else {
                side.history.release().into_iter().collect()
            };
            out.extend(
                messages
                    .into_iter()
                    .map(|message| Effect::Io(Io::Send(message))),
            );
        }
        self.readers.retain(|&reader, history| {
            if history.same_generation(head.cut) {
                return true;
            }
            if let Some(cancel) = history.close() {
                out.push(Effect::Io(Io::Send(cancel)));
            }
            out.push(Effect::Show(Show::Notice(Notice::HistoryChanged {
                reader,
            })));
            false
        });
        let known = head
            .state
            .taken
            .iter()
            .filter(|author| author.client == self.opened.client)
            .map(|author| author.seq.get())
            .max();
        if let Some(known) = known {
            self.unanswered
                .retain(|m| m.client_seq().is_none_or(|seq| seq > known));
        }
        out.extend(
            self.history
                .head(head)
                .into_iter()
                .map(|msg| Effect::Io(Io::Send(msg))),
        );
    }

    fn publish(&mut self, msg: &DaemonMsg, out: &mut Vec<Effect>) {
        let update = self.view.fold(msg);
        if let Some(submission) = &mut self.submission {
            submission.fold(&self.view);
        }
        let Some(update) = update else {
            return;
        };
        let link_changed = match &update.change {
            Change::Synced => {
                if let Link::Replaying { welcome, remote } = self.link {
                    self.link = Link::Live { welcome, remote };
                }
                true
            }
            Change::Draining => {
                self.link = Link::Draining;
                true
            }
            _ => false,
        };
        out.push(Effect::Show(Show::View(update)));
        if link_changed {
            out.push(Effect::Show(Show::Link));
        }
    }

    /// Reattaches to the current target, resuming the view only when it
    /// belongs to that session.
    fn reattach(&self) -> Option<Attach> {
        let Target::Id(session) = self.opened.target else {
            return None;
        };
        Some(Attach {
            client: self.opened.client,
            target: Target::Id(session),
            since: if self.view.session == Some(session) {
                self.view.seq
            } else {
                0
            },
            env: Env::Keep,
            workspace: self.opened.workspace.clone(),
            decl: self.opened.decl.clone(),
        })
    }

    fn connect(&mut self, attach: Attach, restart: bool, link: Link, out: &mut Vec<Effect>) {
        self.conn.0 += 1;
        for side in self.sides.values_mut() {
            side.reference = None;
            side.history.close();
        }
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
    pub fn input(self, segments: impl Into<Segments>) -> Result<Vec<Effect>, Notice> {
        self.send(ClientMsg::input(segments))
    }

    /// Input for the running turn, before its next step.
    pub fn steer(self, segments: impl Into<Segments>) -> Result<Vec<Effect>, Notice> {
        self.running()?;
        self.send(ClientMsg::Input {
            segments: segments.into(),
            mode: Mode::Steer,
            force: false,
            client_seq: None,
        })
    }

    /// Input for the running turn now: its model call is cut short and
    /// the calls that can go on in the background do.
    pub fn force_steer(self, segments: impl Into<Segments>) -> Result<Vec<Effect>, Notice> {
        self.running()?;
        self.send(ClientMsg::Input {
            segments: segments.into(),
            mode: Mode::Steer,
            force: true,
            client_seq: None,
        })
    }

    /// Takes back the user's queued input `input`, as the queue shows it
    /// ([`protocol::view::ViewState::queued`]); an edit sends the new text
    /// after. The daemon refuses it once the input was taken up.
    pub fn withdraw(self, input: protocol::Seq) -> Result<Vec<Effect>, Notice> {
        self.view()?;
        let queued = (self.0.view.state.queued()).any(|q| q.seq == input && q.source.visible());
        if !queued {
            return Err(Notice::NotQueued(input));
        }
        self.send(ClientMsg::Withdraw {
            input,
            client_seq: None,
        })
    }

    /// Cancellation must not depend on publication or room for another input.
    pub fn interrupt(self) -> Result<Vec<Effect>, Notice> {
        if !self.0.turn().can_interrupt {
            self.view()?;
            return Err(Notice::NothingRunning);
        }
        // A later submission needs another interrupt even before the first is acknowledged.
        let last = self
            .0
            .unanswered
            .iter()
            .chain(&self.0.queued)
            .rfind(|msg| is_mutation(msg));
        if matches!(last, Some(ClientMsg::Interrupt { .. })) {
            return Ok(Vec::new());
        }
        self.send(ClientMsg::INTERRUPT)
    }

    /// Answers the approval call `call` waits for.
    pub fn approve(self, call: CallId, approved: bool) -> Result<Vec<Effect>, Notice> {
        self.view()?;
        if !self.0.view.state.inbox().any(|c| c == call) {
            return Err(Notice::NotAsking(call));
        }
        if !self.0.view.state.awaiting_permission(call) {
            return Err(Notice::NotAsking(call));
        }
        let question = self
            .0
            .view
            .state
            .question_id(call)
            .ok_or(Notice::NotAsking(call))?;
        let response = if approved {
            protocol::Response::accept()
        } else {
            protocol::Response::Decline
        };
        self.answer(call, protocol::QuestionAnswer::confirm(question, response))
    }

    /// Sends an answer only to the question the user actually reviewed.
    pub fn answer(
        self,
        call: CallId,
        answer: protocol::QuestionAnswer,
    ) -> Result<Vec<Effect>, Notice> {
        self.view()?;
        if self.0.view.state.question_id(call) != Some(answer.question) {
            return Err(Notice::NotAsking(call));
        }
        self.send(ClientMsg::answer(call, answer))
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

    fn send(self, msg: ClientMsg) -> Result<Vec<Effect>, Notice> {
        let control = matches!(msg, ClientMsg::Interrupt { .. });
        let bytes: usize = self
            .0
            .unanswered
            .iter()
            .chain(&self.0.queued)
            .map(ClientMsg::retained_bytes)
            .sum();
        if !MUTATION_BUDGET.admits(
            bytes.saturating_add(msg.retained_bytes()),
            self.0.unanswered.len() + self.0.queued.len(),
            control,
        ) {
            return Err(Notice::Backpressure);
        }
        Ok(self.0.send(msg).into_iter().collect())
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

pub(crate) const fn is_mutation(msg: &ClientMsg) -> bool {
    matches!(
        msg,
        ClientMsg::Input { .. }
            | ClientMsg::Withdraw { .. }
            | ClientMsg::Answer { .. }
            | ClientMsg::Interrupt { .. }
    )
}

#[cfg(test)]
mod tests {

    use protocol::{
        Audience, Body, Caps, ClientId, ClientSeq, Entry, Epoch, Json, Permission, Seq, SessionId,
        Verdict,
    };

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
                machine: protocol::MachineId([0; 16]),
                upgrade: None,
            },
        }
    }

    fn msg(conn: u64, msg: DaemonMsg) -> Event {
        Event {
            conn: ConnId(conn),
            kind: EventKind::Msg(msg),
        }
    }

    fn controls(head: protocol::history::Head) -> Event {
        msg(
            1,
            DaemonMsg::Head {
                head: Box::new(head),
            },
        )
    }

    fn closed(conn: u64) -> Event {
        Event {
            conn: ConnId(conn),
            kind: EventKind::Closed(Error::Closed),
        }
    }

    fn side_entries() -> Vec<Entry> {
        let mut entries = Vec::new();
        for (purpose, text) in [("caption", "first"), ("internal-note", "private payload")] {
            let call = CallId::new(entries.len() as u64 + 1).unwrap();
            entries.push(Entry::new(
                call.0,
                Epoch(1),
                Body::ModelCall {
                    model: "m".into(),
                    purpose: Some(purpose.into()),
                    r#for: None,
                    attempt: 0,
                    choice: protocol::ToolChoice::None,
                    request: None,
                },
            ));
            entries.push(Entry::new(Seq::new(entries.len() as u64 + 1).unwrap(), Epoch(1), Body::ModelResult {
                call,r#for:None,failure:None,
                outcome:protocol::Outcome::Done {output:Json::encode(&serde_json::json!({"message":{"content":[{"type":"text","text":text}]}})).unwrap()},
            }));
        }
        entries.push(Entry::new(
            Seq::new(5).unwrap(),
            Epoch(1),
            Body::Assistant {
                text: "visible tail".into(),
            },
        ));
        entries
    }

    fn deliver_side_requests(
        client: &mut SessionClient,
        index: &protocol::view::Index,
        entries: &[Entry],
        effects: &[Effect],
    ) -> Vec<Effect> {
        let mut out = Vec::new();
        for message in sends(effects) {
            if let ClientMsg::History { id, range } = message {
                let page = index
                    .page(range, |seq| {
                        Ok(entries[usize::try_from(seq.get()).unwrap() - 1].clone())
                    })
                    .unwrap();
                out.extend(client.step(msg(
                    1,
                    DaemonMsg::HistoryPart {
                        id: *id,
                        offset: 0,
                        text: serde_json::to_string(&page).unwrap(),
                        done: true,
                    },
                )));
            }
        }
        out
    }

    #[test]
    fn requested_sides_share_pages_outside_the_presentation_window_and_release_them() {
        let entries = side_entries();
        let mut index = protocol::view::Index::new(SessionId::new(), "/w".into());
        for entry in &entries {
            index.apply(entry);
        }
        let head = index.head_for(Epoch(1), 0, &["caption".into()]);
        let mut clients = Vec::new();
        for _ in 0..2 {
            let mut client = SessionClient::new(Env::Keep);
            client.demand_sides(["caption".into()]);
            client.open(Target::New);
            client.step(live(1, Caps::ALL));
            client.demand(crate::HistoryDemand {
                anchor: crate::HistoryAnchor::After(2),
                ..Default::default()
            });
            let effects = client.step(controls(head.clone()));
            deliver_side_requests(&mut client, &index, &entries, &effects);
            assert!(
                matches!(&client.view().items[..], [protocol::view::Item::Assistant {text,..}] if text=="visible tail")
            );
            assert_eq!(client.view().side("caption"), Some("first"));
            assert!(client.view().side("internal-note").is_none());
            assert_eq!(client.view().sides.len(), 1);
            clients.push(client);
        }
        let weak = clients[0].view().sides["caption"].downgrade();
        assert!(std::ptr::eq(
            std::ptr::from_ref(&clients[0].view().sides["caption"][0]),
            std::ptr::from_ref(&clients[1].view().sides["caption"][0])
        ));
        let effects = clients[0].demand_sides([]);
        assert!(
            sends(&effects)
                .iter()
                .any(|msg| matches!(msg, ClientMsg::Declare(decl) if decl.sides.is_empty()))
        );
        assert!(clients[0].view().side("caption").is_none());
        drop(clients.remove(0));
        assert!(weak.upgrade().is_some());
        drop(clients);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn side_replacements_wait_for_their_page_and_old_generations_cannot_publish() {
        let mut entries = side_entries();
        let mut index = protocol::view::Index::new(SessionId::new(), "/w".into());
        for entry in &entries {
            index.apply(entry);
        }
        let mut client = SessionClient::new(Env::Keep);
        client.release_history();
        client.demand_sides(["caption".into()]);
        client.open(Target::New);
        client.step(live(1, Caps::ALL));
        let effects = client.step(controls(index.head_for(Epoch(1), 0, &["caption".into()])));
        deliver_side_requests(&mut client, &index, &entries, &effects);
        assert_eq!(client.view().side("caption"), Some("first"));
        entries.extend([
            Entry::new(Seq::new(6).unwrap(),Epoch(1),Body::ModelCall {
                model:"m".into(),purpose:Some("caption".into()),r#for:None,
                attempt:0,choice:protocol::ToolChoice::None,request:None,
            }),
            Entry::new(Seq::new(7).unwrap(),Epoch(1),Body::ModelResult {
                call:CallId::new(6).unwrap(),r#for:None,failure:None,
                outcome:protocol::Outcome::Done {output:Json::encode(&serde_json::json!({"message":{"content":[{"type":"text","text":"replacement"}]}})).unwrap()},
            }),
        ]);
        for entry in &entries[5..] {
            index.apply(entry);
        }
        let old = client.step(controls(index.head_for(Epoch(1), 5, &["caption".into()])));
        assert_eq!(client.view().side("caption"), Some("first"));
        let replacement = client.step(controls(index.head_for(Epoch(2), 0, &["caption".into()])));
        assert!(
            sends(&replacement)
                .iter()
                .any(|message| matches!(message, ClientMsg::Cancel { .. }))
        );
        assert!(client.view().side("caption").is_none());
        deliver_side_requests(&mut client, &index, &entries, &old);
        assert!(client.view().side("caption").is_none());
        let effects = deliver_side_requests(&mut client, &index, &entries, &replacement);
        assert!(effects.iter().any(|effect|matches!(effect, Effect::Show(Show::View(Update {change:Change::Metadata {purpose},..})) if purpose=="caption")));
        assert_eq!(client.view().side("caption"), Some("replacement"));
        assert!(client.view().items.is_empty());
        client.demand_sides([]);
        assert!(client.view().side("caption").is_none());
    }

    #[test]
    fn an_advanced_tail_reconciles_the_open_reply_before_emitting_subsequent_replies() {
        for advanced in [false, true] {
            let mut client = SessionClient::new(Env::Keep);
            client.open(Target::New);
            client.step(live(1, Caps::ALL));
            let call = CallId::new(1).unwrap();
            let mut entries = vec![Entry::new(
                call.0,
                Epoch(1),
                Body::ModelCall {
                    model: "m".into(),
                    purpose: None,
                    r#for: None,
                    attempt: 0,
                    choice: protocol::ToolChoice::None,
                    request: None,
                },
            )];
            let mut index = protocol::view::Index::new(SessionId::new(), "/w".into());
            index.apply(&entries[0]);
            let mut pending = client.step(controls(index.head(Epoch(1))));
            assert!(client.history_pending());
            let mut stream = crate::ReplyStream::default();
            assert_eq!(
                render_reply_updates(&mut client, &mut stream, &mut pending),
                Vec::<String>::new()
            );
            let mut effects = client.step(msg(
                1,
                DaemonMsg::Delta {
                    call,
                    text: "hel".into(),
                    attempt: 0,
                },
            ));
            assert_eq!(
                render_reply_updates(&mut client, &mut stream, &mut effects),
                ["start", "hel"]
            );

            entries.push(Entry::new(Seq::new(2).unwrap(), Epoch(1), Body::ModelResult {
            call, r#for: None, failure: None,
            outcome: protocol::Outcome::Done {
                output: Json::encode(&serde_json::json!({"message":{"content":[{"type":"text","text":"hello!"}]}})).unwrap(),
            },
        }));
            index.apply(&entries[1]);
            let mut effects = client.step(controls(index.head_after(Epoch(1), 1)));
            assert!(client.view().items.is_empty());
            assert!(client.history_pending());
            assert!(!client.view().state.is_open(call));
            assert_eq!(
                render_reply_updates(&mut client, &mut stream, &mut effects),
                Vec::<String>::new()
            );
            pending.extend(effects);

            if advanced {
                entries.extend(later_tool_and_reply());
                for entry in &entries[2..] {
                    index.apply(entry);
                }
                let mut effects = client.step(controls(index.head_after(Epoch(1), 2)));
                assert_eq!(
                    render_reply_updates(&mut client, &mut stream, &mut effects),
                    Vec::<String>::new()
                );
                pending.extend(effects);
            }
            let mut emitted = Vec::new();
            while let Some((id, range)) = pending.into_iter().find_map(|effect| match effect {
                Effect::Io(Io::Send(ClientMsg::History { id, range })) => Some((id, range)),
                _ => None,
            }) {
                let page = index
                    .page(&range, |seq| {
                        Ok(entries[usize::try_from(seq.get()).unwrap() - 1].clone())
                    })
                    .unwrap();
                pending = client.step(msg(
                    1,
                    DaemonMsg::HistoryPart {
                        id,
                        offset: 0,
                        text: serde_json::to_string(&page).unwrap(),
                        done: true,
                    },
                ));
                emitted.extend(render_reply_updates(&mut client, &mut stream, &mut pending));
            }
            let expected = if advanced {
                vec!["lo!", "end", "start", "subsequent reply", "end"]
            } else {
                vec!["lo!", "end"]
            };
            assert_eq!(emitted, expected, "advanced tail: {advanced}");
            assert_eq!(stream.project(client.view()), []);
            assert!(!client.history_pending());
        }
    }

    fn render_reply_updates(
        client: &mut SessionClient,
        stream: &mut crate::ReplyStream,
        effects: &mut Vec<Effect>,
    ) -> Vec<String> {
        let updates: Vec<_> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Show(Show::View(update)) => Some(update.clone()),
                _ => None,
            })
            .collect();
        let mut emitted = Vec::new();
        for update in updates {
            emitted.extend(
                stream
                    .update(client.view(), &update.change)
                    .into_iter()
                    .map(|event| match event {
                        crate::ReplyEvent::Start => "start".into(),
                        crate::ReplyEvent::Text(text) => text.to_owned(),
                        crate::ReplyEvent::End => "end".into(),
                    }),
            );
            if let Some(demand) = stream.history_demand(client.view(), &update.change) {
                effects.extend(client.demand(demand));
            }
        }
        emitted
    }

    fn later_tool_and_reply() -> [Entry; 3] {
        [
            Entry::new(
                Seq::new(3).unwrap(),
                Epoch(1),
                Body::ToolCall {
                    invoke: None,
                    tool: "echo".into(),
                    input: Json::null(),
                    id: None,
                    card: None,
                    by: Audience::Model,
                },
            ),
            Entry::new(
                Seq::new(4).unwrap(),
                Epoch(1),
                Body::ToolResult {
                    call: CallId::new(3).unwrap(),
                    outcome: protocol::Outcome::Done {
                        output: protocol::Payload::Inline(
                            Json::encode(&"x".repeat(128 << 10)).unwrap(),
                        ),
                    },
                    card: None,
                    held: Vec::new(),
                },
            ),
            Entry::new(
                Seq::new(5).unwrap(),
                Epoch(1),
                Body::Assistant {
                    text: "subsequent reply".into(),
                },
            ),
        ]
    }

    #[test]
    fn independent_scans_preserve_sparse_positions_fragments_and_the_original_cut() {
        let mut client = SessionClient::new(Env::Keep);
        client.open(Target::New);
        client.step(live(1, Caps::ALL));
        let texts = [
            "unrelated".to_owned(),
            format!("needle{}", "a".repeat(512)),
            "other".to_owned(),
            format!("needle{}", "b".repeat(512)),
        ];
        let mut entries: Vec<_> = texts
            .iter()
            .enumerate()
            .map(|(i, text)| {
                Entry::new(
                    Seq::new(i as u64 + 1).unwrap(),
                    Epoch(1),
                    Body::Assistant { text: text.clone() },
                )
            })
            .collect();
        let mut index = protocol::view::Index::new(SessionId::new(), protocol::HostPath::default());
        for entry in &entries {
            index.apply(entry);
        }
        let head = index.head(Epoch(1));
        client.step(controls(head.clone()));
        let mut effects = client.read_history_at(
            7,
            head.cut,
            crate::HistoryDemand {
                anchor: crate::HistoryAnchor::After(0),
                filter: protocol::history::Filter::Search("needle".into()),
                bytes: 64,
            },
        );
        let mut delivered = std::collections::BTreeMap::<usize, String>::new();
        loop {
            let (id, range) = effects
                .into_iter()
                .find_map(|effect| match effect {
                    Effect::Io(Io::Send(ClientMsg::History { id, range })) => Some((id, range)),
                    _ => None,
                })
                .unwrap();
            assert_eq!(range.cut, head.cut);
            assert_eq!(range.bytes, 64);
            assert_eq!(
                range.filter,
                protocol::history::Filter::Search("needle".into())
            );
            let page = index
                .page(&range, |seq| {
                    Ok(entries[usize::try_from(seq.get()).unwrap() - 1].clone())
                })
                .unwrap();
            let effects_out = client.step(msg(
                1,
                DaemonMsg::HistoryPart {
                    id,
                    offset: 0,
                    text: serde_json::to_string(&page).unwrap(),
                    done: true,
                },
            ));
            let page = effects_out
                .into_iter()
                .find_map(|effect| match effect {
                    Effect::Show(Show::History { reader: 7, page }) => Some(page),
                    _ => None,
                })
                .unwrap();
            assert_eq!(page.total, 4);
            for (position, item) in page.indices.iter().zip(page.items.iter()) {
                assert!([1, 3].contains(position));
                let protocol::view::Item::Assistant { text, .. } = item else {
                    panic!("expected assistant text")
                };
                delivered.entry(*position).or_default().push_str(text);
            }
            if entries.len() == 4 {
                entries.push(Entry::new(
                    Seq::new(5).unwrap(),
                    Epoch(1),
                    Body::Assistant {
                        text: "needle future".into(),
                    },
                ));
                index.apply(entries.last().unwrap());
                client.step(msg(
                    1,
                    DaemonMsg::Head {
                        head: Box::new(index.head(Epoch(1))),
                    },
                ));
            }
            let Some(next) = page.newer() else { break };
            effects = client.read_history_at(7, page.cut, next);
        }
        assert_eq!(delivered[&1], texts[1]);
        assert_eq!(delivered[&3], texts[3]);
    }

    #[test]
    fn independent_readers_cannot_publish_pages_from_a_superseded_generation() {
        let mut client = SessionClient::new(Env::Keep);
        client.open(Target::New);
        client.step(live(1, Caps::ALL));
        let index = protocol::view::Index::new(SessionId::new(), protocol::HostPath::default());
        let head = index.head(Epoch(1));
        client.step(controls(head.clone()));
        let effects = client.read_history(7, crate::HistoryDemand::default());
        let id = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::Io(Io::Send(ClientMsg::History { id, .. })) => Some(*id),
                _ => None,
            })
            .unwrap();
        let effects = client.step(msg(
            1,
            DaemonMsg::Head {
                head: Box::new(index.head(Epoch(2))),
            },
        ));
        assert!(effects.iter().any(|effect| matches!(effect,
            Effect::Io(Io::Send(ClientMsg::Cancel { id: cancelled })) if *cancelled == id)));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Show(Show::Notice(Notice::HistoryChanged { reader: 7 }))
        )));
        let effects = client.step(msg(
            1,
            DaemonMsg::HistoryPart {
                id,
                offset: 0,
                text: "obsolete".into(),
                done: true,
            },
        ));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Show(Show::History { .. })))
        );
        assert!(matches!(
            client
                .read_history_at(7, head.cut, crate::HistoryDemand::default())
                .as_slice(),
            [Effect::Show(Show::Notice(Notice::HistoryChanged {
                reader: 7
            }))]
        ));
    }

    #[test]
    fn a_welcome_while_an_upgrade_waits_tells_what_holds_it() {
        let mut client = SessionClient::new(Env::Keep);
        client.open(Target::New);
        let mut connected = live(1, Caps::ALL);
        if let EventKind::Connected { upgrade, .. } = &mut connected.kind {
            *upgrade = Some("session 89abcdef is at work".into());
        }
        let out = client.step(connected);
        assert!(out.iter().any(|effect| matches!(effect,
            Effect::Show(Show::Notice(Notice::UpgradeWaits(held))) if held.contains("at work"))));
    }

    #[test]
    fn retained_mutations_reject_bytes_before_numbering_and_keep_cancellation_capacity() {
        for connected in [false, true] {
            let mut client = SessionClient::new(Env::Keep);
            client.open(Target::New);
            if connected {
                client.step(live(1, Caps::ALL));
            }
            let text = "x".repeat(crate::QueueBudget::default().bytes / 3);
            client.offer().unwrap().input(text.clone()).unwrap();
            let seq = client.seq;
            assert!(matches!(
                client.offer().unwrap().input(text),
                Err(Notice::Backpressure)
            ));
            assert_eq!(client.seq, seq);
            assert_eq!(client.queued.len() + client.unanswered.len(), 1);
            client.offer().unwrap().interrupt().unwrap();
            assert_eq!(client.queued.len() + client.unanswered.len(), 2);
            if !connected {
                client.step(live(1, Caps::ALL));
            }
            let bytes: usize = client
                .unanswered
                .iter()
                .map(ClientMsg::retained_bytes)
                .sum();
            assert!(bytes <= crate::QueueBudget::default().bytes);
            client.step(msg(1, DaemonMsg::Ack { client_seq: 1 }));
            assert!(client.offer().unwrap().input("x".repeat(bytes)).is_ok());
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
                workspace: "/w".into(),
            },
        ));
        client.step(msg(
            1,
            DaemonMsg::Entry(Entry::new(
                Seq::new(1).unwrap(),
                Epoch(1),
                Body::Input {
                    segments: "hi".into(),
                    mode: Mode::Queue,
                    taken: true,
                    origin: None,
                    force: false,
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
                invoke: None,
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
                    permission: Permission::Ask(protocol::ApprovalReason::Mode {
                        mode: "ask".into(),
                    }),
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
        let refused = client.step(msg(
            connection.0,
            DaemonMsg::ForkRejected {
                reason: protocol::ForkError::Unfinished,
            },
        ));
        assert_eq!(
            notices(&refused),
            [&Notice::ForkRejected(protocol::ForkError::Unfinished)]
        );
        assert_eq!(refused.len(), 1);
        assert!(matches!(client.link(), Link::Live { .. }));
        assert_eq!(client.view().session, Some(source));
        assert!(client.offer().is_ok());
    }

    #[test]
    fn a_reattach_after_drain_never_resends_the_environment() {
        let (mut client, session) = attached(Env::Origin);
        client.step(msg(1, DaemonMsg::Draining { held: None }));
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
    fn pending_input_borrows_end_at_the_daemons_ownership_boundary() {
        let (mut client, _) = attached(Env::Keep);
        let pasted = Segments(vec![
            protocol::Segment::Text("before ".into()),
            protocol::Segment::Paste("a\nb\nc\nd".into()),
        ]);
        client.run(Cmd::Input(pasted.clone()));
        client.run(Cmd::Input("second".into()));
        assert_eq!(
            client.pending_inputs().collect::<Vec<_>>(),
            [&pasted, &Segments::from("second")]
        );
        let author = Author {
            client: client.opened.client,
            seq: ClientSeq::new(1).unwrap(),
        };
        client.step(msg(
            1,
            DaemonMsg::Entry(Entry::new(
                Seq::new(2).unwrap(),
                Epoch(0),
                Body::Input {
                    segments: pasted.clone(),
                    mode: Mode::Queue,
                    taken: false,
                    origin: Some(author),
                    force: false,
                },
            )),
        ));
        assert_eq!(
            client
                .pending_inputs()
                .map(Segments::text)
                .collect::<Vec<_>>(),
            ["second"]
        );
        assert_eq!(
            client.view().state.queued().next().unwrap().text,
            pasted.text()
        );
        client.step(msg(1, DaemonMsg::Ack { client_seq: 2 }));
        assert_eq!(client.pending_inputs().count(), 0);
        client.step(msg(1, DaemonMsg::Draining { held: None }));
        client.run(Cmd::Input("during reconnect".into()));
        assert_eq!(client.pending_inputs().next().unwrap(), "during reconnect");
        client.open(Target::New);
        assert_eq!(client.pending_inputs().count(), 0);
    }

    #[test]
    fn pending_recall_keeps_offer_order_when_a_drain_requeues_older_input() {
        let (mut client, _) = attached(Env::Keep);
        client.run(Cmd::Input("one".into()));
        client.run(Cmd::Input("two".into()));
        client.step(msg(1, DaemonMsg::Draining { held: None }));
        client.run(Cmd::Input("three".into()));
        client.step(msg(
            1,
            DaemonMsg::Rejected {
                client_seq: 1,
                reason: Rejection::Draining,
                message: "retry".into(),
            },
        ));
        assert_eq!(
            client
                .pending_inputs()
                .map(Segments::text)
                .collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
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
        client.step(msg(1, DaemonMsg::Draining { held: None }));
        assert!(client.run(Cmd::Input("two".into())).is_empty());
        client.step(closed(1));
        let out = client.step(live(2, Caps::ALL));
        // Unanswered, "one" goes again under its number: a daemon that keeps
        // who sent what does it once.
        assert!(
            matches!(&sends(&out)[..], [
                ClientMsg::Input { segments: one, client_seq: Some(1), .. },
                ClientMsg::Input { segments: two, client_seq: Some(2), .. },
            ] if one == "one" && two == "two"),
            "{out:?}"
        );
    }

    #[test]
    fn later_submissions_need_cancellation_even_with_an_earlier_interrupt_unacknowledged() {
        let (mut client, _) = attached(Env::Keep);
        client.run(Cmd::Input("first".into()));
        client.run(Cmd::Interrupt);
        client.run(Cmd::Input("second".into()));
        let out = client.run(Cmd::Interrupt);
        assert!(matches!(
            sends(&out)[..],
            [ClientMsg::Interrupt {
                client_seq: Some(4)
            }]
        ));
    }

    #[test]
    fn queued_reads_do_not_create_interruptible_work() {
        let mut client = SessionClient::new(Env::Keep);
        client.open(Target::New);
        client.run(Cmd::Sessions);
        assert!(!client.turn().can_interrupt);
    }

    #[test]
    fn submitted_input_remains_interruptible_before_publication() {
        for connected in [false, true] {
            let mut client = if connected {
                attached(Env::Keep).0
            } else {
                let mut client = SessionClient::new(Env::Keep);
                client.open(Target::New);
                client
            };
            client.run(Cmd::Input("unpublished".into()));
            assert!(!client.turn().running);
            assert!(client.turn().can_interrupt);
            assert!(client.offer().unwrap().interrupt().is_ok());
        }
    }

    /// An input `number` of `client`'s, logged at `at`.
    fn logged(client: ClientId, number: u64, at: u64) -> DaemonMsg {
        DaemonMsg::Entry(Entry::new(
            Seq::new(at).unwrap(),
            Epoch(1),
            Body::Input {
                segments: "hello".into(),
                mode: Mode::Queue,
                taken: true,
                origin: Some(Author {
                    client,
                    seq: ClientSeq::new(number).unwrap(),
                }),
                force: false,
            },
        ))
    }

    fn idle(at: u64) -> DaemonMsg {
        DaemonMsg::Activity {
            seq: at,
            activity: Activity::Idle,
        }
    }

    #[test]
    fn a_turn_seen_to_end_is_not_interruptible_while_its_ack_lags() {
        let (mut client, _) = attached(Env::Keep);
        client.run(Cmd::Input("hello".into()));
        // Another client's mutation numbered the same is not this one's.
        client.step(msg(1, logged(ClientId::new(), 1, 2)));
        client.step(msg(1, idle(2)));
        assert!(client.turn().can_interrupt);
        client.step(msg(1, logged(client.opened.client, 1, 3)));
        client.step(msg(1, idle(3)));
        assert!(!client.turn().can_interrupt, "{:?}", client.turn());
        assert!(client.offer().unwrap().interrupt().is_err());
    }

    #[test]
    fn a_logged_mutation_answers_the_unlogged_ones_numbered_before_it() {
        let (mut client, _) = attached(Env::Keep);
        client.run(Cmd::Input("first".into()));
        // An interrupt with nothing yet to stop: acked, never logged.
        let out = client.run(Cmd::Interrupt);
        assert!(matches!(
            sends(&out)[..],
            [ClientMsg::Interrupt {
                client_seq: Some(2)
            }]
        ));
        client.run(Cmd::Input("second".into()));
        client.step(msg(1, logged(client.opened.client, 3, 2)));
        client.step(msg(1, idle(2)));
        assert!(!client.turn().can_interrupt, "{:?}", client.turn());
    }

    #[test]
    fn a_full_mutation_window_cannot_drop_cancellation_intent() {
        let (mut client, _) = attached(Env::Keep);
        for n in 0..UNANSWERED {
            client.run(Cmd::Input(n.to_string().into()));
        }
        let out = client.offer().unwrap().interrupt().unwrap();
        assert!(matches!(sends(&out)[..], [ClientMsg::Interrupt { .. }]));
        assert!(client.offer().unwrap().interrupt().unwrap().is_empty());
        assert_eq!(client.unanswered.len(), UNANSWERED + 1);
    }

    #[test]
    fn unanswered_mutations_are_never_evicted_by_new_input() {
        for disconnected in [false, true] {
            let (mut client, _) = attached(Env::Keep);
            if disconnected {
                client.step(msg(1, DaemonMsg::Draining { held: None }));
                client.step(closed(1));
            }
            for n in 0..UNANSWERED {
                let out = client.run(Cmd::Input(n.to_string().into()));
                assert_eq!(notices(&out), [] as [&Notice; 0]);
                assert_eq!(sends(&out).len(), usize::from(!disconnected));
            }
            let out = client.run(Cmd::Input("overflow".into()));
            assert!(sends(&out).is_empty(), "{out:?}");
            assert_eq!(notices(&out), vec![&Notice::Backpressure], "{out:?}");
            if !disconnected {
                client.step(msg(1, DaemonMsg::Draining { held: None }));
                client.step(closed(1));
            }
            let out = client.step(live(2, Caps::ALL));
            let replay = sends(&out);
            assert_eq!(replay.len(), UNANSWERED);
            for (n, msg) in replay.iter().enumerate() {
                assert_eq!(msg.client_seq(), Some(n as u64 + 1));
                assert!(
                    matches!(msg, ClientMsg::Input { segments: text, .. } if *text == *n.to_string())
                );
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
        assert!(
            matches!(sends(&out)[..], [ClientMsg::Input { segments: text, .. }] if text == "for B")
        );
        client.step(msg(
            3,
            DaemonMsg::Attached {
                session: b,
                workspace: "/b".into(),
            },
        ));
        client.step(msg(3, bash_call(2)));
        client.step(msg(3, ask(3, 2)));
        let out = client.run(Cmd::Approve {
            call,
            approved: true,
        });
        assert!(
            matches!(sends(&out)[..], [ClientMsg::Answer { call: approved, .. }] if *approved == call)
        );
    }

    #[test]
    fn pending_input_never_crosses_an_explicit_session_change() {
        let (mut client, _) = attached(Env::Keep);
        client.run(Cmd::Input("unanswered".into()));
        client.step(msg(1, DaemonMsg::Draining { held: None }));
        client.run(Cmd::Input("queued".into()));
        client.open(Target::New);
        let out = client.step(live(2, Caps::ALL));
        assert!(sends(&out).is_empty(), "{out:?}");
    }

    #[test]
    fn pending_input_survives_a_restart_of_the_same_session() {
        let (mut client, _) = attached(Env::Keep);
        client.run(Cmd::Input("unanswered".into()));
        client.step(msg(1, DaemonMsg::Draining { held: None }));
        client.run(Cmd::Input("queued".into()));
        client.restart();
        let out = client.step(live(2, Caps::ALL));
        assert_eq!(sends(&out).len(), 2, "{out:?}");
    }

    #[test]
    fn a_resend_after_reconnect_runs_once() {
        let (mut client, _) = attached(Env::Origin);
        let (id, _) = client.submit("late".into()).unwrap();
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
        client.step(msg(1, DaemonMsg::Draining { held: None }));
        client.step(closed(1));
        let out = client.step(live(2, Caps::ALL));
        assert_eq!(sends(&out).len(), 1, "{out:?}");
        assert_eq!(sends(&out)[0].client_seq(), Some(id.seq.get()));
        assert_eq!(client.opened.client, id.client);
        assert_eq!(client.submission(id), Some(crate::SubmissionState::Queued));
        // Taken this time: a second drain does not send it again.
        client.step(msg(2, DaemonMsg::Ack { client_seq: 1 }));
        client.step(msg(2, DaemonMsg::Draining { held: None }));
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
        assert!(client.offer().unwrap().input("hi").unwrap().is_empty());
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
        assert_eq!(
            client.offer().unwrap().interrupt().err(),
            Some(Notice::NothingRunning)
        );
        assert_eq!(
            sends(&client.offer().unwrap().input("hi").unwrap()).len(),
            1
        );
        assert_eq!(
            client.offer().unwrap().steer("more").err(),
            Some(Notice::NothingRunning)
        );
        let two = CallId::new(2).unwrap();
        assert!(client.offer().unwrap().interrupt().is_ok());
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
            [ClientMsg::Answer {
                call,
                answer,
                ..
            }] if *call == two && !answer.accepted()
        ));
        // Attached but reconnecting: offered, and queued until the
        // connection is up.
        client.step(msg(1, DaemonMsg::Draining { held: None }));
        client.step(closed(1));
        assert!(client.offer().unwrap().steer("go").unwrap().is_empty());
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
