//! The host list: `machines.toml` and the ssh configuration's hosts, as
//! the system `ssh -G` resolves them, watched as the files change.

use std::fs;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use client::{Known, Route, Sources, SshConfig};
use protocol::MachineId;

fn sources(machines: &str, ssh: &str) -> (tempfile::TempDir, Sources) {
    let dir = tempfile::tempdir().unwrap();
    let sources = Sources {
        machines: dir.path().join("config/machines.toml"),
        ssh: Some(SshConfig::File(dir.path().join("ssh/config"))),
    };
    fs::create_dir_all(dir.path().join("config")).unwrap();
    fs::create_dir_all(dir.path().join("ssh")).unwrap();
    fs::write(&sources.machines, machines).unwrap();
    fs::write(dir.path().join("ssh/config"), ssh).unwrap();
    (dir, sources)
}

fn names(machines: &[client::Machine]) -> Vec<&str> {
    machines.iter().map(|m| m.name.as_str()).collect()
}

#[test]
fn machines_toml_names_come_first_and_the_ssh_hosts_follow() {
    let id = MachineId([9; 16]);
    let (_dir, sources) = sources(
        &format!("[machine.mac2]\nssh = \"me@mac2.local\"\nid = \"{id}\"\n\n[machine.box]\n"),
        "Host win mac2\n  HostName win.local\nHost *\n  User me\n",
    );
    let machines = sources.machines().unwrap();
    assert_eq!(names(&machines), ["mac2", "box", "win"]);
    assert_eq!(machines[0].ssh.as_str(), "me@mac2.local");
    assert_eq!(machines[0].pinned, Some(id));
    assert_eq!(
        (machines[1].ssh.as_str(), machines[1].pinned),
        ("box", None)
    );
    assert_eq!(sources.named("win").unwrap().unwrap().ssh.as_str(), "win");

    fs::write(&sources.machines, "[machine.x]\nid = \"not an id\"\n").unwrap();
    let error = sources.machines().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("machines.toml"), "{error}");
}

#[test]
fn ssh_resolves_each_host_and_a_second_name_for_one_route_is_left_out() {
    let (_dir, sources) = sources(
        "",
        "Host one also-one\n  HostName one.example\n  User me\n  Port 2222\nHost two\n  HostName two.example\n  User me\n",
    );
    let known = sources.resolve().unwrap();
    let found: Vec<(&str, Option<&Route>)> = known
        .iter()
        .map(|k| (k.machine.name.as_str(), k.route.as_ref()))
        .collect();
    let route = |hostname: &str, port| Route {
        user: "me".into(),
        hostname: hostname.into(),
        port,
    };
    assert_eq!(
        found,
        [
            ("one", Some(&route("one.example", 2222))),
            ("two", Some(&route("two.example", 22))),
        ]
    );
}

#[test]
fn the_host_list_follows_its_files() {
    let (_dir, sources) = sources("", "Host one\n  HostName one.example\n");
    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let into = Arc::clone(&seen);
    let rt: rt::Rt = Arc::new(rt::Threads::default());
    let _watch = sources
        .watch(&rt, move |known: io::Result<Vec<Known>>| {
            let known = known.unwrap();
            into.lock()
                .unwrap()
                .push(known.into_iter().map(|k| k.machine.name).collect());
        })
        .unwrap();
    let until = |names: &[&str]| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while seen.lock().unwrap().last().is_none_or(|last| last != names) {
            assert!(Instant::now() < deadline, "{:?}", seen.lock().unwrap());
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    until(&["one"]);
    fs::write(&sources.machines, "[machine.mac2]\n").unwrap();
    until(&["mac2", "one"]);
}
