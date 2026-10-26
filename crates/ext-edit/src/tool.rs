//! `edit`: exact replacements in one file, all against the file as it was.
//!
//! A stale edit records the current hash so a retry can proceed after inspection.
//! Reads through `bash` are not tracked.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::path::PathBuf;
use std::sync::Arc;

use protocol::Card;
use rt::BlockingClass::{Cpu, Storage};
use serde::{Deserialize, Serialize};

use ext::{Cx, Fold, Json, Schema, Tool};

pub struct Edit;

const DIFF_LIMIT: usize = 16 * 1024;
const DIFF_WORK: usize = 1 << 20;

#[derive(Clone, Copy)]
struct Limits {
    diff_bytes: usize,
    diff_work: usize,
}

const LIMITS: Limits = Limits {
    diff_bytes: DIFF_LIMIT,
    diff_work: DIFF_WORK,
};

#[derive(Deserialize, Schema)]
pub struct Change {
    /// Relative to the working directory, or absolute.
    pub path: String,
    #[schema(min_items = 1)]
    pub edits: Edits,
}

#[derive(Deserialize)]
#[serde(from = "Vec<Replace>")]
pub struct Edits(Arc<[Replace]>);

impl From<Vec<Replace>> for Edits {
    fn from(edits: Vec<Replace>) -> Self {
        Self(edits.into())
    }
}

impl Schema for Edits {
    fn schema(out: &mut String) {
        Vec::<Replace>::schema(out);
    }
}

#[derive(Deserialize, Schema)]
pub struct Replace {
    /// The exact text to replace.
    pub old: String,
    /// What replaces it.
    pub new: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edited {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub created: bool,
    pub replaced: usize,
    pub added: usize,
    pub removed: usize,
    /// A unified diff of the change, for clients.
    pub diff: String,
    /// Atomic visibility is confirmed; edits do not request power-loss durability.
    pub publication: sys::fs::EditReceipt,
}

/// The hash of each file as this session last wrote it.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Seen(HashMap<PathBuf, u64>);

/// A file this session wrote, and the hash of what it wrote.
#[derive(Debug, Serialize, Deserialize)]
pub struct Wrote {
    path: PathBuf,
    hash: u64,
}

impl Fold for Seen {
    const NAME: &'static str = "seen";
    type Change = Wrote;

    fn apply(&mut self, wrote: &Wrote) {
        self.0.insert(wrote.path.clone(), wrote.hash);
    }
}

impl Tool for Edit {
    type In = Change;
    type Out = Edited;

    const NAME: &'static str = "edit";
    const DESCRIPTION: &'static str = "Edits a text file by exact replacement. Each `old` must appear exactly once in the file as it was before this call, and edits must not overlap. One edit with an empty `old` writes `new` as the whole file, creating it if needed.";

    async fn call(&self, input: &Change, cx: &Cx) -> Result<Edited, String> {
        let path = cx.dir().join(&input.path);
        let shown = &input.path;
        let prior = cx.state(|seen: &Seen| seen.0.get(&path).copied());
        let transaction = Transaction {
            rt: cx
                .service::<dyn rt::Runtime>()
                .ok_or("the engine provides no runtime")?,
            cancel: &cx.cancel,
        };
        let (edited, hash) = match transaction
            .edit(path, input.edits.0.clone(), prior, LIMITS)
            .await
        {
            Ok(result) => result,
            Err(EditError::Stale(wrote)) => {
                cx.update::<Seen>(&wrote).await.map_err(|e| e.to_string())?;
                return Err(format!(
                    "{shown} changed since this session last edited it; look at it again before editing it"
                ));
            }
            Err(EditError::Conflict(reason)) => {
                return Err(format!(
                    "{shown}: edit conflict ({reason}); inspect the file and retry"
                ));
            }
            Err(EditError::InvalidText) => return Err(format!("{shown} is not UTF-8 text")),
            Err(EditError::Failed(error)) => return Err(format!("{shown}: {error}")),
        };
        let seen = Wrote {
            path: edited.path.clone(),
            hash,
        };
        cx.record_effect::<Seen, _>(edited, &seen)
            .await
            .map_err(|e| e.to_string())
    }

    fn to_model(output: &Json) -> llm::Output {
        let Ok(edited) = output.parse::<Edited>() else {
            return llm::Output::Json(output.clone());
        };
        let path = edited.path.display();
        llm::Output::Text(if edited.created {
            format!("created {path} ({} lines)", edited.added)
        } else {
            format!(
                "edited {path}: {} replacement{}, +{} -{} lines",
                edited.replaced,
                if edited.replaced == 1 { "" } else { "s" },
                edited.added,
                edited.removed
            )
        })
    }

    fn present(input: &Change) -> Option<Card> {
        Some(Card {
            title: format!("edit {}", input.path),
            ..Card::default()
        })
    }

    fn present_result(input: &Change, edited: &Edited) -> Option<Card> {
        Some(Card {
            title: format!("edit {}", input.path),
            body: edited.diff.clone(),
            note: format!("+{} -{}", edited.added, edited.removed),
            ..Card::default()
        })
    }
}

#[derive(Debug)]
enum EditError {
    Stale(Wrote),
    Conflict(sys::fs::EditConflict),
    InvalidText,
    Failed(String),
}

impl From<io::Error> for EditError {
    fn from(error: io::Error) -> Self {
        Self::Failed(error.to_string())
    }
}

impl From<sys::fs::EditError> for EditError {
    fn from(error: sys::fs::EditError) -> Self {
        match error {
            sys::fs::EditError::Conflict(reason) => Self::Conflict(reason),
            error => Self::Failed(error.to_string()),
        }
    }
}

struct Transaction<'a> {
    rt: &'a dyn rt::Runtime,
    cancel: &'a ext::Cancel,
}

impl Transaction<'_> {
    async fn run<T: Send + 'static>(
        &self,
        class: rt::BlockingClass,
        bytes: usize,
        work: impl FnOnce() -> Result<T, EditError> + Send + 'static,
    ) -> Result<T, EditError> {
        let cancel = self.cancel.clone();
        rt::try_unblock_with(self.rt, class, bytes, move || {
            if cancel.is_set() {
                Err(EditError::Failed("edit cancelled".into()))
            } else {
                work()
            }
        })
        .await?
    }

    async fn edit(
        &self,
        path: PathBuf,
        edits: Arc<[Replace]>,
        prior_hash: Option<u64>,
        limits: Limits,
    ) -> Result<(Edited, u64), EditError> {
        let captured = edits.iter().map(|edit| edit.old.len()).max().unwrap_or(0)
            + size_of_val(edits.as_ref())
            + edits
                .iter()
                .map(|edit| edit.old.len() + edit.new.len())
                .sum::<usize>();
        let target = path.clone();
        let transaction = self
            .run(Storage, STREAM_BYTES + captured, move || {
                Ok(sys::fs::EditTransaction::capture(&target, None)?)
            })
            .await?;
        let (transaction, content, edited, hash) = self
            .run(Cpu, STREAM_BYTES + captured, move || {
                let original = transaction.original();
                if let Some(bytes) = original {
                    let current = hash_file(bytes)?;
                    if prior_hash.is_some_and(|prior| prior != current) {
                        return Err(EditError::Stale(Wrote {
                            path,
                            hash: current,
                        }));
                    }
                }
                let (content, diff) = apply_file(original, &edits, limits)?;
                let hash = hash_file(&content)?;
                let edited = Edited {
                    path,
                    created: original.is_none(),
                    replaced: edits.len(),
                    added: diff.added,
                    removed: diff.removed,
                    diff: diff.text,
                    publication: sys::fs::EditReceipt {
                        visible: false,
                        durability: sys::fs::Durability::NotRequested,
                        protection: sys::fs::PublicationProtection::Unchanged,
                        limitations: sys::fs::PublicationLimitations::default(),
                    },
                };
                Ok((transaction, content, edited, hash))
            })
            .await?;
        let cancel = self.cancel.clone();
        self.run(
            Storage,
            // Verification holds both bounded metadata values alongside the result.
            STREAM_BYTES + limits.diff_bytes * 2,
            move || {
                let mut edited = edited;
                edited.publication = transaction.publish_reader(
                    &mut &content,
                    content.metadata()?.len(),
                    sys::fs::Persistence::Visible,
                    &|_| cancel.is_set(),
                )?;
                Ok((edited, hash))
            },
        )
        .await
    }
}

// Fixed file buffers, plus replacement strings and the matcher's overlap window.
const STREAM_BYTES: usize = 1 << 20;
const CHUNK: usize = 64 << 10;

/// FNV-1a is stable across builds, so hashes in durable logs still compare.
fn hash_file(mut file: &File) -> io::Result<u64> {
    file.rewind()?;
    let mut buffer = vec![0; CHUNK];
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            file.rewind()?;
            return Ok(hash);
        }
        for &byte in &buffer[..count] {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

/// Validates text across buffer boundaries without retaining a line or the file.
fn text_lines(mut file: &File) -> Result<usize, EditError> {
    file.rewind()?;
    let mut buffer = vec![0; CHUNK + 4];
    let (mut pending, mut lines, mut last) = (0, 0, None);
    loop {
        let count = file.read(&mut buffer[pending..CHUNK])?;
        if count == 0 {
            if pending != 0 {
                return Err(EditError::InvalidText);
            }
            file.rewind()?;
            return Ok(lines + usize::from(last.is_some_and(|byte| byte != b'\n')));
        }
        lines += memchr::memchr_iter(b'\n', &buffer[pending..pending + count]).count();
        last = Some(buffer[pending + count - 1]);
        let end = pending + count;
        pending = match std::str::from_utf8(&buffer[..end]) {
            Ok(_) => 0,
            Err(error) if error.error_len().is_none() => end - error.valid_up_to(),
            Err(_) => return Err(EditError::InvalidText),
        };
        buffer.copy_within(end - pending..end, 0);
    }
}

fn unique_match(mut file: &File, old: &str, n: usize) -> Result<u64, EditError> {
    let finder = memchr::memmem::Finder::new(old.as_bytes());
    let mut buffer = Vec::with_capacity(old.len() + CHUNK);
    let mut chunk = vec![0; CHUNK];
    let (mut offset, mut found) = (0u64, None);
    file.rewind()?;
    loop {
        let count = file.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..count]);
        if let Some(at) = finder.find(&buffer) {
            if found.is_some() || finder.find(&buffer[at + 1..]).is_some() {
                return Err(EditError::Failed(format!(
                    "edit {n}: `old` appears more than once; include more of the text around it"
                )));
            }
            found = Some(offset + at as u64);
        }
        // A full match cannot fit in the retained suffix, so it is never counted
        // twice. Any match crossing the next buffer boundary remains searchable.
        let consumed = buffer.len().saturating_sub(old.len() - 1);
        buffer.copy_within(consumed.., 0);
        buffer.truncate(buffer.len() - consumed);
        offset += consumed as u64;
    }
    found.ok_or_else(|| EditError::Failed(format!("edit {n}: `old` is not in the file")))
}

fn apply_file(
    original: Option<&File>,
    edits: &[Replace],
    limits: Limits,
) -> Result<(File, Diff), EditError> {
    let length = original
        .map(|file| file.metadata().map(|m| m.len()))
        .transpose()?
        .unwrap_or(0);
    let mut content = tempfile::tempfile()?;
    let whole = matches!(edits, [Replace { old, .. }] if old.is_empty());
    if original.is_none() && !whole {
        return Err(EditError::Failed(
            "no such file; to create it, give one edit with an empty `old`".into(),
        ));
    }
    if edits.is_empty() {
        return Err(EditError::Failed("no edits".into()));
    }
    let lines = original.map(text_lines).transpose()?.unwrap_or(0);
    let spans = replacement_spans(original, edits, length)?;
    // A larger source exhausts the formatter's work budget on its first hunk.
    // Keep only the text the formatter can use, independent of output size.
    let preview = original
        .filter(|_| length <= (limits.diff_work / 4).min(DIFF_WORK / 4) as u64)
        .map(|mut file| {
            file.rewind()?;
            let mut text = String::new();
            file.read_to_string(&mut text).map(|_| text)
        })
        .transpose()?;
    let mut diff = Diff::default();
    let (mut last, mut old_line, mut new_line) = (0, 1, 1);
    for (start, end, index) in spans {
        let edit = &edits[index];
        if let Some(mut source) = original {
            source.seek(io::SeekFrom::Start(last))?;
            io::copy(&mut source.take(start - last), &mut content)?;
        }
        content.write_all(edit.new.as_bytes())?;
        if let Some(text) = &preview {
            let start = usize::try_from(start).expect("preview bounds the source offset");
            let end = usize::try_from(end).expect("preview bounds the source offset");
            let skipped = text
                [usize::try_from(last).expect("preview bounds the source offset")..start]
                .matches('\n')
                .count();
            old_line += skipped;
            new_line += skipped;
            diff.hunk(text, (start, end), &edit.new, (old_line, new_line), limits);
            old_line += text[start..end].matches('\n').count();
            new_line += edit.new.matches('\n').count();
        } else if let Some(mut source) = original {
            if diff.work == 0 {
                diff.cut(limits);
            }
            let prefix = if start > 0 {
                source.seek(io::SeekFrom::Start(start - 1))?;
                let mut byte = [0];
                source.read_exact(&mut byte)?;
                byte[0] != b'\n'
            } else {
                false
            };
            let count = |text: &str| {
                text.matches('\n').count()
                    + usize::from(
                        end < length || (!text.ends_with('\n') && (!text.is_empty() || prefix)),
                    )
            };
            diff.removed += if whole { lines } else { count(&edit.old) };
            diff.added += count(&edit.new);
        } else {
            diff.hunk("", (0, 0), &edit.new, (1, 1), limits);
        }
        last = end;
    }
    if let Some(mut source) = original {
        source.seek(io::SeekFrom::Start(last))?;
        io::copy(&mut source, &mut content)?;
    }
    content.rewind()?;
    Ok((content, diff))
}

fn replacement_spans(
    original: Option<&File>,
    edits: &[Replace],
    length: u64,
) -> Result<Vec<(u64, u64, usize)>, EditError> {
    let whole = matches!(edits, [Replace { old, .. }] if old.is_empty());
    let mut spans = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let start = if whole {
            0
        } else {
            if edit.old.is_empty() {
                return Err(EditError::Failed(format!(
                    "edit {}: an empty `old` only works as the only edit",
                    index + 1
                )));
            }
            unique_match(
                original.expect("replacement requires a source"),
                &edit.old,
                index + 1,
            )?
        };
        spans.push((
            start,
            if whole {
                length
            } else {
                start + edit.old.len() as u64
            },
            index,
        ));
    }
    spans.sort_unstable_by_key(|span| span.0);
    for pair in spans.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(EditError::Failed(format!(
                "edits {} and {} overlap",
                pair[0].2 + 1,
                pair[1].2 + 1
            )));
        }
    }
    Ok(spans)
}

#[derive(Default)]
struct Diff {
    text: String,
    added: usize,
    removed: usize,
    work: usize,
}

impl Diff {
    fn hunk(
        &mut self,
        original: &str,
        (start, end): (usize, usize),
        new: &str,
        lines: (usize, usize),
        limits: Limits,
    ) {
        let tail = end < original.len();
        let prefix = start > 0 && original.as_bytes()[start - 1] != b'\n';
        let count = |text: &str| {
            text.matches('\n').count()
                + usize::from(tail || (!text.ends_with('\n') && (!text.is_empty() || prefix)))
        };
        self.removed += count(&original[start..end]);
        self.added += count(new);
        if self.work > limits.diff_work {
            return;
        }
        self.work = self.work.saturating_add((original.len() + new.len()) * 4);
        if self.work > limits.diff_work {
            self.cut(limits);
            return;
        }
        let from = original[..start].rfind('\n').map_or(0, |at| at + 1);
        let to = original[end..]
            .find('\n')
            .map_or(original.len(), |at| end + at + 1);
        let before = &original[from..to];
        let parts = [&original[from..start], new, &original[end..to]];
        let bytes = before.len() + parts.iter().map(|part| part.len()).sum::<usize>();
        if self.text.len() + bytes * 2 + 96 > limits.diff_bytes {
            self.cut(limits);
            return;
        }
        let after = parts.concat();
        let _ = writeln!(
            self.text,
            "@@ -{},{} +{},{} @@",
            lines.0,
            before.lines().count(),
            lines.1,
            after.lines().count()
        );
        for (prefix, text) in [('-', before), ('+', after.as_str())] {
            for line in text.lines() {
                let _ = writeln!(self.text, "{prefix}{line}");
            }
        }
    }

    fn cut(&mut self, limits: Limits) {
        self.work = usize::MAX;
        if self.text.len() + "\n[diff cut]\n".len() <= limits.diff_bytes {
            self.text.push_str("\n[diff cut]\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ext::host::Registry;
    use ext::testing::Rig;
    use protocol::Outcome;
    use serde_json::Value;
    use serde_json::json;

    fn apply(
        original: Option<&str>,
        edits: &[Replace],
        limits: Limits,
    ) -> Result<(String, Diff), String> {
        let source = original.map(|text| {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(text.as_bytes()).unwrap();
            file
        });
        let (mut file, diff) =
            apply_file(source.as_ref(), edits, limits).map_err(|error| match error {
                EditError::Failed(message) => message,
                error => format!("{error:?}"),
            })?;
        let mut content = String::new();
        file.read_to_string(&mut content).unwrap();
        Ok((content, diff))
    }

    fn edits(pairs: &[(&str, &str)]) -> Vec<Replace> {
        pairs
            .iter()
            .map(|(old, new)| Replace {
                old: (*old).into(),
                new: (*new).into(),
            })
            .collect()
    }

    #[test]
    fn overlapping_occurrences_never_publish() {
        let rig = Rig::new("edit-overlapping", registry());
        for (index, (text, old)) in [("aaa", "aa"), ("ééé", "éé"), ("ababa", "aba")]
            .into_iter()
            .enumerate()
        {
            for padding in [0, CHUNK - 1, CHUNK + 1] {
                let path = format!("{index}-{padding}.txt");
                let original = format!("{}{text}", "x".repeat(padding));
                std::fs::write(rig.workspace().join(&path), &original).unwrap();
                assert!(
                    failed(change(&rig, &path, &[(old, "next")]))
                        .contains("appears more than once")
                );
                assert_eq!(
                    std::fs::read_to_string(rig.workspace().join(path)).unwrap(),
                    original
                );
            }
        }
    }

    #[test]
    fn refused_edit_admission_leaves_the_target_untouched() {
        let rig = Rig::new("edit-refused", registry());
        let path = rig.workspace().join("target");
        std::fs::write(&path, "before").unwrap();
        let mut config = rt::Config::for_cores(1);
        config.storage.bytes = 1;
        let runtime = rt::Threads::configured(config);
        let error = rt::block_on(
            Transaction {
                rt: &runtime,
                cancel: &ext::Cancel::default(),
            }
            .edit(
                path.clone(),
                edits(&[("before", "after")]).into(),
                None,
                LIMITS,
            ),
        )
        .unwrap_err();
        assert!(matches!(error, EditError::Failed(message) if message.contains("admission")));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "before");
    }

    #[test]
    fn large_replacement_patterns_do_not_exhaust_worker_admission() {
        let rig = Rig::new("edit-large-pattern", registry());
        let path = rig.workspace().join("target");
        let old = "x".repeat((8 << 20) - 128);
        std::fs::write(&path, &old).unwrap();
        let runtime = rt::Threads::new(1);
        let (edited, current) = rt::block_on(
            Transaction {
                rt: &runtime,
                cancel: &ext::Cancel::default(),
            }
            .edit(
                path.clone(),
                edits(&[(&old, "after\n")]).into(),
                None,
                LIMITS,
            ),
        )
        .unwrap();
        assert!(edited.publication.visible);
        assert_eq!(std::fs::read(&path).unwrap(), b"after\n");
        assert_eq!(current, hash_file(&File::open(path).unwrap()).unwrap());
    }

    #[test]
    fn file_size_and_growth_do_not_limit_edits() {
        let rig = Rig::new("edit-streamed-growth", registry());
        let path = rig.workspace().join("large.txt");
        let mut original = File::create(&path).unwrap();
        original.set_len(9 << 20).unwrap();
        original.seek(io::SeekFrom::End(0)).unwrap();
        original.write_all(b"target\n").unwrap();
        drop(original);
        let runtime = rt::Threads::new(1);
        let cancel = ext::Cancel::default();
        let replacement = format!("{}next\n", "x".repeat(9 << 20));
        let (edited, expected_hash) = rt::block_on(
            Transaction {
                rt: &runtime,
                cancel: &cancel,
            }
            .edit(
                path.clone(),
                edits(&[("target\n", &replacement)]).into(),
                None,
                LIMITS,
            ),
        )
        .unwrap();
        assert!(edited.publication.visible);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), (18 << 20) + 5);
        assert_eq!(
            hash_file(&File::open(&path).unwrap()).unwrap(),
            expected_hash
        );
        assert_eq!((edited.added, edited.removed), (1, 1));
        assert!(edited.diff.len() <= DIFF_LIMIT);
        let (edited, _) = rt::block_on(
            Transaction {
                rt: &runtime,
                cancel: &cancel,
            }
            .edit(
                path.clone(),
                edits(&[("next", "done")]).into(),
                Some(expected_hash),
                LIMITS,
            ),
        )
        .unwrap();
        assert!(edited.publication.visible);
        let mut file = File::open(path).unwrap();
        file.seek(io::SeekFrom::End(-5)).unwrap();
        let mut tail = String::new();
        file.read_to_string(&mut tail).unwrap();
        assert_eq!(tail, "done\n");
    }

    #[test]
    fn streaming_preserves_text_across_buffers_and_rejects_invalid_utf8() {
        let rig = Rig::new("edit-streamed-text", registry());
        let old = format!("{}é", "a".repeat(CHUNK + 3));
        let original = format!("{}é{old}suffix", "x".repeat(CHUNK - 1));
        std::fs::write(rig.workspace().join("text"), &original).unwrap();
        assert!(matches!(
            change(&rig, "text", &[(&old, "new")]),
            Outcome::Done { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(rig.workspace().join("text")).unwrap(),
            format!("{}énewsuffix", "x".repeat(CHUNK - 1))
        );
        let mut invalid = original.into_bytes();
        invalid.push(0xc3);
        std::fs::write(rig.workspace().join("invalid"), &invalid).unwrap();
        assert!(failed(change(&rig, "invalid", &[("suffix", "next")])).contains("not UTF-8"));
        assert_eq!(
            std::fs::read(rig.workspace().join("invalid")).unwrap(),
            invalid
        );
    }

    #[test]
    fn streaming_keeps_every_hunk_the_diff_budget_can_format() {
        let original = format!("{}target\n", "padding\n".repeat(12000));
        let (_, diff) = apply(Some(&original), &edits(&[("target", "next")]), LIMITS).unwrap();
        assert!(diff.text.contains("-target\n+next\n"), "{}", diff.text);
        let (_, diff) = apply(
            Some("first\nsecond\n"),
            &edits(&[("first", "one"), ("second", &"x".repeat(300 << 10))]),
            LIMITS,
        )
        .unwrap();
        assert_eq!(diff.text, "@@ -1,1 +1,1 @@\n-first\n+one\n\n[diff cut]\n");
    }

    #[test]
    fn diff_work_is_bounded_before_formatting() {
        let huge = "é".repeat(1 << 20);
        for limits in [
            Limits {
                diff_work: 1,
                ..LIMITS
            },
            Limits {
                diff_bytes: 64,
                diff_work: usize::MAX,
            },
        ] {
            let (_, diff) = apply(Some(&huge), &edits(&[("", "x")]), limits).unwrap();
            assert_eq!(diff.text, "\n[diff cut]\n");
            assert!(diff.text.capacity() <= limits.diff_bytes);
            assert_eq!((diff.added, diff.removed), (1, 1));
        }
        let original = format!("{huge}target");
        let (_, diff) = apply(Some(&original), &edits(&[("target", "next")]), LIMITS).unwrap();
        assert_eq!(diff.text, "\n[diff cut]\n");
        assert!(diff.text.capacity() <= DIFF_LIMIT);
        assert_eq!((diff.added, diff.removed), (1, 1));
    }

    #[test]
    fn large_edits_do_not_block_async_workers() {
        use std::sync::Mutex;
        struct Gated {
            inner: rt::Threads,
            stage: std::sync::atomic::AtomicUsize,
            at: usize,
            gate: Mutex<Option<(rt::oneshot::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
        }
        impl rt::Runtime for Gated {
            fn spawn(&self, task: rt::BoxFuture<'static, ()>) {
                self.inner.spawn(task);
            }
            fn spawn_flush(
                &self,
                work: Box<dyn FnOnce() + Send>,
            ) -> rt::BoxFuture<'static, io::Result<()>> {
                self.inner.spawn_flush(work)
            }
            fn reactor(&self) -> rt::Reactor {
                self.inner.reactor()
            }
            fn spawn_blocking(
                &self,
                class: rt::BlockingClass,
                bytes: usize,
                work: Box<dyn FnOnce() + Send>,
            ) -> rt::BoxFuture<'static, io::Result<()>> {
                let stage = self
                    .stage
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let gate = if stage == self.at {
                    self.gate.lock().unwrap().take()
                } else {
                    None
                };
                self.inner.spawn_blocking(
                    class,
                    bytes,
                    Box::new(move || {
                        if let Some((started, resume)) = gate {
                            started.send(());
                            resume.recv().unwrap();
                        }
                        work();
                    }),
                )
            }
        }
        for (at, cancelled) in [(1, false), (0, true), (1, true), (2, true)] {
            let rig = Rig::new("edit-worker", registry());
            let path = rig.workspace().join("large.txt");
            let file = path.clone();
            let (started, running) = rt::oneshot::channel();
            let (resume, receiver) = std::sync::mpsc::channel();
            let runtime = Arc::new(Gated {
                inner: rt::Threads::new(1),
                stage: std::sync::atomic::AtomicUsize::new(0),
                at,
                gate: Mutex::new(Some((started, receiver))),
            });
            let cancel = ext::Cancel::default();
            let stop = cancel.clone();
            let worker = runtime.clone();
            let result = rt::spawn(runtime.as_ref(), async move {
                Transaction {
                    rt: worker.as_ref(),
                    cancel: &cancel,
                }
                .edit(
                    path,
                    edits(&[("", &"x".repeat(2 << 20))]).into(),
                    None,
                    LIMITS,
                )
                .await
            });
            rt::block_on(running).unwrap();
            let heartbeat = rt::spawn(runtime.as_ref(), async move {
                if cancelled {
                    stop.set();
                }
                resume.send(()).unwrap();
            });
            let result = rt::block_on(result);
            rt::block_on(heartbeat);
            if cancelled {
                assert!(
                    matches!(result, Err(EditError::Failed(error)) if error == "edit cancelled")
                );
                assert!(!file.exists());
            } else {
                let (edited, current) = result.unwrap();
                assert_eq!(std::fs::read(&file).unwrap(), vec![b'x'; 2 << 20]);
                assert_eq!(current, hash_file(&File::open(file).unwrap()).unwrap());
                assert!(edited.diff.len() <= DIFF_LIMIT);
            }
        }
    }

    #[test]
    fn edits_apply_against_the_original_in_any_order() {
        let original = "one\ntwo\nthree\nfour\n";
        let (content, diff) = apply(
            Some(original),
            &edits(&[("four", "4"), ("one\ntwo", "1")]),
            LIMITS,
        )
        .unwrap();
        assert_eq!(content, "1\nthree\n4\n");
        assert_eq!(
            diff.text,
            "@@ -1,2 +1,1 @@\n-one\n-two\n+1\n@@ -4,1 +3,1 @@\n-four\n+4\n"
        );
        assert_eq!((diff.added, diff.removed), (2, 3));
        let (swapped, _) = apply(Some("a b"), &edits(&[("a", "b"), ("b", "a")]), LIMITS).unwrap();
        assert_eq!(swapped, "b a");
    }

    #[test]
    fn every_old_must_match_exactly_once_without_overlapping() {
        let text = Some("x x y\n");
        let err = |pairs: &[(&str, &str)]| apply(text, &edits(pairs), LIMITS).err().unwrap();
        assert_eq!(err(&[("z", "")]), "edit 1: `old` is not in the file");
        assert_eq!(
            err(&[("y", "Y"), ("x", "")]),
            "edit 2: `old` appears more than once; include more of the text around it"
        );
        assert_eq!(err(&[("x y", "1"), ("y\n", "2")]), "edits 1 and 2 overlap");
        assert_eq!(
            err(&[("y", "1"), ("", "2")]),
            "edit 2: an empty `old` only works as the only edit"
        );
        assert!(apply(None, &edits(&[("x", "y")]), LIMITS).is_err());
        assert_eq!(apply(text, &[], LIMITS).err().unwrap(), "no edits");
    }

    #[test]
    fn an_empty_old_writes_the_whole_file() {
        let (created, diff) = apply(None, &edits(&[("", "a\nb\n")]), LIMITS).unwrap();
        assert_eq!(created, "a\nb\n");
        assert_eq!(diff.text, "@@ -1,0 +1,2 @@\n+a\n+b\n");
        let (replaced, diff) = apply(Some("old\n"), &edits(&[("", "new\n")]), LIMITS).unwrap();
        assert_eq!(replaced, "new\n");
        assert_eq!((diff.added, diff.removed), (1, 1));
    }

    fn registry() -> Registry {
        Registry::default()
            .with(&ext_shell::Shell::default())
            .and_then(|r| r.with(&crate::Edit))
            .unwrap()
    }

    fn change(rig: &Rig, path: &str, pairs: &[(&str, &str)]) -> protocol::ToolOutcome {
        let edits: Vec<Value> = pairs
            .iter()
            .map(|(old, new)| json!({ "old": old, "new": new }))
            .collect();
        rig.call("edit", &json!({ "path": path, "edits": edits }))
    }

    fn failed(outcome: protocol::ToolOutcome) -> String {
        match outcome {
            Outcome::Failed { error } => error,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stale_file_checks_survive_offloaded_edits() {
        let rig = Rig::new("edit", registry());
        let file = rig.workspace().join("notes.txt");
        let created = change(&rig, "notes.txt", &[("", "one\ntwo\n")]);
        assert!(
            matches!(&created, Outcome::Done { output: protocol::Payload::Inline(output) } if output.parse::<Value>().unwrap()["created"] == true)
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\n");
        let Outcome::Done {
            output: protocol::Payload::Inline(output),
        } = &created
        else {
            unreachable!()
        };
        assert_eq!(
            output.parse::<Value>().unwrap()["publication"],
            json!({"visible": true, "durability": "not_requested", "protection": "exclusive_create", "limitations": {"change_after_check": false, "writes_to_old_inode": false}})
        );
        let Outcome::Done {
            output: protocol::Payload::Inline(output),
        } = change(&rig, "notes.txt", &[("two", "2")])
        else {
            panic!()
        };
        assert_eq!(
            output.parse::<Value>().unwrap()["diff"],
            "@@ -2,1 +2,1 @@\n-two\n+2\n"
        );
        let input = Change {
            path: "notes.txt".into(),
            edits: Vec::new().into(),
        };
        let card = Edit::present_result(&input, &output.parse().unwrap());
        assert_eq!(card.unwrap().note, "+1 -1");

        std::fs::write(&file, "one\n2\nthree\n").unwrap();
        rig.restart();
        let stale = failed(change(&rig, "notes.txt", &[("one", "1")]));
        assert!(
            stale.contains("changed since this session last edited it"),
            "{stale}"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n2\nthree\n");
        assert!(matches!(
            change(&rig, "notes.txt", &[("one", "1")]),
            Outcome::Done { .. }
        ));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "1\n2\nthree\n");

        std::fs::write(rig.workspace().join("theirs.txt"), "a\n").unwrap();
        assert!(matches!(
            change(&rig, "theirs.txt", &[("a", "b")]),
            Outcome::Done { .. }
        ));
        let missing = failed(change(&rig, "nope.txt", &[("a", "b")]));
        assert!(missing.starts_with("nope.txt: no such file"), "{missing}");
    }

    #[test]
    fn paths_are_relative_to_where_the_shell_left_off() {
        let rig = Rig::new("edit-cwd", registry());
        rig.done("bash", &json!({ "command": "mkdir sub && cd sub" }));
        assert!(matches!(
            change(&rig, "x.txt", &[("", "x")]),
            Outcome::Done { .. }
        ));
        assert!(rig.workspace().join("sub/x.txt").is_file());
        let binary = rig.workspace().join("sub/b.bin");
        std::fs::write(&binary, [0xff, 0xfe]).unwrap();
        assert_eq!(
            failed(change(&rig, "b.bin", &[("a", "b")])),
            "b.bin is not UTF-8 text"
        );
    }
}
