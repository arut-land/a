//! Crash consistency: `kill -9` the daemon at random points while a session
//! is busy. Every entry a client was shown is durable, so it survives in
//! order, and the log never keeps a torn entry. A run prints its seed;
//! `A_CRASH_SEED=<seed>` replays it.

use std::fs;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::common::{Conn, Env, assert_contiguous, wait_for};
use protocol::{BlobRef, Body, Carried, DaemonMsg, Entry, Held, Outcome, SessionId, Target};
use sys::Signal;

const ROUNDS: usize = 12;

/// What a busy session is asked: chat, commands (not safe to rerun), one
/// that runs long enough to be killed mid-call, and a tool that is rerun
/// after a crash.
const INPUTS: &[&str] = &["hello", "$ echo ran", "$ sleep 0.05; echo slept", "sleep 0"];

/// xorshift64*: deterministic, so a failing seed reproduces.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[usize::try_from(self.below(items.len() as u64)).unwrap()]
    }
}

#[test]
fn every_entry_shown_survives_kill_9_at_random_points() {
    let seed = std::env::var("A_CRASH_SEED").map_or_else(
        |_| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
            u64::try_from(now.as_micros()).unwrap()
        },
        |seed| seed.parse::<u64>().unwrap(),
    );
    // xorshift is stuck at zero.
    let seed = seed.max(1);
    eprintln!("A_CRASH_SEED={seed}");
    let mut rng = Rng(seed);
    let env = Env::new("crash");
    // Every kill here is deliberate, not a crash loop.
    env.daemon("crash_limit = 0");
    let mut session = None;
    let mut shown: Vec<Entry> = Vec::new();

    for round in 0..ROUNDS {
        let pid = env.start();
        let (mut conn, id, replay) = attach(&env, session);
        session = Some(id);
        check(&shown, &replay, round, seed);
        shown = replay;

        for _ in 0..=rng.below(3) {
            conn.input(rng.pick(INPUTS));
        }
        // The kill lands at a random point of the turn: the sleep is the
        // choice of point, not a wait.
        let delay = Duration::from_millis(rng.below(200));
        let killer = thread::spawn(move || {
            thread::sleep(delay);
            crate::support::signal_process(pid, Signal::Kill).unwrap();
        });
        // Live entries until the daemon dies under us.
        while let Some(msg) = conn.try_recv() {
            if matches!(msg, DaemonMsg::Head { .. }) {
                shown.extend(conn.certified_entries());
            }
        }
        killer.join().unwrap();
        env.wait_exited("the killed daemon to go");
    }

    env.start();
    let (mut conn, id, replay) = attach(&env, session);
    check(&shown, &replay, ROUNDS, seed);
    // One more turn, so whatever recovery repaired is written out.
    conn.input("done");
    let mut entries = replay;
    while !matches!(entries.last(), Some(Entry { body: Body::Assistant { text }, .. }) if text == "done")
    {
        if matches!(conn.recv(), DaemonMsg::Head { .. }) {
            entries.extend(conn.certified_entries());
        }
    }
    eprintln!("{} entries across {ROUNDS} kills", entries.len());
    assert_contiguous(&entries);
    assert_eq!(env.log(id), entries, "seed {seed}");
}

/// What the daemon shown before a kill had committed is what it replays
/// after: the same entries, in order, with nothing missing or repeated.
fn check(shown: &[Entry], replay: &[Entry], round: usize, seed: u64) {
    assert_contiguous(replay);
    assert!(
        replay.len() >= shown.len() && replay[..shown.len()] == *shown,
        "round {round}, seed {seed}: shown {shown:#?}, replayed {replay:#?}"
    );
}

/// A connection attached to `session` (a new one if `None`), and the replay
/// up to `Synced`.
fn attach(env: &Env, session: Option<SessionId>) -> (Conn, SessionId, Vec<Entry>) {
    let mut conn = env.connect();
    let (id, replay) = conn.attach(session.map_or(Target::New, Target::Id), &env.ws);
    (conn, id, replay)
}

/// A crash cuts off a call gone on in the background and the groups calls
/// left to the session, their streams still open. The first load after
/// ends each: what it wrote so far is sealed into the blob its interrupted
/// outcome or its end's news holds, one empty is discarded, and no stream
/// is left open.
#[test]
fn a_crash_leaves_no_stream_open_and_what_it_cut_off_holds_what_it_wrote() {
    let env = Env::new("crash-streams");
    env.daemon("crash_limit = 0");
    env.ext("shell", "timeout = 1");
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    for command in ["sleep 300 &", "echo job; sleep 300 &"] {
        conn.input(&format!("$ {command}"));
        conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
    }
    conn.input("$ echo start; sleep 300");
    conn.until(|e| matches!(e.body, Body::Detached { .. }));
    let open = env.home.blobs_dir().join("open").join(id.to_string());
    assert_eq!(fs::read_dir(&open).unwrap().count(), 3, "a stream each");
    drop(conn);
    env.kill_daemon();
    // Whatever the crash left running is the test's to stop.
    let groups: Vec<u32> = (env.log(id).iter())
        .filter_map(|e| match e.body {
            Body::ToolProcess { pgid, .. } => Some(pgid),
            _ => None,
        })
        .collect();
    let _stop = Stop(groups);

    env.start();
    let mut conn = env.connect();
    conn.attach(Target::Id(id), &env.ws);
    let queued = || {
        (env.log(id).into_iter())
            .filter_map(|e| match e.body {
                Body::Enqueue { envelope } => Some((envelope.payload().clone(), envelope.held)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    wait_for("all three ends queued", || queued().len() == 3);
    let queued = queued();
    let held = |is: fn(&Carried) -> bool| -> Vec<Vec<u8>> {
        let (_, held) = queued.iter().find(|(payload, _)| is(payload)).unwrap();
        held.iter().map(|held| bytes(&env, held)).collect()
    };
    assert_eq!(
        held(|p| matches!(p, Carried::Detached(Outcome::Interrupted { .. }))),
        [b"start\n"],
        "{queued:#?}"
    );
    let mut jobs: Vec<_> = (queued.iter())
        .filter(|(payload, _)| matches!(payload, Carried::Job(_)))
        .map(|(_, held)| {
            held.iter()
                .map(|held| bytes(&env, held))
                .collect::<Vec<_>>()
        })
        .collect();
    jobs.sort();
    assert_eq!(jobs, [vec![], vec![b"job\n".to_vec()]], "{queued:#?}");
    // The log file shows the news as it is written; each stream is removed
    // only once that news is durable, so it is waited on, not assumed.
    wait_for("nothing left open", || {
        fs::read_dir(&open).unwrap().next().is_none()
    });
}

/// What a sealed blob holds.
fn bytes(env: &Env, held: &Held) -> Vec<u8> {
    let Held::Blob(BlobRef { id, .. }) = held else {
        panic!("not sealed: {held:?}");
    };
    fs::read(env.home.blobs_dir().join(id.fan()).join(id.to_string())).unwrap()
}

/// Kills these process groups as it drops.
struct Stop(Vec<u32>);

impl Drop for Stop {
    fn drop(&mut self) {
        for &pgid in &self.0 {
            let pgid = rustix::process::Pid::from_raw(i32::try_from(pgid).unwrap());
            if let Some(pgid) = pgid {
                let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL);
            }
        }
    }
}
