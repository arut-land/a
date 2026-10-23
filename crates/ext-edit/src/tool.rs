//! `edit`: exact replacements in one file, all against the file as it was.
//!
//! A stale edit records the current hash so a retry can proceed after inspection.
//! Reads through `bash` are not tracked.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use protocol::Card;
use rt::BlockingClass::{Cpu, Storage};
use serde::{Deserialize, Serialize};

use ext::{Cx, Fold, Json, Schema, Tool};

pub struct Edit;

const DIFF_LIMIT: usize = 16 * 1024;

#[derive(Clone, Copy)]
struct Limits {
    file: usize,
    diff_bytes: usize,
    diff_work: usize,
}

const LIMITS: Limits = Limits {
    file: 8 << 20,
    diff_bytes: DIFF_LIMIT,
    diff_work: 1 << 20,
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
                    "{shown}: edit conflict ({reason:?}); inspect the file and retry"
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

impl From<std::io::Error> for EditError {
    fn from(error: std::io::Error) -> Self {
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
        let (tx, rx) = rt::oneshot::channel();
        let cancel = self.cancel.clone();
        let job = move || {
            tx.send(if cancel.is_set() {
                Err(EditError::Failed("edit cancelled".into()))
            } else {
                work()
            });
        };
        self.rt.spawn_blocking(class, bytes, Box::new(job)).await?;
        rx.await
            .ok_or_else(|| EditError::Failed("edit worker panicked".into()))?
    }

    async fn edit(
        &self,
        path: PathBuf,
        edits: Arc<[Replace]>,
        prior_hash: Option<u64>,
        limits: Limits,
    ) -> Result<(Edited, u64), EditError> {
        let captured = size_of_val(edits.as_ref())
            + edits
                .iter()
                .map(|edit| edit.old.len() + edit.new.len())
                .sum::<usize>();
        if captured > limits.file {
            return Err(EditError::Failed("edits exceed byte limit".into()));
        }
        let target = path.clone();
        let transaction = self
            .run(Storage, limits.file * 2 + captured, move || {
                Ok(sys::fs::EditTransaction::capture(&target, limits.file)?)
            })
            .await?;
        let (transaction, content, edited, hash) = self
            .run(Cpu, limits.file * 5 + captured, move || {
                let original = transaction.original();
                if let Some(bytes) = original {
                    let current = hash(bytes);
                    if prior_hash.is_some_and(|prior| prior != current) {
                        return Err(EditError::Stale(Wrote {
                            path,
                            hash: current,
                        }));
                    }
                }
                let text = original
                    .map(std::str::from_utf8)
                    .transpose()
                    .map_err(|_| EditError::InvalidText)?;
                let (content, diff) = apply(text, &edits, limits).map_err(EditError::Failed)?;
                let hash = hash(content.as_bytes());
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
                    },
                };
                Ok((transaction, content, edited, hash))
            })
            .await?;
        self.run(
            Storage,
            limits.file + content.len() + limits.diff_bytes * 2,
            move || {
                let mut edited = edited;
                edited.publication = transaction.publish(content.as_bytes(), false)?;
                Ok((edited, hash))
            },
        )
        .await
    }
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
    let whole = matches!(edits, [Replace { old, .. }] if old.is_empty());
    let original = match original {
        Some(text) => text,
        None if whole => "",
        None => return Err("no such file; to create it, give one edit with an empty `old`".into()),
    };
    if edits.is_empty() {
        return Err("no edits".to_owned());
    }
    if original.len().saturating_mul(edits.len()) > 64 << 20 {
        return Err("edit matching exceeds work limit".into());
    }
    let mut spans = Vec::with_capacity(edits.len());
    for (n, Replace { old, new }) in (1..).zip(edits) {
        if whole {
            spans.push((0, original.len(), new.as_str(), n));
            break;
        }
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
        .fold(original.len(), |size, &(start, end, new, _)| {
            size - (end - start) + new.len()
        });
    if size > limits.file * 2 {
        return Err("result exceeds byte limit".into());
    }
    let (mut content, mut diff, mut last) = (String::with_capacity(size), Diff::default(), 0);
    let (mut old_line, mut new_line) = (1, 1);
    for &(start, end, new, _) in &spans {
        content.push_str(&original[last..start]);
        content.push_str(new);
        let skipped = original[last..start].matches('\n').count();
        old_line += skipped;
        new_line += skipped;
        last = end;
        diff.hunk(original, (start, end), new, (old_line, new_line), limits);
        old_line += original[start..end].matches('\n').count();
        new_line += new.matches('\n').count();
    }
    content.push_str(&original[last..]);
    Ok((content, diff))
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
                ..LIMITS
            },
            Limits {
                diff_bytes: 64,
                diff_work: usize::MAX,
                ..LIMITS
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
    fn limits_reject_growth_and_matching_before_building_the_result() {
        let limits = Limits { file: 4, ..LIMITS };
        assert!(apply(Some("abcd"), &edits(&[("a", "123456789")]), limits).is_err());
        let original = "x".repeat(1 << 20);
        let many = edits(&vec![("x", "y"); 65]);
        assert_eq!(
            apply(Some(&original), &many, LIMITS).err().unwrap(),
            "edit matching exceeds work limit"
        );
        let rig = Rig::new("edit-limits", registry());
        let path = rig.workspace().join("large.txt");
        std::fs::write(&path, "12345").unwrap();
        let runtime = rt::Threads::new(1);
        assert!(
            rt::block_on(
                Transaction {
                    rt: &runtime,
                    cancel: &ext::Cancel::default()
                }
                .edit(path.clone(), edits(&[("1", "2")]).into(), None, limits)
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "12345");
    }

    #[test]
    fn writing_does_not_clone_the_complete_result() {
        let rig = Rig::new("edit-owned-write", registry());
        let file = rig.workspace().join("large.txt");
        let content = "a".repeat(1 << 20);
        let pointer = content.as_ptr() as usize;
        let runtime = rt::Threads::new(1);
        let cancel = ext::Cancel::default();
        rt::block_on(
            Transaction {
                rt: &runtime,
                cancel: &cancel,
            }
            .run(Storage, content.len(), move || {
                assert_eq!(content.as_ptr() as usize, pointer);
                std::fs::write(&file, content)?;
                assert_eq!(std::fs::metadata(file)?.len(), 1 << 20);
                Ok(())
            }),
        )
        .unwrap();
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
                assert_eq!(current, hash(&vec![b'x'; 2 << 20]));
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
            "edit 2: `old` appears 2 times; include more of the text around it"
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
