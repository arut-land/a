//! Time, allocations and heap of reading a session's log as a client does
//! at attach: every entry decoded, then folded into its view; and of
//! encoding it again, as the daemon does for each entry it sends. Ignored by
//! default: `A_SESSION_LOG=<log> cargo test -p protocol --release --test
//! bench -- --ignored --nocapture`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use protocol::view::SessionView;
use protocol::{Body, Decode, Entry, SessionId};

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);

// Only this test binary counts: the allocator defers to the system's.
#[allow(unsafe_code)]
// SAFETY: every method forwards to `System`, which upholds the contract.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        LIVE.fetch_add(layout.size(), Relaxed);
        // SAFETY: the caller upholds `System.alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        // SAFETY: the caller upholds `System.dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        LIVE.fetch_add(new, Relaxed);
        LIVE.fetch_sub(layout.size(), Relaxed);
        // SAFETY: the caller upholds `System.realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Runs `f` `runs` times and prints its fastest time and its allocations
/// per run.
fn measure(label: &str, runs: usize, mut f: impl FnMut()) {
    f();
    let allocs = ALLOCS.load(Relaxed);
    let mut fastest = std::time::Duration::MAX;
    for _ in 0..runs {
        let start = Instant::now();
        f();
        fastest = fastest.min(start.elapsed());
    }
    let allocs = (ALLOCS.load(Relaxed) - allocs) / runs.max(1);
    eprintln!("{label}: {fastest:?}/run, {allocs} allocs/run");
}

/// The log at `A_SESSION_LOG`, its header skipped.
fn log() -> Option<Vec<String>> {
    let text = std::fs::read_to_string(std::env::var_os("A_SESSION_LOG")?).expect("a log");
    Some(text.lines().skip(1).map(str::to_owned).collect())
}

#[test]
#[ignore = "a benchmark"]
fn attach_to_a_logged_session() {
    let Some(lines) = log() else {
        return;
    };
    let entries: Vec<Entry> = lines
        .iter()
        .map(|line| Entry::from_json(line.as_bytes()).expect("every benchmark entry decodes"))
        .collect();
    assert!(!entries.is_empty(), "the benchmark needs a nonempty log");
    let replies = entries
        .iter()
        .filter(|e| matches!(e.body, Body::ModelResult { .. }))
        .count();
    eprintln!(
        "{} of {} entries decode, {replies} replies",
        entries.len(),
        lines.len()
    );
    measure("decode the log", 10, || {
        for line in &lines {
            let _ = std::hint::black_box(Entry::from_json(line.as_bytes()));
        }
    });
    let mut out = Vec::new();
    measure("encode the log", 10, || {
        for entry in &entries {
            out.clear();
            entry.encode(&mut out).unwrap();
            std::hint::black_box(&out);
        }
    });
    let fold = || SessionView::from_log(SessionId::new(), PathBuf::from("/w"), &entries);
    measure("fold the view", 10, || {
        std::hint::black_box(fold());
    });
    let base = LIVE.load(Relaxed);
    let view = fold();
    let held = LIVE.load(Relaxed) - base;
    eprintln!("the view holds {} KiB", held / 1024);
    drop(view);
    let base = LIVE.load(Relaxed);
    let decoded: Vec<Entry> = lines
        .iter()
        .filter_map(|line| Entry::from_json(line.as_bytes()).ok())
        .collect();
    let held = LIVE.load(Relaxed) - base;
    eprintln!("the decoded log holds {} KiB", held / 1024);
    drop(decoded);
}
