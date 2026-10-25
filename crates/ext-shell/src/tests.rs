use ext::host::{Host, Parts, SessionServices};
use ext::{BoxFuture, MutationError, MutationResult, States};
use protocol::{Json, Outcome, Seq};
use rt::oneshot;
use std::sync::{Arc, Mutex};

struct PausedRecord {
    rig: Arc<Rig>,
    ready: Mutex<Option<oneshot::Sender<()>>>,
    reply: Mutex<Option<oneshot::Receiver<MutationResult>>>,
}
impl Host for PausedRecord {
    fn invoke(
        self: Arc<Self>,
        _: ext::Cx,
        _: String,
        _: String,
        _: Json,
    ) -> BoxFuture<'static, ext::Called> {
        unreachable!("this fixture does not invoke tools")
    }

    fn spawned(&self, pgid: u32, background: bool) {
        self.rig.spawned(pgid, background);
    }
    fn own(&self, process: ext::ProcessLease) {
        self.rig.own(process);
    }
    fn progress(&self, text: &str) {
        self.rig.progress(text);
    }
    fn note(&self, _: &str, state: &str, _: Json) -> BoxFuture<'static, MutationResult> {
        assert_eq!(state, "cwd");
        self.ready.lock().unwrap().take().unwrap().send(());
        let reply = self.reply.lock().unwrap().take().unwrap();
        Box::pin(async move { reply.await.unwrap() })
    }
    fn fact(&self, fact: protocol::ExtensionFact) -> BoxFuture<'static, MutationResult> {
        self.rig.fact(fact)
    }
    fn publish(
        &self,
        namespace: &str,
        key: Json,
        text: String,
    ) -> BoxFuture<'static, MutationResult> {
        self.rig.publish(namespace, key, text)
    }
    fn edit(&self, target: Seq, upto: Option<Seq>, replace: Option<Json>) {
        self.rig.edit(target, upto, replace);
    }
    fn states(&self, read: &mut dyn FnMut(&States)) {
        self.rig.states(read);
    }
    fn registry(&self) -> &Registry {
        &self.rig.registry
    }
}

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use ext::host::Registry;
use ext::testing::Rig;

use crate::Shell;
use crate::state::Cwd;

#[test]
fn an_extension_cannot_hold_session_state_across_an_await() {
    let rig = Rig::new("hold", Registry::default().with(&Shell::default()).unwrap());
    let (reader, writer) = (rig.cx(), rig.cx());
    let ws = rig.workspace();
    let sub = ws.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let (paused, resume) = (mpsc::channel(), mpsc::channel::<()>());
    let read = std::thread::spawn(move || {
        rt::block_on(async {
            // What a read yields is owned: no lock outlives the closure.
            let before = reader.state(|cwd: &Cwd| cwd.after(None).map(Path::to_path_buf));
            paused.0.send(()).unwrap();
            while resume.1.try_recv().is_err() {
                rt::Reactor::AsyncIo.sleep(Duration::from_millis(5)).await;
            }
            let after = reader.state(|cwd: &Cwd| cwd.after(None).map(Path::to_path_buf));
            (before, after)
        })
    });
    paused.1.recv().unwrap();
    // A change commits while the reader is suspended at an await.
    rt::block_on(writer.update::<Cwd>(&Cwd::change(&sub, None))).unwrap();
    resume.0.send(()).unwrap();
    let (before, after) = read.join().unwrap();
    assert_eq!((before, after), (None, Some(sub)));
}

#[test]
fn revoking_a_pending_cwd_record_preserves_the_shell_result() {
    let rig = Arc::new(Rig::new(
        "revoked-cwd",
        Registry::default().with(&Shell::default()).unwrap(),
    ));
    std::fs::create_dir(rig.workspace().join("sub")).unwrap();
    let (ready, waiting) = oneshot::channel();
    let (reply, response) = oneshot::channel();
    let original = rig.cx();
    let cx = Parts {
        workspace: original.workspace,
        env: original.env,
        session: original.session,
        call: original.call,
        model: original.model,
        blobs: original.blobs,
        cancel: original.cancel,
        services: Arc::new(
            SessionServices::default().with::<dyn rt::Runtime>(Arc::new(rt::Threads::default())),
        ),
    }
    .cx(Arc::new(PausedRecord {
        rig: Arc::clone(&rig),
        ready: Mutex::new(Some(ready)),
        reply: Mutex::new(Some(response)),
    }));
    let input = Json::encode(&serde_json::json!({"command": "cd sub; printf settled"})).unwrap();
    let called = rt::block_on(async {
        let mut work = std::pin::pin!(rig.registry.tools().run("bash", &input, None, &cx));
        rt::or(
            async {
                waiting.await.unwrap();
            },
            async {
                let early = work.as_mut().await;
                panic!("shell settled before its cwd record: {:?}", early.outcome);
            },
        )
        .await;
        cx.cancel.set();
        reply.send(Err(MutationError::Revoked));
        work.await
    });
    let Outcome::Done {
        output: protocol::Payload::Inline(output),
    } = called.outcome
    else {
        panic!("{:?}", called.outcome)
    };
    let ran: crate::bash::Ran = output.parse().unwrap();
    assert_eq!(ran.exit, Some(0));
    assert_eq!(ran.output, "settled");
    assert_eq!(ran.cwd, rig.workspace().join("sub"));
    assert!(rig.log.lock().unwrap().is_empty());
}

#[test]
fn published_description_matches_the_selected_executable() {
    let facts = sys::shell::ExecutionFacts::current();
    let registry = Registry::default().with(&Shell::default()).unwrap();
    let specs = registry.tools().specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, facts.tool());
    assert!(specs[0].description.contains(&facts.context()));
    assert_eq!(
        registry.tools().get(facts.tool()).unwrap().description(),
        specs[0].description
    );
}

struct Selected(&'static sys::shell::ExecutionFacts);

impl ext::Extension for Selected {
    const NAME: &'static str = "shell";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut ext::Registrar) {
        super::contribute(r, self.0, crate::bash::TIMEOUT);
    }
}

fn powershell_facts(executable: std::path::PathBuf) -> &'static sys::shell::ExecutionFacts {
    let dialect = if executable
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("powershell.exe"))
    {
        sys::shell::Dialect::WindowsPowerShell
    } else {
        sys::shell::Dialect::PowerShell
    };
    Box::leak(Box::new(sys::shell::ExecutionFacts {
        dialect,
        executable,
        syntax: "PowerShell test runner",
        paths: "Windows literal paths",
        utilities: "PowerShell cmdlets",
        cleanup: "owned job",
        temp: std::env::temp_dir(),
        cwd_command: "",
        runtime_bin: None,
    }))
}

#[test]
fn fallback_registers_exactly_one_powershell_tool() {
    let selected = Selected(powershell_facts("powershell.exe".into()));
    let registry = Registry::default().with(&selected).unwrap();
    let tools = registry.tools().specs();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "powershell");
    assert!(tools[0].description.contains("PowerShell"));
    assert!(!registry.system_prompt().contains("grep"));
    assert!(registry.tools().get("bash").is_none());
}

#[test]
fn powershell_preserves_status_errors_unicode_cwd_and_no_input() {
    // The hosted native lanes provide both installed editions; other hosts
    // still exercise catalog selection and pure quoting/length invariants.
    let Some(executables) = std::env::var_os("A_TEST_POWERSHELL") else {
        return;
    };
    for (index, executable) in std::env::split_paths(&executables).enumerate() {
        eprintln!("PowerShell runner: {}", executable.display());
        let rig = Rig::new(
            &format!("powershell-{index}"),
            Registry::default()
                .with(&Selected(powershell_facts(executable)))
                .unwrap(),
        );
        // A failed cwd receipt must not replace the user's explicit exit code.
        std::fs::create_dir_all(rig.stream(1).with_extension("cwd")).unwrap();
        let run = |command: &str| -> crate::bash::Ran {
            eprintln!("PowerShell command: {command}");
            let ran: crate::bash::Ran = serde_json::from_value(rig.done(
                "powershell",
                &serde_json::json!({"command": command, "timeout": 90}),
            ))
            .unwrap();
            eprintln!(
                "PowerShell result: exit {:?}; timeout {:?}",
                ran.exit, ran.timed_out
            );
            assert!(ran.timed_out.is_none(), "{command}: {}", ran.output);
            ran
        };
        for (command, code) in [
            ("exit 37", 37),
            ("cmd.exe /c exit 7", 1),
            ("cmd.exe /c exit 7; exit $LASTEXITCODE", 7),
            ("cmd.exe /c exit 7; Write-Output 'ok'", 0),
            ("Write-Error 'problem'; Write-Output 'ok'", 0),
            ("Write-Error 'problem'", 1),
            ("throw 'problem'", 1),
            (
                "try { throw 'handled' } catch { Write-Output 'handled' }",
                0,
            ),
            ("return", 0),
            ("", 0),
            ("Read-Host 'input'", 1),
        ] {
            let ran = run(command);
            assert_eq!(ran.exit, Some(code), "{command}: {}", ran.output);
            assert!(!ran.output.contains("CLIXML"), "{command}: {}", ran.output);
            if command.contains("problem") {
                assert!(ran.output.contains("problem"));
            }
        }
        let sub = rig.workspace().join("space ' [café]");
        std::fs::create_dir(&sub).unwrap();
        let ran = run("Set-Location -LiteralPath 'space '' [café]'; Write-Output 'café'; exit 23");
        assert_eq!(ran.exit, Some(23), "{}", ran.output);
        assert_eq!(ran.cwd, sub);
        assert!(ran.output.contains("café"), "{}", ran.output);
        let ran = run("Write-Output (");
        assert_ne!(ran.exit, Some(0));
        assert_eq!(ran.cwd, sub);
    }
}
