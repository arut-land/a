//! The daemon owns its home: its environment, its config file, its lock
//! and crash breaker, and the session list, whichever client started it.
#![cfg(unix)]

mod common;

use std::fs;

use common::Env;
use protocol::{Body, Outcome, Target};

/// The client that starts the daemon does not lend it its environment: a
/// key in it never serves another client's session.
#[test]
fn a_clients_key_never_serves_another_clients_session() {
    let env = Env::new("own-env");
    // The model-backed agent.
    fs::write(env.home.config_file(), "[daemon]\nidle_secs = 15\n").unwrap();
    let first = env
        .cmd(&["new"])
        .env("OPENAI_API_KEY", "sk-first-client")
        // Nothing answers here: a call that took the first client's
        // variables would fail to connect rather than for want of a key.
        .env("OPENAI_BASE_URL", "http://127.0.0.1:9/v1")
        .output()
        .unwrap();
    assert!(first.status.success(), "{first:?}");

    // Another client's session, with no environment of its own.
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    conn.input("hi");
    let entries = conn.until(|e| matches!(e.body, Body::ModelResult { .. }));
    let Body::ModelResult {
        outcome: Outcome::Failed { error },
        ..
    } = &entries.last().unwrap().body
    else {
        panic!("the model call did not fail: {entries:?}");
    };
    assert!(error.contains("OPENAI_API_KEY"), "{error}");
}
