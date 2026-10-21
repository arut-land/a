//! Ordered writes and durable publication belong to the same session writer.

use super::{
    Actor, Admission, Arc, AssertUnwindSafe, Encoded, Entry, Flag, Future, Image, Json, Msg,
    Ordering, Poll, Record, Requirement, SYNC_LAG, Snapshot, WorkId, Writer, catch_unwind, channel,
    io, lock, oneshot, relisted, store_fault, unless,
};

pub(super) struct Journal {
    /// `None` only while a commit has it on a blocking thread.
    pub(super) writer: Option<Writer>,
    pub(super) fold_snapshot: Option<Snapshot>,
    /// Entries written and not yet durable, in order, with whoever waits
    /// for each: shown to no one until a sync. A seq a client has seen is
    /// one that survives a power cut, so it is never given out twice.
    pub(super) unsynced: Vec<(Vec<Entry>, Encoded)>,
    pub(super) waiting_sync: Vec<(u64, oneshot::Sender<()>)>,
    pub(super) waiting_mutations: Vec<(
        protocol::Seq,
        ext::MutationResult,
        oneshot::Sender<ext::MutationResult>,
    )>,
    pub(super) ordered_seq: u64,
    pub(super) durable_seq: u64,
    pub(super) flushes: Vec<PendingFlush>,
    pub(super) sync_armed: bool,
    /// The bytes of log the next load would read past the newest
    /// snapshot: once [`SNAPSHOT_EVERY`], the next rest is snapshotted.
    pub(super) since_snapshot: u64,
}

pub(super) struct PendingFlush {
    pub(super) through: u64,
    pub(super) ended: Flag,
    pub(super) result: Option<io::Result<()>>,
}

impl Journal {
    pub(super) fn is_durable(&self) -> bool {
        self.durable_seq >= self.ordered_seq && self.flushes.is_empty()
    }
}

impl Actor {
    pub(super) fn pending_entries(&self) -> impl Iterator<Item = Entry> + '_ {
        self.journal
            .unsynced
            .iter()
            .flat_map(|(entries, _)| entries.iter().cloned())
    }

    pub(super) async fn drain_flushes(&mut self, mail: &channel::Receiver<Msg>) {
        let _ = self.sync().await;
        while !self.journal.flushes.is_empty() {
            let Some(msg) = mail.recv().await else { break };
            if let Msg::Flushed {
                fence,
                through,
                result,
            } = msg
                && fence == self.live.session.fence.load(Ordering::SeqCst)
            {
                self.completed(through, result).await;
            }
        }
    }

    pub(super) fn covering(&self) -> u64 {
        self.journal
            .flushes
            .iter()
            .map(|pending| pending.through)
            .max()
            .unwrap_or(self.journal.durable_seq)
            .max(self.journal.durable_seq)
    }

    pub(super) async fn durable(&mut self, through: u64) {
        self.journal.durable_seq = self.journal.durable_seq.max(through);
        self.show_synced();
        self.publish();
        self.admit().await;
    }

    pub(super) async fn track(
        &mut self,
        through: u64,
        mut epoch: rt::FlushEpoch,
    ) -> io::Result<()> {
        let polled = {
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            std::pin::Pin::new(&mut epoch).poll(&mut cx)
        };
        if let Poll::Ready(result) = polled {
            result.inspect_err(|e| store_fault(self.live.id, ext_logging::Operation::Commit, e))?;
            if self.journal.flushes.is_empty() {
                self.durable(through).await;
            } else {
                let ended = Flag::default();
                ended.set();
                self.journal.flushes.push(PendingFlush {
                    through,
                    ended,
                    result: Some(Ok(())),
                });
            }
            return Ok(());
        }
        let ended = Flag::default();
        self.journal.flushes.push(PendingFlush {
            through,
            ended: ended.clone(),
            result: None,
        });
        let session = Arc::clone(&self.live.session);
        let fence = session.fence.load(Ordering::SeqCst);
        self.live.session.shared.rt.spawn(Box::pin(async move {
            let result = epoch.await;
            ended.set();
            let _ = session.mail.send(Msg::Flushed {
                fence,
                through,
                result,
            });
        }));
        Ok(())
    }

    pub(super) async fn completed(&mut self, through: u64, result: io::Result<()>) {
        let Some(pending) = self
            .journal
            .flushes
            .iter_mut()
            .find(|pending| pending.through == through)
        else {
            return;
        };
        pending.result = Some(result);
        // Notification tasks may wake out of order. An earlier failure
        // must fence recovery before a later success can publish anything.
        while self
            .journal
            .flushes
            .first()
            .is_some_and(|pending| pending.result.is_some())
        {
            let pending = self.journal.flushes.remove(0);
            match pending.result.expect("completed epoch") {
                Ok(()) => self.durable(pending.through).await,
                Err(error) => {
                    store_fault(self.live.id, ext_logging::Operation::Commit, &error);
                    self.failed(format!("flushing: {error}"), Vec::new()).await;
                    break;
                }
            }
        }
    }

    /// Registers the epoch after the write barriers, in the appending
    /// job. Ordered completion returns before that thread runs its flush,
    /// so replay-safe work can overlap the syscall.
    pub(super) async fn commit(
        &mut self,
        batch: &mut Vec<Entry>,
        durable: bool,
        flush: bool,
        listing: Record,
    ) -> io::Result<()> {
        let mut writer = self
            .journal
            .writer
            .take()
            .ok_or_else(|| io::Error::other("no writer"))?;
        let mut entries = std::mem::take(batch);
        let listed = std::mem::replace(&mut self.listed, listing.clone());
        let listing = relisted(&listed, &listing).then_some(listing);
        let shared = Arc::clone(&self.live.session.shared);
        let rt = Arc::clone(&shared.rt);
        let (writer, entries, written, listing, epoch, durable_on_order) =
            rt::unblock_flush(&*rt, move || {
                let first = listing.as_ref().is_some_and(|l| l.busy);
                if first && let Some(listing) = &listing {
                    shared.catalog.record(listing.clone());
                }
                let written =
                    catch_unwind(AssertUnwindSafe(|| writer.append(&mut entries, durable)))
                        .unwrap_or_else(|_| Err(io::Error::other("committing entries panicked")));
                if !first
                    && written.is_ok()
                    && let Some(listing) = &listing
                {
                    shared.catalog.record(listing.clone());
                }
                let durable_on_order = durable && writer.durable_on_order();
                let request = (written.is_ok() && flush && !durable_on_order)
                    .then(|| writer.enroll())
                    .flatten();
                let (epoch, leader) = match request {
                    Some(request) => (Some(request.epoch), request.leader),
                    None => (None, None),
                };
                (
                    (writer, entries, written, listing, epoch, durable_on_order),
                    leader,
                )
            })
            .await;
        // Not cleared after entries that did not commit.
        if written.is_err() && listing.is_some_and(|l| !l.busy) {
            self.listed = listed;
        }
        self.journal.writer = Some(writer);
        let lines = written
            .inspect_err(|e| store_fault(self.live.id, ext_logging::Operation::Commit, e))?;
        crate::session_event("commit", self.live.id, || {
            let how = if durable { "synced" } else { "written" };
            format!("{} entries {how}", entries.len())
        });
        self.journal.ordered_seq = entries
            .last()
            .map_or(self.journal.ordered_seq, |entry| entry.seq.get());
        self.journal.since_snapshot += lines.lines().map(|line| line.len() as u64 + 1).sum::<u64>();
        self.folded(&entries);
        self.journal.unsynced.push((entries, lines));
        if let Some(epoch) = epoch {
            self.track(self.journal.ordered_seq, epoch).await?;
        } else if durable_on_order || flush {
            self.durable(self.journal.ordered_seq).await;
        }
        if !self.journal.unsynced.is_empty() {
            self.arm_sync();
        }

        Ok(())
    }

    /// Keeps a snapshot of the session beside its log, if it is at rest
    /// and all it logged is durable: what the next load starts from. A
    /// child's is not kept: what it settled with is the daemon's to read
    /// from its log, not the ledger's to carry. One that cannot be taken
    /// is reported and not tried again until as much log follows.
    pub(super) async fn keep_snapshot(&mut self) {
        let covers = self.core.ledger().seq();
        let rest = self.core.at_rest()
            && self.journal.unsynced.is_empty()
            && covers == self.live.frames.seq();
        if !rest || self.live.parent.is_some() || self.journal.writer.is_none() {
            return;
        }
        self.journal.since_snapshot = 0;
        let states = match lock(&self.live.session.state_view()).image() {
            Ok(states) => states,
            Err(e) => {
                ext_logging::emit(ext_logging::Fact::Fault {
                    session: Some(self.live.id),
                    operation: ext_logging::Operation::Snapshot,
                    kind: ext_logging::Fault(io::ErrorKind::InvalidData),
                });
                eprintln!("session {}: snapshot: {e}", self.live.id.short());
                return;
            }
        };
        let image = Image {
            build: self.live.session.shared.build.clone(),
            extensions: self.registry.installed(),
            has_input: self.live.has_input(),
            ledger: self.core.ledger().clone(),
            states,
        };
        let Some(mut writer) = self.journal.writer.take() else {
            return;
        };
        let rt = Arc::clone(&self.live.session.shared.rt);
        let (writer, taken) = rt::unblock(&*rt, move || {
            let taken = Json::encode(&image)
                .map_err(io::Error::other)
                .and_then(|image| {
                    let snapshot = Snapshot { covers, image };
                    writer.snapshot(&snapshot)?;
                    Ok(snapshot)
                });
            (writer, taken)
        })
        .await;
        self.journal.writer = Some(writer);
        match taken {
            Ok(snapshot) => {
                self.journal.fold_snapshot = Some(snapshot);
                crate::session_event("snapshot", self.live.id, || format!("at {covers}"));
            }
            Err(e) => {
                store_fault(self.live.id, ext_logging::Operation::Snapshot, &e);
                eprintln!("session {}: snapshot: {e}", self.live.id.short());
            }
        }
    }

    /// Asks for a sync [`SYNC_LAG`] from now, unless one is on its way.
    pub(super) fn arm_sync(&mut self) {
        if std::mem::replace(&mut self.journal.sync_armed, true) {
            return;
        }
        let shared = &self.live.session.shared;
        let (mail, lag) = (
            Arc::clone(&self.live.session.mail),
            shared.rt.sleep(SYNC_LAG),
        );
        self.live.session.spawn_work(
            WorkId::Sync,
            Admission {
                through: self.journal.ordered_seq,
                requirement: Requirement::Ordered,
            },
            |cancel| {
                Box::pin(async move {
                    if unless(&cancel, lag).await.is_some() {
                        let _ = mail.send(Msg::Sync);
                    }
                })
            },
        );
    }

    /// Makes what was only written durable, and shows it. A sync that
    /// fails leaves the session as a failed commit does.
    pub(super) async fn sync(&mut self) -> Result<(), ()> {
        self.journal.sync_armed = false;
        if self.journal.ordered_seq <= self.covering() {
            return Ok(());
        }
        if let Err(error) = self.order_flush().await {
            self.journal.unsynced.clear();
            self.journal.waiting_sync.clear();
            self.journal.waiting_mutations.clear();
            Box::pin(self.failed(format!("syncing: {error}"), Vec::new())).await;
            return Err(());
        }
        Ok(())
    }

    pub(super) async fn order_flush(&mut self) -> io::Result<()> {
        let Some(mut writer) = self.journal.writer.take() else {
            return Err(io::Error::other("no writer"));
        };
        let rt = Arc::clone(&self.live.session.shared.rt);
        let (writer, result, epoch) = rt::unblock_flush(&*rt, move || {
            let result = writer.sync();
            let request = result.is_ok().then(|| writer.enroll()).flatten();
            let (epoch, leader) = match request {
                Some(request) => (Some(request.epoch), request.leader),
                None => (None, None),
            };
            ((writer, result, epoch), leader)
        })
        .await;
        self.journal.writer = Some(writer);
        result.inspect_err(|e| store_fault(self.live.id, ext_logging::Operation::Commit, e))?;
        if let Some(epoch) = epoch {
            self.track(self.journal.ordered_seq, epoch).await?;
        } else {
            self.durable(self.journal.ordered_seq).await;
        }
        Ok(())
    }

    pub(super) fn show_synced(&mut self) {
        let mut pending = Vec::new();
        for (entries, lines) in std::mem::take(&mut self.journal.unsynced) {
            if entries
                .last()
                .is_some_and(|entry| entry.seq.get() <= self.journal.durable_seq)
            {
                self.show(&entries, &lines);
            } else {
                pending.push((entries, lines));
            }
        }
        self.journal.unsynced = pending;
        self.journal
            .waiting_sync
            .extract_if(.., |(through, _)| *through <= self.journal.durable_seq)
            .for_each(|(_, done)| done.send(()));
        self.journal
            .waiting_mutations
            .extract_if(.., |(seq, _, _)| seq.get() <= self.journal.durable_seq)
            .for_each(|(_, result, done)| done.send(result));
    }
}
