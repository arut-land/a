//! A client holding several daemons, each in an isolated home of its own:
//! sessions found by machine, lists side by side, pinned machines.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use client::{Daemon, Daemons, Error, Listed, Member, SessionWatch};
use protocol::{MachineId, SessionId, Target};

use crate::support::{Isolated, deadline};

/// A home with one session `a` started, its daemon, and the session.
fn started(name: &str) -> (Isolated, Daemon, SessionId) {
    let env = Isolated::new(name);
    env.run(&["-p", "hello"]);
    let staged = (env.home.runtime())
        .stage(Path::new(env!("CARGO_BIN_EXE_a")))
        .unwrap();
    let daemon = Daemon::local(env.home.clone(), staged);
    let session = daemon.sessions().unwrap().remove(0).id;
    (env, daemon, session)
}

fn machine(env: &Isolated) -> MachineId {
    MachineId(sys::machine_id(&env.home).unwrap())
}

fn pair(name: &str) -> ((Isolated, SessionId), (Isolated, SessionId), Daemons) {
    let (one, first, a) = started(&format!("{name}-one"));
    let (two, second, b) = started(&format!("{name}-two"));
    let daemons = Daemons::one("one", first).with(Member {
        name: "two".into(),
        daemon: second,
    });
    ((one, a), (two, b), daemons)
}

#[test]
fn a_session_is_found_on_the_machine_that_holds_it() {
    let ((_one, a), (_two, b), daemons) = pair("daemons-find");
    // A bare prefix is looked for on every machine.
    let (member, target) = daemons
        .resolve(Target::Prefix(b.short().to_string()))
        .unwrap();
    assert_eq!((member.name.as_str(), target), ("two", Target::Id(b)));
    // A machine's name sends the prefix to it as it is.
    let named = format!("one:{}", a.short());
    let (member, target) = daemons.resolve(Target::Prefix(named)).unwrap();
    assert_eq!(
        (member.name.as_str(), target),
        ("one", Target::Prefix(a.short().to_string()))
    );
    let unknown = daemons.resolve(Target::Prefix(format!("three:{}", a.short())));
    assert!(
        matches!(&unknown, Err(Error::Io(_, why)) if why == "no machine named three"),
        "{unknown:?}"
    );
    // What makes a session goes to the first.
    assert_eq!(daemons.resolve(Target::New).unwrap().0.name, "one");
}

#[test]
fn each_machine_lists_its_own_sessions_under_its_own_id() {
    let ((one, a), (two, b), daemons) = pair("daemons-list");
    let listed: Vec<Vec<Listed>> = daemons
        .sessions()
        .into_iter()
        .map(|(_, listed)| listed.unwrap())
        .collect();
    let addresses: Vec<_> = listed.iter().flatten().map(Listed::address).collect();
    assert_eq!(addresses.len(), 2);
    assert_eq!(
        (addresses[0].machine, addresses[0].session),
        (machine(&one), a)
    );
    assert_eq!(
        (addresses[1].machine, addresses[1].session),
        (machine(&two), b)
    );
    assert_ne!(machine(&one), machine(&two));
    assert!(
        listed[1][0]
            .to_string()
            .starts_with(&format!("two:{}", b.short()))
    );
}

#[test]
fn a_watch_merges_every_machines_list_in_the_sets_order() {
    let ((_one, a), (_two, b), daemons) = pair("daemons-watch");
    let seen: Arc<Mutex<Vec<SessionId>>> = Arc::default();
    let into = Arc::clone(&seen);
    let rt: rt::Rt = Arc::new(rt::Threads::default());
    let _watch = SessionWatch::start(&rt, &daemons, move |list| {
        if let Ok(list) = list {
            *into.lock().unwrap() = list.iter().map(|l| l.summary.id).collect();
        }
    });
    let until = deadline();
    while *seen.lock().unwrap() != [a, b] {
        assert!(Instant::now() < until, "{:?}", seen.lock().unwrap());
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn a_daemon_that_is_not_the_pinned_machine_is_refused() {
    let (env, daemon, _) = started("daemons-pin");
    let other = MachineId([7; 16]);
    let pinned = Daemon {
        pinned: Some(other),
        ..daemon.clone()
    };
    assert_eq!(
        pinned.connect(&|| {}).err(),
        Some(Error::OtherMachine {
            pinned: other,
            found: machine(&env),
        })
    );
    let right = Daemon {
        pinned: Some(machine(&env)),
        ..daemon
    };
    assert!(right.connect(&|| {}).is_ok());
}
