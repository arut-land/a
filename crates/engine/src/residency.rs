//! Session transitions update identity, activity and the next real idle deadline.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{SessionId, SessionSummary};

use super::{Live, Slot};

pub(super) struct State {
    pub busy: bool,
    pub jobs: bool,
    pub idle_since: Option<Instant>,
}

struct Loaded {
    live: Arc<Live>,
    busy: bool,
    jobs: bool,
    deadline: Option<Instant>,
}

/// Identity, activity and deadlines change together; a pulse consumes only
/// changed identities and deadlines that have actually expired.
pub(super) struct Sessions {
    known: HashMap<SessionId, usize>,
    ordered: Vec<Arc<Slot>>,
    loaded: HashMap<SessionId, Loaded>,
    pending: BTreeSet<SessionId>,
    changed: BTreeSet<SessionId>,
    handover: Option<BTreeSet<SessionId>>,
    deadlines: BTreeSet<(Instant, SessionId)>,
    listing: BTreeMap<SessionId, SessionSummary>,
    /// Only changed rows retain their previously published value; reversals
    /// before publication must not send duplicate full lists.
    previous: BTreeMap<SessionId, Option<SessionSummary>>,
    busy: usize,
    jobs: usize,
    after: Duration,
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new(Vec::new(), super::Config::default().hibernate)
    }
}

impl Sessions {
    pub fn new(slots: Vec<Arc<Slot>>, after: Duration) -> Self {
        let listing = slots.iter().map(|s| (s.header.id, s.summary())).collect();
        Self {
            known: slots
                .iter()
                .enumerate()
                .map(|(at, slot)| (slot.header.id, at))
                .collect(),
            ordered: slots,
            loaded: HashMap::new(),
            pending: BTreeSet::new(),
            changed: BTreeSet::new(),
            handover: None,
            deadlines: BTreeSet::new(),
            listing,
            previous: BTreeMap::new(),
            busy: 0,
            jobs: 0,
            after,
        }
    }

    pub fn len(&self) -> usize {
        self.known.len()
    }

    pub fn get(&self, id: SessionId) -> Option<Arc<Slot>> {
        self.known.get(&id).map(|&at| Arc::clone(&self.ordered[at]))
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Arc<Slot>> {
        self.ordered.iter()
    }

    #[cfg(test)]
    pub fn all(&self) -> Vec<Arc<Slot>> {
        self.ordered.clone()
    }

    pub fn insert(&mut self, slot: Arc<Slot>) {
        let id = slot.header.id;
        self.replace_listing(id, Some(super::fresh(&slot.header)));
        self.known.insert(id, self.ordered.len());
        self.ordered.push(slot);
        self.loading(id);
    }

    pub fn remove(&mut self, id: SessionId) {
        self.leave(id);
        self.pending.remove(&id);
        self.changed.remove(&id);
        if let Some(at) = self.known.remove(&id) {
            self.ordered.remove(at);
            for (at, slot) in self.ordered.iter().enumerate().skip(at) {
                *self
                    .known
                    .get_mut(&slot.header.id)
                    .expect("a registered slot") = at;
            }
        }
        self.replace_listing(id, None);
    }

    pub fn loading(&mut self, id: SessionId) {
        self.pending.insert(id);
        self.changed(id);
    }

    pub fn install(&mut self, live: Arc<Live>) {
        let id = live.id;
        self.pending.remove(&id);
        self.loaded.insert(
            id,
            Loaded {
                live,
                busy: false,
                jobs: false,
                deadline: None,
            },
        );
        self.changed(id);
    }

    pub fn leave(&mut self, id: SessionId) {
        if let Some(previous) = self.loaded.remove(&id) {
            self.busy -= usize::from(previous.busy);
            self.jobs -= usize::from(previous.jobs);
            if let Some(at) = previous.deadline {
                self.deadlines.remove(&(at, id));
            }
        }
        self.changed(id);
    }

    pub fn stopping(&mut self, id: SessionId) {
        self.leave(id);
        self.pending.insert(id);
    }

    pub fn ended(&mut self, id: SessionId) {
        self.leave(id);
        self.pending.remove(&id);
    }

    pub fn changed(&mut self, id: SessionId) {
        self.changed.insert(id);
        if let Some(pending) = &mut self.handover {
            pending.insert(id);
        }
    }

    pub fn handover_changed(&mut self) -> Vec<(SessionId, Option<Arc<Live>>)> {
        let pending = self
            .handover
            .get_or_insert_with(|| self.loaded.keys().copied().collect());
        std::mem::take(pending)
            .into_iter()
            .map(|id| {
                (
                    id,
                    self.loaded.get(&id).map(|entry| Arc::clone(&entry.live)),
                )
            })
            .collect()
    }

    pub fn take_changed(&mut self) -> Vec<Arc<Slot>> {
        std::mem::take(&mut self.changed)
            .into_iter()
            .filter_map(|id| self.get(id))
            .collect()
    }

    pub fn update(
        &mut self,
        id: SessionId,
        state: Option<State>,
        summary: SessionSummary,
        now: Instant,
    ) {
        if !self.known.contains_key(&id) {
            return;
        }
        if let Some(entry) = self.loaded.get_mut(&id)
            && let Some(state) = state
        {
            self.busy = self.busy - usize::from(entry.busy) + usize::from(state.busy);
            self.jobs = self.jobs - usize::from(entry.jobs) + usize::from(state.jobs);
            entry.busy = state.busy;
            entry.jobs = state.jobs;
            let deadline = if self.after.is_zero() {
                Some(now)
            } else {
                state.idle_since.map(|since| since + self.after)
            };
            if entry.deadline != deadline {
                if let Some(at) = entry.deadline {
                    self.deadlines.remove(&(at, id));
                }
                if let Some(at) = deadline {
                    self.deadlines.insert((at, id));
                }
                entry.deadline = deadline;
            }
        }
        self.replace_listing(id, Some(summary));
    }

    fn replace_listing(&mut self, id: SessionId, next: Option<SessionSummary>) {
        if self.listing.get(&id) == next.as_ref() {
            return;
        }
        let original = self
            .previous
            .get(&id)
            .cloned()
            .unwrap_or_else(|| self.listing.get(&id).cloned());
        if original == next {
            self.previous.remove(&id);
        } else {
            self.previous.insert(id, original);
        }
        match next {
            Some(summary) => {
                self.listing.insert(id, summary);
            }
            None => {
                self.listing.remove(&id);
            }
        }
    }

    pub fn due(&mut self, now: Instant) -> Vec<Arc<Slot>> {
        let mut due = Vec::new();
        while self.deadlines.first().is_some_and(|(at, _)| *at <= now) {
            let (_, id) = self.deadlines.pop_first().expect("a due deadline");
            if let Some(entry) = self.loaded.get_mut(&id) {
                entry.deadline = None;
            }
            if let Some(slot) = self.get(id) {
                due.push(slot);
            }
        }
        due
    }

    #[cfg(test)]
    pub fn scheduled(&self) -> usize {
        self.deadlines.len()
    }

    pub fn next(&self) -> Option<Instant> {
        self.deadlines.first().map(|(at, _)| *at)
    }

    pub fn busy(&self) -> bool {
        self.busy != 0
    }
    pub fn at_work(&self) -> bool {
        self.busy != 0 || self.jobs != 0
    }

    pub fn loaded(&self) -> Vec<Arc<Live>> {
        let mut loaded: Vec<_> = self
            .loaded
            .values()
            .map(|entry| Arc::clone(&entry.live))
            .collect();
        loaded.sort_by_key(|live| self.known[&live.id]);
        loaded
    }

    pub fn active(&self) -> Vec<Arc<Slot>> {
        self.loaded
            .keys()
            .chain(self.pending.iter())
            .filter_map(|id| self.get(*id))
            .collect()
    }

    pub fn list(&self) -> Vec<SessionSummary> {
        self.listing.values().rev().cloned().collect()
    }

    pub fn take_list_changed(&mut self) -> bool {
        !std::mem::take(&mut self.previous).is_empty()
    }
}
