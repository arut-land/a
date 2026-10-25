//! The files of a workspace: relative paths, `/`-separated on every
//! platform, in one contiguous arena, and what is known of each.
//!
//! FFF kept paths as deduplicated 16-byte chunks behind raw pointers so
//! its SIMD scorer could read them in place; one `String` is as cheap for
//! a tree of real paths and needs no `unsafe` to read.

use std::cmp::Ordering;

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
        for file in found {
            files.push(file);
        }
        files.walked = files.items.len();
        files
    }

    fn push(&mut self, found: Found) -> Option<usize> {
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

    /// Its directory, with the trailing `/`; empty at the root.
    pub(crate) fn dir<'a>(&'a self, file: &FileItem) -> &'a str {
        &self.path(file)[..file.name as usize]
    }

    /// The slot of the live file at `path`.
    pub(crate) fn find(&self, path: &str) -> Option<usize> {
        let walked = &self.items[..self.walked];
        let found = walked
            .binary_search_by(|f| self.path(f).cmp(path))
            .ok()
            .or_else(|| {
                (self.walked..self.items.len()).find(|&i| self.path(&self.items[i]) == path)
            })?;
        (!self.items[found].deleted).then_some(found)
    }

    /// Records `found`, as new or changed: the slot it is in, and whether
    /// it is new.
    pub(crate) fn upsert(&mut self, found: Found) -> Option<(usize, bool)> {
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
    pub(crate) fn under(&self, dir: &str) -> Vec<usize> {
        let within = |path: &str| {
            path.strip_prefix(dir)
                .is_some_and(|rest| rest.starts_with('/'))
        };
        (0..self.items.len())
            .filter(|&i| !self.items[i].deleted && within(self.path(&self.items[i])))
            .collect()
    }

    /// What the arena and the slots take.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.arena.capacity() + self.items.capacity() * size_of::<FileItem>()
    }
}

impl FileItem {
    pub(crate) fn is_deleted(&self) -> bool {
        self.deleted
    }

    pub(crate) fn path_len(&self) -> usize {
        self.len as usize
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
        assert_eq!(files.dir(&files.items()[1]), "src/");
        assert_eq!(files.upsert(found("new.rs")), Some((3, true)));
        assert_eq!(files.upsert(found("a.rs")), Some((0, false)));
        assert_eq!(files.find("new.rs"), Some(3));
        assert_eq!(files.under("src"), [1, 2]);
        assert_eq!(files.remove("src/a.rs"), Some(1));
        assert_eq!(files.find("src/a.rs"), None);
        assert_eq!(files.live_count(), 3);
        // Back after a removal: a new slot, the old one stays a tombstone.
        assert_eq!(files.upsert(found("src/a.rs")), Some((4, true)));
    }
}
