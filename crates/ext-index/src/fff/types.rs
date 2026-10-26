//! The files of a workspace: relative paths, `/`-separated on every
//! platform, in one contiguous arena, and what is known of each.
//!
//! FFF kept paths as deduplicated 16-byte chunks behind raw pointers so
//! its SIMD scorer could read them in place; one `String` is as cheap for
//! a tree of real paths and needs no `unsafe` to read.

use std::cmp::Ordering;
use std::io::{self, Read, Write};

use crate::disk::{In, Out, invalid};

/// One file: where its path is in the arena, and its size and age.
#[derive(Clone, Debug)]
pub(crate) struct FileItem {
    start: u32,
    len: u16,
    /// Where its name begins within its path.
    name: u16,
    pub(crate) size: u64,
    /// Seconds since the epoch.
    pub(crate) modified: u64,
    /// Removed since the walk: kept in place so the content index's bit
    /// positions stay valid until it is rebuilt.
    deleted: bool,
}

/// A file as the walk or a change found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) modified: u64,
}

/// Every file of a workspace. `items[..walked]` are those the last walk
/// found, sorted by path; files created since follow in the order they
/// appeared.
#[derive(Clone, Debug, Default)]
pub(crate) struct Files {
    arena: String,
    items: Vec<FileItem>,
    walked: usize,
}

impl Files {
    /// The files a walk found. Paths longer than a `u16` holds are left
    /// out: no tool names one.
    pub(crate) fn walked(mut found: Vec<Found>) -> Self {
        found.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        found.dedup_by(|a, b| a.path == b.path);
        let mut files = Self {
            arena: String::with_capacity(found.iter().map(|f| f.path.len()).sum()),
            items: Vec::with_capacity(found.len()),
            walked: 0,
        };
        for file in &found {
            files.push(file);
        }
        files.walked = files.items.len();
        files
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "the file arena uses compact offsets checked at its boundary"
    )]
    fn push(&mut self, found: &Found) -> Option<usize> {
        let len = u16::try_from(found.path.len()).ok()?;
        let start = u32::try_from(self.arena.len()).ok()?;
        let name = found.path.rfind('/').map_or(0, |slash| slash + 1) as u16;
        self.arena.push_str(&found.path);
        self.items.push(FileItem {
            start,
            len,
            name,
            size: found.size,
            modified: found.modified,
            deleted: false,
        });
        Some(self.items.len() - 1)
    }

    /// Every slot, deleted ones included: what indexes into the content
    /// index and grep's candidates.
    pub(crate) fn items(&self) -> &[FileItem] {
        &self.items
    }

    /// How many of the slots the last walk filled: the files the content
    /// index covers, if it was built since.
    pub(crate) fn walked_count(&self) -> usize {
        self.walked
    }

    pub(crate) fn live_count(&self) -> usize {
        self.items.iter().filter(|f| !f.deleted).count()
    }

    pub(crate) fn path(&self, file: &FileItem) -> &str {
        let start = file.start as usize;
        &self.arena[start..start + file.len as usize]
    }

    pub(crate) fn name<'a>(&'a self, file: &FileItem) -> &'a str {
        &self.path(file)[file.name as usize..]
    }

    /// The slot of the live file at `path`.
    /// A walked file removed and made again is in a slot after the walked
    /// ones, its walked slot left a tombstone.
    pub(crate) fn find(&self, path: &str) -> Option<usize> {
        let walked = &self.items[..self.walked];
        if let Ok(slot) = walked.binary_search_by(|f| self.path(f).cmp(path))
            && !self.items[slot].deleted
        {
            return Some(slot);
        }
        (self.walked..self.items.len())
            .find(|&i| !self.items[i].deleted && self.path(&self.items[i]) == path)
    }

    /// Records `found`, as new or changed: the slot it is in, and whether
    /// it is new.
    pub(crate) fn upsert(&mut self, found: &Found) -> Option<(usize, bool)> {
        if let Some(slot) = self.find(&found.path) {
            let file = &mut self.items[slot];
            file.size = found.size;
            file.modified = found.modified;
            return Some((slot, false));
        }
        self.push(found).map(|slot| (slot, true))
    }

    /// Marks the file at `path` removed: its slot, if it was there.
    pub(crate) fn remove(&mut self, path: &str) -> Option<usize> {
        let slot = self.find(path)?;
        self.items[slot].deleted = true;
        Some(slot)
    }

    /// Marks the file in `slot` removed.
    pub(crate) fn remove_slot(&mut self, slot: usize) {
        self.items[slot].deleted = true;
    }

    /// Slots of the live files under `dir` (a relative path, no trailing
    /// `/`): what a removed directory takes with it.
    /// The walked files are sorted, so theirs are one range, found by
    /// bisection: `dir/…` sorts from `dir/` to before `dir0`, `0` being
    /// the byte after `/`. Only the few created since are scanned.
    pub(crate) fn under(&self, dir: &str) -> Vec<usize> {
        let walked = &self.items[..self.walked];
        let (from, to) = (format!("{dir}/"), format!("{dir}0"));
        let start = walked.partition_point(|f| self.path(f) < from.as_str());
        let end = walked.partition_point(|f| self.path(f) < to.as_str());
        let within = |path: &str| path.starts_with(from.as_str());
        (start..end)
            .chain((self.walked..self.items.len()).filter(|&i| within(self.path(&self.items[i]))))
            .filter(|&i| !self.items[i].deleted)
            .collect()
    }

    /// Live slots in path order: the mapping used when an idle index compacts.
    pub(crate) fn compact_slots(&self) -> Vec<usize> {
        let mut slots: Vec<_> = (0..self.items.len())
            .filter(|&slot| !self.items[slot].deleted)
            .collect();
        slots.sort_unstable_by(|&a, &b| self.path(&self.items[a]).cmp(self.path(&self.items[b])));
        slots
    }

    pub(crate) fn compact(&self, slots: &[usize]) -> Self {
        Self::walked(
            slots
                .iter()
                .map(|&slot| {
                    let file = &self.items[slot];
                    Found {
                        path: self.path(file).to_owned(),
                        size: file.size,
                        modified: file.modified,
                    }
                })
                .collect(),
        )
    }

    /// What the arena and the slots take.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.arena.capacity() + self.items.capacity() * size_of::<FileItem>()
    }

    pub(crate) fn encode(&self, out: &mut Out<impl Write>) -> io::Result<()> {
        out.bytes(self.arena.as_bytes())?;
        out.len(self.walked)?;
        out.len(self.items.len())?;
        for item in &self.items {
            out.u32(item.start)?;
            out.u32(u32::from(item.len) | u32::from(item.name) << 16)?;
            out.u64(item.size)?;
            out.u64(item.modified)?;
            out.u8(u8::from(item.deleted))?;
        }
        Ok(())
    }

    /// The files [`Files::encode`] wrote, if they are what it writes: each
    /// path within the arena, whole characters, the walked ones sorted.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the file arena uses compact offsets checked at its boundary"
    )]
    pub(crate) fn decode(input: &mut In<impl Read>) -> io::Result<Self> {
        let arena = String::from_utf8(input.bytes()?).map_err(|_| invalid())?;
        let walked = input.usize()?;
        let count = input.len(ITEM_BYTES)?;
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            let start = input.u32()?;
            let lengths = input.u32()?;
            let item = FileItem {
                start,
                len: lengths as u16,
                name: (lengths >> 16) as u16,
                size: input.u64()?,
                modified: input.u64()?,
                deleted: match input.u8()? {
                    0 => false,
                    1 => true,
                    _ => return Err(invalid()),
                },
            };
            let (start, end) = (start as usize, start as usize + item.len as usize);
            if end > arena.len()
                || !arena.is_char_boundary(start)
                || !arena.is_char_boundary(end)
                || item.name > item.len
                || !arena.is_char_boundary(start + item.name as usize)
            {
                return Err(invalid());
            }
            let path = &arena[start..end];
            // Cache paths use the walk's portable slash-separated form.
            if path.is_empty()
                || path.contains(['\\', ':', '\0'])
                || path.split('/').any(|part| matches!(part, "" | "." | ".."))
            {
                return Err(invalid());
            }
            items.push(item);
        }
        let files = Self {
            arena,
            items,
            walked,
        };
        let sorted = (files.items.get(..walked).ok_or_else(invalid)?)
            .windows(2)
            .all(|pair| files.path(&pair[0]) < files.path(&pair[1]));
        if !sorted {
            return Err(invalid());
        }
        Ok(files)
    }
}

/// What [`Files::encode`] writes of each file.
const ITEM_BYTES: u64 = 4 + 4 + 8 + 8 + 1;

impl FileItem {
    pub(crate) fn is_deleted(&self) -> bool {
        self.deleted
    }

    pub(crate) fn name_offset(&self) -> usize {
        self.name as usize
    }
}

/// Most recently modified first, then by path: the order files are shown
/// and searched in when nothing else ranks them.
pub(crate) fn recent_first(files: &Files, a: &FileItem, b: &FileItem) -> Ordering {
    b.modified
        .cmp(&a.modified)
        .then_with(|| files.path(a).cmp(files.path(b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flood_of_changes_under_one_directory_never_scans_every_file() {
        let mut files = Files::walked(
            (0..100_000)
                .map(|i| found(&format!("src/m{}/f{i}.rs", i % 100)))
                .collect(),
        );
        files.upsert(&found("target/new.rs"));
        let started = std::time::Instant::now();
        for i in 0..5000 {
            assert_eq!(files.remove(&format!("target/debug/{i}.o")), None);
            assert_eq!(files.under(&format!("target/debug/{i}")), [0_usize; 0]);
        }
        // A scan of every file per change takes seconds here.
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(files.under("target"), [100_000]);
        assert_eq!(files.under("src/m7").len(), 1000);
        assert_eq!(files.under("src/m").len(), 0);
    }

    fn found(path: &str) -> Found {
        Found {
            path: path.to_owned(),
            size: 1,
            modified: 0,
        }
    }

    #[test]
    fn a_file_is_found_by_its_path_until_it_is_removed() {
        let mut files = Files::walked(vec![found("src/b.rs"), found("a.rs"), found("src/a.rs")]);
        assert_eq!(files.find("src/a.rs"), Some(1));
        assert_eq!(files.name(&files.items()[1]), "a.rs");
        assert_eq!(files.upsert(&found("new.rs")), Some((3, true)));
        assert_eq!(files.upsert(&found("a.rs")), Some((0, false)));
        assert_eq!(files.find("new.rs"), Some(3));
        assert_eq!(files.under("src"), [1, 2]);
        assert_eq!(files.remove("src/a.rs"), Some(1));
        assert_eq!(files.find("src/a.rs"), None);
        assert_eq!(files.live_count(), 3);
        // Back after a removal: a new slot, the old one stays a tombstone,
        // and it is found in its new one from then on.
        assert_eq!(files.upsert(&found("src/a.rs")), Some((4, true)));
        assert_eq!(files.upsert(&found("src/a.rs")), Some((4, false)));
        assert_eq!(files.live_count(), 4);
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn cached_paths_cannot_escape_or_change_meaning_when_joined() {
        for path in [
            "",
            "/etc/passwd",
            "../outside",
            "src/../../outside",
            "src/./file",
            "src//file",
            "C:/outside",
            "\\\\server\\share",
            "src\\..\\outside",
            "src/",
            "nul\0path",
        ] {
            let files = Files::walked(vec![Found {
                path: path.into(),
                size: 1,
                modified: 1,
            }]);
            let bytes = crate::disk::encoded(|out| files.encode(out));
            assert!(
                crate::disk::decoded(&bytes, Files::decode).is_err(),
                "{path:?}"
            );
        }
        let files = Files::walked(vec![Found {
            path: "src/λ.rs".into(),
            size: 1,
            modified: 1,
        }]);
        let bytes = crate::disk::encoded(|out| files.encode(out));
        assert!(crate::disk::decoded(&bytes, Files::decode).is_ok());
    }
}
