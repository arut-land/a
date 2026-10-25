//! Fetching blob bytes through the client SDK against the built daemon:
//! the whole output a shell call kept, as the session's log references it.

use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

use client::{Daemon, Unfetched};
use protocol::{BlobId, Decode, Entry, Held, SessionId};

use crate::support::Isolated;

/// A daemon whose one session ran `lines` lines of output through the
/// shell, which kept them whole in a blob: the session, and the blob's
/// bytes as the store holds them.
fn kept(name: &str, lines: u64) -> (Isolated, Daemon, SessionId, BlobId, Vec<u8>) {
    let env = Isolated::new(name);
    let command = if sys::find_bash(std::env::var("PATH").ok().as_deref()).is_some() {
        format!("$ seq 1 {lines}")
    } else {
        format!("$ [Console]::Out.Write(((1..{lines}) -join \"`n\") + \"`n\")")
    };
    env.run(&["--mode", "auto", "-p", &command]);
    let (_, log) = env.logs().into_iter().next().expect("a session");
    let blob = log
        .split(|&b| b == b'\n')
        // Past the log's header.
        .skip(1)
        .filter(|line| !line.is_empty())
        .flat_map(|line| Entry::from_json(line).unwrap().body.blobs())
        .find_map(|held| match held {
            Held::Blob(blob) => Some(blob.id),
            Held::Stream(_) => None,
        })
        .expect("the shell kept its output whole");
    let path = env.home.blobs_dir().join(blob.fan()).join(blob.to_string());
    let bytes = fs::read(path).unwrap();
    let staged = (env.home.runtime())
        .stage(Path::new(env!("CARGO_BIN_EXE_a")))
        .unwrap();
    let daemon = Daemon::local(env.home.clone(), staged);
    let session = daemon.sessions().unwrap().remove(0).id;
    (env, daemon, session, blob, bytes)
}

fn read_all(mut fetched: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fetched.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[test]
fn a_client_streams_a_held_blob_byte_identical() {
    let (_env, daemon, session, blob, stored) = kept("fetch-whole", 200_000);
    // Many chunks' worth.
    assert!(stored.len() > 1 << 20, "{} bytes", stored.len());
    let fetched = read_all(daemon.fetch(session, blob, ..).unwrap()).unwrap();
    assert!(
        fetched == stored,
        "the fetched bytes differ from the stored"
    );
}

#[test]
fn a_range_read_returns_exactly_that_range() {
    let (_env, daemon, session, blob, stored) = kept("fetch-range", 200_000);
    let len = stored.len() as u64;
    let range = |range: std::ops::Range<u64>| {
        read_all(daemon.fetch(session, blob, range).unwrap()).unwrap()
    };
    assert_eq!(range(0..16), stored[..16]);
    // Unaligned, across chunk boundaries.
    assert!(range(1001..700_001) == stored[1001..700_001]);
    // Clamped to the blob's end, and past it nothing.
    assert_eq!(range(len - 5..len + 100), stored[stored.len() - 5..]);
    assert_eq!(range(len + 1..len + 2), b"");
    let tail = read_all(daemon.fetch(session, blob, len - 3..).unwrap()).unwrap();
    assert_eq!(tail, stored[stored.len() - 3..]);
}

#[test]
fn a_blob_no_reachable_session_references_is_refused() {
    let (env, daemon, session, blob, stored) = kept("fetch-refused", 3_000);
    env.run(&["-p", "another"]);
    let other = (daemon.sessions().unwrap().into_iter())
        .find(|s| s.id != session)
        .unwrap()
        .id;
    let refused = |session, blob| {
        let error = read_all(daemon.fetch(session, blob, ..).unwrap()).unwrap_err();
        assert_eq!(Unfetched::of(&error), Some(Unfetched::NotHeld), "{error}");
    };
    // Held by a session, but not this one: the hash alone opens nothing.
    refused(other, blob);
    // Referenced by nothing.
    refused(session, BlobId([0x5a; 32]));
    refused(SessionId::new(), blob);
    // The session that holds it still serves it.
    let head = read_all(daemon.fetch(session, blob, ..16).unwrap()).unwrap();
    assert_eq!(head, stored[..16]);
}

#[test]
fn a_slow_client_paces_the_stream_without_disconnect() {
    // More than a connection's 32 MiB outbox holds: unpaced, the daemon
    // would close the connection on it.
    let (_env, daemon, session, blob, stored) = kept("fetch-slow", 5_000_000);
    assert!(stored.len() > 36 << 20, "{} bytes", stored.len());
    let mut fetched = daemon.fetch(session, blob, ..).unwrap();
    let mut first = [0; 1];
    fetched.read_exact(&mut first).unwrap();
    // The daemon fills what the socket and the outbox hold, then waits.
    std::thread::sleep(Duration::from_secs(3));
    let mut bytes = first.to_vec();
    fetched.read_to_end(&mut bytes).unwrap();
    assert!(bytes == stored, "the paced stream differs from the stored");
}

#[test]
fn a_cancelled_fetch_ends_cancelled_short_of_its_bytes() {
    let (_env, daemon, session, blob, stored) = kept("fetch-cancel", 2_000_000);
    let mut fetched = daemon.fetch(session, blob, ..).unwrap();
    let mut first = [0; 1];
    fetched.read_exact(&mut first).unwrap();
    fetched.canceller().cancel().unwrap();
    let mut bytes = first.to_vec();
    let error = fetched.read_to_end(&mut bytes).unwrap_err();
    assert_eq!(Unfetched::of(&error), Some(Unfetched::Cancelled), "{error}");
    assert!(bytes.len() < stored.len(), "every byte came");
    assert!(stored.starts_with(&bytes));
}
