//! A leased stdio connection: every abnormal return drops it and kills
//! the group, so a late response can never answer a later call.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use ext::{Cx, LaunchSpec, OwnedProcess, ProcessOutput, Processes, SpawnSpec};
use serde_json::{Value, json};

use crate::{Failure, Server, Transport, rpc};

pub(crate) struct Client {
    child: OwnedProcess,
    active: Arc<AtomicBool>,
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
        self.active.load(Ordering::Acquire) && self.child.alive()
    }

    pub(crate) async fn start(
        server: &Server,
        cx: &Cx,
        cache: Weak<crate::Inner>,
    ) -> Result<Self, Failure> {
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
        // Servers share the tools' temporary storage; their own
        // configuration still wins.
        sys::shell::ExecutionFacts::current().apply_environment(&mut env);
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
        let key = (cx.session, server.name.clone());
        let active = Arc::new(AtomicBool::new(true));
        let retained = Arc::clone(&active);
        let process = Arc::clone(&child.process);
        cx.own(
            ext::ProcessLease::from(Arc::clone(&child.process)).on_release(move || {
                retained.store(false, Ordering::Release);
                if let Some(cache) = cache.upgrade() {
                    let mut clients = crate::lock(&cache.clients);
                    if clients
                        .get(&key)
                        .is_some_and(|client| Arc::ptr_eq(&client.child.process, &process))
                    {
                        clients.remove(&key);
                    }
                }
            }),
            // Started again on the next call that needs it.
            ext::Role::Helper,
        );
        Ok(Self {
            child,
            active,
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
        self.child
            .input
            .as_ref()
            .unwrap()
            .flush()
            .await
            .map_err(|e| io(&e))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use ext::host::Registry;
    use ext::testing::Rig;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn fixture(name: &str) -> (crate::Mcp, Rig, Server) {
        let server = Server {
            name: "review-cache".parse().unwrap(),
            description: String::new(),
            transport: Transport::Stdio(vec![
                sys::find_bash(std::env::var("PATH").ok().as_deref()).unwrap(),
                "--noprofile".into(),
                "--norc".into(),
                "-c".into(),
                "read -r line".into(),
            ]),
            env: BTreeMap::new(),
        };
        let mcp = crate::Mcp::new(vec![server.clone()], Duration::from_secs(5)).unwrap();
        let rig = Rig::new(
            name,
            Registry::default()
                .with(&ext_shell::Shell::default())
                .unwrap(),
        );
        (mcp, rig, server)
    }

    const ENV_OUT: &str = "A_TEST_MCP_ENV_OUT";
    const TEMPORARY_TEST: &str =
        "stdio::tests::stdio_servers_keep_temporary_files_in_the_homes_temporary_dir";

    /// The environment a stdio server starts with: this test binary, rerun
    /// as the server, writes its own to the file `ENV_OUT` names. Shells
    /// would rewrite or requote the variables under test.
    fn environment_of(rig: &Rig, mut env: BTreeMap<String, String>) -> BTreeMap<String, String> {
        let out = rig.cx().workspace.join(format!("env-{}.txt", env.len()));
        env.insert(ENV_OUT.into(), out.to_str().unwrap().into());
        let argv = vec![
            std::env::current_exe().unwrap().to_str().unwrap().into(),
            "--exact".into(),
            TEMPORARY_TEST.into(),
        ];
        let server = Server {
            name: "temporary-env".parse().unwrap(),
            description: String::new(),
            transport: Transport::Stdio(argv),
            env,
        };
        let mcp = crate::Mcp::new(vec![server.clone()], Duration::from_secs(5)).unwrap();
        let mut cx = rig.cx();
        // A client whose own temporary directory lies outside the home.
        let mut vars: BTreeMap<String, String> = std::env::vars()
            .filter(|(k, _)| {
                ["PATH", "SYSTEMROOT"]
                    .iter()
                    .any(|n| k.eq_ignore_ascii_case(n))
            })
            .collect();
        for name in ["TMPDIR", "TMP", "TEMP"] {
            vars.insert(name.into(), "/outside".into());
        }
        cx.env = Some(Arc::new(protocol::Environment {
            cwd: cx.workspace.to_path_buf().try_into().unwrap(),
            vars,
            ..Default::default()
        }));
        let client = rt::block_on(Client::start(&server, &cx, Arc::downgrade(&mcp.0))).unwrap();
        let status = rt::block_on(client.child.process.wait()).unwrap();
        assert!(status.success(), "the server exited with {status:?}");
        let written = std::fs::read(&out).expect("the server ran this test as its child");
        serde_json::from_slice(&written).unwrap()
    }

    #[test]
    fn stdio_servers_keep_temporary_files_in_the_homes_temporary_dir() {
        if let Some(out) = std::env::var_os(ENV_OUT) {
            let env: BTreeMap<String, String> = std::env::vars()
                .map(|(k, v)| (k.to_ascii_uppercase(), v))
                .collect();
            std::fs::write(out, serde_json::to_vec(&env).unwrap()).unwrap();
            return;
        }
        let rig = Rig::new(
            "temporary-env",
            Registry::default()
                .with(&ext_shell::Shell::default())
                .unwrap(),
        );
        let env = environment_of(&rig, BTreeMap::new());
        let temp = sys::shell::ExecutionFacts::current().temp.to_str().unwrap();
        assert_eq!(env["TMPDIR"], temp);
        #[cfg(windows)]
        for name in ["TMP", "TEMP"] {
            assert_eq!(env[name], temp);
        }
        let explicit = environment_of(
            &rig,
            BTreeMap::from([("TMPDIR".into(), "/explicit".into())]),
        );
        assert_eq!(explicit["TMPDIR"], "/explicit");
    }

    #[test]
    fn session_shutdown_releases_cached_stdio_pipes() {
        let (mcp, rig, server) = fixture("session-pipes");
        let client =
            rt::block_on(Client::start(&server, &rig.cx(), Arc::downgrade(&mcp.0))).unwrap();
        let process = Arc::clone(&client.child.process);
        crate::lock(&mcp.0.clients).insert((rig.session, server.name), client);
        drop(rig);
        rt::block_on(process.ended());
        assert!(!process.alive());
        assert!(
            crate::lock(&mcp.0.clients).is_empty(),
            "dead cached clients retain both registered pipes and their buffers"
        );
    }

    #[test]
    fn old_process_completion_cannot_evict_a_replacement() {
        let (mcp, rig, server) = fixture("replacement-pipes");
        let cx = rig.cx();
        let old = rt::block_on(Client::start(&server, &cx, Arc::downgrade(&mcp.0))).unwrap();
        let replacement =
            rt::block_on(Client::start(&server, &cx, Arc::downgrade(&mcp.0))).unwrap();
        let process = Arc::clone(&replacement.child.process);
        let key = (rig.session, server.name);
        crate::lock(&mcp.0.clients).insert(key.clone(), replacement);
        let mut completions = rig.ends.count();
        drop(old);
        rt::block_on(rig.ends.changed(&mut completions));
        assert!(Arc::ptr_eq(
            &crate::lock(&mcp.0.clients).get(&key).unwrap().child.process,
            &process
        ));
        assert!(process.alive());
        drop(rig);
        assert!(crate::lock(&mcp.0.clients).is_empty());
    }
}
