//! One value per workspace, shared by everything working there: made on
//! first use and dropped with its last user, so a workspace nobody works
//! in releases its value. Expired map entries retire on the next miss.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

/// The `T` of each workspace in use, held weakly: its users (sessions,
/// queries, tools) keep it alive, not the map.
pub struct WorkspaceShared<T>(Mutex<BTreeMap<PathBuf, Weak<T>>>);

impl<T> Default for WorkspaceShared<T> {
    fn default() -> Self {
        Self(Mutex::default())
    }
}

impl<T> WorkspaceShared<T> {
    /// The `T` in use for `workspace`, or the one `make` makes now, given
    /// its own weak reference (for callbacks that must not keep it alive).
    /// `make` runs under the map's lock, so it only reserves: what opening
    /// takes (a walk, a watch) the value does after, once, behind a
    /// `OnceLock` of its own, while users of other workspaces never wait.
    pub fn get_or_make(&self, workspace: &Path, make: impl FnOnce(&Weak<T>) -> T) -> Arc<T> {
        let mut open = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(value) = open.get(workspace).and_then(Weak::upgrade) {
            return value;
        }
        open.retain(|_, slot| slot.strong_count() > 0);
        let value = Arc::new_cyclic(make);
        open.insert(workspace.to_owned(), Arc::downgrade(&value));
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_shares_one_value_until_its_last_user_drops_it() {
        let shared = WorkspaceShared::<u32>::default();
        let mut count = 0;
        let mut make = |_: &Weak<u32>| {
            count += 1;
            count
        };
        let first = shared.get_or_make(Path::new("/w"), &mut make);
        let second = shared.get_or_make(Path::new("/w"), &mut make);
        assert!(Arc::ptr_eq(&first, &second));
        let other = shared.get_or_make(Path::new("/v"), &mut make);
        assert_eq!((*first, *other), (1, 2));
        drop((first, second));
        assert_eq!(*shared.get_or_make(Path::new("/w"), &mut make), 3);
    }

    #[test]
    fn the_map_never_keeps_a_value_alive() {
        let shared = WorkspaceShared::<Weak<()>>::default();
        let value = shared.get_or_make(Path::new("/w"), |own| {
            // Its own reference does not count as a user.
            assert!(own.upgrade().is_none());
            Weak::new()
        });
        let weak = Arc::downgrade(&value);
        drop(value);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn live_hits_leave_retirement_to_the_next_miss() {
        let shared = WorkspaceShared::<()>::default();
        let live = shared.get_or_make(Path::new("live"), |_| ());
        drop(shared.get_or_make(Path::new("expired"), |_| ()));
        let hit = shared.get_or_make(Path::new("live"), |_| panic!("live value replaced"));
        assert!(Arc::ptr_eq(&live, &hit));
        assert_eq!(shared.0.lock().unwrap().len(), 2);
        let new = shared.get_or_make(Path::new("new"), |_| ());
        let open = shared.0.lock().unwrap();
        assert_eq!(open.len(), 2);
        assert!(!open.contains_key(Path::new("expired")));
        assert!(Arc::ptr_eq(
            &new,
            &open[Path::new("new")].upgrade().unwrap()
        ));
    }
}
