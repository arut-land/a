//! Protocol clients for memory workloads; the parent samples at each boundary.

use protocol::{ClientMsg, DaemonMsg, Decode, Target};
use std::collections::BTreeMap;
use std::io::{BufReader, Write};
use std::time::{Duration, Instant};

fn marker(label: &str) {
    println!("{label}");
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
}
pub fn run(args: &[String]) {
    let home = sys::Home::from(std::path::PathBuf::from(&args[0]));
    let mode = &args[1];
    if mode == "shell" {
        let paths = sys::Paths::from(std::path::PathBuf::from(&args[0]));
        println!(
            "{}",
            sys::shell::ExecutionFacts::initialize(&paths)
                .unwrap()
                .tool()
        );
        return;
    }
    let count: usize = args[2].parse().unwrap();
    let first = std::cell::Cell::new(true);
    let connect = || {
        let mut stream = home.endpoint().connect().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        protocol::write_frame(
            &mut stream,
            &ClientMsg::Hello {
                proto: protocol::PROTO,
                build: "memory-survey".into(),
                caps: protocol::Caps::NONE,
                client: None,
                machine: None,
            },
        )
        .unwrap();
        let welcome = protocol::read_frame::<DaemonMsg>(&mut reader)
            .unwrap()
            .unwrap();
        assert!(matches!(welcome, DaemonMsg::Welcome { .. }));
        if first.replace(false) {
            println!(
                "welcome_bytes:{}",
                protocol::Frame::new(&welcome).unwrap().as_bytes().len()
            );
        }
        (stream, reader)
    };
    if mode == "turns" {
        let (mut stream, mut reader) = connect();
        protocol::write_frame(
            &mut stream,
            &ClientMsg::Attach {
                target: Target::New,
                workspace: Some(std::env::current_dir().unwrap().try_into().unwrap()),
                since: 0,
                env: None,
                decl: Default::default(),
            },
        )
        .unwrap();
        loop {
            match protocol::read_frame::<DaemonMsg>(&mut reader)
                .unwrap()
                .unwrap()
            {
                DaemonMsg::Attached { session, .. } => println!("session:{session}"),
                DaemonMsg::Synced { .. } => break,
                _ => {}
            }
        }
        let mut heads = protocol::history::Heads::default();
        let mut head = None;
        let mut pages = BTreeMap::<u64, Vec<u8>>::new();
        let mut request_id = 0;
        marker("before");
        let started = Instant::now();
        for i in 1..=count {
            let text = format!("message {i}: {}", "x".repeat(128));
            let mut answered = false;
            let mut idle = false;
            protocol::write_frame(
                &mut stream,
                &ClientMsg::Input {
                    segments: text.clone().into(),
                    mode: Default::default(),
                    force: false,
                    client_seq: None,
                },
            )
            .unwrap();
            loop {
                let message = protocol::read_frame::<DaemonMsg>(&mut reader)
                    .unwrap()
                    .unwrap();
                if let Some(next) = public_head(&message, &mut heads) {
                    request_id += 1;
                    pages.insert(request_id, Vec::new());
                    protocol::write_frame(
                        &mut stream,
                        &ClientMsg::History {
                            id: request_id,
                            range: protocol::history::Request {
                                cut: next.cut,
                                anchor: next.count.saturating_sub(1),
                                text_offset: None,
                                direction: protocol::history::Direction::After,
                                bytes: usize::MAX,
                                filter: protocol::history::Filter::Transcript,
                            },
                        },
                    )
                    .unwrap();
                    head = Some(next);
                    idle = false;
                }
                match message {
                    DaemonMsg::HistoryPart {
                        id,
                        offset,
                        text: part,
                        done,
                    } => {
                        let bytes = pages.get_mut(&id).expect("requested range");
                        assert_eq!(offset, bytes.len() as u64);
                        bytes.extend_from_slice(part.as_bytes());
                        if done {
                            let page =
                                protocol::history::Page::from_json(&pages.remove(&id).unwrap())
                                    .unwrap();
                            answered |= page.items.iter().any(|located| match &located.item {
                                protocol::view::Item::Assistant { text: reply, .. }
                                | protocol::view::Item::Reply { text: reply, .. } => *reply == text,
                                _ => false,
                            });
                        }
                    }
                    DaemonMsg::Activity {
                        seq,
                        activity: protocol::Activity::Idle,
                    } if head.as_ref().is_some_and(|head| head.cut.seq == seq) => idle = true,
                    DaemonMsg::Error { message } => panic!("{message}"),
                    DaemonMsg::Entry(_) | DaemonMsg::Snapshot(_) => {
                        panic!("private history on the public stream")
                    }
                    _ => {}
                }
                if answered && idle {
                    break;
                }
            }
            if i == count {
                println!("elapsed_us:{}", started.elapsed().as_micros());
                marker(&i.to_string());
            }
        }
        protocol::write_frame(&mut stream, &ClientMsg::Detach).unwrap();
        while protocol::read_frame::<DaemonMsg>(&mut reader)
            .unwrap()
            .is_some()
        {}
        marker("detached");
        return;
    }
    let mut held = Vec::new();
    let mut session = None;
    marker("before");
    let started = Instant::now();
    for i in 1..=count {
        let (mut stream, mut reader) = connect();
        if mode == "hello" {
        } else if mode == "watch" || mode == "list" {
            protocol::write_frame(
                &mut stream,
                &if mode == "watch" {
                    ClientMsg::WatchSessions
                } else {
                    ClientMsg::List
                },
            )
            .unwrap();
            assert!(matches!(
                protocol::read_frame(&mut reader).unwrap(),
                Some(DaemonMsg::Sessions { .. })
            ));
        } else {
            let target = if mode == "attach" || mode == "clients" || mode == "bare" {
                session.map_or(
                    if mode == "bare" {
                        Target::New
                    } else {
                        Target::Latest {
                            any_workspace: true,
                        }
                    },
                    Target::Id,
                )
            } else {
                Target::New
            };
            protocol::write_frame(
                &mut stream,
                &ClientMsg::Attach {
                    target,
                    workspace: Some(std::env::current_dir().unwrap().try_into().unwrap()),
                    since: 0,
                    env: None,
                    decl: Default::default(),
                },
            )
            .unwrap();
            let mut replay_bytes = 0;
            let mut largest = 0;
            loop {
                match protocol::read_frame::<DaemonMsg>(&mut reader)
                    .unwrap()
                    .unwrap()
                {
                    message @ (DaemonMsg::Head { .. } | DaemonMsg::HeadPart { .. }) => {
                        let bytes = protocol::Frame::new(&message).unwrap().as_bytes().len();
                        replay_bytes += bytes;
                        largest = largest.max(bytes);
                    }
                    DaemonMsg::Attached { session: id, .. } => session = Some(id),
                    DaemonMsg::Synced { .. } => break,
                    DaemonMsg::Error { message } => panic!("{message}"),
                    _ => {}
                }
            }
            if i == 1 {
                println!("replay_bytes:{replay_bytes}:{largest}");
            }
        }
        if i == 1 && mode != "hello" && mode != "watch" && mode != "list" {
            println!("replay_done");
        }
        if mode == "clients" || mode == "hello" || mode == "bare" {
            held.push((stream, reader));
        } else {
            protocol::write_frame(&mut stream, &ClientMsg::Detach).unwrap();
            while protocol::read_frame::<DaemonMsg>(&mut reader)
                .unwrap()
                .is_some()
            {}
        }
        if i == count {
            println!("elapsed_us:{}", started.elapsed().as_micros());
            marker(&i.to_string());
        }
    }
    drop(held);
    marker("detached");
}

fn public_head(
    message: &DaemonMsg,
    parts: &mut protocol::history::Heads,
) -> Option<protocol::history::Head> {
    match message {
        DaemonMsg::Head { head } => Some(head.as_ref().clone()),
        DaemonMsg::HeadPart {
            seq,
            offset,
            text,
            done,
        } => parts.part(*seq, *offset, text, *done).unwrap(),
        _ => None,
    }
}
