//! What a failing handler comes to, point by point.

use super::*;

/// A sandbox whose `spawn` hook panics.
struct Sandbox;

impl Extension for Sandbox {
    const NAME: &'static str = "sandbox";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on::<point::Spawn>(|_, _| panic!("the sandbox is broken"));
    }
}

/// `spawn` fails closed: a sandbox that fails never lets its process run
/// bare. The call fails, saying why, and nothing ran.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "dev builds' cranelift backend cannot unwind through these frames: \
              CARGO_PROFILE_DEV_CODEGEN_BACKEND=llvm cargo nextest run -p engine --run-ignored all"
)]
fn a_failing_spawn_hook_refuses_the_process() {
    let registry = echoing(&Policy::default()).with(&Sandbox).unwrap();
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let session = fixture.start(&[]);
    let marker = std::env::temp_dir().join(format!("a-sandbox-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    session
        .submit(&format!("$ touch {}", marker.display()))
        .unwrap();
    let log = finished(&session, 2);
    session.halt();
    let failed = log.iter().any(|e| {
        matches!(&e.body, Body::ToolResult { outcome: Outcome::Failed { error }, .. } if error.contains("panicked"))
    });
    assert!(failed, "{log:#?}");
    assert!(
        !log.iter()
            .any(|e| matches!(e.body, Body::ToolProcess { .. }))
    );
    assert!(!marker.exists());
}

/// `input` fails closed: input a handler cannot decide on is refused, and
/// its client told why.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "dev builds' cranelift backend cannot unwind through these frames: \
              CARGO_PROFILE_DEV_CODEGEN_BACKEND=llvm cargo nextest run -p engine --run-ignored all"
)]
fn a_failing_input_hook_refuses_the_input() {
    struct Garbled;

    impl Extension for Garbled {
        const NAME: &'static str = "garbled";
        const VERSION: u32 = 1;

        fn contribute(&self, r: &mut Registrar) {
            r.on::<point::Input>(|_, _| panic!("garbled"));
        }
    }

    let registry = Registry::default().with(&Garbled).unwrap();
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let session = fixture.start(&[]);
    let refused = session.submit("hi").unwrap_err();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    assert!(refused.contains("panicked"), "{refused}");
    assert!(!log.iter().any(|e| matches!(e.body, Body::Input { .. })));
}
