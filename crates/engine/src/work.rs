//! Live ownership is generation scoped; durable call and delivery identities stay in the log.

use super::{
    Admission, BoxFuture, CallId, Cancel, Flag, Future, Ordering, Session, SessionId, lock,
};

#[derive(Clone, PartialEq, Eq)]
pub(super) enum WorkId {
    Call(CallId),
    Child(SessionId),
    Reaction(u64),
    Delivery,
    Report(SessionId),
    Job(u32),
    Sync,
}

#[derive(Clone)]
pub(super) struct WorkHandle<K = WorkId> {
    pub(super) id: K,
    pub(super) fence: u64,
    pub(super) admission: Admission,
    pub(super) cancel: Cancel,
    pub(super) ended: Flag,
}

impl<K> WorkHandle<K> {
    pub(super) fn new(id: K, fence: u64, admission: Admission, cancel: Cancel) -> Self {
        Self {
            id,
            fence,
            admission,
            cancel,
            ended: Flag::default(),
        }
    }

    pub(super) async fn run<T>(self, work: impl Future<Output = T>) -> T {
        let result = work.await;
        self.ended.set();
        result
    }
}

pub(super) struct WorkSet<K>(Vec<WorkHandle<K>>);

impl<K> Default for WorkSet<K> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<K: PartialEq> WorkSet<K> {
    pub(super) fn insert(&mut self, handle: WorkHandle<K>) {
        self.cancel(&handle.id);
        self.0.push(handle);
    }

    pub(super) fn get(&self, id: &K) -> Option<&WorkHandle<K>> {
        self.0.iter().find(|handle| &handle.id == id)
    }

    pub(super) fn cancel(&mut self, id: &K) {
        if let Some(at) = self.0.iter().position(|handle| &handle.id == id) {
            self.0.remove(at).cancel.set();
        }
    }

    pub(super) fn revoke(&mut self) {
        for handle in self.0.drain(..) {
            handle.cancel.set();
        }
    }
}

impl<K> Drop for WorkSet<K> {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.cancel.set();
        }
    }
}

impl Session {
    pub(super) fn own_work(&self, id: WorkId, admission: Admission, cancel: Cancel) -> WorkHandle {
        let mut owned = lock(&self.work);
        if matches!(id, WorkId::Call(_))
            && let Some(previous) = owned.0.iter_mut().find(|handle| handle.id == id)
        {
            previous.admission = admission;
            return previous.clone();
        }
        let handle = WorkHandle::new(id, self.fence.load(Ordering::SeqCst), admission, cancel);
        owned.0.retain(|previous| {
            if !matches!(previous.id, WorkId::Call(_)) && previous.ended.is_set() {
                previous.cancel.set();
                return false;
            }
            true
        });
        owned.insert(handle.clone());
        handle
    }

    pub(super) fn spawn_work(
        &self,
        id: WorkId,
        admission: Admission,
        work: impl FnOnce(Cancel) -> BoxFuture<'static, ()>,
    ) {
        let handle = self.own_work(id, admission, Cancel::default());
        let task = work(handle.cancel.clone());
        self.shared.rt.spawn(Box::pin(handle.run(task)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::Requirement;

    #[test]
    fn retirement_revokes_replaced_report_tasks() {
        let mut work = WorkSet::default();
        let id = WorkId::Report(SessionId::new());
        let first = WorkHandle::new(
            id.clone(),
            7,
            Admission {
                through: 11,
                requirement: Requirement::Durable,
            },
            Cancel::default(),
        );
        let second = WorkHandle::new(id, 7, first.admission, Cancel::default());
        work.insert(first.clone());
        work.insert(second.clone());
        work.revoke();
        assert!(second.cancel.is_set());
        assert!(
            first.cancel.is_set(),
            "replacement must not orphan live report work"
        );
    }

    #[test]
    fn generation_retirement_revokes_every_owned_task() {
        let mut work = WorkSet::default();
        let ids = [
            WorkId::Call(CallId::new(1).unwrap()),
            WorkId::Child(SessionId::new()),
            WorkId::Reaction(2),
            WorkId::Delivery,
            WorkId::Report(SessionId::new()),
            WorkId::Job(3),
            WorkId::Sync,
        ];
        let handles: Vec<_> = ids
            .into_iter()
            .map(|id| {
                let handle = WorkHandle::new(
                    id,
                    7,
                    Admission {
                        through: 11,
                        requirement: Requirement::Durable,
                    },
                    Cancel::default(),
                );
                work.insert(handle.clone());
                handle
            })
            .collect();
        work.revoke();
        assert!(handles.iter().all(|handle| handle.cancel.is_set()));
        assert!(work.0.is_empty());
        let next = WorkHandle::new(
            WorkId::Reaction(4),
            8,
            Admission {
                through: 12,
                requirement: Requirement::Ordered,
            },
            Cancel::default(),
        );
        work.insert(next.clone());
        assert!(!next.cancel.is_set());
        drop(work);
        assert!(next.cancel.is_set());
    }
}
