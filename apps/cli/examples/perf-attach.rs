//! Native protocol timing without a shell or a process launch in each sample.
use std::io::BufReader;
use std::time::{Duration, Instant};

use protocol::{ClientMsg, DaemonMsg, Target};

fn attach(home: &sys::Home, target: Target) -> protocol::SessionId {
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
    loop {
        if matches!(
            protocol::read_frame::<DaemonMsg>(&mut reader)
                .unwrap()
                .expect("daemon closed"),
            DaemonMsg::Synced { .. }
        ) {
            return session;
        }
    }
}

fn main() {
    let home = sys::Home::from(std::path::PathBuf::from(
        std::env::var_os("A_HOME").expect("isolated A_HOME"),
    ));
    let id = attach(
        &home,
        Target::Prefix(std::env::args().nth(1).expect("session id")),
    );
    let times: Vec<_> = (0..500)
        .map(|_| {
            let start = Instant::now();
            assert_eq!(attach(&home, Target::Id(id)), id);
            start.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    println!("{}", serde_json::to_string(&times).unwrap());
}
