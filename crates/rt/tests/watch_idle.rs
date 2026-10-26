#![cfg(all(feature = "threads", feature = "watch"))]

use std::fs;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use rt::watch::{Event, Hub, Watch};
use rt::{Threads, wakes};

// The wake counter belongs to this process, so this binary has only one test.
#[test]
fn an_idle_subscription_never_wakes_the_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("root");
    fs::create_dir(&root).unwrap();
    let hub = Hub::new(Arc::new(Threads::new(1)));
    let (send, events) = mpsc::channel();
    let _sub = hub.subscribe(Watch::tree(&root), move |batch| {
        let _ = send.send(batch);
    });
    let file = root.join("file");
    fs::write(&file, "changed").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let batch = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if batch
            .iter()
            .any(|e| matches!(e, Event::Changed(paths) if paths.contains(&file)))
        {
            break;
        }
    }
    let mut before = wakes();
    loop {
        thread::sleep(Duration::from_millis(100));
        let after = wakes();
        if before == after {
            break;
        }
        assert!(Instant::now() < deadline, "watcher never became idle");
        before = after;
    }
    thread::sleep(Duration::from_secs(1));
    assert_eq!(wakes(), before);
}
