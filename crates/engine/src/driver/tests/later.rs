//! Handlers that answer later: what they hold, and what they do not.

use std::sync::atomic::AtomicUsize;

use super::*;

/// Approves nothing and never answers: its gate holds its call until
/// something else settles it. Its side work (here, a task that waits for
/// the call's end) is handed the call's context.
struct Mute {
    /// How many of its calls' cancels have fired.
    ended: Arc<AtomicUsize>,
}

impl Extension for Mute {
    const NAME: &'static str = "mute";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let ended = Arc::clone(&self.ended);
        r.on::<point::Permission>(|_, _| Permission::Ask);
        r.on::<point::Approve>(move |cx, _| {
            let Some(cx) = cx.cx() else {
                return Answer::Now(None);
            };
            let ended = Arc::clone(&ended);
            if let Some(rt) = cx.service::<dyn rt::Runtime>() {
                let cancel = cx.cancel.clone();
                rt.spawn(Box::pin(async move {
                    cancel.wait().await;
                    ended.fetch_add(1, Ordering::SeqCst);
                }));
            }
            Answer::Later(ext::Later::new(std::future::pending()))
        });
    }
}

fn muted() -> (Registry, Arc<AtomicUsize>) {
    let ended = Arc::new(AtomicUsize::new(0));
    let mute = Mute {
        ended: Arc::clone(&ended),
    };
    let registry = Registry::default()
        .with_in(ext::Tier::Pre, &mute)
        .and_then(|r| r.with(&ext_shell::Shell))
        .unwrap();
    (registry, ended)
}

/// Whether the one tool call was settled as interrupted, and not run.
fn interrupted(log: &[Entry]) -> bool {
    let ran = log
        .iter()
        .any(|e| matches!(e.body, Body::ToolProcess { .. }));
    let settled = log.iter().any(|e| {
        matches!(
            &e.body,
            Body::ToolResult {
                outcome: Outcome::Interrupted { .. },
                ..
            }
        )
    });
    assert!(!ran, "the gate did not hold: {log:#?}");
    settled
}

/// Whether the log holds a tool call, which its gate then holds.
fn gated(log: &[Entry]) -> bool {
    log.iter().any(|e| matches!(e.body, Body::ToolCall { .. }))
}

/// A `Later` built from a call's context is cancelled with the call: the
/// context's cancel is the call's own, so work a handler takes it along
/// to (a side request, a task) ends when an interrupt settles the call,
/// not when the model next answers.
#[test]
fn a_later_is_cancelled_with_its_call() {
    let (registry, ended) = muted();
    let session = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[]))).create();
    session.submit("$ true").unwrap();
    until(&session, "gated", gated);
    session.barrier();
    assert_eq!(ended.load(Ordering::SeqCst), 0);
    session.interrupt_now();
    let log = until(&session, "interrupted", interrupted);
    let deadline = Instant::now() + Duration::from_secs(5);
    while ended.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "the side work outlived its call: {log:#?}"
        );
        session.barrier();
    }
    session.halt();
}

/// A handler that never answers holds only its call: an interrupt settles
/// the call, and meanwhile the actor, and every other session on the same
/// worker, goes on serving.
#[test]
fn a_plugin_cannot_block_the_actor() {
    let (registry, _) = muted();
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let held = fixture.create();
    held.submit("$ true").unwrap();
    until(&held, "gated", gated);
    // The one worker is free: another session steps and settles.
    let other = fixture.create();
    other.submit("hello").unwrap();
    settle(&other);
    assert!(
        log_of(&other)
            .iter()
            .any(|e| matches!(e.body, Body::Assistant { .. }))
    );
    // And the held session still takes requests: the interrupt settles
    // the call its gate held.
    held.interrupt_now();
    until(&held, "interrupted", interrupted);
    held.halt();
    other.halt();
}
