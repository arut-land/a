//! Process-wide wake counts require a test binary with no other tests.

use ext::host::Registry;
use ext::testing::Rig;
use serde_json::json;

#[test]
fn a_quiet_command_wakes_nothing_while_it_runs() {
    let rig = Rig::new("quiet", Registry::default().with(&ext_shell::Shell).unwrap());
    rig.done("bash", &json!({ "command": "true" }));
    let before = rt::wakes();
    let ran = rig.done("bash", &json!({ "command": "sleep 1" }));
    let woke = rt::wakes() - before;
    assert_eq!(ran["exit"], 0);
    assert!(woke < 10, "{woke} wakes");
}
