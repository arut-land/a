//! How a group the session owns is seen to end: through its processes'
//! exit events, not by polling, until none of it is left.

use std::process::Child;
use std::time::Duration;

/// How soon a group whose processes cannot be watched (zombies about to
/// be reaped, or members of another user) is looked at again, at first and
/// at most.
const RECHECK: (Duration, Duration) = (Duration::from_millis(50), Duration::from_secs(5));

/// Resolves once no process of group `pgid` is left, reaping `leader` (the
/// child of ours that leads it, if it is one) once it exits.
pub(crate) async fn ended(pgid: u32, mut leader: Option<Child>) {
    let mut recheck = RECHECK.0;
    loop {
        if let Some(child) = &mut leader
            && !matches!(child.try_wait(), Ok(None))
        {
            leader = None;
        }
        if !sys::group_alive(pgid) {
            return;
        }
        if any_exit(pgid).await {
            recheck = RECHECK.0;
        } else {
            rt::Timer::after(recheck).await;
            recheck = (recheck * 2).min(RECHECK.1);
        }
    }
}

/// Waits for any live process of group `pgid` to exit; `false`, at once,
/// when none can be watched.
#[cfg(unix)]
async fn any_exit(pgid: u32) -> bool {
    use std::future::poll_fn;
    use std::task::Poll;

    let watches: Vec<_> = sys::group_members(pgid)
        .into_iter()
        .filter_map(|pid| rt::Async::for_reading(sys::exit_watch(pid).ok()?).ok())
        .collect();
    if watches.is_empty() {
        return false;
    }
    let mut exits: Vec<_> = watches.iter().map(|w| Box::pin(w.readable())).collect();
    poll_fn(|cx| {
        if exits
            .iter_mut()
            .any(|exit| exit.as_mut().poll(cx).is_ready())
        {
            Poll::Ready(true)
        } else {
            Poll::Pending
        }
    })
    .await
}

#[cfg(not(unix))]
fn any_exit(_pgid: u32) -> std::future::Ready<bool> {
    std::future::ready(false)
}
