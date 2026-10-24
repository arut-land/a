#![cfg(feature = "threads")]

use std::pin::pin;
use std::task::{Context, Waker};
use std::thread;
use std::time::{Duration, Instant};

use rt::{Runtime, Threads, block_on, spawn, unblock, wakes};

// The counter is process-wide, so this binary contains only the idle invariant.
#[test]
fn idle_runtime_has_no_periodic_wakeups() {
    let rt = Threads::new(2);
    block_on(spawn(&rt, rt.sleep(Duration::from_millis(5))));
    block_on(unblock(&rt, || ()));
    let mut late = pin!(rt.sleep(Duration::from_secs(30)));
    assert!(
        late.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    // Completion can reach the caller before the worker consumes its last wake.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut before = wakes();
    loop {
        thread::sleep(Duration::from_millis(100));
        let after = wakes();
        if before == after {
            break;
        }
        assert!(Instant::now() < deadline, "runtime never became idle");
        before = after;
    }
    thread::sleep(Duration::from_millis(500));
    assert_eq!(wakes(), before);
}
