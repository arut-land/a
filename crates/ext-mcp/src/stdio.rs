//! A leased stdio connection: every abnormal return drops it and kills
//! the group, so a late response can never answer a later call.

use std::fs::File;
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ext::{Cx, SpawnSpec, Spawner};
use serde_json::{Value, json};

use crate::{Failure, Server, Transport, rpc};

pub(crate) struct Client {
    input: rt::Async<File>,
    output: rt::Async<File>,
    pending: Vec<u8>,
    scratch: Box<[u8; 8192]>,
    next: u64,
    inflight: Option<u64>,
    group: sys::Group,
    alive: Arc<AtomicBool>,
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.alive() {
            if let Some(id) = self.inflight {
                let notification = json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id,"reason":"client stopped waiting"}});
                // The pipe is nonblocking. Never let an uncooperative server
                // prevent the stronger cancellation of its whole process tree.
                let _ = writeln!(self.input.get_ref(), "{notification}");
            }
            let _ = self.group.kill();
        }
    }
}

impl Client {
    pub(crate) fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub(crate) fn start(
        server: &Server,
        cx: &Cx,
        cache: std::sync::Weak<crate::Inner>,
    ) -> Result<Self, Failure> {
        let Transport::Stdio(argv) = &server.transport else {
            unreachable!()
        };
        let rt = cx
            .service::<dyn rt::Runtime>()
            .ok_or_else(|| Failure::Configuration("no runtime".into()))?;
        let spawner = cx
            .service::<dyn Spawner>()
            .ok_or_else(|| Failure::Configuration("no process spawner".into()))?;
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
        let mut command = spawner.process(cx, spec).map_err(Failure::Spawn)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(|e| Failure::Spawn(e.to_string()))?;
        let group = sys::Group(child.id());
        let alive = Arc::new(AtomicBool::new(true));
        let reactor = rt.reactor();
        let input = child.stdin.take().expect("piped stdin");
        let output = child.stdout.take().expect("piped stdout");
        // Ownership precedes registration, so even a reactor failure leaves
        // a child that will be killed and reaped by the daemon.
        let live = Arc::clone(&alive);
        let watch = sys::exit_watch(group.0).and_then(|w| rt::Async::for_reading(&reactor, w));
        let key = (cx.session, server.name.clone());
        cx.own(group.0, async move {
            if let Ok(watch) = watch {
                watch.readable().await;
            } else {
                while matches!(child.try_wait(), Ok(None)) {
                    reactor.sleep(std::time::Duration::from_millis(50)).await;
                }
            }
            let _ = group.kill();
            let _ = child.wait();
            live.store(false, Ordering::Release);
            if let Some(cache) = cache.upgrade() {
                let mut clients = crate::lock(&cache.clients);
                if clients
                    .get(&key)
                    .is_some_and(|client| Arc::ptr_eq(&client.alive, &live))
                {
                    clients.remove(&key);
                }
            }
        });
        let registered = (|| {
            Ok(Self {
                input: rt::Async::new(&rt.reactor(), sys::child_input(input))
                    .map_err(|e| io(&e))?,
                output: rt::Async::for_reading(&rt.reactor(), sys::child_output(output))
                    .map_err(|e| io(&e))?,
                pending: Vec::new(),
                scratch: Box::new([0; 8192]),
                next: 0,
                inflight: None,
                group,
                alive,
            })
        })();
        if registered.is_err() {
            let _ = group.kill();
        }
        registered
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
                .input
                .write_with(|mut f| f.write(remaining))
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
                .output
                .read_with(|mut f| f.read(self.scratch.as_mut()))
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
