//! Upgrades that keep work running. Once a newer build is staged and its
//! first client comes, the daemon makes way for it as soon as it can: at
//! once when nothing runs, and otherwise once what runs runs only in the
//! background (calls gone on there, and the process groups sessions own),
//! by handing that work, live, to the newer build. That needs every piece of
//! it to go on there ([`ext::Resumable`]) and the newer build to declare the
//! handover format this one speaks ([`sys::handover::VERSION`]). Until then
//! the upgrade is pending: this build goes on serving, every client of the
//! newer one is told what holds it, and each pulse tries again. Helpers a
//! session starts again when needed ([`ext::Role::Helper`]) hold nothing.
//!
//! A handover is one commit across every session. Each session freezes
//! first ([`crate::driver::Live::freeze`]): its groups' supervision stops
//! between two chunks of output, and the session takes nothing more up. A
//! session that refuses (at work, or running what no other build takes up)
//! or a group that ends meanwhile thaws them all, and the next pulse tries
//! again. Only once every session froze does each give its groups up for
//! good.
//!
//! On Unix the daemon then execs the newer build in its own process,
//! keeping its pid, its listener (connections queue meanwhile) and its
//! children; on Windows it starts the newer build inheriting the groups'
//! handles and exits once it serves. Either way the newer build claims
//! every group named in the record as it reads it, adopts each through the
//! tool that started it, and kills what it cannot adopt. A call's outcome
//! and a job's end reach the session as they would have. Nothing of this
//! is logged in a session: what it waits on is the same.

use std::fs::File;

use ext_logging::{Fact, Fault, Operation, Stop, emit as fact};
use protocol::{CallId, Json, SessionId};
use serde::{Deserialize, Serialize};

use crate::{Daemon, Exit, lock};

/// Whose a handed group is: what the record says of it beside where it is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub session: SessionId,
    pub call: CallId,
    /// The tool whose call started it, which takes it up again.
    pub tool: String,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// The call's own group, the call gone on in the background: it goes
    /// on from `state` ([`ext::Resumable::State`]).
    Call { state: Json },
    /// A group the call left to the session.
    Job,
}

/// A group the previous build handed over, claimed, and whose it is.
pub struct Handed {
    pub claimed: sys::handover::Claimed,
    pub owner: Owner,
}

/// What the previous build handed over, read: each group with an owner
/// this build can read. One that could not be claimed, or whose owner this
/// build cannot read, is logged and goes on with no one: killed as its
/// claim drops, or, unclaimed, left as a crash leaves it.
pub fn handed(
    record: Vec<(std::io::Result<sys::handover::Claimed>, serde_json::Value)>,
) -> Vec<Handed> {
    (record.into_iter())
        .filter_map(|(claimed, owner)| {
            let owner = serde_json::from_value::<Owner>(owner);
            let session = owner.as_ref().ok().map(|owner| owner.session);
            match (claimed, owner) {
                (Ok(claimed), Ok(owner)) => Some(Handed { claimed, owner }),
                (Err(e), _) => {
                    fault_in(session, &e);
                    None
                }
                (Ok(_), Err(e)) => {
                    fault_in(None, &e.into());
                    None
                }
            }
        })
        .collect()
}

impl Daemon {
    /// The build `runtime/current` names, if it can be handed this one's
    /// work: it is another build, it is there, and it speaks this one's
    /// handover.
    pub(crate) fn successor(&self, runtime: &sys::Runtime) -> Option<std::path::PathBuf> {
        let current = runtime.current()?;
        let exe = runtime.current_exe();
        (current != self.shared.build
            && exe.is_file()
            && runtime.handover(&current) == Some(sys::handover::VERSION))
        .then_some(exe)
    }

    /// What holds an upgrade now, if anything: nothing when no work runs,
    /// or the newer build can be handed all of it.
    pub(crate) async fn upgrade_held(&self, runtime: &sys::Runtime) -> Option<String> {
        if !self.at_work() {
            return None;
        }
        if self.successor(runtime).is_none() {
            return Some("work runs that the newer build cannot take over".to_owned());
        }
        for live in self.loaded() {
            if let Err(why) = live.handable().await {
                return Some(why);
            }
        }
        None
    }

    /// Whether any session works, or owns groups of the user's work.
    pub(crate) fn at_work(&self) -> bool {
        self.has_work() || self.loaded().iter().any(|s| s.has_jobs())
    }

    /// Freezes every loaded session, then commits them all: the groups
    /// given up, each with whose it is. Any session's refusal thaws those
    /// frozen, and nothing is given up.
    pub(crate) async fn hand_over(
        &self,
        life: &crate::upgrade::Lifecycle,
    ) -> Result<Vec<(sys::handover::Given, Owner)>, String> {
        // Asked first, so a session that would refuse stops nothing of
        // the others' to start again.
        for live in &self.loaded() {
            live.handable().await?;
        }
        // No session loads, wakes or is made while the others freeze: the
        // commit covers every session there is.
        *lock(&self.admission) = false;
        let loaded = self.loaded();
        for (at, live) in loaded.iter().enumerate() {
            if let Err(why) = live.freeze().await {
                for frozen in &loaded[..at] {
                    frozen.thaw();
                }
                *lock(&self.admission) = true;
                return Err(why);
            }
        }
        // Requests that came meanwhile are refused as each session
        // commits, to be sent again to the newer build.
        life.drain();
        let mut given = Vec::new();
        for live in &loaded {
            given.extend(live.commit().await);
        }
        Ok(given)
    }
}

impl Exit {
    /// Stops serving as [`Exit::now`] does, but for the groups `given` and,
    /// on Unix, the endpoint, then becomes `successor`. Should the newer
    /// build not start, this one's own image is started in its place with
    /// the same record; only should that fail too does the daemon exit, the
    /// groups left as a crash leaves them.
    pub(crate) async fn hand_over(
        self,
        daemon: &Daemon,
        given: Vec<(sys::handover::Given, Owner)>,
        successor: std::path::PathBuf,
        command: crate::Successor,
    ) -> ! {
        let owned: Vec<_> = (daemon.loaded().iter())
            .flat_map(|live| live.jobs())
            .collect();
        daemon.shutdown().await;
        // What was not handed over (a helper) is stopped with the session;
        // it is reaped before the exec, or it would outlive this build as
        // a child no build knows.
        let reaped = async {
            for lease in &owned {
                lease.ended().await;
            }
        };
        rt::or(reaped, daemon.shared.rt.sleep(crate::STOP_GRACE)).await;
        if let Err(e) = sys::crash::exited(&self.home, &self.pid) {
            fault(&e);
        }
        fact(Fact::HandedOver {
            groups: given.len(),
        });
        fact(Fact::Stopped {
            reason: Stop::Upgrade,
        });
        ext_logging::flush(&*daemon.shared.rt).await;
        let owners: Vec<serde_json::Value> = (given.iter())
            .map(|(_, owner)| serde_json::to_value(owner).unwrap_or_default())
            .collect();
        let record =
            sys::handover::encode(given.iter().map(|(given, _)| &given.group).zip(&owners));
        let handles: Vec<File> = (given.into_iter())
            .flat_map(|(given, _)| given.handles)
            .collect();
        let mut handover = sys::handover::Handover::new(
            &self.home,
            (self.listener, self.endpoint),
            self.pid,
            handles,
            record,
        );
        for exe in [Ok(successor), std::env::current_exe()]
            .into_iter()
            .flatten()
        {
            fault(&handover.replace(command(&exe)));
        }
        if let Err(e) = handover.abandon() {
            fault(&e);
        }
        std::process::exit(0)
    }
}

fn fault(e: &std::io::Error) {
    fault_in(None, e);
}

fn fault_in(session: Option<SessionId>, e: &std::io::Error) {
    fact(Fact::Fault {
        session,
        operation: Operation::Handover,
        kind: Fault(e.kind()),
    });
}

/// What the record hands session `id`, taken out of it.
pub(crate) fn take(handed: &std::sync::Mutex<Vec<Handed>>, id: SessionId) -> Vec<Handed> {
    lock(handed)
        .extract_if(.., |handed| handed.owner.session == id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The record of each handover format, as this build encodes it: one
    /// that encodes otherwise without [`sys::handover::VERSION`] bumped
    /// fails here, so no build reads a record it does not know as one it
    /// does. `UPDATE_HANDOVER=1` writes the file of a new version, never an
    /// existing one.
    #[test]
    fn each_handover_format_encodes_as_it_did() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/handover")
            .join(format!("v{}.json", sys::handover::VERSION));
        let group: sys::handover::Group = serde_json::from_value(serde_json::json!({
            "pid": 4242,
            "output": 7,
            "job": 8,
            "process": 9,
        }))
        .unwrap();
        let owners = [
            Owner {
                session: SessionId::NIL,
                call: CallId::new(3).unwrap(),
                tool: "bash".into(),
                kind: Kind::Call {
                    state: Json::encode(&serde_json::json!({"dir": "/w", "started": 1})).unwrap(),
                },
            },
            Owner {
                session: SessionId::NIL,
                call: CallId::new(5).unwrap(),
                tool: "bash".into(),
                kind: Kind::Job,
            },
        ]
        .map(|owner| serde_json::to_value(owner).unwrap());
        let encoded = sys::handover::encode(owners.iter().map(|owner| (&group, owner)));
        let mut encoded: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        // Windows hands a job and a leader besides, by handle numbers
        // only Windows has.
        for entry in encoded["groups"].as_array_mut().unwrap() {
            let group = entry["group"].as_object_mut().unwrap();
            group.retain(|key, _| key == "pid" || key == "output");
        }
        let encoded = serde_json::to_string_pretty(&encoded).unwrap() + "\n";
        if std::env::var_os("UPDATE_HANDOVER").is_some() && !path.exists() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &encoded).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap_or_default(),
            encoded,
            "the handover encodes otherwise: bump sys::handover::VERSION"
        );
    }
}
