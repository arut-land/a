//! Process-wide wake counts require a test binary with no other tests.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use ext::host::Registry;
use ext::testing::Rig;
use serde_json::json;

#[test]
fn a_quiet_command_wakes_nothing_while_it_runs() {
    let rig = Rig::new(
        "quiet",
        Registry::default().with(&ext_shell::Shell).unwrap(),
    );
    rig.done("bash", &json!({ "command": "true" }));
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let before = rt::wakes();
    let command = thread::spawn(move || {
        rig.done("bash", &json!({
        "command": format!("exec 3<>/dev/tcp/127.0.0.1/{port}; printf ready >&3; read -r reply <&3")
    }))
    });
    let (mut release, _) = listener.accept().unwrap();
    let mut ready = [0; 5];
    release.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready");
    release.write_all(b"done\n").unwrap();
    let ran = command.join().unwrap();
    let woke = rt::wakes() - before;
    assert_eq!(ran["exit"], 0);
    assert!(woke < 10, "{woke} wakes");
}
