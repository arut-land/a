//! Process groups a session owns past the call that started them: a
//! background command, or what a command left running. An extension that
//! hands one over with [`Cx::own`](crate::Cx::own) says how to tell it
//! ended; then [`ENDED`] is notified, so whoever counts jobs looks again.

/// Notified each time an owned group ends.
pub static ENDED: rt::Notify = rt::Notify::new();
