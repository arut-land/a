//! CPU workloads over the actual reducer, reply stream and renderers.

use protocol::view::{Item, SessionView, Stream};
use protocol::{Body, CallId, DaemonMsg, Entry, Epoch, Seq, SessionId};
use std::{hint::black_box, time::Instant};
fn entry(seq: u64, body: Body) -> DaemonMsg {
    DaemonMsg::Entry(Entry::new(Seq::new(seq).unwrap(), Epoch(1), body))
}
fn model(seq: u64) -> DaemonMsg {
    entry(
        seq,
        Body::ModelCall {
            request: None,
            model: "fixture".into(),
            attempt: 0,
            choice: protocol::ToolChoice::Auto,
            r#for: None,
            purpose: None,
        },
    )
}
fn delta(call: u64, text: &str) -> DaemonMsg {
    DaemonMsg::Delta {
        call: CallId::new(call).unwrap(),
        attempt: 0,
        text: text.into(),
    }
}
fn assistant(seq: u64) -> Item {
    Item::Assistant {
        seq: Seq::new(seq).unwrap(),
        text: "history".into(),
    }
}
fn main() {
    let a: Vec<_> = std::env::args().collect();
    if a[1] == "memory" {
        memory::run(&a[2..]);
        return;
    }
    let mode = &a[1];
    let history: usize = a[2].parse().unwrap();
    let chunks: usize = a[3].parse().unwrap();
    let chunk = if a.get(4).is_some_and(|s| s == "newline") {
        "plain text with spaces to wrap. \n"
    } else {
        "plain text with spaces to wrap.  "
    };
    let call = history as u64 + 1;
    if mode == "reply" || mode == "fold" {
        let mut v = SessionView::default();
        v.session = Some(SessionId::new());
        v.items = (1..=history as u64).map(assistant).collect();
        let mut stream = client::ReplyStream::default();
        black_box(stream.project(&v));
        v.fold(&model(call));
        let d = delta(call, chunk);
        let t = Instant::now();
        let mut emitted = 0;
        for _ in 0..chunks {
            let update = v.fold(&d);
            if mode == "reply"
                && let Some(update) = update
            {
                for event in stream.update(&v, &update.change) {
                    if let client::ReplyEvent::Text(text) = event {
                        emitted += text.len();
                    }
                    black_box(event);
                }
            }
        }
        if mode == "reply" {
            assert_eq!(emitted, chunks * chunk.len());
        }
        println!(
            "{{\"mode\":\"{}\",\"history\":{},\"chunks\":{},\"ms\":{}}}",
            mode,
            history,
            chunks,
            t.elapsed().as_secs_f64() * 1000.
        );
    } else if mode == "live" {
        let mut live = tui::view::Live::default();
        let mut rows = Vec::new();
        let mut s = Stream {
            call: CallId::new(1).unwrap(),
            attempt: 0,
            text: String::new(),
        };
        let t = Instant::now();
        for _ in 0..chunks {
            s.text.push_str(chunk);
            live.update(&mut rows, &s, 100);
            black_box(&rows);
        }
        println!(
            "{{\"mode\":\"live\",\"chunks\":{},\"newline\":{},\"rows\":{},\"ms\":{}}}",
            chunks,
            chunk.ends_with('\n'),
            rows.len(),
            t.elapsed().as_secs_f64() * 1000.
        );
    } else if mode == "web" {
        let (mut p, _) = page::Page::new(SessionId::new());
        let event = client::Event {
            conn: client::ConnId(1),
            kind: client::EventKind::Connected {
                welcome: client::Welcome {
                    proto: protocol::PROTO,
                    caps: protocol::Caps::ALL,
                },
                remote: false,
                commands: Vec::new(),
                machine: protocol::MachineId([0; 16]),
                upgrade: None,
            },
        };
        let effects = p.client.step(event);
        client::pump(|_| Ok(()), &mut p, effects);
        for seq in 1..=history as u64 {
            web_fold(
                &mut p,
                entry(
                    seq,
                    Body::Assistant {
                        text: "history".into(),
                    },
                ),
            );
        }
        web_fold(
            &mut p,
            DaemonMsg::Synced {
                seq: history as u64,
            },
        );
        p.settle();
        black_box(p.take());
        web_fold(&mut p, model(call));
        p.settle();
        black_box(p.take());
        let d = delta(call, chunk);
        let t = Instant::now();
        for _ in 0..chunks {
            web_fold(&mut p, d.clone());
            p.settle();
            black_box(p.take());
        }
        println!(
            "{{\"mode\":\"web\",\"history\":{},\"chunks\":{},\"ms\":{}}}",
            history,
            chunks,
            t.elapsed().as_secs_f64() * 1000.
        );
    }
}
fn web_fold(p: &mut page::Page, msg: DaemonMsg) {
    let e = client::Event {
        conn: p.client.connection(),
        kind: client::EventKind::Msg(msg),
    };
    let effects = p.client.step(e);
    client::pump(|_| Ok(()), p, effects);
}

#[allow(dead_code)]
#[path = "@SOURCE@/apps/web/src/page.rs"]
mod page;
#[allow(dead_code)]
#[path = "@SOURCE@/apps/web/src/patch.rs"]
mod patch;
#[allow(dead_code)]
#[path = "@SOURCE@/apps/web/src/style.rs"]
mod style;
#[allow(dead_code)]
#[path = "@SOURCE@/apps/web/src/view.rs"]
mod view;
mod http {
    pub const SCRIPT_PATH: &str = "/datastar.js";
}

#[path = "@HARNESS@/memory-probe.rs"]
mod memory;
