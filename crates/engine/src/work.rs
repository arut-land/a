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
    // Retained control identity, so recycling a leader pid cannot replace live work.
    Job(usize),
    Sync,
}

#[derive(Clone)]
pub(super) struct WorkHandle<K = WorkId> {
    pub(super) id: K,
    pub(super) fence: u64,
    pub(super) admission: Admission,
    pub(super) cancel: Cancel,
    /// Set when the session stops waiting on a call ([`kernel::Work::Detach`]).
    pub(super) detach: Flag,
    pub(super) ended: Flag,
}

impl<K: Send + 'static> WorkHandle<K> {
    pub(super) fn new(id: K, fence: u64, admission: Admission, cancel: Cancel) -> Self {
        Self {
            id,
            fence,
            admission,
            cancel,
            detach: Flag::default(),
            ended: Flag::default(),
        }
    }

    pub(super) fn spawn(
        self,
        rt: &dyn rt::Runtime,
        work: impl Future<Output = ()> + Send + 'static,
    ) {
        rt.spawn(Box::pin(async move {
            work.await;
            self.ended.set();
        }));
    }
}

#[derive(Default)]
pub(super) struct WorkSet(Vec<WorkHandle>);

impl WorkSet {
    pub(super) fn insert(&mut self, handle: WorkHandle) {
        self.cancel(&handle.id);
        self.0.push(handle);
    }

    pub(super) fn get_mut(&mut self, id: &WorkId) -> Option<&mut WorkHandle> {
        self.0.iter_mut().find(|handle| &handle.id == id)
    }

    pub(super) fn cancel(&mut self, id: &WorkId) {
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

impl Drop for WorkSet {
    fn drop(&mut self) {
        self.revoke();
    }
}

impl Session {
    pub(super) fn own_work(&self, id: WorkId, admission: Admission, cancel: Cancel) -> WorkHandle {
        let mut owned = lock(&self.work);
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
        handle.spawn(&*self.shared.rt, task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::Requirement;

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
        let replaced = handles[4].clone();
        let mut replacement = replaced.clone();
        replacement.cancel = Cancel::default();
        work.insert(replacement.clone());
        assert!(replaced.cancel.is_set());
        assert!(!replacement.cancel.is_set());
        work.revoke();
        assert!(replacement.cancel.is_set());
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
