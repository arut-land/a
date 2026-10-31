#![cfg(feature = "threads")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rt::{BlockingClass, Config, Runtime, Threads, block_on, spawn_maintenance, try_unblock_with};

#[test]
fn refused_blocking_work_returns_an_error_without_running() {
    let mut config = Config::for_cores(1);
    config.cpu.bytes = 1;
    let rt = Threads::configured(config);
    let ran = Arc::new(AtomicUsize::new(0));
    let captured = ran.clone();
    let error = block_on(try_unblock_with(&rt, BlockingClass::Cpu, 2, move || {
        captured.fetch_add(1, Ordering::SeqCst);
    }))
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert_eq!(
        block_on(try_unblock_with(&rt, BlockingClass::Cpu, 1, || 42)).unwrap(),
        42
    );
}

#[test]
fn running_maintenance_never_occupies_the_foreground_cpu_worker() {
    let rt = Threads::configured(Config::for_cores(1));
    let (entered, running) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel();
    spawn_maintenance(Box::new(move || {
        entered.send(()).unwrap();
        held.recv().unwrap();
    }))
    .unwrap();
    running.recv_timeout(Duration::from_secs(5)).unwrap();
    let (finished, foreground) = std::sync::mpsc::channel();
    block_on(rt.spawn_blocking(
        BlockingClass::Cpu,
        0,
        Box::new(move || finished.send(()).unwrap()),
    ))
    .unwrap();
    let result = foreground.recv_timeout(Duration::from_secs(5));
    release.send(()).unwrap();
    result.expect("foreground work must finish while maintenance is still blocked");
}
