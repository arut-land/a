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
    fn own(&self, pgid: u32, ended: BoxFuture<'static, ()>) {
        self.rig.own(pgid, ended);
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
    let rig = Rig::new("hold", Registry::default().with(&Shell).unwrap());
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
        Registry::default().with(&Shell).unwrap(),
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
        spill: original.spill,
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
    let Outcome::Done { output } = called.outcome else {
        panic!("{:?}", called.outcome)
    };
    let ran: crate::bash::Ran = output.parse().unwrap();
    assert_eq!(ran.exit, Some(0));
    assert_eq!(ran.output, "settled");
    assert_eq!(ran.cwd, rig.workspace().join("sub"));
    assert!(rig.log.lock().unwrap().is_empty());
}
