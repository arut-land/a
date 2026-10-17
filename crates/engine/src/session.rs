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
        let cx = Cx {
            cwd: self.workspace.clone(),
        };
        if let Err(e) = self.recover(&mut agent, tools, &cx) {
            eprintln!("session {}: recovery: {e}", self.id.short());
        }
        for text in inbox {
            let result = self
                .append(Body::User { text: text.clone() })
                .and_then(|_| self.drive(&mut agent, tools, &cx, Input::User { text }));
            if let Err(e) = result {
                eprintln!("session {}: {e}", self.id.short());
            }
        }
    }

    fn drive(
        &self,
        agent: &mut impl Agent,
        tools: &Toolset,
        cx: &Cx,
        input: Input,
    ) -> io::Result<()> {
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
                        let outcome = tools.run(&tool, &input, cx);
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

    /// Finishes the effect sandwich for calls that have an intent but no
    /// outcome: safe tools run again, everything else is marked interrupted.
    fn recover(&self, agent: &mut impl Agent, tools: &Toolset, cx: &Cx) -> io::Result<()> {
        let dangling = dangling_calls(&self.lock().entries);
        for (call, tool, input) in dangling {
            let outcome = match tools.get(&tool).map(agent::Tool::replay) {
                Some(Replay::Safe) => tools.run(&tool, &input, cx),
                _ => Outcome::Interrupted,
            };
            self.append(Body::ToolResult {
                call,
                outcome: outcome.clone(),
            })?;
            self.drive(agent, tools, cx, Input::ToolDone { call, outcome })?;
        }
        Ok(())
    }
}

fn dangling_calls(entries: &[Entry]) -> Vec<(u64, String, serde_json::Value)> {
    let finished: HashSet<u64> = entries
        .iter()
        .filter_map(|e| match &e.body {
            Body::ToolResult { call, .. } => Some(*call),
            _ => None,
        })
        .collect();
    entries
        .iter()
        .filter(|e| !finished.contains(&e.seq))
        .filter_map(|e| match &e.body {
            Body::ToolCall { tool, input } => Some((e.seq, tool.clone(), input.clone())),
            _ => None,
        })
        .collect()
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
        ];
        let dangling = dangling_calls(&entries);
        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].0, 3);
    }
}
