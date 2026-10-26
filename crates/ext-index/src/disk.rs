//! The index of a workspace on disk: a cache derived from the tree,
//! never trusted. Two files per workspace, named for its root, in the
//! daemon's cache: `<key>.files` (the file list, which files are binary,
//! what changed since the content index was built) and `<key>.bigrams`
//! (the content index as built, which only a new build changes, so a
//! file list rewritten after a few edits leaves it as it is). Each is
//! versioned, names its root (or its build), and ends in a CRC-32C of
//! all before it; anything amiss, and the workspace is walked and built
//! again as if neither were there.
//!
//! They are written streaming (`sys::fs::write_private_with`, private to
//! this user, replaced whole) and read the same way: a file list or a
//! content index is never in memory twice.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use crc::{CRC_32_ISCSI, CRC_64_XZ, Crc, NoTable, Table};

use crate::fff::index::{BigramFilter, BigramOverlay};
use crate::fff::types::Files;

/// Bumped with any change to what either file holds or means.
const FORMAT: u32 = 2;

/// Idle caches are retained for a month; reclamation happens only at rest.
pub(crate) const MAX_AGE: Duration = Duration::from_hours(720);
const FILES: [u8; 8] = *b"a-files\0";
const BIGRAMS: [u8; 8] = *b"a-bigrm\0";

// A table with relocations in static storage dirties pages during image loading,
// even if no workspace ever uses the index.
static CHECK: OnceLock<Box<Crc<u32, Table<16>>>> = OnceLock::new();

fn checksum() -> &'static Crc<u32, Table<16>> {
    CHECK.get_or_init(|| Box::new(Crc::<u32, Table<16>>::new(&CRC_32_ISCSI)))
}

/// Where the index of the workspace at `root` is kept in `dir`.
#[derive(Clone)]
pub(crate) struct Place {
    files: PathBuf,
    bigrams: PathBuf,
}

impl Place {
    pub(crate) fn new(dir: &Path, root: &Path) -> Self {
        let key =
            Crc::<u64, NoTable>::new(&CRC_64_XZ).checksum(root.as_os_str().as_encoded_bytes());
        Self {
            files: dir.join(format!("{key:016x}.files")),
            bigrams: dir.join(format!("{key:016x}.bigrams")),
        }
    }
}

/// What a file list on disk holds.
pub(crate) struct Stored {
    pub(crate) files: Files,
    pub(crate) snapshot: uuid::Uuid,
    /// When it was taken, in seconds since the epoch: a file modified in
    /// that second or later may have changed since without its size or
    /// time telling (as git treats a racily clean entry).
    pub(crate) taken: u64,
    pub(crate) content: Option<StoredContent>,
}

pub(crate) struct StoredContent {
    pub(crate) id: u64,
    pub(crate) filter: BigramFilter,
    pub(crate) overlay: BigramOverlay,
    pub(crate) binary: Vec<u64>,
    pub(crate) unknown: Vec<u64>,
}

/// What [`write`] writes of the content index.
pub(crate) struct Content<'a> {
    pub(crate) id: u64,
    /// The content index as built, unless the bigrams file already holds
    /// build `id`.
    pub(crate) filter: Option<&'a BigramFilter>,
    pub(crate) overlay: &'a BigramOverlay,
    pub(crate) binary: &'a [u64],
    pub(crate) unknown: &'a [u64],
}

/// Writes the file list (and the content index, when it is new) of the
/// workspace at `root`: the bigrams first, so a file list never names a
/// build that is not on disk.
pub(crate) fn write(
    place: &Place,
    root: &Path,
    taken: u64,
    snapshot: uuid::Uuid,
    files: &Files,
    content: Option<&Content<'_>>,
) -> io::Result<()> {
    if let Some(Content {
        id,
        filter: Some(filter),
        ..
    }) = content
    {
        save(&place.bigrams, BIGRAMS, |out| {
            out.u64(*id)?;
            filter.encode(out)
        })?;
    }
    save(&place.files, FILES, |out| {
        out.bytes(root.as_os_str().as_encoded_bytes())?;
        out.u64(taken)?;
        out.bytes(snapshot.as_bytes())?;
        files.encode(out)?;
        match content {
            None => out.u8(0),
            Some(content) => {
                out.u8(1)?;
                out.u64(content.id)?;
                out.words(content.binary)?;
                out.words(content.unknown)?;
                content.overlay.encode(out)
            }
        }
    })
}

/// The index of the workspace at `root` as last written, if it is there
/// and whole: `None` for anything else (missing, torn, another format,
/// another root's). A content index whose build is not on disk, or does
/// not fit the files, is left out.
pub(crate) fn read(place: &Place, root: &Path, budget: usize) -> Option<Stored> {
    let (files, taken, snapshot, content) = load(&place.files, FILES, |input| {
        if input.bytes()? != root.as_os_str().as_encoded_bytes() {
            return Err(invalid());
        }
        let taken = input.u64()?;
        let snapshot = uuid::Uuid::from_slice(&input.bytes()?).map_err(|_| invalid())?;
        let files = Files::decode(input)?;
        input.budget = budget.saturating_sub(files.heap_bytes());
        let content = match input.u8()? {
            0 => None,
            1 => {
                let id = input.u64()?;
                let binary = input.words()?;
                let unknown = input.words()?;
                let overlay = BigramOverlay::decode(input)?;
                Some((id, binary, unknown, overlay))
            }
            _ => return Err(invalid()),
        };
        Ok((files, taken, snapshot, content))
    })
    .ok()?;
    let walked = files.walked_count();
    let content = content.and_then(|(id, binary, unknown, overlay)| {
        let filter = load(&place.bigrams, BIGRAMS, |input| {
            if input.u64()? != id {
                return Err(invalid());
            }
            input.budget = budget
                .saturating_sub(files.heap_bytes())
                .saturating_sub((binary.len() + unknown.len()) * 8)
                .saturating_sub(overlay.heap_bytes());
            BigramFilter::decode(input)
        })
        .ok()?;
        (filter.file_count() == walked
            && binary.len() == walked.div_ceil(64)
            && unknown.len() == walked.div_ceil(64)
            && overlay.fits(walked))
        .then_some(StoredContent {
            id,
            filter,
            overlay,
            binary,
            unknown,
        })
    });
    touch(&place.files);
    touch(&place.bigrams);
    Some(Stored {
        files,
        snapshot,
        taken,
        content,
    })
}

fn save(
    path: &Path,
    magic: [u8; 8],
    write: impl FnOnce(&mut Out<BufWriter<&mut File>>) -> io::Result<()>,
) -> io::Result<()> {
    sys::fs::write_private_with(path, |file| {
        let mut out = Out {
            inner: BufWriter::with_capacity(64 << 10, file),
            check: checksum().digest(),
        };
        out.put(&magic)?;
        out.u32(FORMAT)?;
        write(&mut out)?;
        let check = out.check.finalize();
        out.inner.write_all(&check.to_le_bytes())?;
        out.inner.flush()
    })
}

fn load<T>(
    path: &Path,
    magic: [u8; 8],
    read: impl FnOnce(&mut In<BufReader<File>>) -> io::Result<T>,
) -> io::Result<T> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut input = In {
        inner: BufReader::with_capacity(64 << 10, file),
        check: checksum().digest(),
        left: len.checked_sub(4).ok_or_else(invalid)?,
        budget: usize::MAX,
    };
    if input.take::<8>()? != magic || input.u32()? != FORMAT {
        return Err(invalid());
    }
    let value = read(&mut input)?;
    if input.left != 0 {
        return Err(invalid());
    }
    let mut stored = [0; 4];
    input.inner.read_exact(&mut stored)?;
    if u32::from_le_bytes(stored) != input.check.finalize() {
        return Err(invalid());
    }
    Ok(value)
}

pub(crate) fn invalid() -> io::Error {
    io::ErrorKind::InvalidData.into()
}

/// Little-endian values out, each counted into the checksum.
pub(crate) struct Out<W> {
    inner: W,
    check: crc::Digest<'static, u32, Table<16>>,
}

impl<W: Write> Out<W> {
    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.check.update(bytes);
        self.inner.write_all(bytes)
    }

    pub(crate) fn u8(&mut self, value: u8) -> io::Result<()> {
        self.put(&[value])
    }

    pub(crate) fn u32(&mut self, value: u32) -> io::Result<()> {
        self.put(&value.to_le_bytes())
    }

    pub(crate) fn u64(&mut self, value: u64) -> io::Result<()> {
        self.put(&value.to_le_bytes())
    }

    pub(crate) fn len(&mut self, len: usize) -> io::Result<()> {
        self.u64(len as u64)
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.len(bytes.len())?;
        self.put(bytes)
    }

    pub(crate) fn words(&mut self, words: &[u64]) -> io::Result<()> {
        self.len(words.len())?;
        self.each(words, u64::to_le_bytes)
    }

    pub(crate) fn halves(&mut self, halves: &[u16]) -> io::Result<()> {
        self.len(halves.len())?;
        self.each(halves, u16::to_le_bytes)
    }

    pub(crate) fn u32s(&mut self, values: &[u32]) -> io::Result<()> {
        self.len(values.len())?;
        self.each(values, u32::to_le_bytes)
    }

    /// `values` encoded a buffer at a time, `N` bytes each.
    fn each<T: Copy, const N: usize>(
        &mut self,
        values: &[T],
        encode: impl Fn(T) -> [u8; N],
    ) -> io::Result<()> {
        let mut buf = [0u8; 8192];
        for chunk in values.chunks(buf.len() / N) {
            for (value, out) in chunk.iter().zip(buf.as_chunks_mut::<N>().0) {
                *out = encode(*value);
            }
            self.put(&buf[..chunk.len() * N])?;
        }
        Ok(())
    }
}

/// What [`Out`] wrote, checked as it is read: a length is never believed
/// beyond what the file still holds.
pub(crate) struct In<R> {
    inner: R,
    check: crc::Digest<'static, u32, Table<16>>,
    /// Bytes before the checksum not yet read.
    left: u64,
    budget: usize,
}

impl<R: Read> In<R> {
    fn get(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.left = (self.left.checked_sub(buf.len() as u64)).ok_or_else(invalid)?;
        self.inner.read_exact(buf)?;
        self.check.update(buf);
        Ok(())
    }

    fn take<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut buf = [0; N];
        self.get(&mut buf)?;
        Ok(buf)
    }

    pub(crate) fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take::<1>()?[0])
    }

    pub(crate) fn u32(&mut self) -> io::Result<u32> {
        self.take().map(u32::from_le_bytes)
    }

    pub(crate) fn u64(&mut self) -> io::Result<u64> {
        self.take().map(u64::from_le_bytes)
    }

    /// A count of things `size` bytes each, if the file holds that many.
    pub(crate) fn len(&mut self, size: u64) -> io::Result<usize> {
        let len = self.u64()?;
        if len.checked_mul(size).is_none_or(|bytes| bytes > self.left) {
            return Err(invalid());
        }
        let len = usize::try_from(len).map_err(|_| invalid())?;
        self.budget = self
            .budget
            .checked_sub(
                len.checked_mul(usize::try_from(size).map_err(|_| invalid())?)
                    .ok_or_else(invalid)?,
            )
            .ok_or_else(invalid)?;
        Ok(len)
    }

    pub(crate) fn usize(&mut self) -> io::Result<usize> {
        usize::try_from(self.u64()?).map_err(|_| invalid())
    }

    pub(crate) fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0; self.len(1)?];
        self.get(&mut bytes)?;
        Ok(bytes)
    }

    pub(crate) fn words(&mut self) -> io::Result<Vec<u64>> {
        self.each(u64::from_le_bytes)
    }

    /// Decodes into storage already allocated by the owning type, including
    /// anonymous mappings that release their pages when the index rests.
    pub(crate) fn read_words(&mut self, words: &mut [u64]) -> io::Result<()> {
        let mut buf = [0u8; 8192];
        for chunk in words.chunks_mut(buf.len() / 8) {
            self.get(&mut buf[..chunk.len() * 8])?;
            for (word, bytes) in chunk.iter_mut().zip(buf.as_chunks::<8>().0) {
                *word = u64::from_le_bytes(*bytes);
            }
        }
        Ok(())
    }

    pub(crate) fn halves(&mut self) -> io::Result<Vec<u16>> {
        self.each(u16::from_le_bytes)
    }

    pub(crate) fn u32s(&mut self) -> io::Result<Vec<u32>> {
        self.each(u32::from_le_bytes)
    }

    fn each<T, const N: usize>(&mut self, decode: impl Fn([u8; N]) -> T) -> io::Result<Vec<T>> {
        let len = self.len(N as u64)?;
        let mut values = Vec::with_capacity(len);
        let mut buf = [0u8; 8192];
        let mut rest = len;
        while rest > 0 {
            let n = rest.min(buf.len() / N);
            self.get(&mut buf[..n * N])?;
            values.extend(buf[..n * N].as_chunks::<N>().0.iter().map(|b| decode(*b)));
            rest -= n;
        }
        Ok(values)
    }
}

/// Opening refreshes cache age without rewriting the snapshot.
fn touch(path: &Path) {
    if let Ok(file) = File::options().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// Derived files unopened for thirty days are reclaimed during rest, never
/// by a periodic task. The current workspace is being written right now.
pub(crate) fn prune(dir: &Path, current: &Place) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == current.files || path == current.bigrams {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.len() != 16
            || !name.bytes().all(|b| b.is_ascii_hexdigit())
            || !path
                .extension()
                .is_some_and(|e| e == "files" || e == "bigrams")
        {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| SystemTime::now().duration_since(at).ok())
            .is_some_and(|age| age > MAX_AGE);
        if old {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
pub(crate) fn encoded(write: impl FnOnce(&mut Out<Vec<u8>>) -> io::Result<()>) -> Vec<u8> {
    let mut out = Out {
        inner: Vec::new(),
        check: checksum().digest(),
    };
    write(&mut out).unwrap();
    out.inner
}

#[cfg(test)]
pub(crate) fn decoded<'a, T>(
    bytes: &'a [u8],
    read: impl FnOnce(&mut In<&'a [u8]>) -> io::Result<T>,
) -> io::Result<T> {
    let mut input = In {
        inner: bytes,
        left: bytes.len() as u64,
        check: checksum().digest(),
        budget: usize::MAX,
    };
    read(&mut input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_a_snapshot_refreshes_its_cache_age() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        let place = Place::new(dir.path(), &root);
        write(
            &place,
            &root,
            0,
            uuid::Uuid::now_v7(),
            &Files::default(),
            None,
        )
        .unwrap();
        File::options()
            .write(true)
            .open(&place.files)
            .unwrap()
            .set_modified(SystemTime::now() - MAX_AGE - Duration::from_hours(24))
            .unwrap();
        assert!(read(&place, &root, usize::MAX).is_some());
        prune(dir.path(), &Place::new(dir.path(), Path::new("another")));
        assert!(
            place.files.exists(),
            "a recently opened snapshot is retained"
        );
    }

    #[test]
    fn rest_reclaims_only_derived_files_unopened_for_thirty_days() {
        let dir = tempfile::tempdir().unwrap();
        let current = Place::new(dir.path(), Path::new("current"));
        let stale = Place::new(dir.path(), Path::new("stale"));
        let recent = Place::new(dir.path(), Path::new("recent"));
        let unrelated = dir.path().join("notes.files");
        for path in [
            &current.files,
            &current.bigrams,
            &stale.files,
            &stale.bigrams,
            &recent.files,
            &recent.bigrams,
            &unrelated,
        ] {
            File::create(path).unwrap();
        }
        for path in [&stale.files, &stale.bigrams, &current.files, &unrelated] {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_hours(24 * 31))
                .unwrap();
        }
        prune(dir.path(), &current);
        assert!(!stale.files.exists());
        assert!(!stale.bigrams.exists());
        assert!(current.files.exists());
        assert!(current.bigrams.exists());
        assert!(recent.files.exists());
        assert!(recent.bigrams.exists());
        assert!(unrelated.exists());
    }
}
