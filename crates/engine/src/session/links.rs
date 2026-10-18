//! Session links: a session starts a child it owns and durably watches it.
//! The link, the watch and its firing are entries in the parent's log, so a
//! watch outlives the daemon: recovery re-arms every watch that has not
//! fired, and a watch fires at most once because only the parent's actor
//! logs it, after checking the log.

use std::collections::HashSet;
use std::io;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use protocol::{Body, Entry, Environment, Outcome, SessionId};
use serde_json::Value;
use store::{Header, Loaded, Parent};

use super::{Msg, Session};
use crate::{Daemon, Slot, lock};

/// The watches in `entries` that have not fired, as `(seq, child)`.
pub fn armed(entries: &[Entry]) -> impl Iterator<Item = (u64, SessionId)> {
    let fired: HashSet<u64> = entries
        .iter()
        .filter_map(|e| match e.body {
            Body::Fired { watch, .. } => Some(watch),
            _ => None,
        })
        .collect();
    entries.iter().filter_map(move |e| match e.body {
        Body::Watch { child } if !fired.contains(&e.seq) => Some((e.seq, child)),
        _ => None,
    })
}

/// What a settled child hands back: its last answer or outcome.
fn result(entries: &[Entry]) -> Outcome {
    entries
        .iter()
        .rev()
        .find_map(|e| match &e.body {
            Body::Assistant { text } => Some(Outcome::Done {
                output: Value::String(text.clone()),
            }),
            Body::ToolResult { outcome, .. }
            | Body::ModelResult { outcome, .. }
            | Body::Fired { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .unwrap_or_else(|| Outcome::Failed {
            error: "the child settled without a result".to_owned(),
        })
}

impl Session {
    /// Starts a child on `input` and watches it. The link commits first, so
    /// a crash before the child exists, or before the watch, leaves the rest
    /// to recovery.
    pub(super) fn spawn(&self, input: &str) -> io::Result<()> {
        let child = SessionId::new();
        let call = self.append(Body::Link {
            child,
            input: input.to_owned(),
        })?;
        self.start_child(child, call, input)?;
        self.watch(child)
    }

    /// Commits a watch on `child`; it counts as work until it fires.
    fn watch(&self, child: SessionId) -> io::Result<()> {
        self.append(Body::Watch { child })?;
        self.queued.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn start_child(&self, child: SessionId, call: u64, input: &str) -> io::Result<()> {
        let daemon = self
            .shared
            .daemon
            .upgrade()
            .ok_or_else(|| io::Error::other("no daemon to start a child session in"))?;
        let header = Header {
            id: child,
            workspace: self.workspace.clone(),
            parent: Some(Parent {
                session: self.id,
                call,
            }),
        };
        daemon.start_child(header, input, lock(&self.env).clone())
    }

    /// Re-arms links after a restart: a link without a watch gets one, and
    /// every child still watched is started, so it runs to its end and
    /// reports back.
    pub(super) fn relink(&self) -> io::Result<()> {
        let todo: Vec<(u64, SessionId, String, bool)> = {
            let st = self.lock();
            let armed: Vec<SessionId> = armed(&st.entries).map(|(_, child)| child).collect();
            let watched = |child| {
                st.entries
                    .iter()
                    .any(|e| matches!(e.body, Body::Watch { child: c } if c == child))
            };
            st.entries
                .iter()
                .filter_map(|e| match &e.body {
                    Body::Link { child, input } => {
                        let watched = watched(*child);
                        (!watched || armed.contains(child))
                            .then(|| (e.seq, *child, input.clone(), watched))
                    }
                    _ => None,
                })
                .collect()
        };
        for (call, child, input, watched) in todo {
            self.start_child(child, call, &input)?;
            if !watched {
                self.watch(child)?;
            }
        }
        Ok(())
    }

    /// Queues child `child`'s result for this session's actor.
    fn settle(&self, child: SessionId, outcome: Outcome) {
        let _ = self.mailbox.send(Msg::Settled { child, outcome });
    }

    /// Fires every armed watch on `child`; one that has fired already stays
    /// as it is.
    pub(super) fn fire(&self, child: SessionId, outcome: &Outcome) {
        let watches: Vec<u64> = armed(&self.lock().entries)
            .filter_map(|(watch, watched)| (watched == child).then_some(watch))
            .collect();
        for watch in watches {
            self.work(|| {
                let outcome = outcome.clone();
                self.append(Body::Fired { watch, outcome }).map(drop)
            });
        }
    }

    /// Tells the parent, if any, that this session has settled, and with
    /// what. The parent ignores reports for watches that have fired, so
    /// reporting again is harmless.
    pub(super) fn report(&self) {
        let (Some(parent), Some(daemon)) = (self.parent, self.shared.daemon.upgrade()) else {
            return;
        };
        let outcome = result(&self.lock().entries);
        if let Err(e) = daemon.settled(parent.session, self.id, outcome) {
            self.warn(format_args!("reporting to {}: {e}", parent.session.short()));
        }
    }
}

impl Daemon {
    /// Starts child session `header.id` on `input` unless it runs already:
    /// made if missing, given `input` if its log is empty, and loaded if
    /// hibernated. Calling it again is harmless, which recovery relies on.
    /// It runs commands in `env`, its parent's.
    fn start_child(&self, header: Header, input: &str, env: Option<Environment>) -> io::Result<()> {
        let mut slots = lock(&self.slots);
        let Some(slot) = slots.iter().find(|s| s.header.id == header.id).cloned() else {
            let loaded = self.shared.store.create(&header)?;
            let session = self.seed(header.clone(), loaded, input, env)?;
            slots.push(Arc::new(Slot {
                header,
                live: Mutex::new(Some(session)),
            }));
            return Ok(());
        };
        drop(slots);
        let mut live = lock(&slot.live);
        if live.is_none() {
            let loaded = self.shared.store.load(slot.header.id)?;
            *live = Some(self.seed(slot.header.clone(), loaded, input, env)?);
        }
        Ok(())
    }

    /// Starts a child, first committing `env` (redacted) and `input` as its
    /// opening entries if its log is empty. Nothing else writes a log that
    /// has no actor yet.
    fn seed(
        &self,
        header: Header,
        mut loaded: Loaded,
        input: &str,
        env: Option<Environment>,
    ) -> io::Result<Arc<Session>> {
        if loaded.entries.is_empty() {
            let opening = env.as_ref().map(|env| Body::Environment(env.redacted()));
            let user = Body::User {
                text: input.to_owned(),
            };
            for (seq, body) in (1..).zip(opening.into_iter().chain([user])) {
                let entry = Entry {
                    seq,
                    epoch: 0,
                    body,
                };
                loaded.log.append(&entry)?;
                loaded.entries.push(entry);
            }
        }
        Ok(Session::start_in(
            header,
            loaded,
            Arc::clone(&self.shared),
            env,
        ))
    }

    /// Hands child `child`'s result to its parent, loading the parent if it
    /// is hibernated.
    fn settled(&self, parent: SessionId, child: SessionId, outcome: Outcome) -> io::Result<()> {
        let slot = self
            .slots()
            .into_iter()
            .find(|s| s.header.id == parent)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such parent session"))?;
        self.with_live(&slot, |session| session.settle(child, outcome))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64, body: Body) -> Entry {
        Entry {
            seq,
            epoch: 0,
            body,
        }
    }

    #[test]
    fn only_unfired_watches_are_armed() {
        let (a, b) = (SessionId::new(), SessionId::new());
        let entries = [
            entry(1, Body::Watch { child: a }),
            entry(2, Body::Watch { child: b }),
            entry(
                3,
                Body::Fired {
                    watch: 1,
                    outcome: Outcome::Interrupted,
                },
            ),
        ];
        assert_eq!(armed(&entries).collect::<Vec<_>>(), [(2, b)]);
    }

    #[test]
    fn a_child_hands_back_its_last_answer_or_outcome() {
        let said = |text: &str| Body::Assistant { text: text.into() };
        let done = Outcome::Done {
            output: serde_json::json!({ "slept": 1 }),
        };
        let ran = Body::ToolResult {
            call: 1,
            outcome: done.clone(),
        };
        assert_eq!(result(&[entry(1, said("a")), entry(2, ran)]), done);
        assert_eq!(
            result(&[entry(1, said("a")), entry(2, said("b"))]),
            Outcome::Done { output: "b".into() }
        );
        assert!(matches!(result(&[]), Outcome::Failed { .. }));
    }
}
