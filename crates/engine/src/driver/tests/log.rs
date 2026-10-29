//! The log as a session keeps it: the lines of its file, read back by seq
//! when a client comes to them, never held in the daemon's memory nor
//! encoded again.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fs;

use super::*;
use crate::store::{FORMAT, seal};

/// The system allocator, counting the bytes this thread holds and the most
/// it has held.
struct Counting;

thread_local! {
    static HELD: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn held(change: isize) {
    let _ = HELD.try_with(|held| {
        held.set(held.get() + change);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(held.get())));
    });
}

// Only this test binary counts: the allocator defers to the system's.
#[allow(unsafe_code)]
// SAFETY: every method forwards to `System`, which upholds the contract.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        held(layout.size().cast_signed());
        // SAFETY: the caller upholds `System.alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        held(-layout.size().cast_signed());
        // SAFETY: the caller upholds `System.dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        held(size.cast_signed() - layout.size().cast_signed());
        // SAFETY: the caller upholds `System.realloc`'s contract.
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// Sessions logged as JSONL files in a directory of their own, removed
/// once the test is done.
struct OnDisk {
    fixture: Fixture,
    dir: PathBuf,
}

impl Drop for OnDisk {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn on_disk(name: &str) -> OnDisk {
    let dir = std::env::temp_dir().join(format!("a-engine-log-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let store = Arc::new(crate::store::Jsonl::open(&dir).unwrap());
    let fixture = Fixture::on(
        store,
        ext_fixtures::ECHO,
        Registry::default(),
        openai(&Script::new(&[])),
    );
    OnDisk { fixture, dir }
}

impl OnDisk {
    /// A new session whose file holds `lines` after its header.
    fn write(&self, lines: &[Vec<u8>]) -> Header {
        let header = Header {
            id: SessionId::new(),
            workspace: std::env::temp_dir(),
            parent: None,
        };
        let fields = serde_json::to_string(&header).unwrap();
        let mut file = format!(r#"{{"format":{FORMAT},{}"#, &fields[1..]).into_bytes();
        seal(&mut file, 0);
        for line in lines {
            let at = file.len();
            file.extend_from_slice(line);
            seal(&mut file, at);
        }
        fs::create_dir_all(self.path(header.id).parent().unwrap()).unwrap();
        fs::write(self.path(header.id), file).unwrap();
        header
    }

    fn load(&self, header: &Header) -> Arc<Live> {
        Live::start(header, &self.fixture.0, Open::Load, Owner::None).unwrap()
    }

    fn path(&self, id: SessionId) -> PathBuf {
        self.dir.join(format!("{id}/log.jsonl"))
    }
}

/// `n` turns' worth of entries, the user's and the answer's in turn, each
/// with `len` bytes of text.
fn turns(n: u64, len: usize) -> Vec<Entry> {
    (1..=n)
        .map(|seq| {
            let text = format!("{seq:0len$}");
            let body = if seq % 2 == 1 {
                user(&text)
            } else {
                Body::Assistant { text }
            };
            entry(seq, body)
        })
        .collect()
}

fn line(e: &Entry) -> Vec<u8> {
    let mut line = Vec::new();
    e.encode(&mut line).unwrap();
    line
}

/// The frames a client attaching after `since` is sent.
fn sent(session: &Live, since: u64) -> Vec<Frame> {
    let outbox = Outbox::new();
    session.follow(&outbox, since);
    std::iter::from_fn(|| outbox.try_next()).collect()
}

#[test]
fn attach_contains_only_control_state_and_a_seekable_cut() {
    let disk = on_disk("paging");
    let session = disk.load(&disk.write(&turns(300, 1024).iter().map(line).collect::<Vec<_>>()));
    settle(&session);
    for since in [0, 172, 300] {
        let frames = sent(&session, since);
        assert_eq!(frames.len(), 4);
        let mut view = SessionView::default();
        for frame in frames {
            view.fold(
                &protocol::read_frame::<DaemonMsg>(&mut frame.as_bytes())
                    .unwrap()
                    .unwrap(),
            );
        }
        assert!(view.items.is_empty());
        let position = view.history.unwrap();
        assert_eq!(position.count, 300);
        assert_eq!(position.cut.seq, 300);
        let page = session
            .frames
            .history
            .page(
                &session.frames,
                &protocol::history::Request {
                    cut: position.cut,
                    anchor: 0,
                    text_offset: None,
                    direction: protocol::history::Direction::After,
                    bytes: usize::MAX,
                    filter: protocol::history::Filter::Transcript,
                },
            )
            .unwrap();
        let canonical =
            SessionView::from_log(session.id, "/w".into(), session.frames.entries().unwrap());
        assert_eq!(
            page.items
                .into_iter()
                .map(|row| row.item)
                .collect::<Vec<_>>(),
            canonical.items
        );
    }
    session.halt();
}

/// The bytes loading a session of `n` turns leaves held on this thread,
/// and the most it held meanwhile, past what was held before.
fn held_by_loading(disk: &OnDisk, n: u64) -> (isize, isize) {
    let lines: Vec<Vec<u8>> = turns(n, 1024).iter().map(line).collect();
    let header = disk.write(&lines);
    drop(lines);
    let before = HELD.get();
    PEAK.set(before);
    let session = disk.load(&header);
    let held = (HELD.get() - before, PEAK.get() - before);
    settle(&session);
    session.halt();
    held
}

#[test]
fn a_loaded_sessions_memory_does_not_grow_with_log_length() {
    let disk = on_disk("memory");
    let (short, short_peak) = held_by_loading(&disk, 1_000);
    let (long, long_peak) = held_by_loading(&disk, 15_000);
    // The entries' text alone is 1 KiB each: 14 MiB more. What the session
    // keeps of each is its place in the file, 8 bytes, twice that while
    // the index grows.
    let per_entry = |more: isize| more / 14_000;
    assert!(per_entry(long - short) <= 16, "{short} then {long}");
    assert!(
        per_entry(long_peak - short_peak) <= 24,
        "{short_peak} then {long_peak}"
    );
    // Loading holds one line and a read buffer more than it keeps.
    assert!(
        long_peak - long <= 256 << 10,
        "{long} held, {long_peak} at most"
    );
}
