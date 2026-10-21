//! `edit`: exact replacements in one file, all against the file as it was.
//!
//! Stale reads: the `edit` extension records a hash of every file it writes
//! as its `seen` state. An edit to a file whose content no longer has the
//! hash recorded for it (it changed since this session last wrote it, by
//! `bash` or by someone else) fails, and records the new hash: the model is
//! told to look again, and its next edit, made knowing the change, goes
//! through. Files the session never wrote have nothing to compare against,
//! as reads through `bash` are not seen.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use protocol::Card;
use serde::{Deserialize, Serialize};

use ext::Fold;

use ext::{Cx, Json, Schema, Tool};

fn runtime(cx: &Cx) -> Result<&dyn rt::Runtime, String> {
    cx.service::<dyn rt::Runtime>()
        .ok_or_else(|| "the engine provides no runtime".to_owned())
}

pub struct Edit;

/// The most of a diff an outcome keeps.
const DIFF_LIMIT: usize = 16 * 1024;
const FILE_LIMIT: usize = 8 << 20;

#[derive(Clone, Copy)]
struct Limits {
    file: usize,
    diff_bytes: usize,
    diff_work: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            file: FILE_LIMIT,
            diff_bytes: DIFF_LIMIT,
            diff_work: 1 << 20,
        }
    }
}

#[derive(Deserialize, Schema)]
pub struct Change {
    /// Relative to the working directory, or absolute.
    pub path: String,
    #[schema(min_items = 1)]
    pub edits: Vec<Replace>,
}

#[derive(Clone, Deserialize, Schema)]
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
        let shown = input.path.clone();
        let prior = cx.state(|seen: &Seen| seen.0.get(&path).copied());
        let result = edit_file(
            runtime(cx)?,
            path.clone(),
            &input.edits,
            prior,
            Limits::default(),
        )
        .await;
        let EditResult { edited, hash } = match result {
            Ok(result) => result,
            Err(EditError::Stale(hash)) => {
                cx.update::<Seen>(&Wrote { path, hash })
                    .await
                    .map_err(|e| e.to_string())?;
                return Err(format!(
                    "{shown} changed since this session last edited it; look at it again before editing it"
                ));
            }
            Err(EditError::InvalidText) => return Err(format!("{shown} is not UTF-8 text")),
            Err(EditError::Failed(error)) => return Err(format!("{shown}: {error}")),
        };
        let seen = Wrote { path, hash };
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

struct EditResult {
    edited: Edited,
    hash: u64,
}

#[derive(Debug)]
enum EditError {
    Stale(u64),
    InvalidText,
    Failed(String),
}

impl From<std::io::Error> for EditError {
    fn from(error: std::io::Error) -> Self {
        Self::Failed(error.to_string())
    }
}

async fn admitted<T: Send + 'static>(
    rt: &dyn rt::Runtime,
    class: rt::BlockingClass,
    bytes: usize,
    work: impl FnOnce() -> Result<T, EditError> + Send + 'static,
) -> Result<T, EditError> {
    let (tx, rx) = rt::oneshot::channel();
    rt.spawn_blocking(class, bytes, Box::new(move || tx.send(work())))
        .await?;
    rx.await
        .ok_or_else(|| EditError::Failed("edit worker panicked".into()))?
}

async fn edit_file(
    rt: &dyn rt::Runtime,
    path: PathBuf,
    edits: &[Replace],
    prior_hash: Option<u64>,
    limits: Limits,
) -> Result<EditResult, EditError> {
    let captured = edits
        .iter()
        .try_fold(0usize, |size, edit| {
            size.checked_add(edit.old.len())?
                .checked_add(edit.new.len())?
                .checked_add(size_of::<Replace>())
        })
        .filter(|&size| size <= limits.file)
        .ok_or_else(|| EditError::Failed("edits exceed byte limit".into()))?;
    let edits = edits.to_vec();
    let read = path.clone();
    let original = admitted(
        rt,
        rt::BlockingClass::Storage,
        limits.file * 2 + captured,
        move || {
            let file = match std::fs::File::open(read) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            let mut bytes = Vec::new();
            file.take(limits.file as u64 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > limits.file {
                return Err(EditError::Failed("file exceeds byte limit".into()));
            }
            Ok(Some(bytes))
        },
    )
    .await?;
    let (content, result) = admitted(
        rt,
        rt::BlockingClass::Cpu,
        limits.file * 5 + captured,
        move || {
            if let Some(bytes) = &original {
                let current = hash(bytes);
                if prior_hash.is_some_and(|prior| prior != current) {
                    return Err(EditError::Stale(current));
                }
            }
            let text = original
                .as_ref()
                .map(|bytes| std::str::from_utf8(bytes))
                .transpose()
                .map_err(|_| EditError::InvalidText)?;
            let (content, diff) = apply(text, &edits, limits).map_err(EditError::Failed)?;
            let result = EditResult {
                hash: hash(content.as_bytes()),
                edited: Edited {
                    path,
                    created: original.is_none(),
                    replaced: edits.len(),
                    added: diff.added,
                    removed: diff.removed,
                    diff: diff.text,
                },
            };
            Ok((content, result))
        },
    )
    .await?;
    admitted(
        rt,
        rt::BlockingClass::Storage,
        content.len() + limits.diff_bytes * 2,
        move || {
            write_content(&result.edited.path, content)?;
            Ok(result)
        },
    )
    .await
}

fn write_content(path: &Path, content: impl AsRef<[u8]>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)
}

#[derive(Default)]
struct Diff {
    text: String,
    added: usize,
    removed: usize,
    work: usize,
}

fn apply(
    original: Option<&str>,
    edits: &[Replace],
    limits: Limits,
) -> Result<(String, Diff), String> {
    if let [Replace { old, new }] = edits
        && old.is_empty()
    {
        if new.len() > limits.file * 2 {
            return Err("result exceeds byte limit".into());
        }
        let mut diff = Diff::default();
        diff.lines((1, 1), original.unwrap_or_default(), new, limits);
        return Ok((new.clone(), diff));
    }
    let original = original
        .ok_or("no such file; to create it, give one edit with an empty `old`".to_owned())?;
    if edits.is_empty() {
        return Err("no edits".to_owned());
    }
    if original.len().saturating_mul(edits.len()) > 64 << 20 {
        return Err("edit matching exceeds work limit".into());
    }
    let mut spans = Vec::with_capacity(edits.len());
    for (n, Replace { old, new }) in (1..).zip(edits) {
        if old.is_empty() {
            return Err(format!(
                "edit {n}: an empty `old` only works as the only edit"
            ));
        }
        let mut found = original.match_indices(old.as_str());
        let Some((at, _)) = found.next() else {
            return Err(format!("edit {n}: `old` is not in the file"));
        };
        let more = found.count();
        if more > 0 {
            return Err(format!(
                "edit {n}: `old` appears {} times; include more of the text around it",
                more + 1
            ));
        }
        spans.push((at, at + old.len(), new.as_str(), n));
    }
    spans.sort_unstable_by_key(|span| span.0);
    for pair in spans.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(format!("edits {} and {} overlap", pair[0].3, pair[1].3));
        }
    }
    let size = spans
        .iter()
        .try_fold(original.len(), |size, &(start, end, new, _)| {
            size.checked_sub(end - start)?.checked_add(new.len())
        })
        .filter(|&size| size <= limits.file * 2)
        .ok_or("result exceeds byte limit")?;
    let (mut content, mut diff, mut last) = (String::with_capacity(size), Diff::default(), 0);
    // How far line numbers in the new text have moved from the old.
    let mut shift: isize = 0;
    let mut line = 1;
    for &(start, end, new, _) in &spans {
        content.push_str(&original[last..start]);
        content.push_str(new);
        line += line_count(&original[last..start]);
        last = end;
        diff.hunk(
            original,
            (start, end),
            new,
            (line, line.saturating_add_signed(shift)),
            limits,
        );
        let removed = line_count(&original[start..end]);
        shift += line_count(new).cast_signed() - removed.cast_signed();
        line += removed;
    }
    content.push_str(&original[last..]);
    Ok((content, diff))
}

fn line_count(text: &str) -> usize {
    text.matches('\n').count()
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
        let old = &original[start..end];
        let tail = end < original.len();
        let prefix = start > 0 && original.as_bytes()[start - 1] != b'\n';
        self.removed += line_count(old)
            + usize::from(
                tail || if old.is_empty() {
                    prefix
                } else {
                    !old.ends_with('\n')
                },
            );
        self.added += line_count(new)
            + usize::from(
                tail || if new.is_empty() {
                    prefix
                } else {
                    !new.ends_with('\n')
                },
            );
        // Exact line totals are linear in the replacements; contextual scans and
        // formatting have a separate ceiling, even for a single enormous line.
        let work = original.len().saturating_add(new.len()).saturating_mul(4);
        if !self.admit(work, limits) {
            return;
        }
        let from = original[..start].rfind('\n').map_or(0, |at| at + 1);
        let to = original[end..]
            .find('\n')
            .map_or(original.len(), |at| end + at + 1);
        let before = &original[from..to];
        let parts = [&original[from..start], new, &original[end..to]];
        let bytes = before.len() + parts.iter().map(|part| part.len()).sum::<usize>();
        if self.text.len() + bytes + 96 > limits.diff_bytes {
            self.cut(limits);
            return;
        }
        let after = parts.concat();
        self.format(lines, before, &after, limits);
    }

    fn lines(&mut self, lines: (usize, usize), old: &str, new: &str, limits: Limits) {
        self.removed += old.lines().count();
        self.added += new.lines().count();
        if self.admit(
            old.len().saturating_add(new.len()).saturating_mul(3),
            limits,
        ) {
            self.format(lines, old, new, limits);
        }
    }

    fn admit(&mut self, work: usize, limits: Limits) -> bool {
        if self.work > limits.diff_work {
            return false;
        }
        self.work = self.work.saturating_add(work);
        if self.work <= limits.diff_work && self.text.len() + 96 <= limits.diff_bytes {
            return true;
        }
        self.cut(limits);
        false
    }

    fn cut(&mut self, limits: Limits) {
        self.work = usize::MAX;
        if self.text.len() + "\n[diff cut]\n".len() <= limits.diff_bytes {
            self.text.push_str("\n[diff cut]\n");
        }
    }

    fn format(&mut self, lines: (usize, usize), old: &str, new: &str, limits: Limits) {
        let _ = writeln!(
            self.text,
            "@@ -{},{} +{},{} @@",
            lines.0,
            old.lines().count(),
            lines.1,
            new.lines().count()
        );
        for (prefix, text) in [('-', old), ('+', new)] {
            for line in text.lines() {
                // Empty lines also consume output bytes.
                if self.text.len() + line.len() + 2 + 12 > limits.diff_bytes {
                    self.cut(limits);
                    return;
                }
                let _ = writeln!(self.text, "{prefix}{line}");
            }
        }
    }
}

/// FNV-1a: stable across builds, so hashes in old logs still compare.
fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ext::host::Registry;
    use ext::testing::Rig;
    use protocol::Outcome;
    use serde_json::Value;
    use serde_json::json;

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
    fn diff_work_is_bounded_before_formatting() {
        let huge = "é".repeat(1 << 20);
        for limits in [
            Limits {
                diff_work: 1,
                ..Limits::default()
            },
            Limits {
                diff_bytes: 64,
                ..Limits::default()
            },
        ] {
            let (_, diff) = apply(Some(&huge), &edits(&[("", "x")]), limits).unwrap();
            assert_eq!(diff.text, "\n[diff cut]\n");
            assert!(diff.text.capacity() <= limits.diff_bytes);
            assert_eq!((diff.added, diff.removed), (1, 1));
        }
        let original = format!("{huge}target");
        let (_, diff) = apply(
            Some(&original),
            &edits(&[("target", "next")]),
            Limits::default(),
        )
        .unwrap();
        assert_eq!(diff.text, "\n[diff cut]\n");
        assert!(diff.text.capacity() <= DIFF_LIMIT);
        assert_eq!((diff.added, diff.removed), (1, 1));
    }

    #[test]
    fn limits_reject_growth_and_matching_before_building_the_result() {
        let limits = Limits {
            file: 4,
            ..Limits::default()
        };
        assert!(apply(Some("abcd"), &edits(&[("a", "123456789")]), limits).is_err());
        let original = "x".repeat(1 << 20);
        let many = edits(&vec![("x", "y"); 65]);
        assert_eq!(
            apply(Some(&original), &many, Limits::default())
                .err()
                .unwrap(),
            "edit matching exceeds work limit"
        );
        let rig = Rig::new("edit-limits", registry());
        let path = rig.workspace().join("large.txt");
        std::fs::write(&path, "12345").unwrap();
        let runtime = rt::Threads::new(1);
        assert!(
            rt::block_on(edit_file(
                &runtime,
                path.clone(),
                &edits(&[("1", "2")]),
                None,
                limits
            ))
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "12345");
    }

    #[test]
    fn writing_does_not_clone_the_complete_result() {
        struct Contents(String);
        impl AsRef<[u8]> for Contents {
            fn as_ref(&self) -> &[u8] {
                self.0.as_bytes()
            }
        }
        let rig = Rig::new("edit-owned-write", registry());
        let file = rig.workspace().join("large.txt");
        let content = Contents("a".repeat(1 << 20));
        // The write accepts ownership without requiring a cloneable buffer.
        write_content(&file, content).unwrap();
        assert_eq!(std::fs::metadata(file).unwrap().len(), 1 << 20);
    }

    #[test]
    fn large_edits_do_not_block_async_workers() {
        use std::sync::{Arc, Mutex};
        struct Gated {
            inner: rt::Threads,
            cpu: Mutex<Option<rt::oneshot::Sender<()>>>,
            resume: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
        }
        impl rt::Runtime for Gated {
            fn spawn(&self, task: rt::BoxFuture<'static, ()>) {
                self.inner.spawn(task);
            }
            fn spawn_flush(
                &self,
                work: Box<dyn FnOnce() + Send>,
            ) -> rt::BoxFuture<'static, std::io::Result<()>> {
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
            ) -> rt::BoxFuture<'static, std::io::Result<()>> {
                let started = if matches!(class, rt::BlockingClass::Cpu) {
                    self.cpu.lock().unwrap().take()
                } else {
                    None
                };
                let resume = self.resume.clone();
                self.inner.spawn_blocking(
                    class,
                    bytes,
                    Box::new(move || {
                        if let Some(started) = started {
                            started.send(());
                            resume.lock().unwrap().recv().unwrap();
                        }
                        work();
                    }),
                )
            }
        }
        let rig = Rig::new("edit-worker", registry());
        let path = rig.workspace().join("large.txt");
        let (started, running) = rt::oneshot::channel();
        let (resume, receiver) = std::sync::mpsc::channel();
        let runtime = Arc::new(Gated {
            inner: rt::Threads::new(1),
            cpu: Mutex::new(Some(started)),
            resume: Arc::new(Mutex::new(receiver)),
        });
        let worker = runtime.clone();
        let result = rt::spawn(runtime.as_ref(), async move {
            edit_file(
                worker.as_ref(),
                path,
                &edits(&[("", &"x".repeat(2 << 20))]),
                None,
                Limits::default(),
            )
            .await
        });
        rt::block_on(running).unwrap();
        let heartbeat = rt::spawn(runtime.as_ref(), async move {
            resume.send(()).unwrap();
        });
        let result = rt::block_on(result).unwrap();
        rt::block_on(heartbeat);
        assert_eq!(result.hash, hash(&vec![b'x'; 2 << 20]));
        assert!(result.edited.diff.len() <= DIFF_LIMIT);
    }

    #[test]
    fn edits_apply_against_the_original_in_any_order() {
        let original = "one\ntwo\nthree\nfour\n";
        let (content, diff) = apply(
            Some(original),
            &edits(&[("four", "4"), ("one\ntwo", "1")]),
            Limits::default(),
        )
        .unwrap();
        assert_eq!(content, "1\nthree\n4\n");
        assert_eq!(
            diff.text,
            "@@ -1,2 +1,1 @@\n-one\n-two\n+1\n@@ -4,1 +3,1 @@\n-four\n+4\n"
        );
        assert_eq!((diff.added, diff.removed), (2, 3));
        // An edit's `new` is never searched by a later one.
        let (swapped, _) = apply(
            Some("a b"),
            &edits(&[("a", "b"), ("b", "a")]),
            Limits::default(),
        )
        .unwrap();
        assert_eq!(swapped, "b a");
    }

    #[test]
    fn every_old_must_match_exactly_once_without_overlapping() {
        let text = Some("x x y\n");
        let err =
            |pairs: &[(&str, &str)]| apply(text, &edits(pairs), Limits::default()).err().unwrap();
        assert_eq!(err(&[("z", "")]), "edit 1: `old` is not in the file");
        assert_eq!(
            err(&[("y", "Y"), ("x", "")]),
            "edit 2: `old` appears 2 times; include more of the text around it"
        );
        assert_eq!(err(&[("x y", "1"), ("y\n", "2")]), "edits 1 and 2 overlap");
        assert_eq!(
            err(&[("y", "1"), ("", "2")]),
            "edit 2: an empty `old` only works as the only edit"
        );
        assert!(apply(None, &edits(&[("x", "y")]), Limits::default()).is_err());
        assert_eq!(
            apply(text, &[], Limits::default()).err().unwrap(),
            "no edits"
        );
    }

    #[test]
    fn an_empty_old_writes_the_whole_file() {
        let (created, diff) = apply(None, &edits(&[("", "a\nb\n")]), Limits::default()).unwrap();
        assert_eq!(created, "a\nb\n");
        assert_eq!(diff.text, "@@ -1,0 +1,2 @@\n+a\n+b\n");
        let (replaced, diff) =
            apply(Some("old\n"), &edits(&[("", "new\n")]), Limits::default()).unwrap();
        assert_eq!(replaced, "new\n");
        assert_eq!((diff.added, diff.removed), (1, 1));
    }

    fn registry() -> Registry {
        Registry::default()
            .with(&ext_shell::Shell)
            .and_then(|r| r.with(&crate::Edit))
            .unwrap()
    }

    fn change(rig: &Rig, path: &str, pairs: &[(&str, &str)]) -> Outcome {
        let edits: Vec<Value> = pairs
            .iter()
            .map(|(old, new)| json!({ "old": old, "new": new }))
            .collect();
        rig.call("edit", &json!({ "path": path, "edits": edits }))
    }

    fn failed(outcome: Outcome) -> String {
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
            matches!(&created, Outcome::Done { output } if output.parse::<Value>().unwrap()["created"] == true)
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\n");
        let Outcome::Done { output } = change(&rig, "notes.txt", &[("two", "2")]) else {
            panic!()
        };
        assert_eq!(
            output.parse::<Value>().unwrap()["diff"],
            "@@ -2,1 +2,1 @@\n-two\n+2\n"
        );
        let input = Change {
            path: "notes.txt".into(),
            edits: Vec::new(),
        };
        let card = Edit::present_result(&input, &output.parse().unwrap());
        assert_eq!(card.unwrap().note, "+1 -1");

        // Changed behind the session's back, even across a restart.
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

        // A file the session never wrote has nothing to be stale against.
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
