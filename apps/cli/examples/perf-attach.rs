//! Native protocol timing without a shell or a process launch in each sample.
use std::io::BufReader;
use std::time::{Duration, Instant};

use protocol::{ClientMsg, DaemonMsg, Decode, Entry, Target};

fn attach(home: &sys::Home, target: Target) -> (protocol::SessionId, Vec<Entry>, u64) {
    let mut stream = home.endpoint().connect().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    protocol::write_frame(
        &mut stream,
        &ClientMsg::Hello {
            proto: protocol::PROTO,
            build: "perf".into(),
            min: None,
            caps: protocol::Caps::NONE,
            client: None,
        },
    )
    .unwrap();
    assert!(matches!(
        protocol::read_frame(&mut reader).unwrap(),
        Some(DaemonMsg::Welcome { .. })
    ));
    protocol::write_frame(
        &mut stream,
        &ClientMsg::Attach {
            target,
            workspace: std::env::current_dir().unwrap(),
            since: 0,
            env: None,
            decl: protocol::ClientDecl::default(),
        },
    )
    .unwrap();
    let Some(DaemonMsg::Attached { session, .. }) = protocol::read_frame(&mut reader).unwrap()
    else {
        panic!("expected attached");
    };
    let mut replay = Vec::new();
    loop {
        match protocol::read_frame::<DaemonMsg>(&mut reader)
            .unwrap()
            .expect("daemon closed")
        {
            DaemonMsg::Entry(entry) => replay.push(entry),
            DaemonMsg::Activity { .. } => {}
            DaemonMsg::Synced { seq } => return (session, replay, seq),
            message => panic!("unexpected replay message: {message:?}"),
        }
    }
}

fn main() {
    let home = sys::Home::from(std::path::PathBuf::from(
        std::env::var_os("A_HOME").expect("isolated A_HOME"),
    ));
    let (id, replay, seq) = attach(
        &home,
        Target::Prefix(std::env::args().nth(1).expect("session id")),
    );
    // The persisted log is independent of the replay path under measurement.
    let text = std::fs::read_to_string(home.sessions_dir().join(id.to_string()).join("log.jsonl"))
        .unwrap();
    assert!(text.ends_with('\n'), "torn session log");
    let expected: Vec<_> = text
        .lines()
        .skip(1)
        .map(|line| Entry::from_json(line.as_bytes()).unwrap())
        .collect();
    assert!(!expected.is_empty(), "attach workload must have history");
    verify_replay(&expected, &replay, seq);
    let times: Vec<_> = (0..500)
        .map(|_| {
            let start = Instant::now();
            let (session, replay, seq) = attach(&home, Target::Id(id));
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(session, id);
            verify_replay(&expected, &replay, seq);
            elapsed
        })
        .collect();
    println!("{}", serde_json::to_string(&times).unwrap());
}

fn verify_replay(expected: &[Entry], replay: &[Entry], seq: u64) {
    assert_eq!(
        replay, expected,
        "attach must replay the complete persisted log"
    );
    assert_eq!(
        seq,
        expected.last().unwrap().seq.get(),
        "synced must cover the replay"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_changed_or_prematurely_synced_replays_are_never_samples() {
        let expected: Vec<_> = (1..=2)
            .map(|seq| {
                Entry::new(
                    protocol::Seq::new(seq).unwrap(),
                    protocol::Epoch(1),
                    protocol::Body::Input {
                        segments: format!("message {seq}").into(),
                        mode: protocol::Mode::Queue,
                        taken: true,
                        origin: None,
                    },
                )
            })
            .collect();
        verify_replay(&expected, &expected, 2);
        let mut changed = expected.clone();
        changed[0].body = expected[1].body.clone();
        for (replay, seq) in [(&expected[..1], 2), (&changed[..], 2), (&expected[..], 1)] {
            assert!(std::panic::catch_unwind(|| verify_replay(&expected, replay, seq)).is_err());
        }
    }
}
