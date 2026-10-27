//! Protocol clients for memory workloads; the parent samples at each boundary.

use protocol::{ClientMsg, DaemonMsg, Target};
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
        marker("before");
        let started = Instant::now();
        for i in 1..=count {
            let text = format!("message {i}: {}", "x".repeat(128));
            let mut answered = false;
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
                match protocol::read_frame::<DaemonMsg>(&mut reader)
                    .unwrap()
                    .unwrap()
                {
                    DaemonMsg::Entry(entry) => {
                        if let protocol::Body::Assistant { text: reply } = entry.body {
                            answered |= reply == text;
                        }
                    }
                    DaemonMsg::Activity {
                        activity: protocol::Activity::Idle,
                        ..
                    } if answered => break,
                    DaemonMsg::Error { message } => panic!("{message}"),
                    _ => {}
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
                    DaemonMsg::Entry(entry) => {
                        let bytes = protocol::Frame::entry(&entry).unwrap().as_bytes().len();
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
