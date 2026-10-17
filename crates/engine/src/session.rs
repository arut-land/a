use std::collections::{HashSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use agent::{Action, Agent, Cx, Echo, Input, Replay, Toolset};
use protocol::{Body, DaemonMsg, Entry, Outcome, SessionId, SessionSummary};
use store::Log;

pub struct Session {
    pub id: SessionId,
    pub workspace: PathBuf,
    state: Mutex<State>,
    inbox: Sender<String>,
}

struct State {
    entries: Vec<Entry>,
    log: Log,
    clients: Vec<Sender<DaemonMsg>>,
}

impl Session {
    /// Starts the session's actor: the single writer that steps the agent and
    /// runs its tools. Calls left dangling by a crash are recovered first.
    pub fn start(
        id: SessionId,
        workspace: PathBuf,
        entries: Vec<Entry>,
        log: Log,
        tools: Arc<Toolset>,
    ) -> Arc<Self> {
        let (inbox, rx) = mpsc::channel();
        let state = Mutex::new(State {
            entries,
            log,
            clients: Vec::new(),
        });
        let session = Arc::new(Self {
            id,
            workspace,
            state,
            inbox,
        });
        let actor = session.clone();
        thread::spawn(move || actor.run(&tools, &rx));
        session
    }

    pub fn submit(&self, text: String) {
        let _ = self.inbox.send(text);
    }

    pub fn attach(&self, client: &Sender<DaemonMsg>, since: u64) {
        let mut st = self.lock();
        let _ = client.send(DaemonMsg::Attached {
            session: self.id,
            workspace: self.workspace.clone(),
        });
        for e in st.entries.iter().filter(|e| e.seq > since) {
            let _ = client.send(DaemonMsg::Entry(e.clone()));
        }
        let seq = st.entries.last().map_or(0, |e| e.seq);
        let _ = client.send(DaemonMsg::Synced { seq });
        st.clients.push(client.clone());
    }

    pub fn summary(&self) -> SessionSummary {
        let st = self.lock();
        SessionSummary {
            id: self.id,
            workspace: self.workspace.clone(),
            entries: st.entries.len() as u64,
            clients: u32::try_from(st.clients.len()).unwrap_or(u32::MAX),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Commits an entry; only committed entries are ever shown to clients.
    fn append(&self, body: Body) -> io::Result<u64> {
        let mut st = self.lock();
        let seq = st.entries.len() as u64 + 1;
        let entry = Entry {
            seq,
            epoch: 0,
            body,
        };
        st.log.append(&entry)?;
        st.clients
            .retain(|c| c.send(DaemonMsg::Entry(entry.clone())).is_ok());
        st.entries.push(entry);
        Ok(seq)
    }

    fn run(&self, tools: &Toolset, inbox: &Receiver<String>) {
        let mut agent = Echo;
        if let Err(e) = self.recover(&mut agent, tools) {
            eprintln!("session {}: recovery: {e}", self.id.short());
        }
        for text in inbox {
            let result = self
                .append(Body::User { text: text.clone() })
                .and_then(|_| self.drive(&mut agent, tools, Input::User { text }));
            if let Err(e) = result {
                eprintln!("session {}: {e}", self.id.short());
            }
        }
    }

    fn drive(&self, agent: &mut impl Agent, tools: &Toolset, input: Input) -> io::Result<()> {
        let mut inputs = VecDeque::from([input]);
        while let Some(input) = inputs.pop_front() {
            for action in agent.step(input) {
                match action {
                    Action::Emit(body) => {
                        self.append(body)?;
                    }
                    Action::RunTool { tool, input } => {
                        let call = self.append(Body::ToolCall {
                            tool: tool.clone(),
                            input: input.clone(),
                        })?;
                        let outcome = self.run_tool(tools, call, &tool, &input);
                        self.append(Body::ToolResult {
                            call,
                            outcome: outcome.clone(),
                        })?;
                        inputs.push_back(Input::ToolDone { call, outcome });
                    }
                }
            }
        }
        Ok(())
    }

    fn run_tool(
        &self,
        tools: &Toolset,
        call: u64,
        tool: &str,
        input: &serde_json::Value,
    ) -> Outcome {
        let spawned = |pgid| {
            let Some(boot) = sys::boot_id() else { return };
            let body = Body::ToolProcess {
                call,
                pgid,
                boot: boot.to_owned(),
            };
            if let Err(e) = self.append(body) {
                eprintln!("session {}: recording process {pgid}: {e}", self.id.short());
            }
        };
        tools.run(
            tool,
            input,
            &Cx {
                cwd: &self.workspace,
                spawned: &spawned,
            },
        )
    }

    /// Finishes the effect sandwich for calls that have an intent but no
    /// outcome: their leftover processes are killed, then safe tools run
    /// again and everything else is marked interrupted.
    fn recover(&self, agent: &mut impl Agent, tools: &Toolset) -> io::Result<()> {
        let dangling = dangling_calls(&self.lock().entries);
        for call in dangling {
            let this_boot = call
                .processes
                .iter()
                .filter(|(_, boot)| Some(boot.as_str()) == sys::boot_id());
            for (pgid, _) in this_boot {
                if let Err(e) = sys::kill_process_group(*pgid) {
                    eprintln!(
                        "session {}: killing process group {pgid}: {e}",
                        self.id.short()
                    );
                }
            }
            let outcome = match tools.get(&call.tool).map(agent::Tool::replay) {
                Some(Replay::Safe) => self.run_tool(tools, call.seq, &call.tool, &call.input),
                _ => Outcome::Interrupted,
            };
            self.append(Body::ToolResult {
                call: call.seq,
                outcome: outcome.clone(),
            })?;
            self.drive(
                agent,
                tools,
                Input::ToolDone {
                    call: call.seq,
                    outcome,
                },
            )?;
        }
        Ok(())
    }
}

struct Dangling {
    seq: u64,
    tool: String,
    input: serde_json::Value,
    processes: Vec<(u32, String)>,
}

fn dangling_calls(entries: &[Entry]) -> Vec<Dangling> {
    let finished: HashSet<u64> = entries
        .iter()
        .filter_map(|e| match &e.body {
            Body::ToolResult { call, .. } => Some(*call),
            _ => None,
        })
        .collect();
    let mut dangling: Vec<Dangling> = Vec::new();
    for e in entries {
        match &e.body {
            Body::ToolCall { tool, input } if !finished.contains(&e.seq) => {
                dangling.push(Dangling {
                    seq: e.seq,
                    tool: tool.clone(),
                    input: input.clone(),
                    processes: Vec::new(),
                });
            }
            Body::ToolProcess { call, pgid, boot } => {
                if let Some(d) = dangling.iter_mut().find(|d| d.seq == *call) {
                    d.processes.push((*pgid, boot.clone()));
                }
            }
            _ => {}
        }
    }
    dangling
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(seq: u64, body: Body) -> Entry {
        Entry {
            seq,
            epoch: 0,
            body,
        }
    }

    #[test]
    fn finds_calls_without_outcomes() {
        let entries = [
            entry(
                1,
                Body::ToolCall {
                    tool: "sleep".into(),
                    input: json!({}),
                },
            ),
            entry(
                2,
                Body::ToolResult {
                    call: 1,
                    outcome: Outcome::Interrupted,
                },
            ),
            entry(
                3,
                Body::ToolCall {
                    tool: "run_command".into(),
                    input: json!({}),
                },
            ),
            entry(
                4,
                Body::ToolProcess {
                    call: 3,
                    pgid: 42,
                    boot: "boot".into(),
                },
            ),
        ];
        let dangling = dangling_calls(&entries);
        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].seq, 3);
        assert_eq!(dangling[0].processes, [(42, "boot".to_owned())]);
    }
}
