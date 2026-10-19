//! The contract every backend meets, written once and run against each, plus
//! the crash cases only the JSONL files can show.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use protocol::{Body, Entry, Frame, SessionId};

use super::*;

/// A fresh store, and the directory to remove once the test is done.
struct Fixture {
    store: Box<dyn LogStore>,
    dir: Option<PathBuf>,
}

impl Deref for Fixture {
    type Target = dyn LogStore;

    fn deref(&self) -> &Self::Target {
        &*self.store
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

fn temp_dir() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("a-store-{}-{n}", std::process::id()))
}

fn jsonl() -> Fixture {
    let dir = temp_dir();
    Fixture {
        store: Box::new(Jsonl::open(&dir).unwrap()),
        dir: Some(dir),
    }
}

fn memory() -> Fixture {
    Fixture {
        store: Box::new(Memory::default()),
        dir: None,
    }
}

/// One `#[test]` per case for a backend.
macro_rules! cases {
    ($backend:ident; $($case:ident),* $(,)?) => {
        mod $backend {
            $(
                #[test]
                fn $case() {
                    super::$case(&*super::$backend());
                }
            )*
        }
    };
}

/// Every case, for every backend.
macro_rules! conformance {
    ($($backend:ident),*) => {
        $(cases!($backend;
            round_trip,
            appends_keep_their_order_across_reopens,
            batches_append_in_order,
            framed_batches_append_as_their_frames_hold,
            headers_list_sessions_oldest_first,
            counts_follow_appends,
            busy_markers_set_clear_and_repeat,
            sessions_are_isolated,
            concurrent_sessions_do_not_interfere,
            ids_are_unique_and_missing_ones_not_found,
            each_load_leases_a_newer_epoch,
            a_superseded_or_mismatched_writer_cannot_append,
        );)*
    };
}

conformance!(jsonl, memory);

fn header(workspace: &str) -> Header {
    Header {
        id: SessionId::new(),
        workspace: workspace.into(),
        parent: None,
        extensions: vec![Installed {
            name: "builtin".into(),
            version: 1,
        }],
    }
}

fn entry(seq: u64, epoch: u32) -> Entry {
    Entry::new(
        seq,
        epoch,
        Body::User {
            text: format!("m{seq}"),
        },
    )
}

/// Appends entries `seqs` under `loaded`'s lease.
fn append(loaded: &mut Loaded, seqs: impl IntoIterator<Item = u64>) -> Vec<Entry> {
    seqs.into_iter()
        .map(|seq| {
            let e = entry(seq, loaded.epoch);
            loaded.log.append(&e).unwrap();
            e
        })
        .collect()
}

fn round_trip(store: &dyn LogStore) {
    let h = header("/w");
    let mut created = store.create(&h).unwrap();
    assert_eq!(created.entries, []);
    let written = append(&mut created, 1..=3);
    drop(created);

    assert_eq!(store.load(h.id).unwrap().entries, written);
    assert_eq!(store.headers().unwrap(), [h]);
}

fn appends_keep_their_order_across_reopens(store: &dyn LogStore) {
    let h = header("/w");
    let mut written = append(&mut store.create(&h).unwrap(), 1..=50);
    for round in 0..3 {
        let mut loaded = store.load(h.id).unwrap();
        assert_eq!(loaded.entries, written);
        let next = 51 + round * 10;
        written.extend(append(&mut loaded, next..next + 10));
    }
    assert_eq!(store.load(h.id).unwrap().entries, written);
}

fn batches_append_in_order(store: &dyn LogStore) {
    let h = header("/w");
    let mut log = store.create(&h).unwrap();
    let mut written = append(&mut log, 1..=2);
    let batch: Vec<Entry> = (3..=6).map(|seq| entry(seq, log.epoch)).collect();
    log.log.append_all(&batch).unwrap();
    log.log.append_all(&[]).unwrap();
    written.extend(batch);
    written.extend(append(&mut log, 7..=7));
    assert_eq!(store.load(h.id).unwrap().entries, written);
    assert_eq!(store.count(h.id).unwrap(), 7);
}

fn framed_batches_append_as_their_frames_hold(store: &dyn LogStore) {
    let (encoded, plain) = (header("/w"), header("/w"));
    let mut log = store.create(&encoded).unwrap();
    let batch: Vec<Entry> = (1..=3).map(|seq| entry(seq, log.epoch)).collect();
    let frames: Vec<Frame> = batch.iter().map(|e| Frame::entry(e).unwrap()).collect();
    log.log.append_framed(&batch, &frames).unwrap();
    store
        .create(&plain)
        .unwrap()
        .log
        .append_all(&batch)
        .unwrap();
    assert_eq!(store.load(encoded.id).unwrap().entries, batch);
    assert_eq!(
        store.entries(encoded.id).unwrap(),
        store.entries(plain.id).unwrap()
    );
    let stale: Vec<Entry> = (4..=4).map(|seq| entry(seq, log.epoch)).collect();
    let stale_frames: Vec<Frame> = stale.iter().map(|e| Frame::entry(e).unwrap()).collect();
    drop(store.load(encoded.id).unwrap());
    let refused = log.log.append_framed(&stale, &stale_frames).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::ResourceBusy);
}

fn headers_list_sessions_oldest_first(store: &dyn LogStore) {
    let mut made: Vec<Header> = ["/a", "/b", "/c"].map(header).into();
    made[2].parent = Some(Parent {
        session: made[0].id,
        call: 1,
    });
    for h in made.iter().rev() {
        append(&mut store.create(h).unwrap(), 1..=2);
    }
    assert_eq!(store.headers().unwrap(), made);
}

fn counts_follow_appends(store: &dyn LogStore) {
    let h = header("/w");
    let mut log = store.create(&h).unwrap();
    assert_eq!(store.count(h.id).unwrap(), 0);
    append(&mut log, 1..=4);
    assert_eq!(store.count(h.id).unwrap(), 4);
    drop(log);
    append(&mut store.load(h.id).unwrap(), 5..=6);
    assert_eq!(store.count(h.id).unwrap(), 6);
}

fn busy_markers_set_clear_and_repeat(store: &dyn LogStore) {
    let (a, b) = (header("/a"), header("/b"));
    store.create(&a).unwrap();
    store.create(&b).unwrap();
    assert!(!store.is_busy(a.id));
    store.mark_busy(a.id, true).unwrap();
    store.mark_busy(a.id, true).unwrap();
    assert!(store.is_busy(a.id));
    assert!(!store.is_busy(b.id));
    store.mark_busy(a.id, false).unwrap();
    assert!(!store.is_busy(a.id));
    store.mark_busy(a.id, false).unwrap();
    store.mark_busy(b.id, false).unwrap();
    assert!(!store.is_busy(a.id) && !store.is_busy(b.id));
}

fn sessions_are_isolated(store: &dyn LogStore) {
    let (a, b) = (header("/a"), header("/b"));
    let mut log_a = store.create(&a).unwrap();
    let mut log_b = store.create(&b).unwrap();
    let mut written_a = Vec::new();
    for seq in 1..=3 {
        written_a.extend(append(&mut log_a, [seq]));
        append(&mut log_b, [seq * 100]);
    }
    store.mark_busy(b.id, true).unwrap();
    assert_eq!(store.load(a.id).unwrap().entries, written_a);
    assert_eq!(store.count(b.id).unwrap(), 3);
    assert!(!store.is_busy(a.id));
}

fn concurrent_sessions_do_not_interfere(store: &dyn LogStore) {
    let made: Vec<Header> = (0..4).map(|_| header("/w")).collect();
    let written: Vec<Vec<Entry>> = thread::scope(|s| {
        let workers: Vec<_> = made
            .iter()
            .map(|h| {
                s.spawn(move || {
                    let mut log = store.create(h).unwrap();
                    append(&mut log, 1..=25)
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    for (h, written) in made.iter().zip(written) {
        assert_eq!(store.load(h.id).unwrap().entries, written);
    }
    assert_eq!(store.headers().unwrap().len(), made.len());
}

fn ids_are_unique_and_missing_ones_not_found(store: &dyn LogStore) {
    let h = header("/w");
    store.create(&h).unwrap();
    let again = store.create(&h).map(drop).unwrap_err();
    assert_eq!(again.kind(), ErrorKind::AlreadyExists);
    let missing = SessionId::new();
    assert_eq!(
        store.load(missing).map(drop).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        store.count(missing).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(store.headers().unwrap(), [h]);
}

fn each_load_leases_a_newer_epoch(store: &dyn LogStore) {
    let h = header("/w");
    let mut created = store.create(&h).unwrap();
    append(&mut created, 1..=2);
    drop(created);
    let mut epochs = Vec::new();
    for seq in 3..=5 {
        let mut loaded = store.load(h.id).unwrap();
        epochs.push(loaded.epoch);
        append(&mut loaded, [seq]);
    }
    assert!(epochs[0] > 0, "{epochs:?}");
    assert!(epochs.windows(2).all(|w| w[0] < w[1]), "{epochs:?}");
    let loaded = store.load(h.id).unwrap();
    let logged: Vec<u32> = loaded.entries.iter().map(|e| e.epoch).collect();
    assert_eq!(logged[2..], epochs);
    assert!(loaded.epoch > epochs[2]);
}

/// Reading entries leaves the writer's lease alone; loading supersedes it.
fn a_superseded_or_mismatched_writer_cannot_append(store: &dyn LogStore) {
    let h = header("/w");
    let mut stale = store.create(&h).unwrap();
    let written = append(&mut stale, 1..=2);
    assert_eq!(store.entries(h.id).unwrap(), written);
    append(&mut stale, [3]);
    let mut current = store.load(h.id).unwrap();
    let refused = stale.log.append(&entry(4, stale.epoch)).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::ResourceBusy);
    let batch = [entry(4, current.epoch), entry(5, current.epoch - 1)];
    let mismatched = current.log.append_all(&batch).unwrap_err();
    assert_eq!(mismatched.kind(), ErrorKind::ResourceBusy);
    assert_eq!(store.entries(h.id).unwrap(), current.entries);
    assert_eq!(current.entries.len(), 3);
}

/// A JSONL session file with `tail` written after its entries, as a crash
/// mid-append leaves it.
fn torn(tail: &[u8]) -> (Fixture, PathBuf, Header, Vec<Entry>) {
    let store = jsonl();
    let h = header("/w");
    let written = append(&mut store.create(&h).unwrap(), 1..=2);
    let path = store.dir.as_ref().unwrap().join(format!("{}.jsonl", h.id));
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(tail)
        .unwrap();
    (store, path, h, written)
}

#[test]
fn jsonl_drops_a_torn_tail_and_appends_after_it() {
    for tail in [
        &br#"{"seq":3,"epo"#[..],
        br#"{"seq":3,"epoch":0,"kind":"user","text":"m3"}"#,
    ] {
        let (store, _, h, mut written) = torn(tail);
        assert_eq!(store.count(h.id).unwrap(), 2);
        let mut loaded = store.load(h.id).unwrap();
        assert_eq!(loaded.entries, written);
        written.extend(append(&mut loaded, 3..=4));
        assert_eq!(store.load(h.id).unwrap().entries, written);
        assert_eq!(store.count(h.id).unwrap(), 4);
    }
}

#[test]
fn jsonl_headers_from_before_extension_records_load() {
    let store = jsonl();
    let id = SessionId::new();
    let path = store.dir.as_ref().unwrap().join(format!("{id}.jsonl"));
    fs::write(
        &path,
        format!("{{\"id\":\"{}\",\"workspace\":\"/w\"}}\n", id.0),
    )
    .unwrap();
    let headers = store.headers().unwrap();
    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].extensions, []);
}

#[test]
fn jsonl_skips_a_file_whose_header_a_crash_cut_off() {
    let store = jsonl();
    let h = header("/w");
    store.create(&h).unwrap();
    let dir = store.dir.as_ref().unwrap();
    fs::write(
        dir.join(format!("{}.jsonl", SessionId::new())),
        br#"{"id":"#,
    )
    .unwrap();
    assert_eq!(store.headers().unwrap(), [h]);
}

#[test]
fn jsonl_files_are_a_header_line_then_one_line_per_entry() {
    let (store, path, h, written) = torn(b"");
    store.mark_busy(h.id, true).unwrap();
    let mut expected = serde_json::to_string(&h).unwrap() + "\n";
    for e in &written {
        expected += &(serde_json::to_string(e).unwrap() + "\n");
    }
    assert_eq!(fs::read_to_string(&path).unwrap(), expected);
    assert!(path.with_extension("busy").exists());
}

#[test]
fn jsonl_leases_past_the_epochs_its_files_hold() {
    let store = jsonl();
    let h = header("/w");
    let mut created = store.create(&h).unwrap();
    append(&mut created, 1..=2);
    // As a later daemon, with no leases of its own yet.
    let later = Jsonl::open(store.dir.as_ref().unwrap()).unwrap();
    assert!(later.load(h.id).unwrap().epoch > created.epoch);
}
