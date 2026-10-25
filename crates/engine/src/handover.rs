//! Upgrades that keep work running. A drain that has nothing left but
//! calls gone on in the background and the process groups sessions own
//! hands those groups, live, to the build staged as `runtime/current`
//! rather than waiting for them to end, when every one of them can go on
//! there ([`ext::Resumable`]) and that build declares the handover format
//! this one speaks ([`sys::handover::VERSION`]). Otherwise the drain waits
//! for them all, however long they run.
//!
//! On Unix the daemon execs the next build in its own process, keeping
//! its pid, its listener (connections queue meanwhile) and its children;
//! on Windows it starts the next build inheriting the groups' handles and
//! exits once it serves. Either way the next build reads the [`Record`],
//! adopts each group through the tool that started it, and goes on: a
//! call's outcome and a job's end reach the session as they would have.
//! Nothing of this is logged in a session: what it waits on is the same.

use std::fs::File;

use ext_logging::{Fact, Fault, Operation, Stop, emit as fact};
use protocol::{CallId, Json, SessionId};
use serde::{Deserialize, Serialize};

use crate::{Daemon, Exit, lock};

/// What a daemon hands the next build: every group, with the session and
/// call that own it.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Record {
    pub groups: Vec<Group>,
}

impl Record {
    /// The record `bytes` holds. One this build cannot read hands it
    /// nothing: its groups are left as a crash leaves them, and their calls
    /// recover as cut off.
    pub fn read(bytes: &[u8]) -> Self {
        serde_json::from_slice(bytes).unwrap_or_else(|e| {
            fault(&e.into());
            Self::default()
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Group {
    pub session: SessionId,
    pub call: CallId,
    /// The tool whose call started it, which takes it up again.
    pub tool: String,
    /// How its process provider finds it ([`ext::Handed::state`]).
    pub handed: Json,
    pub kind: Kind,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Kind {
    /// The call's own group, the call gone on in the background: it goes
    /// on from `state` ([`ext::Resumable::State`]).
    Call { state: Json },
    /// A group the call left to the session.
    Job,
}

impl Daemon {
    /// The build `runtime/current` names, if it can be handed this one's
    /// work: it is another build, it is there, and it speaks this one's
    /// handover.
    pub(crate) fn successor(&self, home: &sys::Home) -> Option<std::path::PathBuf> {
        let runtime = home.runtime();
        let current = runtime.current()?;
        let exe = runtime.current_exe();
        (current != self.shared.build
            && exe.is_file()
            && runtime.handover(&current) == Some(sys::handover::VERSION))
        .then_some(exe)
    }

    /// Hands every loaded session's groups over, once each says it can:
    /// one that is at work, or runs a group no tool takes up again, keeps
    /// the drain waiting instead. A group that ends meanwhile is left to
    /// end here.
    pub(crate) async fn hand_over(&self) -> Result<(Record, Vec<File>), String> {
        let loaded = self.loaded();
        for live in &loaded {
            live.handable().await?;
        }
        let mut record = Record::default();
        let mut handles = Vec::new();
        for live in &loaded {
            for (group, held) in live.hand_over().await {
                record.groups.push(group);
                handles.extend(held);
            }
        }
        Ok((record, handles))
    }
}

impl Exit {
    /// Stops serving as [`Exit::now`] does, but for the groups `record`
    /// hands over and, on Unix, the endpoint, then becomes `successor`:
    /// execs it on Unix, starts it on Windows. Should the next build not
    /// start, this one's own image is started in its place with the same
    /// record; only should that fail too does the daemon exit, the groups
    /// left as a crash leaves them.
    pub(crate) async fn hand_over(
        self,
        daemon: &Daemon,
        record: Record,
        handles: Vec<File>,
        successor: std::path::PathBuf,
        command: crate::Successor,
    ) -> ! {
        let groups = record.groups.len();
        daemon.shutdown().await;
        if let Err(e) = sys::crash::exited(&self.home, &self.pid) {
            fault(&e);
        }
        fact(Fact::HandedOver { groups });
        fact(Fact::Stopped {
            reason: Stop::Upgrade,
        });
        ext_logging::flush(&*daemon.shared.rt).await;
        let endpoint = self.endpoint.for_successor().unwrap_or_else(|e| {
            fault(&e);
            None
        });
        let bytes = serde_json::to_vec(&record).unwrap_or_default();
        match sys::handover::record(&self.home, &bytes) {
            Ok(record) => {
                for exe in [Ok(successor), std::env::current_exe()]
                    .into_iter()
                    .flatten()
                {
                    let error = sys::handover::Successor::new(command(&exe))
                        .endpoint(self.listener, &self.pid)
                        .record(&record)
                        .handles(&handles)
                        .replace(&self.home);
                    fault(&error);
                }
            }
            Err(e) => fault(&e),
        }
        if let Some(Err(e)) = endpoint.map(sys::EndpointRetirement::retire) {
            fault(&e);
        }
        drop(self.pid);
        std::process::exit(0)
    }
}

fn fault(e: &std::io::Error) {
    fact(Fact::Fault {
        session: None,
        operation: Operation::Handover,
        kind: Fault(e.kind()),
    });
}

/// The groups of session `id` the record holds, taken out of it.
pub(crate) fn take(record: &std::sync::Mutex<Record>, id: SessionId) -> Vec<Group> {
    let mut record = lock(record);
    let (taken, kept) = std::mem::take(&mut record.groups)
        .into_iter()
        .partition(|group| group.session == id);
    record.groups = kept;
    taken
}
