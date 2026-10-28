//! Newline framing and launch inputs; process lifetime belongs to the scoped owner.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ext::{OwnedProcess, SpawnSpec};
use serde_json::Value;

use crate::{Failure, Server, Transport, rpc};

// Grow for active frames; dormant helpers retain only the initial chunk.
const INITIAL_READ: usize = 512;
const MAX_READ: usize = 8192;

pub(crate) fn snapshot() -> BTreeMap<String, String> {
    protocol::Environment::new(protocol::HostPath::default(), std::env::vars_os(), false)
        .vars
        .into_iter()
        .filter(|(key, _)| !protocol::Environment::is_secret(key))
        .collect()
}

pub(crate) fn spec(server: &Server, cwd: PathBuf, mut env: BTreeMap<String, String>) -> SpawnSpec {
    let Transport::Stdio(argv) = &server.transport else {
        unreachable!("stdio declaration")
    };
    sys::shell::ExecutionFacts::current().apply_environment(&mut env);
    env.extend(server.env.clone());
    SpawnSpec {
        argv: argv.clone(),
        cwd,
        env,
    }
}

pub(crate) struct Reader {
    pending: Vec<u8>,
    scanned: usize,
    scratch: Box<[u8]>,
}

impl Default for Reader {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            scanned: 0,
            scratch: vec![0; INITIAL_READ].into_boxed_slice(),
        }
    }
}

impl Reader {
    pub(crate) async fn read(&mut self, child: &OwnedProcess) -> Result<(Value, usize), Failure> {
        loop {
            if let Some(end) = self.pending[self.scanned..]
                .iter()
                .position(|b| *b == b'\n')
            {
                let end = self.scanned + end;
                if end > rpc::LIMIT {
                    return Err(Failure::Limit);
                }
                let value = serde_json::from_slice(&self.pending[..end])
                    .map_err(|e| Failure::Protocol(e.to_string()))?;
                // Keep only unread bytes, not the completed message's allocation.
                self.pending = self.pending[end + 1..].to_vec();
                self.scanned = 0;
                if self.scratch.len() > INITIAL_READ {
                    self.scratch = vec![0; INITIAL_READ].into_boxed_slice();
                }
                return Ok((value, end + 1));
            }
            if self.pending.len() > rpc::LIMIT {
                return Err(Failure::Limit);
            }
            self.scanned = self.pending.len();
            let n = child
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
            if n == self.scratch.len() && self.scratch.len() < MAX_READ {
                self.scratch = vec![0; self.scratch.len() * 2].into_boxed_slice();
            }
        }
    }
}

pub(crate) fn io(error: &std::io::Error) -> Failure {
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
    use ext::{Cx, LaunchSpec, ProcessOutput, Processes};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    struct Client {
        child: Arc<OwnedProcess>,
        reader: Reader,
    }
    impl Client {
        async fn start(server: &Server, cx: &Cx) -> Result<Self, Failure> {
            let processes = cx.service::<dyn Processes>().unwrap();
            let child = processes
                .spawn(
                    cx,
                    LaunchSpec {
                        process: spec(
                            server,
                            cx.dir(),
                            cx.env
                                .as_ref()
                                .map_or_else(snapshot, |env| env.vars.clone()),
                        ),
                        output: ProcessOutput::Stdio,
                    },
                )
                .await
                .map_err(|e| Failure::Spawn(e.to_string()))?;
            Ok(Self {
                child: Arc::new(child),
                reader: Reader::default(),
            })
        }
        async fn read(&mut self) -> Result<(Value, usize), Failure> {
            self.reader.read(&self.child).await
        }
    }

    fn fixture(name: &str) -> (Rig, Server) {
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
        let rig = Rig::new(
            name,
            Registry::default()
                .with(&ext_shell::Shell::default())
                .unwrap(),
        );
        (rig, server)
    }

    #[test]
    fn completed_frames_release_storage_and_keep_adjacent_messages() {
        let (rig, mut server) = fixture("frame-storage");
        let Transport::Stdio(argv) = &mut server.transport else {
            unreachable!()
        };
        *argv.last_mut().unwrap() =
            r#"printf '\"'; printf '%0900000d' 0; printf '\"\n{\"next\":true}\n'; read -r line"#
                .into();
        rt::block_on(async {
            let mut client = Client::start(&server, &rig.cx()).await.unwrap();
            let (value, bytes) = client.read().await.unwrap();
            assert_eq!(value.as_str().unwrap().len(), 900_000);
            assert_eq!(bytes, 900_003);
            assert!(client.reader.pending.capacity() <= MAX_READ);
            assert_eq!(client.reader.scratch.len(), INITIAL_READ);
            let (value, bytes) = client.read().await.unwrap();
            assert_eq!(value, json!({"next":true}));
            assert_eq!(bytes, 14);
            assert_eq!(client.reader.pending.capacity(), 0);
        });
    }

    #[test]
    fn cancelling_a_read_preserves_the_fragment_and_scan_cursor() {
        use std::task::Poll;

        let (rig, mut server) = fixture("frame-cancel");
        let Transport::Stdio(argv) = &mut server.transport else {
            unreachable!()
        };
        *argv.last_mut().unwrap() =
            r#"printf '\"fragment'; read -r line; printf 'ed\"\n'; read -r line"#.into();
        rt::block_on(async {
            let mut client = Client::start(&server, &rig.cx()).await.unwrap();
            while client.reader.pending.is_empty() {
                {
                    let mut read = std::pin::pin!(client.read());
                    std::future::poll_fn(|cx| {
                        assert!(read.as_mut().poll(cx).is_pending());
                        Poll::Ready(())
                    })
                    .await;
                }
                rt::Reactor::AsyncIo.sleep(Duration::from_millis(1)).await;
            }
            assert_eq!(client.reader.pending, b"\"fragment");
            assert_eq!(client.reader.scanned, client.reader.pending.len());
            client
                .child
                .input
                .as_ref()
                .unwrap()
                .write(b"continue\n")
                .await
                .unwrap();
            let (value, bytes) = client.read().await.unwrap();
            assert_eq!(value, json!("fragmented"));
            assert_eq!(bytes, 13);
            assert_eq!(client.reader.pending.capacity(), 0);
            assert_eq!(client.reader.scanned, 0);
        });
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
        let client = rt::block_on(Client::start(&server, &cx)).unwrap();
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
    #[cfg(unix)]
    fn fallback_environment_skips_non_utf8_host_values() {
        use std::os::unix::ffi::OsStringExt;
        const CHILD: &str = "A_TEST_MCP_FALLBACK_ENV";
        if std::env::var_os(CHILD).is_some() {
            let rig = Rig::new(
                "mcp-byte-environment",
                Registry::default()
                    .with(&ext_shell::Shell::default())
                    .unwrap(),
            );
            let cx = rig.cx();
            let server = Server {
                name: "environment".parse().unwrap(),
                description: String::new(),
                transport: Transport::Stdio(vec![
                    "/bin/sh".into(), "-c".into(),
                    r#"printf '{"lang":"%s","terminfo":"%s"}\n' "$LANG" "${TERMINFO-unset}"; read release"#.into(),
                ]),
                env: BTreeMap::new(),
            };
            rt::block_on(async {
                let mut client = Client::start(&server, &cx).await.unwrap();
                let (value, _) = client.read().await.unwrap();
                assert_eq!(value, json!({"lang":"C", "terminfo":"unset"}));
                client
                    .child
                    .input
                    .as_ref()
                    .unwrap()
                    .write(b"release\n")
                    .await
                    .unwrap();
                assert!(client.child.wait().await.unwrap().success());
                client.child.ended().await;
            });
            return;
        }
        let rig = Rig::new("mcp-byte-environment-parent", Registry::default());
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "stdio::tests::fallback_environment_skips_non_utf8_host_values",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("A_HOME", &rig.dir)
            .env("LANG", "C")
            .env("TERMINFO", std::ffi::OsString::from_vec(vec![0xff]))
            .env(std::ffi::OsString::from_vec(vec![b'X', 0xff]), "byte name")
            .status()
            .unwrap();
        assert!(status.success());
    }
}
