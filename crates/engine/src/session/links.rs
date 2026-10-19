//! Session links: a session starts a child it owns and durably watches it.
//! The link, the watch and its firing are entries in the parent's log, so a
//! watch outlives the daemon: recovery re-arms every watch that has not
//! fired, and a watch fires at most once because only the parent's actor
//! logs it, after checking the log.

use std::collections::{HashSet, VecDeque};
use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use agent::{Agent, Input};
use protocol::{Body, Entry, Environment, Outcome, SessionId};
use store::{Header, Loaded, Parent};

use super::{Msg, Session, installed};
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

/// Where a link stands, by its watch. A link not yet watched or armed
/// carries the child's input, to start it with.
enum Linked {
    Unwatched(String),
    Armed(String),
    Fired,
}

/// What a settled child hands back: its last answer or outcome.
fn result(entries: &[Entry]) -> Outcome {
    entries
        .iter()
        .rev()
        .find_map(|e| match &e.body {
            Body::Assistant { text } => Some(Outcome::Done {
                output: text.as_str().into(),
            }),
            Body::ToolResult { outcome, .. }
            | Body::ModelResult { outcome, .. }
            | Body::Fired { outcome, .. } => Some(outcome.clone()),
            Body::Interrupt => Some(Outcome::Interrupted { output: None }),
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
    pub(super) async fn spawn(&self, input: String) -> io::Result<()> {
        let child = SessionId::new();
        let call = self
            .append(Body::Link {
                child,
                input: input.clone(),
            })
            .await?;
        self.start_child(child, call, input).await?;
        self.watch(child).await
    }

    /// Commits a watch on `child`; it counts as work until it fires.
    async fn watch(&self, child: SessionId) -> io::Result<()> {
        self.append(Body::Watch { child }).await?;
        self.queued.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Starts the child off the async workers: making, seeding or loading
    /// it is store I/O.
    async fn start_child(&self, child: SessionId, call: u64, input: String) -> io::Result<()> {
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
            extensions: installed(&self.registry()),
        };
        let env = lock(&self.env).clone();
        rt::unblock(&*self.shared.rt, move || {
            daemon.start_child(header, &input, env)
        })
        .await
    }

    /// Re-arms links after a restart: a link without a watch gets one, and
    /// every child still watched is started, so it runs to its end and
    /// reports back. Children whose watch fired are told their reports are
    /// no longer needed.
    pub(super) async fn relink(&self) -> io::Result<()> {
        let links: Vec<(u64, SessionId, Linked)> = {
            let st = self.lock();
            let armed: HashSet<SessionId> = armed(&st.entries).map(|(_, child)| child).collect();
            let watched: HashSet<SessionId> = st
                .entries
                .iter()
                .filter_map(|e| match e.body {
                    Body::Watch { child } => Some(child),
                    _ => None,
                })
                .collect();
            st.entries
                .iter()
                .filter_map(|e| match &e.body {
                    Body::Link { child, input } => {
                        let linked = if !watched.contains(child) {
                            Linked::Unwatched(input.clone())
                        } else if armed.contains(child) {
                            Linked::Armed(input.clone())
                        } else {
                            Linked::Fired
                        };
                        Some((e.seq, *child, linked))
                    }
                    _ => None,
                })
                .collect()
        };
        for (call, child, linked) in links {
            match linked {
                Linked::Fired => self.heard(child),
                Linked::Armed(input) => self.start_child(child, call, input).await?,
                Linked::Unwatched(input) => {
                    self.start_child(child, call, input).await?;
                    self.watch(child).await?;
                }
            }
        }
        Ok(())
    }

    /// Queues child `child`'s result for this session's actor.
    pub(super) fn settle(&self, child: SessionId, outcome: Outcome) {
        let _ = self.mailbox.send(Msg::Settled { child, outcome });
    }

    /// Fires every armed watch on `child` and hands the agent the child's
    /// result; one that has fired already stays as it is.
    pub(super) async fn fire(
        &self,
        agent: &mut dyn Agent,
        child: SessionId,
        outcome: Outcome,
    ) -> bool {
        let watches: Vec<u64> = armed(&self.lock().entries)
            .filter_map(|(watch, watched)| (watched == child).then_some(watch))
            .collect();
        let mut ok = true;
        for watch in watches {
            let outcome = outcome.clone();
            ok &= self
                .work(Box::pin(async {
                    let fired = Body::Fired {
                        watch,
                        outcome: outcome.clone(),
                    };
                    self.record(fired);
                    let inputs = VecDeque::from([Input::ChildDone { watch, outcome }]);
                    self.drive(agent, VecDeque::new(), inputs).await
                }))
                .await;
        }
        self.heard(child);
        ok
    }

    /// Marks `child`'s reports as no longer needed: its watch has fired.
    fn heard(&self, child: SessionId) {
        if let Some(daemon) = self.shared.daemon.upgrade() {
            daemon.heard(child);
        }
    }

    /// Tells the parent, if any, that this session has settled, and with
    /// what. The parent ignores reports for watches that have fired, so
    /// reporting again is harmless; once the parent says its watch fired,
    /// reports stop, so they never load a hibernated parent for nothing.
    pub(super) async fn report(&self) {
        let (Some(parent), Some(daemon)) = (self.parent, self.shared.daemon.upgrade()) else {
            return;
        };
        let (child, outcome) = (self.id, result(&self.lock().entries));
        let reported = rt::unblock(&*self.shared.rt, move || {
            daemon.settled(parent.session, child, outcome)
        });
        if let Err(e) = reported.await {
            self.warn(format_args!("reporting to {}: {e}", parent.session.short()));
        }
    }
}

impl Daemon {
    /// The slots of the sessions `parent` owns.
    fn owned(&self, parent: SessionId) -> impl Iterator<Item = Arc<Slot>> {
        self.slots()
            .into_iter()
            .filter(move |s| s.header.parent.is_some_and(|p| p.session == parent))
    }

    /// The loaded sessions `parent` owns.
    pub(crate) fn children(&self, parent: SessionId) -> Vec<Arc<Session>> {
        self.owned(parent)
            .filter_map(|s| lock(&s.live).clone())
            .collect()
    }

    /// Unloads the sessions `parent` owns, each after its own children.
    pub(crate) fn unload_children(&self, parent: SessionId) {
        for slot in self.owned(parent) {
            let live = lock(&slot.live).take();
            if let Some(child) = live {
                child.stop();
            }
        }
    }

    /// Whether session `id` exists.
    pub(crate) fn knows(&self, id: SessionId) -> bool {
        self.slot(id).is_some()
    }

    /// Starts child session `header.id` on `input` unless it runs already:
    /// made if missing, given `input` if its log is empty, and loaded if
    /// hibernated. Calling it again is harmless, which recovery relies on.
    /// It runs commands in `env`, its parent's.
    fn start_child(&self, header: Header, input: &str, env: Option<Environment>) -> io::Result<()> {
        let mut slots = lock(&self.slots);
        let Some(slot) = slots.iter().find(|s| s.header.id == header.id).cloned() else {
            let loaded = self.shared.store.create(&header)?;
            let session = self.seed(header.clone(), loaded, input, env)?;
            slots.push(Slot::new(header, Some(session)));
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
            if let Some(env) = &env {
                loaded.append(Body::Environment(env.redacted()))?;
            }
            loaded.append(Body::User {
                text: input.to_owned(),
            })?;
        }
        Ok(Session::start_in(
            header,
            loaded,
            Arc::clone(&self.shared),
            env,
        ))
    }

    /// Hands child `child`'s result to its parent, loading the parent if it
    /// is hibernated, unless the parent has heard from it already.
    fn settled(&self, parent: SessionId, child: SessionId, outcome: Outcome) -> io::Result<()> {
        if self
            .slot(child)
            .is_some_and(|s| s.heard.load(Ordering::SeqCst))
        {
            return Ok(());
        }
        let parent = self
            .slot(parent)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such parent session"))?;
        self.with_live(&parent, |session| session.settle(child, outcome))
    }

    /// Records that `child`'s parent has fired its watch on it.
    fn heard(&self, child: SessionId) {
        if let Some(slot) = self.slot(child) {
            slot.heard.store(true, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u64, body: Body) -> Entry {
        Entry::new(seq, 0, body)
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
                    outcome: Outcome::Interrupted { output: None },
                },
            ),
        ];
        assert_eq!(armed(&entries).collect::<Vec<_>>(), [(2, b)]);
    }

    #[test]
    fn a_child_hands_back_its_last_answer_or_outcome() {
        let said = |text: &str| Body::Assistant { text: text.into() };
        let done = Outcome::Done {
            output: serde_json::json!({ "slept": 1 }).into(),
        };
        let ran = Body::ToolResult {
            call: 1,
            outcome: done.clone(),
            card: None,
        };
        assert_eq!(result(&[entry(1, said("a")), entry(2, ran)]), done);
        assert_eq!(
            result(&[entry(1, said("a")), entry(2, said("b"))]),
            Outcome::Done { output: "b".into() }
        );
        assert!(matches!(result(&[]), Outcome::Failed { .. }));
    }
}
