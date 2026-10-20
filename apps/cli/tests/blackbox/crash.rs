//! Crash consistency: `kill -9` the daemon at random points while a session
//! is busy. Every entry a client was shown is durable, so it survives in
//! order, and the log never keeps a torn entry. A run prints its seed;
//! `A_CRASH_SEED=<seed>` replays it.

use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::common::{Conn, Env, assert_contiguous};
use protocol::{Body, DaemonMsg, Entry, SessionId, Target};
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
            sys::signal_process(pid, Signal::Kill).unwrap();
        });
        // Live entries until the daemon dies under us.
        while let Some(msg) = conn.try_recv() {
            if let DaemonMsg::Entry(entry) = msg {
                shown.push(entry);
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
        if let DaemonMsg::Entry(entry) = conn.recv() {
            entries.push(entry);
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
