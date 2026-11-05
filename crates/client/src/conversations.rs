//! A conversation family observed through public heads without transcript demand.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use protocol::{Activity, SessionId, Target, ledger::OutcomeKind, view::Usage};

use crate::{Daemon, Driver, Effect, Env, Event, Notice, SessionClient};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationHead {
    pub session: SessionId,
    pub seq: u64,
    pub identity: protocol::history::Conversation,
    pub usage: Usage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversationStatus {
    Connecting,
    Running,
    Finished,
    Failed,
    Interrupted,
}

// Grace intervals apply only to terminal rows outside the current view.
const TERMINAL_RETENTION: [(ConversationStatus, u64); 3] = [
    (ConversationStatus::Finished, 5),
    (ConversationStatus::Failed, 10),
    (ConversationStatus::Interrupted, 30),
];

impl ConversationStatus {
    fn retention(self) -> Option<Duration> {
        TERMINAL_RETENTION
            .iter()
            .find_map(|&(status, seconds)| (self == status).then(|| Duration::from_secs(seconds)))
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationRow {
    pub session: SessionId,
    pub parent: Option<SessionId>,
    pub depth: usize,
    pub agent: String,
    pub title: String,
    pub status: ConversationStatus,
    pub admitted_unix_ms: Option<u64>,
    pub elapsed_ns: Option<u64>,
    pub usage: Usage,
}

impl ConversationRow {
    /// Human observations; an unavailable duration is never invented from attach time.
    pub fn detail(&self, now_unix_ms: u64) -> String {
        let elapsed = if self.status == ConversationStatus::Running {
            self.admitted_unix_ms
                .map(|start| now_unix_ms.saturating_sub(start) / 1000)
        } else {
            self.elapsed_ns.map(|elapsed| elapsed / 1_000_000_000)
        };
        let duration = elapsed.map_or_else(|| "—".into(), |seconds| format!("{seconds}s"));
        format!(
            "{} · {duration} · {} tokens",
            self.status.name(),
            self.usage.input.saturating_add(self.usage.output)
        )
    }
}

/// Relationships and progress come from public heads, independently of history pages.
#[derive(Debug)]
pub struct ConversationTree {
    origin: SessionId,
    nodes: BTreeMap<SessionId, ConversationRow>,
    expires: BTreeMap<SessionId, (Option<u64>, Instant)>,
}

impl ConversationTree {
    pub fn new(origin: SessionId) -> Self {
        Self {
            origin,
            nodes: BTreeMap::new(),
            expires: BTreeMap::new(),
        }
    }

    /// Changes the pinned view without resetting any outcome's expiry.
    pub fn view(&mut self, session: SessionId) {
        self.origin = session;
    }

    pub fn apply(&mut self, head: &ConversationHead, activity: Option<Activity>) {
        self.apply_at(head, activity, Instant::now(), unix_ms());
    }

    /// Folds a head at an explicit clock sample without extending repeated outcomes.
    pub fn apply_at(
        &mut self,
        head: &ConversationHead,
        activity: Option<Activity>,
        now: Instant,
        now_unix_ms: u64,
    ) {
        let identity = &head.identity;
        let status = match activity {
            Some(Activity::Idle | Activity::Done | Activity::Failed) => match identity.outcome {
                Some(OutcomeKind::Failed) => ConversationStatus::Failed,
                Some(OutcomeKind::Interrupted) => ConversationStatus::Interrupted,
                _ if activity == Some(Activity::Failed) => ConversationStatus::Failed,
                _ => ConversationStatus::Finished,
            },
            None => self
                .nodes
                .get(&head.session)
                .map_or(ConversationStatus::Connecting, |row| row.status),
            _ => ConversationStatus::Running,
        };
        if let Some(retention) = status.retention() {
            let age = identity.settled_unix_ms.map_or(Duration::ZERO, |at| {
                Duration::from_millis(now_unix_ms.saturating_sub(at)).min(retention)
            });
            let deadline = now + retention.saturating_sub(age);
            let expires = self
                .expires
                .entry(head.session)
                .or_insert((identity.settled_unix_ms, deadline));
            if expires.0 != identity.settled_unix_ms {
                *expires = (identity.settled_unix_ms, deadline);
            }
        } else {
            self.expires.remove(&head.session);
        }
        self.nodes.insert(
            head.session,
            ConversationRow {
                session: head.session,
                parent: identity.parent,
                depth: 0,
                agent: identity.agent.clone().unwrap_or_else(|| "agent".into()),
                title: identity.title.clone().unwrap_or_else(|| "main".into()),
                status,
                admitted_unix_ms: identity.admitted_unix_ms,
                elapsed_ns: identity.elapsed_ns,
                usage: head.usage,
            },
        );
        for child in &identity.children {
            self.nodes
                .entry(child.session)
                .or_insert_with(|| ConversationRow {
                    session: child.session,
                    parent: Some(head.session),
                    depth: 0,
                    agent: child.agent.clone(),
                    title: child.title.clone(),
                    status: ConversationStatus::Connecting,
                    admitted_unix_ms: Some(child.admitted_unix_ms),
                    elapsed_ns: None,
                    usage: Usage::default(),
                });
        }
    }

    pub fn rows(&self) -> Vec<ConversationRow> {
        self.rows_at(Instant::now())
    }

    /// The current view stays reachable even after its terminal grace interval.
    /// Expiry changes presentation only; transcript navigation retains the session.
    pub fn rows_at(&self, now: Instant) -> Vec<ConversationRow> {
        let mut root = self.origin;
        while let Some(parent) = self.nodes.get(&root).and_then(|row| row.parent) {
            root = parent;
        }
        let mut rows = Vec::new();
        self.branch(root, 0, &mut rows);
        rows.retain(|row| {
            row.depth == 0
                || row.session == self.origin
                || self
                    .expires
                    .get(&row.session)
                    .is_none_or(|(_, deadline)| now < *deadline)
        });
        rows
    }

    /// One wake suffices for the earliest unpinned terminal row still visible.
    pub fn next_expiry(&self, now: Instant) -> Option<Instant> {
        self.nodes
            .values()
            .filter(|row| row.parent.is_some() && row.session != self.origin)
            .filter_map(|row| {
                self.expires
                    .get(&row.session)
                    .map(|(_, deadline)| *deadline)
            })
            .filter(|at| *at > now)
            .min()
    }

    fn branch(&self, session: SessionId, depth: usize, rows: &mut Vec<ConversationRow>) {
        if let Some(row) = self.nodes.get(&session) {
            let mut row = row.clone();
            row.depth = depth;
            if depth == 0 {
                row.title = "main".into();
            }
            rows.push(row);
        }
        for child in self
            .nodes
            .values()
            .filter(|row| row.parent == Some(session))
        {
            self.branch(child.session, depth + 1, rows);
        }
    }
}

struct Monitor {
    client: SessionClient,
    handle: crate::ClientHandle,
}
struct State {
    tree: ConversationTree,
    monitors: BTreeMap<SessionId, Monitor>,
    observed: BTreeMap<SessionId, u64>,
    published: Vec<ConversationRow>,
}
struct Shared {
    rt: rt::Rt,
    daemon: Daemon,
    state: Mutex<State>,
    sink: Box<dyn Fn(Vec<ConversationRow>) + Send + Sync>,
    ticking: AtomicBool,
    closed: AtomicBool,
    wake: Arc<rt::Notify>,
}

/// Owns head-only connections for one family. Dropping it releases every observer.
pub struct ConversationWatch {
    shared: Arc<Shared>,
    _catalog: crate::SessionWatch,
}

impl ConversationWatch {
    pub fn start(
        rt: rt::Rt,
        daemon: Daemon,
        origin: SessionId,
        sink: impl Fn(Vec<ConversationRow>) + Send + Sync + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            rt,
            daemon,
            state: Mutex::new(State {
                tree: ConversationTree::new(origin),
                monitors: BTreeMap::new(),
                observed: BTreeMap::new(),
                published: Vec::new(),
            }),
            sink: Box::new(sink),
            ticking: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            wake: Arc::new(rt::Notify::new()),
        });
        shared.add(origin);
        let weak = Arc::downgrade(&shared);
        let catalog = crate::SessionWatch::start(
            &shared.rt,
            &crate::Daemons::one(crate::Member::LOCAL, shared.daemon.clone()),
            move |listed, _| {
                if let Some(shared) = weak.upgrade() {
                    shared.listed(listed);
                }
            },
        );
        Self {
            shared,
            _catalog: catalog,
        }
    }

    /// Keeps this family's observations while changing which session is pinned.
    pub fn view(&self, session: SessionId) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.tree.view(session);
        let rows = state.tree.rows_at(self.shared.rt.now());
        if state.published != rows {
            state.published.clone_from(&rows);
            (self.shared.sink)(rows);
        }
        drop(state);
        self.shared.arm();
    }

    /// Uses the child's existing cancellation path, with normal admission rules.
    pub fn interrupt(&self, session: SessionId) -> Result<(), Notice> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let monitor = state
            .monitors
            .get_mut(&session)
            .ok_or(Notice::NotConnected)?;
        let effects = monitor.client.offer()?.interrupt()?;
        send(&monitor.handle, effects);
        Ok(())
    }
}

impl Shared {
    fn add(self: &Arc<Self>, session: SessionId) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.monitors.contains_key(&session) || state.observed.contains_key(&session) {
            return;
        }
        let weak = Arc::downgrade(self);
        let handle = Driver::new(move |_, event| {
            if let Some(shared) = weak.upgrade() {
                shared.event(session, event);
            }
        })
        .start(self.rt.clone(), self.daemon.clone());
        let mut client = SessionClient::new(Env::Keep);
        client.release_history();
        let effects = client.open(Target::Id(session));
        state.monitors.insert(
            session,
            Monitor {
                client,
                handle: handle.clone(),
            },
        );
        drop(state);
        send(&handle, effects);
    }

    fn listed(self: &Arc<Self>, listed: &[crate::Listed]) {
        let changed: Vec<_> = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            listed
                .iter()
                .filter_map(|listed| {
                    let id = listed.summary.id;
                    if !state.monitors.contains_key(&id)
                        && state
                            .observed
                            .get(&id)
                            .is_some_and(|seq| listed.summary.entries > *seq)
                    {
                        state.observed.remove(&id);
                        Some(id)
                    } else {
                        None
                    }
                })
                .collect()
        };
        for session in changed {
            self.add(session);
        }
    }

    fn arm(self: &Arc<Self>) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        self.wake.notify();
        if self.ticking.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        let rt = self.rt.clone();
        let wake = Arc::clone(&self.wake);
        self.rt.spawn(Box::pin(async move {
            let mut seen = wake.count();
            loop {
                let delay = {
                    let Some(shared) = weak.upgrade() else { return };
                    if shared.closed.load(Ordering::Acquire) {
                        return;
                    }
                    let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
                    let now = rt.now();
                    let rows = state.tree.rows_at(now);
                    let running = rows
                        .iter()
                        .any(|row| row.status == ConversationStatus::Running);
                    if state.published != rows || running {
                        state.published.clone_from(&rows);
                        (shared.sink)(rows);
                    }
                    let next = state.tree.next_expiry(now);
                    let delay = if running {
                        Some(next.map_or(Duration::from_secs(1), |at| {
                            (at - now).min(Duration::from_secs(1))
                        }))
                    } else {
                        next.map(|at| at - now)
                    };
                    if delay.is_none() {
                        shared.ticking.store(false, Ordering::Release);
                    }
                    delay
                };
                let Some(delay) = delay else { return };
                rt::or(rt.sleep(delay), wake.changed(&mut seen)).await;
            }
        }));
    }

    fn event(self: &Arc<Self>, session: SessionId, event: Event) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(monitor) = state.monitors.get_mut(&session) else {
            return;
        };
        let effects = monitor.client.step(event);
        send(&monitor.handle, effects);
        let Some(head) = monitor.client.conversation().cloned() else {
            return;
        };
        let activity = monitor.client.turn().activity;
        let needed: Vec<_> = head
            .identity
            .children
            .iter()
            .map(|child| child.session)
            .chain(head.identity.parent)
            .collect();
        state.observed.insert(session, head.seq);
        state
            .tree
            .apply_at(&head, activity, self.rt.now(), unix_ms());
        if matches!(
            activity,
            Some(Activity::Idle | Activity::Done | Activity::Failed)
        ) {
            state.monitors.remove(&session);
        }
        let rows = state.tree.rows_at(self.rt.now());
        if state.published != rows {
            state.published.clone_from(&rows);
            (self.sink)(rows);
        }
        drop(state);
        for session in needed {
            self.add(session);
        }
        self.arm();
    }
}

fn send(handle: &crate::ClientHandle, effects: Vec<Effect>) {
    for effect in effects {
        if let Effect::Io(io) = effect {
            let _ = handle.io(io);
        }
    }
}

fn unix_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

impl Drop for ConversationWatch {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.wake.notify();
    }
}
