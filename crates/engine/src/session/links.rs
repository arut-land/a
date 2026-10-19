//! Session links: a call starts a child session it owns and durably
//! watches it. The link, what the call waits on, the watch and its firing
//! are entries in the parent's log, so a watch outlives the daemon: the
//! call, run again by recovery, finds its child by what it waits on, and a
//! watch fires at most once because only the parent's actor logs it, after
//! checking the log. Watches an earlier build armed for its agent are
//! re-armed by recovery, and their firing goes to the agent.

use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::io;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;

use ext::{Agent, Input};
use protocol::ledger::{Ledger, Phase};
use protocol::{Body, CallId, Environment, Outcome, Seq, SessionId, WaitOn};
use store::{Header, Parent};

use super::{Msg, Open, Session, installed, unless};
use crate::{Daemon, Slot, lock};

/// How long an interrupted call waits for its child, interrupted first, to
/// settle: it hears the child's own account of where it stopped.
const SETTLING: Duration = ext::Kill::GRACE.saturating_mul(2);

/// The child call `call` waits on, by its link: the child and its input.
fn waited_on(ledger: &Ledger, call: u64) -> Option<(u64, SessionId, String)> {
    let Phase::Waiting(WaitOn::Child { link }) = &ledger.call(call)?.phase else {
        return None;
    };
    let link = ledger.links().iter().find(|l| l.seq == link.get())?;
    Some((link.seq, link.child, link.input.clone()))
}

/// Whether a call waits on `child`: its result goes to the call, not to the
/// agent.
fn owned(ledger: &Ledger, child: SessionId) -> bool {
    ledger
        .open_calls()
        .any(|(call, _)| waited_on(ledger, call).is_some_and(|(_, c, _)| c == child))
}

/// Where a link stands, by its watch. A link not yet watched or armed
/// carries the child's input, to start it with.
enum Linked {
    Unwatched(String),
    Armed(String),
    Fired,
}

/// What a settled child hands back: its last answer or outcome.
fn result(ledger: &Ledger) -> Outcome {
    ledger.result().cloned().unwrap_or_else(|| Outcome::Failed {
        error: "the child settled without a result".to_owned(),
    })
}

impl Session {
    /// What a child session started on `input` for call `call` settles
    /// with. Its link, the call's wait on it and the watch commit before
    /// it starts, so a call run again after a crash finds the child it
    /// started and waits for it again. The watch counts as work until it
    /// fires. An interrupt reaches the child first: the call waits a while
    /// for it to settle.
    pub(super) async fn child(&self, call: u64, input: String) -> io::Result<Outcome> {
        let turn = self.turn();
        let linked = waited_on(&self.lock().ledger, call);
        let (link, child, input) = if let Some(linked) = linked {
            linked
        } else {
            self.link(call, input).await?
        };
        let watch = (self.lock().ledger.armed()).find_map(|(w, c)| (c == child).then_some(w));
        let Some(watch) = watch else {
            return Err(io::Error::other("the child's watch has fired"));
        };
        self.start_child(child, link, input).await?;
        let reported = || lock(&self.reported).contains_key(&child);
        if unless(&turn, self.reports.until(reported)).await.is_none() {
            let settled = self.reports.until(reported);
            let late = self.shared.rt.sleep(SETTLING);
            let (mut settled, mut late) = (pin!(settled), pin!(late));
            poll_fn(|cx| {
                if settled.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(());
                }
                late.as_mut().poll(cx)
            })
            .await;
        }
        let outcome = lock(&self.reported).remove(&child);
        let outcome = outcome.unwrap_or(Outcome::Interrupted { output: None });
        self.record(Body::Fired {
            watch,
            outcome: outcome.clone(),
        });
        self.queued.fetch_sub(1, Ordering::SeqCst);
        self.heard(child);
        Ok(outcome)
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
            let ledger = &st.ledger;
            ledger
                .links()
                .iter()
                .map(|link| {
                    let input = link.input.clone();
                    let linked = if !ledger.watched(link.child) {
                        Linked::Unwatched(input)
                    } else if ledger.armed().any(|(_, child)| child == link.child) {
                        Linked::Armed(input)
                    } else {
                        Linked::Fired
                    };
                    (link.seq, link.child, linked)
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

    /// Links a new child on `input` for call `call`, which waits on it,
    /// and watches it: its link, the child and its input.
    async fn link(&self, call: u64, input: String) -> io::Result<(u64, SessionId, String)> {
        let child = SessionId::new();
        let link = self.record(Body::Link {
            child,
            input: input.clone(),
        });
        let (Some(call), Some(on)) = (CallId::new(call), Seq::new(link)) else {
            return Err(io::Error::other("a call has a seq"));
        };
        let on = WaitOn::Child { link: on };
        self.record(Body::Wait { call, on });
        self.watch(child).await?;
        Ok((link, child, input))
    }

    /// Hands child `child`'s result to the call waiting on it, or else
    /// queues it for this session's actor.
    pub(super) fn settle(&self, child: SessionId, outcome: Outcome) {
        if owned(&self.lock().ledger, child) {
            lock(&self.reported).insert(child, outcome);
            self.reports.notify();
            return;
        }
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
        let watches: Vec<u64> = (self.lock().ledger.armed())
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
        let (child, outcome) = (self.id, result(&self.lock().ledger));
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
            .filter_map(|s| lock(&s.residence).live().cloned())
            .collect()
    }

    /// Unloads the sessions `parent` owns, each after its own children.
    pub(crate) fn unload_children(&self, parent: SessionId) {
        for slot in self.owned(parent) {
            lock(&slot.residence).unload();
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
            let session = self.seed(header.clone(), Open::Create, input, env)?;
            slots.push(Slot::new(header, Some(session)));
            return Ok(());
        };
        drop(slots);
        let start = || self.seed(slot.header.clone(), Open::Load, input, env);
        slot.live_or(start, |_| ())
    }

    /// Starts a child, first committing `env` (redacted) and `input` as its
    /// opening entries if its log is empty. Nothing else writes a log that
    /// has no actor yet.
    fn seed(
        &self,
        header: Header,
        open: Open,
        input: &str,
        env: Option<Environment>,
    ) -> io::Result<Arc<Session>> {
        let opening = env
            .as_ref()
            .map(|env| Body::Environment(env.redacted()))
            .into_iter()
            .chain([Body::User {
                text: input.to_owned(),
            }])
            .collect();
        Session::start_in(header, Arc::clone(&self.shared), open, env, opening)
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
    use protocol::Entry;

    use super::*;

    fn entry(seq: u64, body: Body) -> Entry {
        Entry::new(seq, 0, body)
    }

    #[test]
    fn a_child_hands_back_its_last_answer_or_outcome() {
        let said = |text: &str| Body::Assistant { text: text.into() };
        let done = Outcome::Done {
            output: serde_json::json!({ "slept": 1 }).into(),
        };
        let call = Body::ToolCall {
            tool: "sleep".into(),
            input: protocol::Json::null(),
            id: None,
            card: None,
            by: protocol::Audience::Model,
        };
        let ran = Body::ToolResult {
            call: 2,
            outcome: done.clone(),
            card: None,
        };
        let of = |entries: &[Entry]| result(&Ledger::of(entries));
        assert_eq!(
            of(&[entry(1, said("a")), entry(2, call), entry(3, ran)]),
            done
        );
        assert_eq!(
            of(&[entry(1, said("a")), entry(2, said("b"))]),
            Outcome::Done { output: "b".into() }
        );
        assert!(matches!(of(&[]), Outcome::Failed { .. }));
    }
}
