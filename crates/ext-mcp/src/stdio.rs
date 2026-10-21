//! A leased stdio connection: every abnormal return drops it and kills
//! the group, so a late response can never answer a later call.

use std::sync::Arc;

use ext::{Cx, LaunchSpec, OwnedProcess, ProcessOutput, Processes, SpawnSpec};
use serde_json::{Value, json};

use crate::{Failure, Server, Transport, rpc};

pub(crate) struct Client {
    child: OwnedProcess,
    pending: Vec<u8>,
    scratch: Box<[u8; 8192]>,
    next: u64,
    inflight: Option<u64>,
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.alive()
            && let Some(id) = self.inflight
        {
            let notification = json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id,"reason":"client stopped waiting"}});
            // The pipe is nonblocking. Never let an uncooperative server
            // prevent the stronger cancellation of its whole process tree.
            let _ = self
                .child
                .input
                .as_ref()
                .unwrap()
                .try_write(format!("{notification}\n").as_bytes());
        }
    }
}

impl Client {
    pub(crate) fn alive(&self) -> bool {
        self.child.alive()
    }

    pub(crate) async fn start(server: &Server, cx: &Cx) -> Result<Self, Failure> {
        let Transport::Stdio(argv) = &server.transport else {
            unreachable!()
        };
        let processes = cx
            .service::<dyn Processes>()
            .ok_or_else(|| Failure::Configuration("no process provider".into()))?;
        let mut env = cx.env.as_ref().map_or_else(
            || {
                std::env::vars()
                    .filter(|(k, _)| !protocol::Environment::is_secret(k))
                    .collect()
            },
            |e| e.vars.clone(),
        );
        env.extend(server.env.clone());
        let spec = SpawnSpec {
            argv: argv.clone(),
            cwd: cx.dir(),
            env,
        };
        let child = processes
            .spawn(
                cx,
                LaunchSpec {
                    process: spec,
                    output: ProcessOutput::Stdio,
                },
            )
            .await
            .map_err(|e| Failure::Spawn(e.to_string()))?;
        cx.own(OwnedProcess::new(Arc::clone(&child.process), None, None));
        Ok(Self {
            child,
            pending: Vec::new(),
            scratch: Box::new([0; 8192]),
            next: 0,
            inflight: None,
        })
    }

    pub(crate) async fn request(
        &mut self,
        method: &str,
        params: Value,
        cx: &Cx,
    ) -> Result<Value, Failure> {
        self.next += 1;
        let id = self.next;
        self.inflight = Some(id);
        let request = rpc::request(id, method, params);
        let bytes = serde_json::to_vec(&request).map_err(|e| Failure::Protocol(e.to_string()))?;
        if bytes.len() > rpc::LIMIT {
            return Err(Failure::Limit);
        }
        let mut bytes = bytes;
        bytes.push(b'\n');
        let mut remaining = bytes.as_slice();
        while !remaining.is_empty() {
            let written = self
                .child
                .input
                .as_ref()
                .unwrap()
                .write(remaining)
                .await
                .map_err(|e| io(&e))?;
            if written == 0 {
                return Err(Failure::Crashed);
            }
            remaining = &remaining[written..];
        }
        let mut incoming = crate::incoming::Incoming::new(id);
        loop {
            let (value, bytes) = incoming.wait(cx, self.read()).await?;
            let result = incoming.decode(&value, bytes)?;
            incoming.publish(cx, result.is_some());
            if let Some(result) = result {
                self.inflight = None;
                return Ok(result);
            }
        }
    }

    async fn read(&mut self) -> Result<(Value, usize), Failure> {
        loop {
            if let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                if end > rpc::LIMIT {
                    return Err(Failure::Limit);
                }
                let value = serde_json::from_slice(&self.pending[..end])
                    .map_err(|e| Failure::Protocol(e.to_string()))?;
                self.pending.drain(..=end);
                return Ok((value, end + 1));
            }
            if self.pending.len() > rpc::LIMIT {
                return Err(Failure::Limit);
            }
            let n = self
                .child
                .output
                .as_ref()
                .unwrap()
                .read(self.scratch.as_mut())
                .await
                .map_err(|e| io(&e))?;
            if n == 0 {
                return Err(Failure::Crashed);
            }
            self.pending.extend_from_slice(&self.scratch[..n]);
        }
    }
}

fn io(error: &std::io::Error) -> Failure {
    match error.kind() {
        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof => Failure::Crashed,
        _ => Failure::Transport(error.to_string()),
    }
}
