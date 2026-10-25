//! The content prefilter: an inverted index from each printable bigram
//! (and each pair one byte apart, the "skip-1" bigrams) to the files that
//! hold it, ANDed per query so grep reads only files that can match.
//!
//! FFF built it from many threads writing one shared slab through raw
//! pointers. Here a builder is owned by the one job that fills it: the
//! index is built in chunks of files, each chunk its own builder, merged
//! into the whole by bit position.

use std::collections::HashMap;

/// Maximum number of distinct bigrams tracked in the inverted index.
/// 95 printable ASCII chars (32..=126) after lowercasing → ~70 distinct → 4900 possible.
/// We cap at 5000 to cover all printable bigrams with margin.
const MAX_BIGRAM_COLUMNS: usize = 5000;

/// Sentinel value: bigram has no allocated column.
const NO_COLUMN: u16 = u16::MAX;

/// Bigram keys only ever pair printable bytes (32..=126) which is 95 ^ 2
pub const BIGRAM_KEY_SLOTS: usize = 95 * 95;

// Slot in the compact lookup for a printable bigram key (`hi << 8 | lo`).
#[inline(always)]
fn key_slot(key: u16) -> usize {
    let hi = (key >> 8) as usize;
    let lo = (key & 0xFF) as usize;
    debug_assert!((32..=126).contains(&hi) && (32..=126).contains(&lo));
    (hi - 32) * 95 + (lo - 32)
}

/// 1024 × u64 = 8 KB covers all 65536 possible bigram keys.
const SEEN_WORDS: usize = 1024;

/// Content size where the branchless two-pass `add_long_content` overtakes
/// the single-pass `add_short_content`: ~-35% on 4 KB files, but its fixed
/// flush scan dominates files under ~1 KB.
const LONG_CONTENT_MIN_LEN: usize = 1024;

/// Sparse-column cutoff for the skip-1 sub-index. Rare skip columns add
/// little filtering power but ~25-30% of index memory, so we drop
/// anything appearing in < 12 % of populated files.
pub(crate) const SKIP_INDEX_MIN_DENSITY_PCT: u32 = 12;

/// The most the builders of `file_count` files can take: every column of
/// both, dense. What admission weighs before building.
pub(crate) fn build_bytes(file_count: usize) -> usize {
    2 * MAX_BIGRAM_COLUMNS * file_count.div_ceil(64) * size_of::<u64>()
}

/// A dense bigram index being built: one bitset column per bigram seen,
/// one bit per file.
pub struct BigramIndexBuilder {
    lookup: Vec<u16>,
    columns: Vec<Box<[u64]>>,
    words: usize,
    file_count: usize,
    populated: usize,
    /// Scratch for normalised content, reused across files.
    normalized: Vec<u8>,
}

impl BigramIndexBuilder {
    pub fn new(file_count: usize) -> Self {
        Self {
            lookup: vec![NO_COLUMN; BIGRAM_KEY_SLOTS],
            columns: Vec::new(),
            words: file_count.div_ceil(64),
            file_count,
            populated: 0,
            normalized: Vec::new(),
        }
    }

    #[inline]
    fn get_or_alloc_column(&mut self, key: u16) -> u16 {
        let slot = key_slot(key);
        let current = self.lookup[slot];
        if current != NO_COLUMN {
            return current;
        }
        if self.columns.len() >= MAX_BIGRAM_COLUMNS {
            return NO_COLUMN;
        }
        let col = self.columns.len() as u16;
        self.columns.push(vec![0; self.words].into_boxed_slice());
        self.lookup[slot] = col;
        col
    }

    #[inline]
    fn set(&mut self, key: u16, word_idx: usize, bit_mask: u64) {
        let col = self.get_or_alloc_column(key);
        if col != NO_COLUMN {
            self.columns[col as usize][word_idx] |= bit_mask;
        }
    }

    /// Records the bigrams of `content`, file `file_idx`'s, in `self` and
    /// its skip-1 pairs in `skip_builder`.
    pub fn add_file_content(&mut self, skip_builder: &mut Self, file_idx: usize, content: &[u8]) {
        if content.len() < 2 {
            return;
        }

        debug_assert!(file_idx < self.file_count);
        let word_idx = file_idx / 64;
        let bit_mask = 1u64 << (file_idx % 64);

        let mut n = std::mem::take(&mut self.normalized);
        n.clear();
        n.extend(content.iter().map(|&b| normalize_byte_scalar(b)));

        // Both paths record the identical bigram set; the split exists
        // purely for speed (see LONG_CONTENT_MIN_LEN).
        if n.len() >= LONG_CONTENT_MIN_LEN {
            self.add_long_content(skip_builder, &n, word_idx, bit_mask);
        } else {
            self.add_short_content(skip_builder, &n, word_idx, bit_mask);
        }
        self.normalized = n;

        self.populated += 1;
        skip_builder.populated += 1;
    }

    // Branchless two-pass: set every pair in stack-local bitmaps, including
    // pairs touching the 0 sentinel — flush_seen masks those out.
    fn add_long_content(
        &mut self,
        skip_builder: &mut Self,
        n: &[u8],
        word_idx: usize,
        bit_mask: u64,
    ) {
        // Stack-local dedup bitsets: 1024 × u64 = 8 KB each, covers all 65536
        // bigram keys. Has to fit in L1 cache.
        let mut seen_consec = [0u64; SEEN_WORDS];
        let mut seen_skip = [0u64; SEEN_WORDS];

        let mut n0 = n[0];
        let mut n1 = n[1];

        let key = (n0 as usize) << 8 | n1 as usize;
        seen_consec[key >> 6] |= 1u64 << (key & 63);

        for &cur in &n[2..] {
            let ck = (n1 as usize) << 8 | cur as usize;
            let sk = (n0 as usize) << 8 | cur as usize;
            seen_consec[ck >> 6] |= 1u64 << (ck & 63);
            seen_skip[sk >> 6] |= 1u64 << (sk & 63);

            n0 = n1;
            n1 = cur;
        }

        self.flush_seen(&seen_consec, word_idx, bit_mask);
        skip_builder.flush_seen(&seen_skip, word_idx, bit_mask);
    }

    fn add_short_content(
        &mut self,
        skip_builder: &mut Self,
        n: &[u8],
        word_idx: usize,
        bit_mask: u64,
    ) {
        let mut seen_consec = [0u64; SEEN_WORDS];
        let mut seen_skip = [0u64; SEEN_WORDS];

        let mut n0 = n[0];
        let mut n1 = n[1];

        if n0 != 0 && n1 != 0 {
            let key = (n0 as u16) << 8 | n1 as u16;
            self.record_bigram(&mut seen_consec, key, word_idx, bit_mask);
        }

        for &cur in &n[2..] {
            if cur != 0 {
                if n1 != 0 {
                    let key = (n1 as u16) << 8 | cur as u16;
                    self.record_bigram(&mut seen_consec, key, word_idx, bit_mask);
                }
                if n0 != 0 {
                    let key = (n0 as u16) << 8 | cur as u16;
                    skip_builder.record_bigram(&mut seen_skip, key, word_idx, bit_mask);
                }
            }
            n0 = n1;
            n1 = cur;
        }
    }

    #[inline]
    fn record_bigram(
        &mut self,
        seen: &mut [u64; SEEN_WORDS],
        key: u16,
        word_idx: usize,
        bit_mask: u64,
    ) {
        let k = key as usize;
        let w = k >> 6;
        let bit = 1u64 << (k & 63);
        if seen[w] & bit == 0 {
            seen[w] |= bit;
            self.set(key, word_idx, bit_mask);
        }
    }

    fn flush_seen(&mut self, seen: &[u64; SEEN_WORDS], word_idx: usize, bit_mask: u64) {
        // SEEN_WORDS is a multiple of 8, so the remainder is always empty.
        for (blk, block) in seen.as_chunks::<8>().0.iter().enumerate() {
            // OR-test whole blocks so the mostly-empty bitmap scans fast.
            if block.iter().fold(0u64, |a, &w| a | w) == 0 {
                continue;
            }
            for (j, &word_bits) in block.iter().enumerate() {
                let w = blk * 8 + j;
                // Keys below 0x0100 and those whose low byte is 0 pair a
                // byte with the non-printable sentinel.
                let mut bits = match w & 3 {
                    _ if w < 4 => 0,
                    0 => word_bits & !1,
                    _ => word_bits,
                };
                while bits != 0 {
                    let key = (w << 6 | bits.trailing_zeros() as usize) as u16;
                    bits &= bits - 1;
                    self.set(key, word_idx, bit_mask);
                }
            }
        }
    }

    /// ORs `chunk`, a builder of the files from `first` on (a multiple of
    /// 64), into this one.
    pub(crate) fn merge(&mut self, chunk: &Self, first: usize) {
        debug_assert_eq!(first % 64, 0);
        let start = first / 64;
        for (slot, &col) in chunk.lookup.iter().enumerate() {
            if col == NO_COLUMN {
                continue;
            }
            let key = (((slot / 95) + 32) << 8 | ((slot % 95) + 32)) as u16;
            let into = self.get_or_alloc_column(key);
            if into == NO_COLUMN {
                continue;
            }
            let from = &chunk.columns[col as usize];
            for (dst, src) in self.columns[into as usize][start..]
                .iter_mut()
                .zip(from.iter())
            {
                *dst |= src;
            }
        }
        self.populated += chunk.populated;
    }

    pub fn is_ready(&self) -> bool {
        self.populated > 0
    }

    pub fn columns_used(&self) -> u16 {
        self.columns.len() as u16
    }

    /// What its columns take now.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.columns.len() * self.words * size_of::<u64>()
    }

    #[cfg(test)]
    fn column_bitset(&self, col: u16) -> &[u64] {
        &self.columns[col as usize]
    }

    /// Compress the dense builder into a compact `BigramFilter`.
    ///
    /// Keeps rare columns as gap lists unless `min_density_pct` excludes them.
    /// Ubiquitous columns (≥90%) are omitted because they barely filter.
    pub fn compress(self, min_density_pct: Option<u32>) -> BigramFilter {
        let words = self.words;
        let file_count = self.file_count;
        let populated = self.populated;
        let dense_bytes = words * 8; // cost of one dense column
        let mut columns: Vec<Option<Box<[u64]>>> = self.columns.into_iter().map(Some).collect();

        // Pass 1: pick the columns worth keeping.
        let mut kept: Vec<(usize, u16, u32)> = Vec::new();
        for (slot, &col) in self.lookup.iter().enumerate() {
            if col == NO_COLUMN {
                continue;
            }
            let Some(bitset) = columns[col as usize].as_deref() else {
                continue;
            };
            let popcount: u32 = bitset.iter().map(|w| w.count_ones()).sum();

            if min_density_pct.is_some_and(|min_pct| {
                populated == 0 || (popcount as usize) * 100 < populated * min_pct as usize
            }) {
                continue;
            }

            // Drop ubiquitous bigrams — columns ≥90% ones carry almost no
            // filtering power and just waste memory + AND cycles.
            if populated > 0 && (popcount as usize) * 10 >= populated * 9 {
                continue;
            }

            kept.push((slot, col, popcount));
        }
        kept.sort_unstable_by_key(|&(_, col, _)| col);
        let mut rare_keys = vec![0u64; BIGRAM_KEY_SLOTS.div_ceil(64)];
        if min_density_pct.is_none() {
            for &(slot, _, count) in &kept {
                if (count as usize) * 4 < dense_bytes {
                    rare_keys[slot / 64] |= 1 << (slot % 64);
                }
            }
        }

        // Pass 2: low-density columns become gap lists when that encodes
        // smaller than a dense bitset; the rest stay as they are.
        let mut lookup: Vec<u16> = vec![NO_COLUMN; BIGRAM_KEY_SLOTS];
        let mut dense: Vec<Box<[u64]>> = Vec::new();
        let mut sparse_slots: Vec<usize> = Vec::new();
        let mut sparse_offsets: Vec<u32> = vec![0];
        let mut sparse_data: Vec<u8> = Vec::new();

        for &(slot, col, popcount) in &kept {
            let Some(bitset) = columns[col as usize].take() else {
                continue;
            };
            if (popcount as usize) < dense_bytes {
                let mut out = Vec::with_capacity(popcount as usize + 8);
                encode_sparse_column(&bitset, &mut out);
                if out.len() < dense_bytes {
                    sparse_slots.push(slot);
                    sparse_data.extend_from_slice(&out);
                    sparse_offsets.push(sparse_data.len() as u32);
                    continue;
                }
            }
            lookup[slot] = dense.len() as u16;
            dense.push(bitset);
        }
        drop(columns);

        for (i, slot) in sparse_slots.into_iter().enumerate() {
            lookup[slot] = (dense.len() + i) as u16;
        }
        sparse_data.shrink_to_fit();
        dense.shrink_to_fit();

        BigramFilter {
            rare_keys,
            lookup,
            dense,
            sparse_offsets,
            sparse_data,
            words,
            file_count,
            populated,
            skip_index: None,
        }
    }
}

// Append the set bit positions of `bitset` as LEB128 gaps.
fn encode_sparse_column(bitset: &[u64], out: &mut Vec<u8>) {
    let mut prev = 0usize;
    for (w, &word) in bitset.iter().enumerate() {
        let mut bits = word;
        while bits != 0 {
            let pos = w * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let mut gap = pos - prev;
            prev = pos;
            while gap >= 0x80 {
                out.push((gap as u8) | 0x80);
                gap >>= 7;
            }
            out.push(gap as u8);
        }
    }
}

// `result &= column` for a sparse column: sorted positions are merged in one
// pass, zeroing every word the column leaves untouched.
fn and_sparse_column(result: &mut [u64], data: &[u8]) {
    let mut word = 0usize;
    let mut mask = 0u64;
    let mut pos = 0usize;
    let mut i = 0usize;
    while i < data.len() {
        let mut gap = 0usize;
        let mut shift = 0;
        loop {
            let b = data[i];
            i += 1;
            gap |= ((b & 0x7F) as usize) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        pos += gap;
        let w = pos >> 6;
        if w != word {
            if word < result.len() {
                result[word] &= mask;
            }
            let end = w.min(result.len());
            result[(word + 1).min(end)..end].fill(0);
            word = w;
            mask = 0;
        }
        mask |= 1u64 << (pos & 63);
    }
    if word < result.len() {
        result[word] &= mask;
        result[word + 1..].fill(0);
    }
}

/// One bigram column: a stride-`words` dense bitset or a varint gap list.
pub(crate) enum ColumnRef<'a> {
    Dense(&'a [u64]),
    Sparse(&'a [u8]),
}

/// Inverted bigram index with optional "skip-1" extension
/// Copmressed into bitset for minimal usage, the layout of this struct actually matters
#[derive(Debug)]
pub struct BigramFilter {
    lookup: Vec<u16>,
    rare_keys: Vec<u64>,
    /// The dense columns, `words` long each: one allocation per column,
    /// so compressing never copies the builder's columns into a new slab.
    dense: Vec<Box<[u64]>>,
    /// Sparse columns are numbered after the dense ones: column `dense.len() + i`
    /// lives at `sparse_data[sparse_offsets[i]..sparse_offsets[i + 1]]`.
    sparse_offsets: Vec<u32>,
    sparse_data: Vec<u8>,
    words: usize,
    file_count: usize,
    populated: usize,
    /// Optional skip-1 bigram index (stride 2). Built from character pairs
    /// at distance 2, e.g. "ABCDE" → (A,C),(B,D),(C,E). ANDead with the
    /// consecutive bigram candidates during query to dramatically reduce
    /// false positives.
    skip_index: Option<Box<BigramFilter>>,
}

/// SIMD-friendly bitwise AND of two equal-length bitsets.
// Auto vectorized (don't touch)
#[inline]
fn bitset_and(result: &mut [u64], bitset: &[u64]) {
    result
        .iter_mut()
        .zip(bitset.iter())
        .for_each(|(r, b)| *r &= *b);
}

impl BigramFilter {
    /// AND the posting lists for all query bigrams (consecutive + skip).
    /// Returns None if no query bigrams are tracked.
    pub fn query(&self, pattern: &[u8]) -> Option<Vec<u64>> {
        if pattern.len() < 2 {
            return None;
        }

        let mut result = vec![u64::MAX; self.words];
        if !self.file_count.is_multiple_of(64) {
            let last = self.words - 1;
            result[last] = (1u64 << (self.file_count % 64)) - 1;
        }

        let mut has_filter = false;

        let mut prev = pattern[0];
        for &b in &pattern[1..] {
            if (32..=126).contains(&prev) && (32..=126).contains(&b) {
                let key = (prev.to_ascii_lowercase() as u16) << 8 | b.to_ascii_lowercase() as u16;
                if let Some(col) = self.column_ref(key) {
                    Self::and_column(&mut result, col);
                    has_filter = true;
                }
            }
            prev = b;
        }

        // strid-1 bigrams
        if let Some(skip) = &self.skip_index
            && pattern.len() >= 3
            && let Some(skip_candidates) = skip.query_skip(pattern)
        {
            bitset_and(&mut result, &skip_candidates);
            has_filter = true;
        }

        has_filter.then_some(result)
    }

    /// Query using stride-2 bigrams from the pattern.
    /// For "ABCDE" queries with keys (A,C), (B,D), (C,E).
    fn query_skip(&self, pattern: &[u8]) -> Option<Vec<u64>> {
        let mut result = vec![u64::MAX; self.words];
        if !self.file_count.is_multiple_of(64) {
            let last = self.words - 1;
            result[last] = (1u64 << (self.file_count % 64)) - 1;
        }

        let mut has_filter = false;

        for i in 0..pattern.len().saturating_sub(2) {
            let a = pattern[i];
            let b = pattern[i + 2];
            if (32..=126).contains(&a) && (32..=126).contains(&b) {
                let key = (a.to_ascii_lowercase() as u16) << 8 | b.to_ascii_lowercase() as u16;
                if let Some(col) = self.column_ref(key) {
                    Self::and_column(&mut result, col);
                    has_filter = true;
                }
            }
        }

        has_filter.then_some(result)
    }

    /// Resolve a bigram key to its stored column, if the index kept one.
    #[inline]
    pub(crate) fn column_ref(&self, key: u16) -> Option<ColumnRef<'_>> {
        let col = self.column(key);
        if col == NO_COLUMN {
            return None;
        }
        let col = col as usize;
        if let Some(dense) = self.dense.get(col) {
            Some(ColumnRef::Dense(dense))
        } else {
            let i = col - self.dense.len();
            let start = *self.sparse_offsets.get(i)? as usize;
            let end = *self.sparse_offsets.get(i + 1)? as usize;
            self.sparse_data.get(start..end).map(ColumnRef::Sparse)
        }
    }

    #[inline]
    pub(crate) fn and_column(result: &mut [u64], col: ColumnRef<'_>) {
        match col {
            ColumnRef::Dense(bits) => bitset_and(result, bits),
            ColumnRef::Sparse(data) => and_sparse_column(result, data),
        }
    }

    // Regex and fuzzy filters keep their original columns; literals can use rare ones.
    pub(crate) fn common_column_bitset(&self, key: u16) -> Option<std::borrow::Cow<'_, [u64]>> {
        if self.column(key) == NO_COLUMN {
            return None;
        }
        let slot = key_slot(key);
        if self.rare_keys[slot / 64] & (1 << (slot % 64)) != 0 {
            return None;
        }
        self.column_bitset(key)
    }

    /// Column as a materialized bitset (borrowed for dense, decoded for sparse).
    pub(crate) fn column_bitset(&self, key: u16) -> Option<std::borrow::Cow<'_, [u64]>> {
        Some(match self.column_ref(key)? {
            ColumnRef::Dense(bits) => std::borrow::Cow::Borrowed(bits),
            ColumnRef::Sparse(data) => {
                let mut bits = vec![u64::MAX; self.words];
                and_sparse_column(&mut bits, data);
                std::borrow::Cow::Owned(bits)
            }
        })
    }

    /// Attach a skip-1 bigram index for tighter candidate filtering.
    pub fn set_skip_index(&mut self, skip: BigramFilter) {
        self.skip_index = Some(Box::new(skip));
    }

    #[inline]
    pub fn is_candidate(candidates: &[u64], file_idx: usize) -> bool {
        let word = file_idx / 64;
        let bit = file_idx % 64;
        word < candidates.len() && candidates[word] & (1u64 << bit) != 0
    }

    pub fn count_candidates(candidates: &[u64]) -> usize {
        candidates.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn is_ready(&self) -> bool {
        self.populated > 0
    }

    pub fn file_count(&self) -> usize {
        self.file_count
    }

    pub fn columns_used(&self) -> usize {
        self.dense.len() + self.sparse_count()
    }

    /// Number of columns stored as varint gap lists.
    pub fn sparse_count(&self) -> usize {
        self.sparse_offsets.len().saturating_sub(1)
    }

    /// Bytes held by the sparse (gap-encoded) columns.
    pub fn sparse_bytes(&self) -> usize {
        self.sparse_data.len() + self.sparse_offsets.len() * std::mem::size_of::<u32>()
    }

    /// Total heap bytes used by this index (lookup + dense + sparse + skip).
    pub fn heap_bytes(&self) -> usize {
        let lookup_bytes = self.lookup.len() * std::mem::size_of::<u16>();
        let dense_bytes = self.dense.len() * self.words * size_of::<u64>();
        let skip_bytes = self.skip_index.as_ref().map_or(0, |s| s.heap_bytes());
        lookup_bytes + self.rare_keys.len() * 8 + dense_bytes + self.sparse_bytes() + skip_bytes
    }

    /// Check whether a bigram key is present in this index.
    pub fn has_key(&self, key: u16) -> bool {
        self.column(key) != NO_COLUMN
    }

    /// Dense column for a printable bigram key, or `u16::MAX` when absent.
    #[inline]
    pub fn column(&self, key: u16) -> u16 {
        let hi = key >> 8;
        let lo = key & 0xFF;
        if !(32..=126).contains(&hi) || !(32..=126).contains(&lo) {
            return NO_COLUMN;
        }
        self.lookup[key_slot(key)]
    }

    /// The optional skip-1 bigram sub-index.
    pub fn skip_index(&self) -> Option<&BigramFilter> {
        self.skip_index.as_deref()
    }

    #[cfg(test)]
    fn dense_count(&self) -> usize {
        self.dense.len()
    }
}

/// Single-byte normalize: 0 for non-printable, lowercased byte otherwise.
/// 0 is a safe sentinel: lowered printable bytes are 32..=126.
#[inline(always)]
fn normalize_byte_scalar(b: u8) -> u8 {
    let printable = b.wrapping_sub(32) <= 94;
    let lower = b | ((b.wrapping_sub(b'A') < 26) as u8 * 0x20);
    if printable { lower } else { 0 }
}

pub fn extract_bigrams(content: &[u8]) -> Vec<u16> {
    if content.len() < 2 {
        return Vec::new();
    }
    // Use a flat bitset (65536 bits = 8 KB) for dedup — faster than HashSet.
    let mut seen = vec![0u64; 1024]; // 1024 * 64 = 65536 bits
    let mut bigrams = Vec::new();

    let mut prev = content[0];
    for &b in &content[1..] {
        if (32..=126).contains(&prev) && (32..=126).contains(&b) {
            let key = (prev.to_ascii_lowercase() as u16) << 8 | b.to_ascii_lowercase() as u16;
            let word = key as usize / 64;
            let bit = 1u64 << (key as usize % 64);
            if seen[word] & bit == 0 {
                seen[word] |= bit;
                bigrams.push(key);
            }
        }
        prev = b;
    }
    bigrams
}

/// Modified and added files store their own bigram sets. Deleted files are
/// tombstoned in a bitset so they can be excluded from base query results.
/// This overlay is updated by the background watcher on every file event
/// and cleared when the base index is rebuilt.
#[derive(Clone, Debug)]
pub struct BigramOverlay {
    /// Per-file bigram sets for files modified since the base was built.
    /// Key = file index in the base `Vec<FileItem>`.
    modified: HashMap<usize, Vec<u16>>,

    /// Tombstone bitset — one bit per base file. Set bits are excluded
    /// from base query results.
    tombstones: Vec<u64>,

    /// Original files count this overlay was created for.
    base_file_count: usize,
}

impl BigramOverlay {
    pub(crate) fn new(base_file_count: usize) -> Self {
        let words = base_file_count.div_ceil(64);
        Self {
            modified: HashMap::new(),
            tombstones: vec![0u64; words],
            base_file_count,
        }
    }

    /// Records the bigrams a modified file holds now.
    pub(crate) fn modify_bigrams(&mut self, file_idx: usize, bigrams: Vec<u16>) {
        self.modified.insert(file_idx, bigrams);
    }

    pub(crate) fn delete_file(&mut self, file_idx: usize) {
        if file_idx < self.base_file_count {
            let word = file_idx / 64;
            self.tombstones[word] |= 1u64 << (file_idx % 64);
        }
        self.modified.remove(&file_idx);
    }

    /// Return base file indices of modified files whose bigrams match ALL
    /// of the given `pattern_bigrams`.
    pub(crate) fn query_modified(&self, pattern_bigrams: &[u16]) -> Vec<usize> {
        if pattern_bigrams.is_empty() {
            return self.modified.keys().copied().collect();
        }
        self.modified
            .iter()
            .filter_map(|(&file_idx, bigrams)| {
                pattern_bigrams
                    .iter()
                    .all(|pb| bigrams.contains(pb))
                    .then_some(file_idx)
            })
            .collect()
    }

    /// Number of base files this overlay was created for.
    pub(crate) fn base_file_count(&self) -> usize {
        self.base_file_count
    }

    /// Get the tombstone bitset for clearing base candidates.
    pub(crate) fn tombstones(&self) -> &[u64] {
        &self.tombstones
    }

    /// Get all modified file indices (for conservative overlay merging when
    /// we can't extract precise bigrams, e.g. regex patterns).
    pub(crate) fn modified_indices(&self) -> Vec<usize> {
        self.modified.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sparse_roundtrip(bitset: &[u64]) {
        let mut data = Vec::new();
        encode_sparse_column(bitset, &mut data);
        let mut got = vec![u64::MAX; bitset.len()];
        and_sparse_column(&mut got, &data);
        assert_eq!(got, bitset);

        // AND semantics against an arbitrary partner bitset
        let partner: Vec<u64> = (0..bitset.len() as u64)
            .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5555_5555_5555_5555)
            .collect();
        let mut anded = partner.clone();
        and_sparse_column(&mut anded, &data);
        let expected: Vec<u64> = partner.iter().zip(bitset).map(|(a, b)| a & b).collect();
        assert_eq!(anded, expected);
    }

    #[test]
    fn rare_bigrams_reject_absent_queries_without_losing_matches() {
        let mut builder = BigramIndexBuilder::new(4096);
        let mut skip = BigramIndexBuilder::new(4096);
        for file_idx in 0..4096 {
            let content: &[u8] = match file_idx {
                31 => b"marker Qq blah",
                500 => b"marker qz blah",
                1000 => b"marker zy blah",
                3000 => b"marker yx blah",
                _ => b"ordinary content",
            };
            builder.add_file_content(&mut skip, file_idx, content);
        }
        let index = builder.compress(None);
        let absent = index.query(b"Qqzyx").expect("rare bigrams retained");
        assert_eq!(BigramFilter::count_candidates(&absent), 0);
        let present = index.query(b"qq").unwrap();
        assert_eq!(BigramFilter::count_candidates(&present), 1);
        assert!(BigramFilter::is_candidate(&present, 31));
        assert!(index.common_column_bitset(key(b'q', b'q')).is_none());
        assert!(index.sparse_count() > 0);
        assert!(!skip.compress(Some(12)).has_key(key(b'q', b' ')));
    }

    #[test]
    fn sparse_column_roundtrip_variants() {
        sparse_roundtrip(&[]);
        sparse_roundtrip(&[0]);
        sparse_roundtrip(&[1]);
        sparse_roundtrip(&[1 << 63]);
        sparse_roundtrip(&[0, 0, 0, 1 << 5, 0, 0]);
        sparse_roundtrip(&[u64::MAX, u64::MAX]);
        // gaps above 127 exercise multi-byte varints
        let mut wide = vec![0u64; 64];
        wide[0] = 1;
        wide[10] = 1 << 3;
        wide[63] = 1 << 63;
        sparse_roundtrip(&wide);
        let pseudo: Vec<u64> = (0..37u64)
            .map(|i| i.wrapping_mul(0xD1B5_4A32_D192_ED03))
            .collect();
        sparse_roundtrip(&pseudo);
    }

    #[test]
    fn compress_picks_sparse_for_rare_bigrams_and_queries_agree() {
        // 4096 files: "zq" in 5% of them (sparse), "ab" in 50% (dense)
        let n = 4096;
        let mut consec = BigramIndexBuilder::new(n);
        let mut skip = BigramIndexBuilder::new(n);
        for i in 0..n {
            let mut content = String::from("padding text ");
            if i % 20 == 0 {
                content.push_str("zq");
            }
            if i % 2 == 0 {
                content.push_str(" ab");
            }
            consec.add_file_content(&mut skip, i, content.as_bytes());
        }
        let index = consec.compress(Some(1));
        assert!(index.sparse_count() >= 1, "rare column should be sparse");
        assert!(index.dense_count() >= 1, "common column should stay dense");

        let zq = index.query(b"zq").expect("zq tracked");
        for i in 0..n {
            assert_eq!(BigramFilter::is_candidate(&zq, i), i % 20 == 0, "file {i}");
        }
        let ab = index.query(b"ab").expect("ab tracked");
        for i in 0..n {
            assert_eq!(BigramFilter::is_candidate(&ab, i), i % 2 == 0, "file {i}");
        }
        let both = index.query(b"zq ab").expect("tracked");
        for i in 0..n {
            assert_eq!(
                BigramFilter::is_candidate(&both, i),
                i % 20 == 0,
                "file {i}"
            );
        }
    }

    /// Build a key the same way `add_file_content` does: two printable-ASCII
    /// bytes, lowercased, packed as `(hi << 8) | lo`.
    fn key(a: u8, b: u8) -> u16 {
        ((a.to_ascii_lowercase() as u16) << 8) | b.to_ascii_lowercase() as u16
    }

    /// Return the sorted list of (consec, skip) bigram keys that should appear
    /// for `content`. Used as the reference implementation.
    fn expected_bigrams(content: &[u8]) -> (Vec<u16>, Vec<u16>) {
        let mut consec: std::collections::BTreeSet<u16> = Default::default();
        let mut skip: std::collections::BTreeSet<u16> = Default::default();
        let printable = |b: u8| (32..=126).contains(&b);
        for i in 1..content.len() {
            let a = content[i - 1];
            let b = content[i];
            if printable(a) && printable(b) {
                consec.insert(key(a, b));
            }
            if i >= 2 {
                let a = content[i - 2];
                let b = content[i];
                if printable(a) && printable(b) {
                    skip.insert(key(a, b));
                }
            }
        }
        (consec.into_iter().collect(), skip.into_iter().collect())
    }

    /// Query: does the builder record file 0 as having this bigram set?
    fn builder_has_key_for_file_0(b: &BigramIndexBuilder, k: u16) -> bool {
        if (k >> 8) < 32 || (k >> 8) > 126 || (k & 0xFF) < 32 || (k & 0xFF) > 126 {
            return false;
        }
        let col = b.lookup[key_slot(k)];
        if col == NO_COLUMN {
            return false;
        }
        b.column_bitset(col)[0] & 1 != 0
    }

    fn run_and_compare(content: &[u8]) {
        let mut consec = BigramIndexBuilder::new(1);
        let mut skip = BigramIndexBuilder::new(1);
        consec.add_file_content(&mut skip, 0, content);

        let (expected_consec, expected_skip) = expected_bigrams(content);

        // Every expected bigram must be recorded.
        for k in &expected_consec {
            assert!(
                builder_has_key_for_file_0(&consec, *k),
                "consec bigram 0x{k:04x} missing for content {content:?}",
            );
        }
        for k in &expected_skip {
            assert!(
                builder_has_key_for_file_0(&skip, *k),
                "skip bigram 0x{k:04x} missing for content {content:?}",
            );
        }

        // No unexpected bigrams — iterate lookup for set columns.
        for k in 0u32..=0xFFFF {
            let recorded_consec = builder_has_key_for_file_0(&consec, k as u16);
            let recorded_skip = builder_has_key_for_file_0(&skip, k as u16);
            if recorded_consec {
                assert!(
                    expected_consec.contains(&(k as u16)),
                    "unexpected consec bigram 0x{k:04x} in content {content:?}",
                );
            }
            if recorded_skip {
                assert!(
                    expected_skip.contains(&(k as u16)),
                    "unexpected skip bigram 0x{k:04x} in content {content:?}",
                );
            }
        }
    }

    #[test]
    fn add_file_empty_is_noop() {
        let mut consec = BigramIndexBuilder::new(1);
        let mut skip = BigramIndexBuilder::new(1);
        consec.add_file_content(&mut skip, 0, b"");
        assert_eq!(consec.columns_used(), 0);
        assert_eq!(skip.columns_used(), 0);
        // populated counter not incremented for empty input
        assert_eq!(consec.populated, 0);
    }

    #[test]
    fn add_file_single_byte_is_noop() {
        let mut consec = BigramIndexBuilder::new(1);
        let mut skip = BigramIndexBuilder::new(1);
        consec.add_file_content(&mut skip, 0, b"a");
        assert_eq!(consec.columns_used(), 0);
        assert_eq!(skip.columns_used(), 0);
    }

    #[test]
    fn add_file_two_bytes_consec_only() {
        // With exactly 2 bytes there's no skip bigram (needs i >= 2 in the loop).
        run_and_compare(b"ab");
    }

    #[test]
    fn add_file_three_bytes_has_skip() {
        // "abc" -> consec {"ab", "bc"}, skip {"ac"}
        run_and_compare(b"abc");
    }

    #[test]
    fn add_file_ascii_words() {
        run_and_compare(b"hello world");
        run_and_compare(b"the quick brown fox jumps over the lazy dog");
        run_and_compare(b"fn main() { println!(\"hi\"); }");
    }

    #[test]
    fn add_file_case_is_lowered() {
        // Uppercase should be lowercased before keying, so "AB" == "ab".
        let mut upper = BigramIndexBuilder::new(1);
        let mut upper_skip = BigramIndexBuilder::new(1);
        upper.add_file_content(&mut upper_skip, 0, b"ABC");

        let mut lower = BigramIndexBuilder::new(1);
        let mut lower_skip = BigramIndexBuilder::new(1);
        lower.add_file_content(&mut lower_skip, 0, b"abc");

        // Both should have identical bigram keys.
        for k in 0u32..=0xFFFF {
            let u = builder_has_key_for_file_0(&upper, k as u16);
            let l = builder_has_key_for_file_0(&lower, k as u16);
            assert_eq!(u, l, "consec 0x{k:04x}: upper={u} lower={l}");
            let u = builder_has_key_for_file_0(&upper_skip, k as u16);
            let l = builder_has_key_for_file_0(&lower_skip, k as u16);
            assert_eq!(u, l, "skip 0x{k:04x}: upper={u} lower={l}");
        }
    }

    #[test]
    fn add_file_rejects_non_printable() {
        // Bigrams where either byte is outside 32..=126 are rejected. But
        // the skip-1 bigram can still connect two printable bytes across a
        // non-printable one: for "\0a\0b", consec sees no valid pair but
        // skip sees (a,b) at i=3. Use the reference implementation.
        run_and_compare(b"\0a\0b");

        // All-zero input: truly nothing recorded.
        let mut consec = BigramIndexBuilder::new(1);
        let mut skip = BigramIndexBuilder::new(1);
        consec.add_file_content(&mut skip, 0, b"\0\0\0\0");
        assert_eq!(consec.columns_used(), 0);
        assert_eq!(skip.columns_used(), 0);
    }

    #[test]
    fn add_file_mixed_printable_and_control() {
        // "a\tb\nc d" — \t (9) and \n (10) are below 32. Consec:
        //   (a, \t) x, (\t, b) x, (b, \n) x, (\n, c) x, (c, ' ') ok, (' ', d) ok
        // Skip (i-2, i):
        //   (a, b) ok, (\t, \n) x, (b, c) ok, (\n, ' ') x, (c, d) ok
        run_and_compare(b"a\tb\nc d");
    }

    #[test]
    fn add_file_repeats_are_deduped() {
        // "ababab" has many repeats of "ab", "ba" — each unique bigram should
        // be recorded exactly once (the stack-local `seen_*` dedup works).
        run_and_compare(b"ababababab");
    }

    #[test]
    fn add_file_tombstone_separation() {
        // Two separate files share no bits; file 1's content doesn't bleed
        // into file 0's row and vice-versa.
        let mut consec = BigramIndexBuilder::new(2);
        let mut skip = BigramIndexBuilder::new(2);
        consec.add_file_content(&mut skip, 0, b"xy");
        consec.add_file_content(&mut skip, 1, b"zw");

        let key_xy = key(b'x', b'y');
        let key_zw = key(b'z', b'w');

        // file 0 has "xy" but not "zw"
        let col_xy = consec.lookup[key_slot(key_xy)];
        let col_zw = consec.lookup[key_slot(key_zw)];
        let bitset_xy = consec.column_bitset(col_xy)[0];
        let bitset_zw = consec.column_bitset(col_zw)[0];
        assert_eq!(bitset_xy & 0b01, 0b01, "file 0 should have xy");
        assert_eq!(bitset_zw & 0b01, 0, "file 0 should NOT have zw");
        assert_eq!(bitset_xy & 0b10, 0, "file 1 should NOT have xy");
        assert_eq!(bitset_zw & 0b10, 0b10, "file 1 should have zw");
    }

    #[test]
    fn add_file_long_content() {
        // Stress test: ~8 KB of printable ASCII. Should complete without
        // overflowing any stack-local bitset and produce the full set.
        let mut buf = Vec::with_capacity(8192);
        for i in 0..8192 {
            buf.push(32u8 + ((i * 7) % 95) as u8); // cycle through printable range
        }
        run_and_compare(&buf);
    }

    #[test]
    fn add_file_mixed_lengths_agree() {
        // Content of several lengths, with non-printable bytes and repeats,
        // records exactly the reference bigram set.
        let mut mixed = Vec::with_capacity(256);
        for i in 0..256usize {
            mixed.push(match i % 9 {
                0 => 0,     // NUL
                1 => 0x7F,  // DEL (just above 126)
                2 => b'\n', // below 32
                _ => 32 + ((i * 13) % 95) as u8,
            });
        }

        run_and_compare(&mixed[..127]);
        run_and_compare(&mixed);
        run_and_compare(&mixed[..192]);
    }

    #[test]
    fn add_file_long_short_paths_agree() {
        // Same mixed content checked just below, at, and above
        // LONG_CONTENT_MIN_LEN so both add_short_content and add_long_content
        // are validated against the reference implementation.
        let mut mixed = Vec::with_capacity(LONG_CONTENT_MIN_LEN * 2);
        for i in 0..LONG_CONTENT_MIN_LEN * 2 {
            mixed.push(match i % 11 {
                0 => 0,
                1 => 0x7F,
                2 => b'\n',
                _ => 32 + ((i * 31) % 95) as u8,
            });
        }
        run_and_compare(&mixed[..LONG_CONTENT_MIN_LEN - 1]);
        run_and_compare(&mixed[..LONG_CONTENT_MIN_LEN]);
        run_and_compare(&mixed);
    }

    #[test]
    fn add_file_respects_file_count_boundary() {
        // file_count=100, file_idx=63 (last bit in word 0) and file_idx=64
        // (first bit in word 1). Make sure the word_idx math is right.
        let mut consec = BigramIndexBuilder::new(100);
        let mut skip = BigramIndexBuilder::new(100);
        consec.add_file_content(&mut skip, 63, b"ab");
        consec.add_file_content(&mut skip, 64, b"cd");

        let kab = key(b'a', b'b');
        let kcd = key(b'c', b'd');
        let col_ab = consec.lookup[key_slot(kab)];
        let col_cd = consec.lookup[key_slot(kcd)];

        let ab_bitset = consec.column_bitset(col_ab);
        let cd_bitset = consec.column_bitset(col_cd);
        // ab in word 0, bit 63
        assert_eq!(ab_bitset[0], 1u64 << 63);
        assert_eq!(ab_bitset[1], 0);
        // cd in word 1, bit 0
        assert_eq!(cd_bitset[0], 0);
        assert_eq!(cd_bitset[1], 1);
    }

    #[test]
    fn chunks_merged_by_position_equal_one_builder() {
        let content = |i: usize| format!("file {i} holds {} and {}", i % 7, i * 31 % 101);
        let n = 300;
        let (mut whole, mut whole_skip) = (BigramIndexBuilder::new(n), BigramIndexBuilder::new(n));
        for i in 0..n {
            whole.add_file_content(&mut whole_skip, i, content(i).as_bytes());
        }
        let (mut merged, mut merged_skip) =
            (BigramIndexBuilder::new(n), BigramIndexBuilder::new(n));
        for first in (0..n).step_by(128) {
            let len = (n - first).min(128);
            let (mut chunk, mut chunk_skip) =
                (BigramIndexBuilder::new(len), BigramIndexBuilder::new(len));
            for i in 0..len {
                chunk.add_file_content(&mut chunk_skip, i, content(first + i).as_bytes());
            }
            merged.merge(&chunk, first);
            merged_skip.merge(&chunk_skip, first);
        }
        for k in 0u32..=0xFFFF {
            let k = k as u16;
            if (k >> 8) < 32 || (k >> 8) > 126 || (k & 0xFF) < 32 || (k & 0xFF) > 126 {
                continue;
            }
            for (a, b) in [(&whole, &merged), (&whole_skip, &merged_skip)] {
                let col = |x: &BigramIndexBuilder| {
                    let c = x.lookup[key_slot(k)];
                    (c != NO_COLUMN).then(|| x.column_bitset(c).to_vec())
                };
                assert_eq!(col(a), col(b), "0x{k:04x}");
            }
        }
        assert_eq!(whole.populated, merged.populated);
    }
}
