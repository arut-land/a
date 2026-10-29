//! Public control-attach timing; committed history is requested separately.
use std::io::BufReader;
use std::time::{Duration, Instant};

use protocol::history::{Head, Heads};
use protocol::{ClientMsg, DaemonMsg, Decode, Entry, Target};

fn attach(home: &sys::Home, target: Target) -> (protocol::HostPath, Head, u64) {
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
            caps: protocol::Caps::NONE,
            client: None,
            machine: None,
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
            workspace: Some(std::env::current_dir().unwrap().try_into().unwrap()),
            since: 0,
            env: None,
            decl: protocol::ClientDecl::default(),
        },
    )
    .unwrap();
    let Some(DaemonMsg::Attached { workspace, .. }) = protocol::read_frame(&mut reader).unwrap()
    else {
        panic!("expected attached");
    };
    let mut head = None;
    let mut parts = Heads::default();
    loop {
        match protocol::read_frame::<DaemonMsg>(&mut reader)
            .unwrap()
            .expect("daemon closed")
        {
            DaemonMsg::Head { head: received } => head = Some(*received),
            DaemonMsg::HeadPart {
                seq,
                offset,
                text,
                done,
            } => {
                if let Some(received) = parts.part(seq, offset, &text, done).unwrap() {
                    head = Some(received);
                }
            }
            DaemonMsg::Activity { .. } => {}
            DaemonMsg::Synced { seq } => {
                return (workspace, head.expect("attach supplies controls"), seq);
            }
            message => panic!("unexpected replay message: {message:?}"),
        }
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--memory") {
        let pid = std::env::args()
            .nth(2)
            .expect("process id")
            .parse()
            .unwrap();
        println!(
            "{}",
            serde_json::json!({
                "charged_bytes": sys::process_memory::charged_bytes(pid).unwrap(),
                "metric": sys::process_memory::METRIC,
            })
        );
        return;
    }
    let home = sys::Home::from(std::path::PathBuf::from(
        std::env::var_os("A_HOME").expect("isolated A_HOME"),
    ));
    let (workspace, head, seq) = attach(
        &home,
        Target::Prefix(std::env::args().nth(1).expect("session id")),
    );
    let id = head.cut.session;
    // The persisted log is independent of the control path under measurement.
    let text = std::fs::read_to_string(home.sessions_dir().join(id.to_string()).join("log.jsonl"))
        .unwrap();
    assert!(text.ends_with('\n'), "torn session log");
    let expected: Vec<_> = text
        .lines()
        .skip(1)
        .map(|line| Entry::from_json(line.as_bytes()).unwrap())
        .collect();
    assert!(!expected.is_empty(), "attach workload must have history");
    let mut index = protocol::view::Index::new(id, workspace);
    for entry in &expected {
        index.apply(entry);
    }
    let expected = index.head(head.cut.generation);
    verify_head(&expected, &head, seq);
    let times: Vec<_> = (0..500)
        .map(|_| {
            let start = Instant::now();
            let (_, head, seq) = attach(&home, Target::Id(id));
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(head.cut.session, id);
            verify_head(&expected, &head, seq);
            elapsed
        })
        .collect();
    println!("{}", serde_json::to_string(&times).unwrap());
}

fn verify_head(expected: &Head, head: &Head, seq: u64) {
    assert_eq!(
        head, expected,
        "attach must supply complete public controls"
    );
    assert_eq!(seq, head.cut.seq, "synced must certify the control cut");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_or_prematurely_synced_controls_are_never_samples() {
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
                        force: false,
                    },
                )
            })
            .collect();
        let mut index = protocol::view::Index::new(protocol::SessionId::new(), "/work".into());
        for entry in &expected {
            index.apply(entry);
        }
        let expected = index.head(protocol::Epoch(1));
        verify_head(&expected, &expected, 2);
        let mut changed = expected.clone();
        changed.count = 0;
        for (head, seq) in [(&changed, 2), (&expected, 1)] {
            assert!(std::panic::catch_unwind(|| verify_head(&expected, head, seq)).is_err());
        }
    }
}
