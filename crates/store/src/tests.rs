//! The contract every backend meets, written once and run against each, plus
//! the crash cases only the JSONL files can show.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::ops::{Deref, RangeInclusive};
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
            lines_read_back_as_appended,
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

/// Loads session `id`, and the entries the load handed over.
fn load(store: &dyn LogStore, id: SessionId) -> (Loaded, Vec<Entry>) {
    let mut entries = Vec::new();
    let loaded = store.load(id, &mut |e| entries.push(e)).unwrap();
    assert_eq!(loaded.seq, entries.len() as u64);
    (loaded, entries)
}

fn entries_of(store: &dyn LogStore, id: SessionId) -> Vec<Entry> {
    load(store, id).1
}

fn round_trip(store: &dyn LogStore) {
    let h = header("/w");
    let mut created = store.create(&h).unwrap();
    assert_eq!(created.seq, 0);
    let written = append(&mut created, 1..=3);
    drop(created);

    assert_eq!(entries_of(store, h.id), written);
    assert_eq!(store.headers().unwrap(), [h]);
}

fn appends_keep_their_order_across_reopens(store: &dyn LogStore) {
    let h = header("/w");
    let mut written = append(&mut store.create(&h).unwrap(), 1..=50);
    for round in 0..3 {
        let (mut loaded, entries) = load(store, h.id);
        assert_eq!(entries, written);
        let next = 51 + round * 10;
        written.extend(append(&mut loaded, next..next + 10));
    }
    assert_eq!(entries_of(store, h.id), written);
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
    assert_eq!(entries_of(store, h.id), written);
    assert_eq!(store.count(h.id).unwrap(), 7);
}

/// Entry `seq`'s line, read back through `lines`.
fn line(lines: &dyn Lines, seq: u64) -> io::Result<Vec<u8>> {
    let (count, mut read) = run(lines, seq..=seq, 0)?;
    assert_eq!((count, read.pop()), (1, Some(b'\n')));
    Ok(read)
}

/// How many lines reading `seqs` within `limit` gets, and what it appends.
fn run(lines: &dyn Lines, seqs: RangeInclusive<u64>, limit: usize) -> io::Result<(u64, Vec<u8>)> {
    let mut out = b"kept".to_vec();
    let count = lines.read(seqs, limit, &mut out)?;
    assert!(out.starts_with(b"kept"), "read appends");
    Ok((count, out.split_off(4)))
}

/// Entries' lines end to end, as a file holds them.
fn file_of<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Vec<u8> {
    entries
        .into_iter()
        .flat_map(|e| [encoded(e), b"\n".to_vec()].concat())
        .collect()
}

fn encoded(e: &Entry) -> Vec<u8> {
    let mut line = Vec::new();
    e.encode(&mut line).unwrap();
    line
}

/// Each committed entry reads back as the line it was encoded to once,
/// through the lines of the load that committed it or loaded it; nothing
/// past the last committed entry reads.
fn lines_read_back_as_appended(store: &dyn LogStore) {
    let h = header("/w");
    let mut created = store.create(&h).unwrap();
    let mut written = append(&mut created, 1..=3);
    let (mut loaded, _) = load(store, h.id);
    assert_eq!(loaded.seq, 3);
    written.extend(append(&mut loaded, 4..=5));
    let refused = created.log.append(&entry(6, created.epoch)).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::ResourceBusy);
    let again = load(store, h.id).0;
    for (lines, upto) in [(&created.lines, 3), (&loaded.lines, 5), (&again.lines, 5)] {
        for e in &written[..upto] {
            assert_eq!(line(&**lines, e.seq).unwrap(), encoded(e), "{}", e.seq);
        }
        for seq in [0, 6] {
            assert_eq!(line(&**lines, seq).unwrap_err().kind(), ErrorKind::NotFound);
        }
    }
    // A run is as many whole lines as fit, and at least one.
    let lines = &*again.lines;
    assert_eq!(
        run(lines, 1..=5, usize::MAX).unwrap(),
        (5, file_of(&written))
    );
    let two = file_of(&written[1..3]);
    assert_eq!(run(lines, 2..=5, two.len() + 1).unwrap(), (2, two));
    assert_eq!(
        run(lines, 4..=6, 0).unwrap_err().kind(),
        ErrorKind::NotFound
    );
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
    append(&mut load(store, h.id).0, 5..=6);
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
    assert_eq!(entries_of(store, a.id), written_a);
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
        assert_eq!(entries_of(store, h.id), written);
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
        store.load(missing, &mut drop).map(drop).unwrap_err().kind(),
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
        let mut loaded = load(store, h.id).0;
        epochs.push(loaded.epoch);
        append(&mut loaded, [seq]);
    }
    assert!(epochs[0] > 0, "{epochs:?}");
    assert!(epochs.windows(2).all(|w| w[0] < w[1]), "{epochs:?}");
    let (loaded, entries) = load(store, h.id);
    let logged: Vec<u32> = entries.iter().map(|e| e.epoch).collect();
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
    let (mut current, entries) = load(store, h.id);
    let refused = stale.log.append(&entry(4, stale.epoch)).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::ResourceBusy);
    let batch = [entry(4, current.epoch), entry(5, current.epoch - 1)];
    let mismatched = current.log.append_all(&batch).unwrap_err();
    assert_eq!(mismatched.kind(), ErrorKind::ResourceBusy);
    assert_eq!(store.entries(h.id).unwrap(), entries);
    assert_eq!(entries.len(), 3);
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
        let (mut loaded, entries) = load(&*store, h.id);
        assert_eq!(entries, written);
        written.extend(append(&mut loaded, 3..=4));
        assert_eq!(entries_of(&*store, h.id), written);
        assert_eq!(store.count(h.id).unwrap(), 4);
    }
}

/// What a crash cut short is cut off before anything indexes it: no read
/// lands in it, and what is appended after it reads back whole.
#[test]
fn a_torn_tail_is_never_indexed() {
    let unterminated = br#"{"seq":3,"epoch":0,"kind":"user","text":"m3"}"#;
    for tail in [&br#"{"seq":3,"epo"#[..], unterminated, b"not json\n"] {
        let (store, path, h, mut written) = torn(tail);
        let (mut loaded, _) = load(&*store, h.id);
        assert_eq!(
            line(&*loaded.lines, 3).unwrap_err().kind(),
            ErrorKind::NotFound
        );
        let cut = fs::metadata(&path).unwrap().len();
        written.extend(append(&mut loaded, 3..=4));
        for lines in [&loaded.lines, &load(&*store, h.id).0.lines] {
            for e in &written {
                assert_eq!(line(&**lines, e.seq).unwrap(), encoded(e), "{}", e.seq);
            }
        }
        let file = fs::read(&path).unwrap();
        let after: Vec<u8> = written[2..]
            .iter()
            .flat_map(|e| [encoded(e), b"\n".to_vec()].concat())
            .collect();
        assert_eq!(file[usize::try_from(cut).unwrap()..], after);
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
    assert!(later.load(h.id, &mut drop).unwrap().epoch > created.epoch);
}
