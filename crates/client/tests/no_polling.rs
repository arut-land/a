//! A client never polls for its daemon. Against a listener that never
//! accepts, the connection completes in the backlog: `connect` gets in on
//! its first try, sends its hello, and blocks in one read until the hello
//! times out, using no CPU meanwhile. Unlike the strace test in apps/cli,
//! this runs on every Unix, macOS included. Its own test binary, so the CPU
//! it measures is its own.
#![cfg(unix)]

use std::process::Command;
use std::time::{Duration, Instant};
use std::{fs, process};

use client::{Daemon, Error, HELLO_TIMEOUT};
use protocol::{ClientMsg, read_frame};
use sys::Home;

#[test]
fn a_daemon_that_never_accepts_is_waited_on_without_spinning() {
    let root = std::env::temp_dir().join(format!("a-client-no-polling-{}", process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let home = Home::from(root.join("home"));
    let exe = root.join("a");
    fs::write(&exe, "a build that is never run").unwrap();
    let staged = home.runtime().stage(&exe).unwrap();
    let listener = home.bind().unwrap();

    let (start, cpu) = (Instant::now(), cpu_time());
    let err = Daemon::local(home.clone(), staged)
        .connect(&|| {})
        .err()
        .expect("nothing answers the hello");
    let (waited, used) = (start.elapsed(), cpu_time().saturating_sub(cpu));
    eprintln!("waited {waited:?} for the hello, using {used:?} of CPU");

    assert_eq!(err, Error::NotResponding);
    // One wait, not a retry loop or a spawn.
    assert!(
        waited >= HELLO_TIMEOUT && waited < HELLO_TIMEOUT + Duration::from_millis(500),
        "{waited:?}"
    );
    assert!(used <= Duration::from_millis(20), "{used:?} of CPU");

    // Exactly one connection is in the backlog, its hello already sent.
    listener.set_nonblocking(true).unwrap();
    let mut conn = listener.accept().unwrap();
    conn.set_nonblocking(false).unwrap();
    let hello = read_frame::<ClientMsg>(&mut conn).unwrap();
    assert!(matches!(hello, Some(ClientMsg::Hello { .. })), "{hello:?}");
    assert_eq!(
        listener.accept().err().map(|e| e.kind()),
        Some(std::io::ErrorKind::WouldBlock)
    );
    home.endpoint().remove();
    fs::remove_dir_all(&root).unwrap();
}

/// This process's user and system time so far.
fn cpu_time() -> Duration {
    if let Ok(stat) = fs::read_to_string("/proc/self/stat") {
        // utime and stime, in clock ticks (100 a second), after the
        // parenthesized command name.
        let fields: Vec<&str> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        let ticks = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
        return Duration::from_millis(ticks * 10);
    }
    // `ps` elsewhere: M:SS.cc.
    let out = Command::new("ps")
        .args(["-o", "time=", "-p", &process::id().to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let (min, sec) = text.trim().split_once(':').unwrap();
    Duration::from_secs_f64(min.parse::<f64>().unwrap() * 60.0 + sec.parse::<f64>().unwrap())
}
