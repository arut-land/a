#![cfg(feature = "threads")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rt::{BlockingClass, Config, Threads, block_on, try_unblock_with};

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
