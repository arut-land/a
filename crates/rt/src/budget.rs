//! Credit for data retained across independently admitted execution lanes.

use std::sync::{Arc, Mutex};

use crate::{Notify, lock};

/// A shared byte budget whose credit follows data rather than worker execution.
#[derive(Clone, Debug)]
pub struct Budget(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    limit: usize,
    used: Mutex<usize>,
    released: Notify,
}

/// Reserved bytes, returned to their budget on drop. Move this with the data,
/// including into accepted workers that may outlive their awaiting caller.
#[derive(Debug)]
#[must_use]
pub struct Credit {
    budget: Arc<Inner>,
    bytes: usize,
}

impl Budget {
    /// Creates a shared retained-data budget.
    ///
    /// # Panics
    /// Panics if `bytes` is zero.
    pub fn new(bytes: usize) -> Self {
        assert!(bytes > 0, "a retained-data budget must be positive");
        Self(Arc::new(Inner {
            limit: bytes,
            used: Mutex::new(0),
            released: Notify::new(),
        }))
    }

    /// Waits for credit before allocating data. A request larger than the budget
    /// reserves the whole budget and runs alone; no request is refused for size.
    /// Dropping a waiting future unregisters it without returning unacquired credit.
    pub async fn acquire(&self, bytes: usize) -> Credit {
        let bytes = bytes.min(self.0.limit);
        let mut credit = None;
        self.0
            .released
            .until(|| {
                let mut used = lock(&self.0.used);
                if bytes > self.0.limit - *used {
                    return false;
                }
                *used += bytes;
                credit = Some(Credit {
                    budget: Arc::clone(&self.0),
                    bytes,
                });
                true
            })
            .await;
        credit.expect("the waiter acquired credit")
    }
}

impl Drop for Credit {
    fn drop(&mut self) {
        *lock(&self.budget.used) -= self.bytes;
        self.budget.released.notify();
    }
}
