//! The contract every backend meets, written once and run against each, plus
//! the crash cases only the JSONL files can show.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::ops::{Deref, RangeInclusive};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use protocol::{Body, Decode, Entry, Json, ReadOnly, SessionId};

use super::jsonl::{FORMAT, seal, unseal};
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

            sessions_are_isolated,
            concurrent_sessions_do_not_interfere,
            ids_are_unique_and_missing_ones_not_found,
            each_load_leases_a_newer_epoch,
            reading_entries_leaves_the_writer_alone,
        );)*
    };
}

conformance!(jsonl, memory);

fn header(workspace: &str) -> Header {
    Header {
        id: SessionId::new(),
        workspace: workspace.into(),
        parent: None,
    }
}

fn entry(seq: u64, epoch: u32) -> Entry {
    Entry::new(
        seq,
        epoch,
        Body::Input {
            text: format!("m{seq}"),
            mode: protocol::Mode::Queue,
            taken: true,
            origin: None,
        },
    )
}

/// Appends entries `seqs` under `loaded`'s lease.
fn append(loaded: &mut Loaded, seqs: impl IntoIterator<Item = u64>) -> Vec<Entry> {
    seqs.into_iter()
        .map(|seq| {
            let e = entry(seq, loaded.writer.epoch());
            loaded
                .writer
                .log
                .append_all(std::slice::from_ref(&e))
                .unwrap();
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
    let batch: Vec<Entry> = (3..=6).map(|seq| entry(seq, log.writer.epoch())).collect();
    log.writer.log.append_all(&batch).unwrap();
    log.writer.log.append_all(&[]).unwrap();
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

fn sessions_are_isolated(store: &dyn LogStore) {
    let (a, b) = (header("/a"), header("/b"));
    let mut log_a = store.create(&a).unwrap();
    let mut log_b = store.create(&b).unwrap();
    let mut written_a = Vec::new();
    for seq in 1..=3 {
        written_a.extend(append(&mut log_a, [seq]));
        append(&mut log_b, [seq * 100]);
    }
    assert_eq!(entries_of(store, a.id), written_a);
    assert_eq!(store.count(b.id).unwrap(), 3);
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
        epochs.push(loaded.writer.epoch());
        append(&mut loaded, [seq]);
    }
    assert!(epochs[0] > 0, "{epochs:?}");
    assert!(epochs.windows(2).all(|w| w[0] < w[1]), "{epochs:?}");
    let (loaded, entries) = load(store, h.id);
    let logged: Vec<u32> = entries.iter().map(|e| e.epoch).collect();
    assert_eq!(logged[2..], epochs);
    assert!(loaded.writer.epoch() > epochs[2]);
}

/// Reading entries takes no lease: the writer goes on appending.
fn reading_entries_leaves_the_writer_alone(store: &dyn LogStore) {
    let h = header("/w");
    let mut writing = store.create(&h).unwrap();
    let written = append(&mut writing, 1..=2);
    assert_eq!(store.entries(h.id).unwrap(), written);
    let more = append(&mut writing, [3]);
    assert_eq!(store.entries(h.id).unwrap()[2..], more);
}

/// Entries' lines end to end, sealed, as a file holds them.
fn sealed<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Vec<u8> {
    let mut file = Vec::new();
    for e in entries {
        let at = file.len();
        e.encode(&mut file).unwrap();
        seal(&mut file, at);
    }
    file
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
        assert_eq!(file[usize::try_from(cut).unwrap()..], sealed(&written[2..]));
    }
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
    let (_store, path, h, written) = torn(b"");
    let header = serde_json::to_string(&h).unwrap();
    let mut expected = format!(r#"{{"format":{FORMAT},{}"#, &header[1..]).into_bytes();
    seal(&mut expected, 0);
    expected.extend(sealed(&written));
    assert_eq!(fs::read(&path).unwrap(), expected);
    let header = String::from_utf8(expected).unwrap();
    assert!(header.starts_with(r#"{"format":1,"id":""#), "{header}");
    assert!(
        header
            .lines()
            .all(|l| l.len() > 18 && l[l.len() - 18..].starts_with(r#","crc":""#))
    );
}

#[test]
fn jsonl_leases_past_the_epochs_its_files_hold() {
    let store = jsonl();
    let h = header("/w");
    let mut created = store.create(&h).unwrap();
    append(&mut created, 1..=2);
    // As a later daemon, with no leases of its own yet.
    let later = Jsonl::open(store.dir.as_ref().unwrap()).unwrap();
    assert!(later.load(h.id, &mut drop).unwrap().writer.epoch() > created.writer.epoch());
}

/// A JSONL session of entries 1 to 4, and its file as written.
fn written() -> (Fixture, PathBuf, Header, Vec<Entry>, Vec<u8>) {
    let (store, path, h, mut written) = torn(b"");
    let mut log = load(&*store, h.id).0;
    written.extend(append(&mut log, 3..=4));
    let file = fs::read(&path).unwrap();
    (store, path, h, written, file)
}

/// The file's lines, newline included: the header's first.
fn lines_of(file: &[u8]) -> Vec<Vec<u8>> {
    file.split_inclusive(|&b| b == b'\n')
        .map(<[u8]>::to_vec)
        .collect()
}

/// That a load of `id` keeps its log as it is, for `why`: it hands over
/// `entries`, appends nothing and leaves the file as `file`.
fn kept_as_is(
    store: &dyn LogStore,
    id: SessionId,
    path: &PathBuf,
    file: &[u8],
    why: ReadOnly,
    entries: &[Entry],
) {
    let (mut loaded, got) = load(store, id);
    assert_eq!(got, entries);
    assert_eq!(loaded.writer.read_only(), Some(why));
    let refused = loaded.append(entry(9, 0).body).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::ReadOnlyFilesystem);
    assert_eq!(refused.to_string(), why.to_string());
    assert_eq!(fs::read(path).unwrap(), file, "the file is never touched");
    for e in entries {
        assert_eq!(line(&*loaded.lines, e.seq).unwrap(), encoded(e));
    }
}

/// A sealed line is never cut, whatever it holds: one of a kind this
/// build does not know reads as such, is passed over and is read back as
/// it was written; one this build cannot read keeps the file as it is.
#[test]
fn a_valid_line_is_never_cut() {
    let (store, path, h, mut written, file) = written();
    let unknown = br#"{"seq":5,"epoch":7,"kind":"newer","text":5,"x":[1]}"#;
    let mut grown = file.clone();
    let at = grown.len();
    grown.extend_from_slice(unknown);
    seal(&mut grown, at);
    fs::write(&path, &grown).unwrap();
    let (mut loaded, entries) = load(&*store, h.id);
    let raw: Json = serde_json::from_slice(unknown).unwrap();
    let kind = "newer".to_owned();
    written.push(Entry::new(5, 7, Body::Unknown { kind, raw }));
    assert_eq!(entries, written);
    assert_eq!(loaded.writer.read_only(), None);
    assert_eq!(line(&*loaded.lines, 5).unwrap(), unknown);
    assert_eq!(
        encoded(&written[4]),
        unknown,
        "it is written back as it was read"
    );
    written.extend(append(&mut loaded, [6]));
    assert_eq!(entries_of(&*store, h.id), written);
    assert_eq!(fs::read(&path).unwrap()[..grown.len()], grown);

    // A sealed line of a kind this build knows, which it cannot read, in
    // the middle and at the end.
    let bad = br#"{"seq":3,"epoch":1,"kind":"input","text":5}"#;
    let mut lines = lines_of(&file);
    lines[3] = bad.to_vec();
    seal(&mut lines[3], 0);
    let middle = lines.concat();
    fs::write(&path, &middle).unwrap();
    kept_as_is(
        &*store,
        h.id,
        &path,
        &middle,
        ReadOnly::Damaged { seq: 3 },
        &written[..2],
    );
    let mut tail = file.clone();
    let at = tail.len();
    tail.extend_from_slice(&bad[..]);
    seal(&mut tail, at);
    fs::write(&path, &tail).unwrap();
    kept_as_is(
        &*store,
        h.id,
        &path,
        &tail,
        ReadOnly::Damaged { seq: 5 },
        &written[..4],
    );
}

/// What a crash leaves is a last line cut short, or not sealed: that alone
/// is cut, and only it.
#[test]
fn only_a_torn_tail_is_trimmed() {
    let (store, path, h, written, file) = written();
    let mut next = Vec::new();
    entry(5, 1).encode(&mut next).unwrap();
    let mut sealed = next.clone();
    seal(&mut sealed, 0);
    let flipped = |at: usize| {
        let mut line = sealed.clone();
        line[at] ^= 0x20;
        line
    };
    for tail in [
        next.clone(),
        [&next[..], b"\n"].concat(),
        sealed[..sealed.len() - 1].to_vec(),
        sealed[..sealed.len() - 4].to_vec(),
        flipped(3),
        flipped(sealed.len() - 3),
        b"\n".to_vec(),
    ] {
        fs::write(&path, [&file[..], &tail].concat()).unwrap();
        let (loaded, entries) = load(&*store, h.id);
        assert_eq!(entries, written, "{}", String::from_utf8_lossy(&tail));
        assert_eq!(loaded.writer.read_only(), None);
        assert_eq!(fs::read(&path).unwrap(), file);
    }
}

/// Damage anywhere but the last line is not what a crash leaves: the log
/// is kept as it is, its entries before the damage shown, and is never
/// cut or appended to.
#[test]
fn damage_before_the_tail_opens_read_only() {
    let (store, path, h, written, file) = written();
    let lines = lines_of(&file);
    for (line, at) in [(2, 5), (3, lines[3].len() - 4), (1, lines[1].len() - 1)] {
        let mut damaged = lines.clone();
        damaged[line][at] ^= 1;
        let damaged = [&damaged.concat()[..], b"{\"seq\":5"].concat();
        fs::write(&path, &damaged).unwrap();
        let why = ReadOnly::Damaged { seq: line as u64 };
        kept_as_is(&*store, h.id, &path, &damaged, why, &written[..line - 1]);
        assert!(store.headers().unwrap().contains(&h), "it is still listed");
    }
}

/// A log in a format this build does not write, a newer build's, is read
/// as far as this build can and kept as it is, torn tail and all.
#[test]
fn a_newer_log_is_never_written_by_an_older_daemon() {
    let (store, path, h, written, file) = written();
    let mut lines = lines_of(&file);
    assert!(unseal(&mut lines[0]));
    let header = String::from_utf8(lines[0].clone()).unwrap();
    let newer = FORMAT + 1;
    lines[0] = header
        .replacen(
            &format!(r#""format":{FORMAT}"#),
            &format!(r#""format":{newer}"#),
            1,
        )
        .into_bytes();
    seal(&mut lines[0], 0);
    let file = [&lines.concat()[..], br#"{"seq":5,"ep"#].concat();
    fs::write(&path, &file).unwrap();
    assert_eq!(store.headers().unwrap(), std::slice::from_ref(&h));
    kept_as_is(
        &*store,
        h.id,
        &path,
        &file,
        ReadOnly::Format { version: newer },
        &written,
    );
}

/// Every flip of a bit of a sealed line, the seal's own included, is
/// found: the line no longer unseals.
#[test]
fn crc_detects_a_flipped_byte() {
    let mut line = Vec::new();
    entry(1, 2).encode(&mut line).unwrap();
    let object = line.clone();
    seal(&mut line, 0);
    let mut again = line.clone();
    assert!(unseal(&mut again));
    assert_eq!(again, object);
    assert_eq!(Entry::from_json(&again).unwrap(), entry(1, 2));
    for at in 0..line.len() {
        for bit in 0..8 {
            let mut flipped = line.clone();
            flipped[at] ^= 1 << bit;
            assert!(!unseal(&mut flipped), "byte {at}, bit {bit}");
        }
    }
}
