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
use std::path::{Path, PathBuf};

use protocol::Card;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ext::Fold;

use crate::{Cx, Tool};

pub struct Edit;

/// The most of a diff an outcome keeps.
const DIFF_LIMIT: usize = 16 * 1024;

#[derive(Deserialize)]
pub struct Change {
    pub path: String,
    pub edits: Vec<Replace>,
}

#[derive(Deserialize)]
pub struct Replace {
    pub old: String,
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
#[derive(Debug, Default)]
pub struct Seen(HashMap<PathBuf, u64>);

/// A file this session wrote, and the hash of what it wrote.
#[derive(Debug, Serialize, Deserialize)]
pub struct Wrote {
    path: PathBuf,
    hash: u64,
}

impl Seen {
    fn change(path: &Path, content: &[u8]) -> Wrote {
        Wrote {
            path: path.to_path_buf(),
            hash: hash(content),
        }
    }
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

    fn schema() -> Value {
        super::schema(
            r#"{
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative to the working directory, or absolute."
                    },
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old": { "type": "string", "description": "The exact text to replace." },
                                "new": { "type": "string", "description": "What replaces it." }
                            },
                            "required": ["old", "new"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["path", "edits"],
                "additionalProperties": false
            }"#,
        )
    }

    async fn call(&self, input: Change, cx: &Cx) -> Result<Edited, String> {
        let path = cx.dir().join(&input.path);
        let shown = input.path.clone();
        let read = path.clone();
        let original = rt::unblock(&*cx.rt, move || match std::fs::read(&read) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        })
        .await
        .map_err(|e| format!("{shown}: {e}"))?;

        if let Some(bytes) = &original {
            let last = cx.state(|seen: &Seen| seen.0.get(&path).copied());
            if last.is_some_and(|last| last != hash(bytes)) {
                cx.update::<Seen>(Seen::change(&path, bytes));
                return Err(format!(
                    "{shown} changed since this session last edited it; look at it again before editing it"
                ));
            }
        }
        let text = match &original {
            Some(bytes) => {
                Some(std::str::from_utf8(bytes).map_err(|_| format!("{shown} is not UTF-8 text"))?)
            }
            None => None,
        };
        let (content, diff) = apply(text, &input.edits).map_err(|e| format!("{shown}: {e}"))?;

        let write = (path.clone(), content.clone());
        rt::unblock(&*cx.rt, move || {
            if let Some(parent) = write.0.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&write.0, &write.1)
        })
        .await
        .map_err(|e| format!("{shown}: {e}"))?;
        cx.update::<Seen>(Seen::change(&path, content.as_bytes()));
        Ok(Edited {
            path,
            created: original.is_none(),
            replaced: input.edits.len(),
            added: diff.added,
            removed: diff.removed,
            diff: diff.text,
        })
    }

    fn to_model(edited: &Edited) -> llm::Output {
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
        })
    }
}

#[derive(Default)]
struct Diff {
    text: String,
    added: usize,
    removed: usize,
}

/// `edits` applied to `original`, and their diff.
fn apply(original: Option<&str>, edits: &[Replace]) -> Result<(String, Diff), String> {
    if let [Replace { old, new }] = edits
        && old.is_empty()
    {
        let mut diff = Diff::default();
        diff.lines((1, 1), original.unwrap_or_default(), new);
        return Ok((new.clone(), diff));
    }
    let original = original
        .ok_or("no such file; to create it, give one edit with an empty `old`".to_owned())?;
    if edits.is_empty() {
        return Err("no edits".to_owned());
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
    let (mut content, mut diff, mut last) = (String::new(), Diff::default(), 0);
    // How far line numbers in the new text have moved from the old.
    let mut shift: isize = 0;
    for &(start, end, new, _) in &spans {
        content.push_str(&original[last..start]);
        content.push_str(new);
        last = end;
        let line = original[..start].matches('\n').count() + 1;
        diff.hunk(
            original,
            (start, end),
            new,
            (line, line.saturating_add_signed(shift)),
        );
        shift += line_count(new).cast_signed() - line_count(&original[start..end]).cast_signed();
    }
    content.push_str(&original[last..]);
    Ok((content, diff))
}

fn line_count(text: &str) -> usize {
    text.matches('\n').count()
}

impl Diff {
    /// The hunk for replacing `original[start..end]` with `new`: the whole
    /// lines the span touches, before and after.
    fn hunk(
        &mut self,
        original: &str,
        (start, end): (usize, usize),
        new: &str,
        lines: (usize, usize),
    ) {
        let from = original[..start].rfind('\n').map_or(0, |at| at + 1);
        let to = original[end..]
            .find('\n')
            .map_or(original.len(), |at| end + at + 1);
        let after = format!("{}{new}{}", &original[from..start], &original[end..to]);
        self.lines(lines, &original[from..to], &after);
    }

    /// A hunk turning `old` lines, from line `lines.0`, into `new`, from
    /// line `lines.1`.
    fn lines(&mut self, lines: (usize, usize), old: &str, new: &str) {
        let old: Vec<&str> = old.lines().collect();
        let new: Vec<&str> = new.lines().collect();
        self.removed += old.len();
        self.added += new.len();
        if self.text.len() >= DIFF_LIMIT {
            return;
        }
        let _ = writeln!(
            self.text,
            "@@ -{},{} +{},{} @@",
            lines.0,
            old.len(),
            lines.1,
            new.len()
        );
        for line in old {
            let _ = writeln!(self.text, "-{line}");
        }
        for line in new {
            let _ = writeln!(self.text, "+{line}");
        }
        if self.text.len() > DIFF_LIMIT {
            let cut = self.text.floor_char_boundary(DIFF_LIMIT);
            self.text.truncate(cut);
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
    use crate::testing::Rig;
    use crate::{DynTool, Policy, builtins};
    use protocol::Outcome;
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
    fn edits_apply_against_the_original_in_any_order() {
        let original = "one\ntwo\nthree\nfour\n";
        let (content, diff) =
            apply(Some(original), &edits(&[("four", "4"), ("one\ntwo", "1")])).unwrap();
        assert_eq!(content, "1\nthree\n4\n");
        assert_eq!(
            diff.text,
            "@@ -1,2 +1,1 @@\n-one\n-two\n+1\n@@ -4,1 +3,1 @@\n-four\n+4\n"
        );
        assert_eq!((diff.added, diff.removed), (2, 3));
        // An edit's `new` is never searched by a later one.
        let (swapped, _) = apply(Some("a b"), &edits(&[("a", "b"), ("b", "a")])).unwrap();
        assert_eq!(swapped, "b a");
    }

    #[test]
    fn every_old_must_match_exactly_once_without_overlapping() {
        let text = Some("x x y\n");
        let err = |pairs: &[(&str, &str)]| apply(text, &edits(pairs)).err().unwrap();
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
        assert!(apply(None, &edits(&[("x", "y")])).is_err());
        assert_eq!(apply(text, &[]).err().unwrap(), "no edits");
    }

    #[test]
    fn an_empty_old_writes_the_whole_file() {
        let (created, diff) = apply(None, &edits(&[("", "a\nb\n")])).unwrap();
        assert_eq!(created, "a\nb\n");
        assert_eq!(diff.text, "@@ -1,0 +1,2 @@\n+a\n+b\n");
        let (replaced, diff) = apply(Some("old\n"), &edits(&[("", "new\n")])).unwrap();
        assert_eq!(replaced, "new\n");
        assert_eq!((diff.added, diff.removed), (1, 1));
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
    fn edits_create_and_change_files_and_catch_stale_reads() {
        let rig = Rig::new("edit", builtins(&Policy::default()).unwrap());
        let file = rig.workspace().join("notes.txt");
        let created = change(&rig, "notes.txt", &[("", "one\ntwo\n")]);
        assert!(matches!(&created, Outcome::Done { output } if output.value()["created"] == true));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\n");
        let Outcome::Done { output } = change(&rig, "notes.txt", &[("two", "2")]) else {
            panic!()
        };
        assert_eq!(output.value()["diff"], "@@ -2,1 +2,1 @@\n-two\n+2\n");
        let card =
            DynTool::present_result(&Edit, &json!({ "path": "notes.txt", "edits": [] }), &output);
        assert_eq!(card.unwrap().note, "+1 -1");

        // Changed behind the session's back, even across a restart.
        std::fs::write(&file, "one\n2\nthree\n").unwrap();
        rig.restart();
        let stale = failed(change(&rig, "notes.txt", &[("one", "1")]));
        assert!(
            stale.contains("changed since this session last edited it"),
            "{stale}"
        );
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
        let rig = Rig::new("edit-cwd", builtins(&Policy::default()).unwrap());
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
