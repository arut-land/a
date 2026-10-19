//! A session as the handlers at its decision points see it.

use ext::point::View;
use ext::{PointCx, States};

use super::Session;
use crate::lock;

/// Read only when a handler asks: a point with no handler costs nothing.
impl View for Session {
    fn model(&self) -> String {
        self.model_spec()
    }

    fn states(&self, read: &mut dyn FnMut(&States)) {
        read(&lock(&self.states));
    }
}

impl Session {
    pub(super) fn point(&self) -> PointCx<'_> {
        PointCx::new(self.id, &self.workspace, self)
    }
}
