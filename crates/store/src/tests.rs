//! The contract every backend meets, written once and run against each, plus
//! the crash cases only the JSONL files can show.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use protocol::{Body, Entry, SessionId};

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
            headers_list_sessions_oldest_first,
            counts_follow_appends,
            busy_markers_set_clear_and_repeat,
            sessions_are_isolated,
            concurrent_sessions_do_not_interfere,
            ids_are_unique_and_missing_ones_not_found,
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

fn entry(seq: u64) -> Entry {
    Entry {
        seq,
        epoch: 0,
        body: Body::User {
            text: format!("m{seq}"),
        },
    }
}

fn append(log: &mut dyn Log, seqs: impl IntoIterator<Item = u64>) -> Vec<Entry> {
    seqs.into_iter()
        .map(|seq| {
            let e = entry(seq);
            log.append(&e).unwrap();
            e
        })
        .collect()
}

fn round_trip(store: &dyn LogStore) {
    let h = header("/w");
    let mut created = store.create(&h).unwrap();
    assert_eq!(created.entries, []);
    let written = append(&mut *created.log, 1..=3);
    drop(created);

    assert_eq!(store.load(h.id).unwrap().entries, written);
    assert_eq!(store.headers().unwrap(), [h]);
}

fn appends_keep_their_order_across_reopens(store: &dyn LogStore) {
    let h = header("/w");
    let mut written = append(&mut *store.create(&h).unwrap().log, 1..=50);
    for round in 0..3 {
        let mut loaded = store.load(h.id).unwrap();
        assert_eq!(loaded.entries, written);
        let next = 51 + round * 10;
        written.extend(append(&mut *loaded.log, next..next + 10));
    }
    assert_eq!(store.load(h.id).unwrap().entries, written);
}

fn batches_append_in_order(store: &dyn LogStore) {
    let h = header("/w");
    let mut log = store.create(&h).unwrap().log;
    let mut written = append(&mut *log, 1..=2);
    let batch: Vec<Entry> = (3..=6).map(entry).collect();
    log.append_all(&batch).unwrap();
    log.append_all(&[]).unwrap();
    written.extend(batch);
    written.extend(append(&mut *log, 7..=7));
    assert_eq!(store.load(h.id).unwrap().entries, written);
    assert_eq!(store.count(h.id).unwrap(), 7);
}

fn headers_list_sessions_oldest_first(store: &dyn LogStore) {
    let mut made: Vec<Header> = ["/a", "/b", "/c"].map(header).into();
    made[2].parent = Some(Parent {
        session: made[0].id,
        call: 1,
    });
    for h in made.iter().rev() {
        append(&mut *store.create(h).unwrap().log, 1..=2);
    }
    assert_eq!(store.headers().unwrap(), made);
}

fn counts_follow_appends(store: &dyn LogStore) {
    let h = header("/w");
    let mut log = store.create(&h).unwrap().log;
    assert_eq!(store.count(h.id).unwrap(), 0);
    append(&mut *log, 1..=4);
    assert_eq!(store.count(h.id).unwrap(), 4);
    drop(log);
    append(&mut *store.load(h.id).unwrap().log, 5..=6);
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
    let mut log_a = store.create(&a).unwrap().log;
    let mut log_b = store.create(&b).unwrap().log;
    let mut written_a = Vec::new();
    for seq in 1..=3 {
        written_a.extend(append(&mut *log_a, [seq]));
        append(&mut *log_b, [seq * 100]);
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
                    let mut log = store.create(h).unwrap().log;
                    append(&mut *log, 1..=25)
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

/// A JSONL session file with `tail` written after its entries, as a crash
/// mid-append leaves it.
fn torn(tail: &[u8]) -> (Fixture, PathBuf, Header, Vec<Entry>) {
    let store = jsonl();
    let h = header("/w");
    let written = append(&mut *store.create(&h).unwrap().log, 1..=2);
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
        written.extend(append(&mut *loaded.log, 3..=4));
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
