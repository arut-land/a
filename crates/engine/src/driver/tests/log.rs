//! The log as a session keeps it: the lines of its file, read back by seq
//! when a client comes to them, never held in the daemon's memory nor
//! encoded again.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fs;

use protocol::Decode;

use super::*;

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
        let mut file = serde_json::to_vec(&header).unwrap();
        file.push(b'\n');
        for line in lines {
            file.extend_from_slice(line);
            file.push(b'\n');
        }
        fs::write(self.path(header.id), file).unwrap();
        header
    }

    fn load(&self, header: &Header) -> Arc<Live> {
        Live::start(header, &self.fixture.0, Open::Load, Owner::None).unwrap()
    }

    fn path(&self, id: SessionId) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }

    /// The lines of session `id`'s file after its header.
    fn lines(&self, id: SessionId) -> Vec<Vec<u8>> {
        let file = fs::read(self.path(id)).unwrap();
        let mut lines: Vec<Vec<u8>> = file.split(|&b| b == b'\n').map(<[u8]>::to_vec).collect();
        lines.pop();
        lines.remove(0);
        lines
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

fn entries(frames: impl IntoIterator<Item = Frame>) -> Vec<Frame> {
    frames.into_iter().filter(|f| f.seq.is_some()).collect()
}

#[test]
fn a_committed_entry_is_encoded_exactly_once() {
    let disk = on_disk("once");
    // Encoded as no daemon encodes them, with a space after the seq: a
    // frame that holds the space was read back, not encoded again.
    let lines: Vec<Vec<u8>> = turns(300, 2)
        .iter()
        .map(|e| {
            let mut line = line(e);
            let comma = line.iter().position(|&b| b == b',').unwrap();
            line.insert(comma + 1, b' ');
            line
        })
        .collect();
    let session = disk.load(&disk.write(&lines));
    settle(&session);
    let shared = |a: &Frame, b: &Frame| std::ptr::eq(a.as_bytes(), b.as_bytes());
    let (first, second) = (sent(&session, 0), sent(&session, 0));
    assert_eq!(first.len(), 3, "attached, snapshot, synced");
    assert!(shared(&first[1], &second[1]), "one snapshot for both");
    let near = 300 - SNAPSHOT_GAP;
    let replayed = entries(sent(&session, near));
    assert_eq!(replayed.len() as u64, SNAPSHOT_GAP);
    for (frame, line) in replayed
        .iter()
        .zip(&lines[usize::try_from(near).unwrap()..])
    {
        let seq = frame.seq.unwrap();
        let read = Frame::from_line(seq, line).unwrap();
        assert_eq!(frame.as_bytes(), read.as_bytes(), "entry {seq}");
    }
    // Live, then replayed: committed once, and read back as written.
    let live = Outbox::new();
    session.follow(&live, 300);
    session.submit("again").unwrap();
    settle(&session);
    let live = entries(std::iter::from_fn(|| live.try_next()));
    let replayed = entries(sent(&session, 300));
    session.halt();
    let written = disk.lines(session.id);
    assert_eq!(live.len(), 2, "the input and its answer");
    for (live, replayed) in live.iter().zip(&replayed) {
        let seq = live.seq.unwrap();
        let stored = &written[usize::try_from(seq).unwrap() - 1];
        assert_eq!(*stored, line(&Entry::from_json(stored).unwrap()));
        assert_eq!(
            live.as_bytes(),
            Frame::from_line(seq, stored).unwrap().as_bytes()
        );
        assert_eq!(live.as_bytes(), replayed.as_bytes(), "entry {seq}");
    }
}

#[test]
fn replay_from_the_index_equals_the_live_frames() {
    let disk = on_disk("replay");
    let header = disk.write(&[]);
    let session = disk.load(&header);
    let live = Outbox::new();
    session.follow(&live, 0);
    for turn in 0..20 {
        session.submit(&format!("turn {turn}")).unwrap();
        settle(&session);
    }
    let live = entries(std::iter::from_fn(|| live.try_next()));
    let replayed = entries(sent(&session, 0));
    session.halt();
    let reloaded = disk.load(&header);
    settle(&reloaded);
    let after_reload = entries(sent(&reloaded, 0));
    reloaded.halt();
    assert_eq!(live.len(), 40);
    for frames in [&replayed, &after_reload] {
        assert_eq!(frames.len(), live.len());
        for (live, frame) in live.iter().zip(frames) {
            assert_eq!(live.seq, frame.seq);
            assert_eq!(live.as_bytes(), frame.as_bytes(), "entry {:?}", live.seq);
        }
    }
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
