//! Frozen context reads ordinary source logs; only projections exist in memory.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::sync::Arc;

use ext::blob::Places;
use protocol::publication::{
    ContextCut, ContextInput, ContextLayer, ContextValue, LogCut, RunnerKind, SpawnImage,
};
use protocol::{Body, Entry};

use crate::store::LogStore;

/// Publication checkpoints avoid refolding prefixes when side turns freeze an older cut.
#[derive(Default)]
pub(crate) struct Cursor(BTreeMap<u64, ContextCut>);

impl Cursor {
    pub(crate) fn before(&self, through: u64) -> (u64, ContextCut) {
        self.0
            .range(..=through)
            .next_back()
            .map_or((0, ContextCut::default()), |(&seq, cut)| (seq, cut.clone()))
    }
    pub(crate) fn insert(&mut self, through: u64, image: ContextCut) {
        self.0.insert(through, image);
    }
}

pub(crate) fn freeze(
    session: protocol::SessionId,
    through: u64,
    mut image: ContextCut,
    entries: &[Entry],
) -> io::Result<ContextCut> {
    for entry in entries {
        image.publications.apply(entry);
        match &entry.body {
            Body::Spawned { intent } => {
                image.runner = SpawnImage::from_json(intent)?.runner;
                image.presentation = Some(LogCut {
                    session,
                    through: entry.seq,
                });
            }
            Body::Extensions { .. } => {
                image.presentation = Some(LogCut {
                    session,
                    through: entry.seq,
                });
            }
            _ => {}
        }
    }
    image.source = protocol::Seq::new(through).map(|through| LogCut { session, through });
    Ok(image)
}

/// The log and blob readers share one lifetime across a projection.
#[derive(Clone)]
pub(crate) struct Source {
    logs: Arc<dyn LogStore>,
    places: Places,
}

impl Source {
    pub(crate) fn new(logs: Arc<dyn LogStore>, places: Places) -> Self {
        Self { logs, places }
    }

    fn suffix(
        &self,
        cut: LogCut,
        window: Option<usize>,
        runner: RunnerKind,
    ) -> io::Result<(Vec<Segment>, Option<protocol::Seq>)> {
        let mut entries = self.open(cut)?;
        let mut nodes = Vec::new();
        let mut node = Segment::default();
        let mut start = None;
        for entry in &mut entries {
            let entry = entry?;
            let boundary = matches!(entry.body, Body::ModelResult { r#for: None, .. });
            node.push(&entry)?;
            if !boundary && node.inputs.len() < 64 {
                continue;
            }
            node.finish();
            nodes.push(std::mem::take(&mut node));
            if let Some(window) = window
                && let Some(seq) = suffix_start(&nodes, window, runner)
            {
                start = Some(seq);
                break;
            }
        }
        if !node.inputs.is_empty() {
            node.finish();
            nodes.push(node);
        }
        let start =
            start.or_else(|| window.and_then(|window| suffix_start(&nodes, window, runner)));
        Ok((nodes, start))
    }

    fn open(&self, cut: LogCut) -> io::Result<Box<dyn Iterator<Item = io::Result<Entry>> + Send>> {
        let reader = self.logs.cut(cut);
        #[cfg(test)]
        if reader.is_err() {
            for logs in crate::lock(&TEST_LOGS).iter() {
                if let Ok(reader) = logs.cut(cut) {
                    return Ok(reader);
                }
            }
        }
        reader
    }
}

impl std::ops::Deref for Source {
    type Target = Places;
    fn deref(&self) -> &Places {
        &self.places
    }
}

impl From<&Source> for Source {
    fn from(source: &Source) -> Self {
        source.clone()
    }
}

#[cfg(test)]
impl Default for Source {
    fn default() -> Self {
        TEST_STORE.with(|(_, logs, source)| Self::new(logs.clone(), source.places.clone()))
    }
}

#[cfg(test)]
impl From<&Places> for Source {
    fn from(places: &Places) -> Self {
        Self {
            places: places.clone(),
            ..Self::default()
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct PresentationImage {
    pub(crate) installed: Vec<protocol::Installed>,
    pub(crate) extensions: Vec<String>,
}

pub(crate) fn presentation(image: &ContextCut, source: &Source) -> io::Result<protocol::Json> {
    let cut = image
        .presentation
        .ok_or_else(|| io::Error::other("missing presentation cut"))?;
    let entry = source
        .open(cut)?
        .next()
        .transpose()?
        .ok_or_else(|| io::Error::other("missing presentation authority"))?;
    match entry.body {
        Body::Extensions { installed } => {
            return protocol::Json::encode(&PresentationImage {
                extensions: installed
                    .iter()
                    .map(|installed| installed.name.clone())
                    .collect(),
                installed,
            })
            .map_err(io::Error::other);
        }
        Body::Spawned { intent } => {
            let authority =
                crate::driver::spawn::SpawnIntent::from_json(&intent).map_err(io::Error::other)?;
            return protocol::Json::encode(&PresentationImage {
                installed: authority.installed,
                extensions: authority.extensions,
            })
            .map_err(io::Error::other);
        }
        _ => {}
    }
    Err(io::Error::other("missing presentation authority"))
}

#[derive(Default)]
struct Segment {
    inputs: Vec<ContextInput>,
    inherited: Vec<ContextCut>,
    edits: Vec<(u64, u64, Option<String>)>,
}

impl Segment {
    fn push(&mut self, entry: &Entry) -> io::Result<()> {
        let value = if let Body::Spawned { intent } = &entry.body {
            let spawn = SpawnImage::from_json(intent)?;
            let at = self.inherited.len();
            self.inherited.push(spawn.image);
            ContextValue::Inherited(at)
        } else {
            if let Some(edit) = crate::transcript::context_edit(entry) {
                self.edits.push(edit);
            }
            crate::transcript::context_fact(&entry.body).unwrap_or(ContextValue::Position)
        };
        self.inputs.push(ContextInput(entry.seq, value));
        Ok(())
    }

    fn finish(&mut self) {
        self.inputs.reverse();
        self.edits.reverse();
    }
}

pub(crate) fn expand(image: &ContextCut, source: &Source) -> io::Result<Vec<ContextLayer>> {
    expand_with(image, source, None)
}

pub(crate) fn expand_bounded(
    image: &ContextCut,
    source: &Source,
    exchanges: u16,
) -> io::Result<Vec<ContextLayer>> {
    if exchanges == 0 {
        return Ok(Vec::new());
    }
    expand_with(image, source, Some(usize::from(exchanges) + 1))
}

fn expand_with(
    image: &ContextCut,
    source: &Source,
    window: Option<usize>,
) -> io::Result<Vec<ContextLayer>> {
    if window == Some(0) {
        return Ok(Vec::new());
    }
    let Some(cut) = image.source else {
        return Ok(Vec::new());
    };
    let (nodes, start) = source.suffix(cut, window, image.runner)?;
    let edits = crate::transcript::Edits::new(
        nodes
            .iter()
            .rev()
            .flat_map(|node| node.edits.iter().cloned())
            .collect(),
    );
    let local_users = window.map_or_default(|_| user_boundaries(&nodes, &edits, image.runner));
    let completed: HashSet<_> = nodes
        .iter()
        .flat_map(|node| &node.inputs)
        .filter_map(|ContextInput(_, value)| match value {
            ContextValue::ToolResult(call, ..) => Some(call.0),
            ContextValue::Taken(input) | ContextValue::Into(input, _) => Some(*input),
            _ => None,
        })
        .collect();
    let runner = image.runner;
    let mut image = vec![ContextLayer(runner, Vec::new())];
    for node in nodes.into_iter().rev() {
        for ContextInput(seq, value) in node.inputs {
            if start.is_some_and(|start| seq < start) {
                continue;
            }
            let value = if let Some((from, text)) = edits.at(seq) {
                if from != seq.get() {
                    continue;
                }
                let Some(text) = text else {
                    continue;
                };
                ContextValue::User(text.clone())
            } else if let ContextValue::Inherited(at) = value {
                // The leading exchange belongs to the whole cursor, not to
                // every inherited scope it crosses.
                let remaining = window.map(|window| {
                    window.saturating_sub(local_users.iter().filter(|user| **user > seq).count())
                });
                let mut inherited = expand_with(&node.inherited[at], source, remaining)?;
                if inherited.is_empty() {
                    continue;
                }
                let offset = image.len();
                for ContextLayer(_, inputs) in &mut inherited {
                    for ContextInput(_, value) in inputs {
                        if let ContextValue::Inherited(child) = value {
                            *child += offset;
                        }
                    }
                }
                image.extend(inherited);
                ContextValue::Inherited(offset)
            } else {
                if matches!(value, ContextValue::Position) {
                    continue;
                }
                if matches!(
                    value,
                    ContextValue::ToolCall(..)
                        | ContextValue::Queued(..)
                        | ContextValue::Inbox(..)
                        | ContextValue::Child(_)
                        | ContextValue::Background(..)
                        | ContextValue::Job(..)
                ) && !completed.contains(&seq)
                {
                    continue;
                }
                value
            };
            image[0].1.push(ContextInput(seq, value));
        }
    }
    Ok(image)
}

/// Keep one extra exchange so selection never starts within a tool group.
/// An older call/queue dependency forces the reader to continue into the preceding segment.
fn suffix_start(nodes: &[Segment], window: usize, runner: RunnerKind) -> Option<protocol::Seq> {
    let edits = crate::transcript::Edits::new(
        nodes
            .iter()
            .rev()
            .flat_map(|node| node.edits.iter().cloned())
            .collect(),
    );
    let boundaries = user_boundaries(nodes, &edits, runner);
    let start = *boundaries.get(boundaries.len().checked_sub(window)?)?;
    if window == 1 {
        return Some(start);
    }
    let requested = boundaries[boundaries.len() - (window - 1)];
    for ContextInput(seq, value) in nodes.iter().flat_map(|node| &node.inputs) {
        // The extra leading exchange protects tool-group boundaries; its own
        // dependencies cannot extend the requested suffix indefinitely.
        if *seq < requested || edits.at(*seq).is_some() {
            continue;
        }
        let dependency = match value {
            ContextValue::ToolResult(call, ..)
            | ContextValue::Background(call, ..)
            | ContextValue::Job(call, ..) => Some(call.0),
            ContextValue::Taken(input) | ContextValue::Into(input, _) => Some(*input),
            _ => None,
        };
        if dependency.is_some_and(|seq| seq < start) {
            return None;
        }
    }
    Some(start)
}

fn user_boundaries(
    nodes: &[Segment],
    edits: &crate::transcript::Edits,
    runner: RunnerKind,
) -> Vec<protocol::Seq> {
    let taken: HashSet<_> = nodes
        .iter()
        .flat_map(|node| &node.inputs)
        .filter_map(|ContextInput(seq, value)| {
            if edits.at(*seq).is_none()
                && let ContextValue::Taken(input) = value
            {
                Some(*input)
            } else {
                None
            }
        })
        .collect();
    nodes
        .iter()
        .rev()
        .flat_map(|node| &node.inputs)
        .filter_map(|ContextInput(seq, value)| {
            match edits.at(*seq) {
                Some((from, Some(_))) if from == seq.get() => return Some(*seq),
                Some(_) => return None,
                None => {}
            }
            let turn = match value {
                ContextValue::User(_) => return Some(*seq),
                ContextValue::Queued(segments, _) if taken.contains(seq) => {
                    if runner == RunnerKind::Agent {
                        return Some(*seq);
                    }
                    crate::transcript::ModelInput::Direct(&segments.text()).turn()
                }
                ContextValue::Inbox(_, text) if taken.contains(seq) => {
                    if runner == RunnerKind::Agent {
                        return Some(*seq);
                    }
                    crate::transcript::ModelInput::Inbox(text).turn()
                }
                _ => return None,
            };
            turn.is_none_or(|turn| {
                turn.messages
                    .iter()
                    .any(|message| matches!(message, llm::Message::User(_)))
            })
            .then_some(*seq)
        })
        .collect()
}

/// Attachment access follows the effective projection, including inherited payloads.
pub(crate) fn references(
    image: &ContextCut,
    source: &Source,
    blob: protocol::BlobId,
) -> io::Result<bool> {
    Ok(expand(image, source)?
        .into_iter()
        .flat_map(|ContextLayer(_, inputs)| inputs)
        .flat_map(|ContextInput(_, value)| value.blobs())
        .any(|held| matches!(held, protocol::Held::Blob(reference) if reference.id == blob)))
}

#[cfg(test)]
static TEST_LOGS: std::sync::Mutex<Vec<Arc<dyn LogStore>>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn register_test_logs(logs: Arc<dyn LogStore>) {
    let mut known = crate::lock(&TEST_LOGS);
    if !known.iter().any(|old| Arc::ptr_eq(old, &logs)) {
        known.push(logs);
    }
}

#[cfg(test)]
thread_local! {
    static TEST_STORE: (tempfile::TempDir, Arc<crate::store::Jsonl>, Source) = {
        let dir = tempfile::tempdir().unwrap();
        let logs = Arc::new(crate::store::Jsonl::open(dir.path().join("sessions")).unwrap());
        register_test_logs(logs.clone());
        let source = Source::new(logs.clone(), Places::default());
        (dir, logs, source)
    };
}

#[cfg(test)]
pub(crate) fn test_places() -> Source {
    Source::default()
}

#[cfg(test)]
pub(crate) fn test_append(entries: &[Entry]) -> ContextCut {
    TEST_STORE.with(|(_, logs, _)| {
        let session = protocol::SessionId::new();
        logs.import(
            &crate::store::Header {
                id: session,
                workspace: std::path::PathBuf::new(),
                parent: None,
            },
            entries,
        )
        .unwrap();
        freeze(
            session,
            entries.last().map_or(0, |entry| entry.seq.get()),
            ContextCut::default(),
            entries,
        )
        .unwrap()
    })
}

#[cfg(test)]
pub(crate) fn test_append_chunks(entries: &[Entry], _: usize) -> ContextCut {
    test_append(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Header, Jsonl, LogStore};
    use protocol::publication::RunnerKind;
    use protocol::{CallId, Epoch, Json, Mode, Payload, SessionId, ToolOutcome};
    use std::sync::Arc;

    fn entry(seq: u64, body: Body) -> Entry {
        Entry::new(protocol::Seq::new(seq).unwrap(), Epoch(0), body)
    }

    fn user(seq: u64, text: &str) -> Entry {
        entry(
            seq,
            Body::Input {
                segments: text.into(),
                mode: Mode::Queue,
                taken: true,
                origin: None,
                resume: None,
                force: false,
            },
        )
    }

    fn spawn(seq: u64, image: ContextCut) -> Entry {
        entry(
            seq,
            Body::Spawned {
                intent: SpawnImage {
                    runner: RunnerKind::Agent,
                    authority: Json::null(),
                    image,
                }
                .to_json()
                .unwrap(),
            },
        )
    }

    #[test]
    fn a_presentation_cut_keeps_its_admitted_generation_after_later_extensions() {
        TEST_STORE.with(|(_, logs, source)| {
            let old = append(
                ContextCut::default(),
                [entry(
                    1,
                    Body::Extensions {
                        installed: Vec::new(),
                    },
                )],
                logs,
            )
            .unwrap();
            let new = append(
                old.clone(),
                [entry(
                    2,
                    Body::Extensions {
                        installed: vec![protocol::Installed {
                            name: "later".into(),
                            version: 1,
                            image: None,
                        }],
                    },
                )],
                logs,
            )
            .unwrap();
            assert_eq!(
                old.presentation.unwrap().through,
                protocol::Seq::new(1).unwrap()
            );
            let original: PresentationImage = presentation(&old, source).unwrap().parse().unwrap();
            let changed: PresentationImage = presentation(&new, source).unwrap().parse().unwrap();
            assert_eq!(original.installed, []);
            assert_eq!(changed.extensions, ["later"]);
        });
    }

    #[test]
    fn a_frozen_child_ignores_later_parent_edits() {
        TEST_STORE.with(|(_, store, places)| {
            let cut = append(ContextCut::default(), [user(1, "original")], store).unwrap();
            let child = append(ContextCut::default(), [spawn(1, cut.clone())], store).unwrap();
            let edited = append(
                cut,
                [entry(
                    2,
                    Body::Edit {
                        target: protocol::Seq::new(1).unwrap(),
                        upto: None,
                        replace: Some(Json::encode("changed").unwrap()),
                    },
                )],
                store,
            )
            .unwrap();
            let text = |image| {
                expand(image, places)
                    .unwrap()
                    .into_iter()
                    .flat_map(|ContextLayer(_, inputs)| inputs)
                    .filter_map(|ContextInput(_, value)| {
                        if let ContextValue::User(text) = value {
                            Some(text)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(text(&child), ["original"]);
            assert_eq!(text(&edited), ["changed"]);
        });
    }

    #[test]
    fn bounded_side_context_does_not_materialize_unrequested_ancestry() {
        TEST_STORE.with(|(_, store, places)| {
            let parent = append(
                ContextCut::default(),
                [user(1, &"ancestor".repeat(100_000))],
                store,
            )
            .unwrap();
            let branch = append(ContextCut::default(), [spawn(1, parent.clone())], store).unwrap();
            let branch = append(
                branch,
                [
                    user(2, "older local"),
                    user(3, "recent local"),
                    user(4, "newest local"),
                ],
                store,
            )
            .unwrap();
            let entries = [spawn(1, branch.clone())];
            let registry = Arc::new(ext::host::Registry::default());
            let full =
                crate::transcript::Transcript::placed(&entries, registry.clone(), "", places)
                    .unwrap()
                    .into_context();
            let current = llm::Messages::from_iter([llm::Message::user("mandatory instructions")]);
            let policy = llm::ContextPolicy {
                exchanges: 1,
                bytes: 4096,
            };
            let expected = policy.assemble(&full, &[], &current).unwrap();
            remove_source(&parent.source.unwrap());
            let recent = crate::transcript::Transcript::replay_bounded(
                |each| {
                    for entry in &entries {
                        each(entry);
                    }
                    Ok(())
                },
                registry,
                "",
                places,
                Some(policy.exchanges),
            )
            .unwrap()
            .into_context();
            assert_eq!(policy.assemble(&recent, &[], &current).unwrap(), expected);
            assert!(
                expand(&branch, places).is_err(),
                "the full projection still needs the removed ancestor"
            );
        });
    }

    #[test]
    fn a_global_window_keeps_system_only_inherited_inputs() {
        TEST_STORE.with(|(_, store, places)| {
            let owner = SessionId::new();
            let mut entries = vec![entry(
                1,
                Body::Spawned {
                    intent: SpawnImage {
                        runner: RunnerKind::ModelOnly,
                        authority: Json::null(),
                        image: ContextCut::default(),
                    }
                    .to_json()
                    .unwrap(),
                },
            )];
            for seq in [2, 4, 6] {
                let mut turn = ext::Conversation::turn::<ext_title::Title>("system", "")
                    .request(CallId::new(seq).unwrap());
                turn.messages =
                    llm::Messages::from_iter([llm::Message::system(format!("system {seq}"))]);
                let input = protocol::publication::ThreadInput {
                    turn: turn.intent(None).unwrap(),
                    context: ContextCut::default(),
                }
                .to_json()
                .unwrap();
                entries.extend([
                    entry(
                        seq,
                        Body::Enqueue {
                            envelope: protocol::Envelope::new(
                                protocol::EnvelopeId {
                                    sender: owner,
                                    call: CallId::new(seq).unwrap(),
                                    purpose: "system".into(),
                                },
                                owner,
                                protocol::Carried::Message(input.raw().into()),
                                Vec::new(),
                            )
                            .unwrap(),
                        },
                    ),
                    entry(
                        seq + 1,
                        Body::Take {
                            input: protocol::Seq::new(seq).unwrap(),
                            into: None,
                        },
                    ),
                ]);
            }
            let parent = append(ContextCut::default(), entries, store).unwrap();
            let child = append(
                ContextCut::default(),
                [spawn(1, parent), user(2, "first"), user(3, "second")],
                store,
            )
            .unwrap();
            let registry = Arc::new(ext::host::Registry::default());
            let policy = llm::ContextPolicy {
                exchanges: 2,
                bytes: 100_000,
            };
            let current = llm::Messages::from_iter([llm::Message::user("mandatory")]);
            let full =
                crate::transcript::Transcript::cut(&child, registry.clone(), "", places, None)
                    .unwrap()
                    .into_messages();
            let bounded = crate::transcript::Transcript::cut(
                &child,
                registry,
                "",
                places,
                Some(policy.exchanges),
            )
            .unwrap()
            .into_messages();
            assert_eq!(
                policy.assemble(&[], &bounded, &current).unwrap(),
                policy.assemble(&[], &full, &current).unwrap()
            );
        });
    }

    #[test]
    fn a_bounded_side_never_walks_past_its_inherited_exchange_budget() {
        TEST_STORE.with(|(_, store, places)| {
            let mut cut = append(ContextCut::default(), [user(1, "old root")], store).unwrap();
            let mut unrequested = None;
            for depth in 0..24 {
                cut = append(
                    ContextCut::default(),
                    [
                        spawn(1, cut),
                        user(2, &format!("fork {depth}")),
                        entry(
                            3,
                            Body::Assistant {
                                text: format!("answer {depth}"),
                            },
                        ),
                    ],
                    store,
                )
                .unwrap();
                if depth == 12 {
                    unrequested = cut.source;
                }
            }
            let registry = Arc::new(ext::host::Registry::default());
            let policy = llm::ContextPolicy {
                exchanges: 2,
                bytes: 100_000,
            };
            let current = llm::Messages::from_iter([llm::Message::user("mandatory")]);
            let full = crate::transcript::Transcript::cut(&cut, registry.clone(), "", places, None)
                .unwrap()
                .into_messages();
            let expected = policy.assemble(&[], &full, &current).unwrap();
            remove_source(&unrequested.unwrap());
            let recent = crate::transcript::Transcript::cut(
                &cut,
                registry,
                "",
                places,
                Some(policy.exchanges),
            )
            .unwrap()
            .into_messages();
            assert_eq!(policy.assemble(&[], &recent, &current).unwrap(), expected);
            assert!(expand(&cut, places).is_err());
        });
    }

    #[test]
    fn a_bounded_side_never_follows_dependencies_of_its_unrequested_leading_exchange() {
        TEST_STORE.with(|(_, store, places)| {
            let old = append(ContextCut::default(), [user(1, "old")], store).unwrap();
            let mut cut = branch(&old, store);
            let sender = SessionId::new();
            for turn in 0..16 {
                let seq = 10 + turn * 10;
                let mut entries = Vec::new();
                if turn > 0 {
                    entries.push(entry(
                        seq,
                        Body::Enqueue {
                            envelope: protocol::Envelope::new(
                                protocol::EnvelopeId {
                                    sender,
                                    call: CallId::new(seq - 7).unwrap(),
                                    purpose: "job".into(),
                                },
                                sender,
                                protocol::Carried::Job("finished".into()),
                                Vec::new(),
                            )
                            .unwrap(),
                        },
                    ));
                }
                entries.push(user(seq + 1, &format!("input {turn}")));
                if turn > 0 {
                    entries.push(entry(
                        seq + 2,
                        Body::Take {
                            input: protocol::Seq::new(seq).unwrap(),
                            into: None,
                        },
                    ));
                }
                entries.extend([
                    entry(
                        seq + 3,
                        Body::ToolCall {
                            tool: "bash".into(),
                            input: Json::null(),
                            id: Some(format!("job{turn}")),
                            invoke: None,
                            card: None,
                            by: protocol::Audience::Model,
                        },
                    ),
                    entry(
                        seq + 4,
                        Body::ToolResult {
                            call: CallId::new(seq + 3).unwrap(),
                            outcome: protocol::Outcome::Done {
                                output: Json::null().into(),
                            },
                            card: None,
                            held: Vec::new(),
                        },
                    ),
                    entry(
                        seq + 5,
                        Body::Assistant {
                            text: "answer".into(),
                        },
                    ),
                ]);
                cut = append(cut, entries, store).unwrap();
            }
            let registry = Arc::new(ext::host::Registry::default());
            let policy = llm::ContextPolicy {
                exchanges: 2,
                bytes: 100_000,
            };
            let current = llm::Messages::from_iter([llm::Message::user("mandatory")]);
            let full = crate::transcript::Transcript::cut(&cut, registry.clone(), "", places, None)
                .unwrap()
                .into_messages();
            let expected = policy.assemble(&full, &[], &current).unwrap();
            remove_source(&old.source.unwrap());
            let recent = crate::transcript::Transcript::cut(
                &cut,
                registry,
                "",
                places,
                Some(policy.exchanges),
            )
            .unwrap()
            .into_messages();
            assert_eq!(policy.assemble(&recent, &[], &current).unwrap(), expected);
            assert!(expand(&cut, places).is_err());
        });
    }

    #[test]
    fn bounded_thread_history_reads_only_taken_inbox_exchanges() {
        TEST_STORE.with(|(_, store, places)| {
            let old = append(
                ContextCut::default(),
                [
                    entry(
                        1,
                        Body::Spawned {
                            intent: SpawnImage {
                                runner: RunnerKind::ModelOnly,
                                authority: Json::null(),
                                image: ContextCut::default(),
                            }
                            .to_json()
                            .unwrap(),
                        },
                    ),
                    user(2, "unrequested ancestor"),
                ],
                store,
            )
            .unwrap();
            let mut cut = branch(&old, store);
            let mut entries = Vec::new();
            for seq in [3, 5, 7, 9] {
                let turn =
                    ext::Conversation::turn::<ext_title::Title>("test", format!("input {seq}"))
                        .request(CallId::new(seq).unwrap());
                let text = protocol::publication::ThreadInput {
                    turn: turn.intent(None).unwrap(),
                    context: ContextCut::default(),
                }
                .to_json()
                .unwrap();
                entries.extend([
                    entry(
                        seq,
                        Body::Enqueue {
                            envelope: protocol::Envelope::new(
                                protocol::EnvelopeId {
                                    sender: SessionId::new(),
                                    call: CallId::new(seq).unwrap(),
                                    purpose: "title".into(),
                                },
                                SessionId::NIL,
                                protocol::Carried::Message(text.raw().into()),
                                Vec::new(),
                            )
                            .unwrap(),
                        },
                    ),
                    entry(
                        seq + 1,
                        Body::Take {
                            input: protocol::Seq::new(seq).unwrap(),
                            into: None,
                        },
                    ),
                ]);
            }
            cut = append(cut, entries, store).unwrap();
            let registry = Arc::new(ext::host::Registry::default());
            let full = crate::transcript::Transcript::cut(&cut, registry.clone(), "", places, None)
                .unwrap()
                .into_messages();
            let policy = llm::ContextPolicy {
                exchanges: 1,
                bytes: 4096,
            };
            let current = llm::Messages::from_iter([llm::Message::user("mandatory")]);
            let expected = policy.assemble(&full, &[], &current).unwrap();
            remove_source(&old.source.unwrap());
            let recent = crate::transcript::Transcript::cut(
                &cut,
                registry,
                "",
                places,
                Some(policy.exchanges),
            )
            .unwrap()
            .into_messages();
            assert_eq!(policy.assemble(&recent, &[], &current).unwrap(), expected);
            assert!(expand(&cut, places).is_err());
        });
    }

    #[test]
    fn an_owner_cut_preserves_pending_provider_exchanges() {
        TEST_STORE.with(|(_, store, places)| {
            let reply = llm::Response {
                message: llm::Assistant {
                    content: vec![llm::AssistantPart::ToolCall(llm::ToolCall {
                        id: "pending".into(),
                        name: "read".into(),
                        input: Json::encode(&serde_json::json!({})).unwrap(),
                        invalid: None,
                        provider_executed: false,
                        meta: llm::Meta::new(),
                    })],
                    origin: None,
                },
                finish: llm::Finish::new(llm::Reason::ToolCalls, None),
                usage: llm::Usage::default(),
                id: None,
                model: None,
                meta: llm::Meta::new(),
            };
            let entries = [
                user(1, "owner request"),
                entry(
                    2,
                    Body::ModelResult {
                        call: CallId::new(2).unwrap(),
                        outcome: protocol::Outcome::Done {
                            output: Json::encode(&reply).unwrap(),
                        },
                        r#for: None,
                        failure: None,
                    },
                ),
            ];
            let registry = Arc::new(ext::host::Registry::default());
            let expected =
                crate::transcript::Transcript::placed(&entries, registry.clone(), "system", places)
                    .unwrap()
                    .into_messages();
            assert!(expected.iter().any(|message| matches!(message,
                llm::Message::Assistant(reply) if reply.tool_calls().any(|call| call.id == "pending"))));
            let base = append(ContextCut::default(), entries, store).unwrap();
            let cut = base;
            let actual =
                crate::transcript::Transcript::cut(&cut, registry, "system", places, Some(2))
                    .unwrap()
                    .into_messages();
            assert_eq!(actual, expected);
        });
    }

    struct RetainedTree {
        _dir: tempfile::TempDir,
        source: Source,
        cut: ContextCut,
        blobs: Arc<blob_files::Files>,
        blob: protocol::BlobRef,
        orphan: protocol::BlobRef,
        expected: llm::Messages,
    }

    fn retained_payloads(
        dir: &std::path::Path,
    ) -> (Arc<blob_files::Files>, protocol::BlobRef, protocol::BlobRef) {
        use ext::blob::BlobStore;
        use std::io::Write;
        let blobs = Arc::new(blob_files::Files::open(dir.join("blobs"), dir).unwrap());
        let mut writer = blobs.writer(7).unwrap();
        writer.write_all(b"payload").unwrap();
        let blob = writer
            .finish(
                "text/plain",
                protocol::BlobOrigin::ToolOutput(CallId::new(2).unwrap()),
            )
            .unwrap();
        let mut orphan = blobs.writer(6).unwrap();
        orphan.write_all(b"orphan").unwrap();
        let orphan = orphan
            .finish("text/plain", protocol::BlobOrigin::Paste)
            .unwrap();
        (blobs, blob, orphan)
    }

    fn saved_cut(session: SessionId, through: u64) -> ContextCut {
        ContextCut {
            source: Some(LogCut {
                session,
                through: protocol::Seq::new(through).unwrap(),
            }),
            ..Default::default()
        }
    }

    fn retained_tree() -> RetainedTree {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions");
        let logs = Arc::new(Jsonl::open(&path).unwrap());
        let (blobs, blob, orphan) = retained_payloads(dir.path());
        let parent = Header {
            id: SessionId::new(),
            workspace: dir.path().into(),
            parent: None,
        };
        let mut loaded = logs.create(&parent).unwrap();
        loaded.append(user(1, "ancestor").body).unwrap();
        loaded
            .append(Body::ToolCall {
                tool: "read".into(),
                input: Json::null(),
                id: Some("call".into()),
                invoke: None,
                card: None,
                by: protocol::Audience::Model,
            })
            .unwrap();
        loaded
            .append(Body::ToolResult {
                call: CallId::new(2).unwrap(),
                outcome: ToolOutcome::Done {
                    output: Payload::Blob(Box::new(blob.clone())),
                },
                card: None,
                held: Vec::new(),
            })
            .unwrap();
        let parent_cut = saved_cut(parent.id, loaded.seq);
        logs.retain(parent_cut.source.unwrap()).unwrap();
        let visible = path.join(parent.id.to_string());
        let retained = path.join("retained");
        assert_eq!(
            sys::fs::identity(&visible.join("log.jsonl")).unwrap(),
            sys::fs::identity(&retained.join(format!("{}.jsonl", parent.id))).unwrap()
        );
        loaded.append(user(4, "after the frozen cut").body).unwrap();
        let child = Header {
            id: SessionId::new(),
            parent: Some(crate::store::Parent {
                session: parent.id,
                call: 1,
            }),
            ..parent.clone()
        };
        logs.import(&child, &[spawn(1, parent_cut), user(2, "child")])
            .unwrap();
        let child_cut = saved_cut(child.id, 2);
        logs.retain(child_cut.source.unwrap()).unwrap();
        let grandchild = Header {
            id: SessionId::new(),
            ..parent.clone()
        };
        logs.import(&grandchild, &[spawn(1, child_cut), user(2, "grandchild")])
            .unwrap();
        let cut = saved_cut(grandchild.id, 2);
        let places = Places::new(
            blobs.clone(),
            Arc::new(blob_files::CacheDir::new(
                dir.path().join("tree"),
                blobs.clone(),
            )),
        );
        let source = Source::new(logs.clone(), places.clone());
        let expected = crate::transcript::Transcript::cut(
            &cut,
            Arc::new(ext::host::Registry::default()),
            "",
            &source,
            None,
        )
        .unwrap()
        .into_messages();
        drop((source, loaded, logs));
        std::fs::remove_dir_all(visible).unwrap();
        let logs = Arc::new(Jsonl::open(&path).unwrap());
        assert_eq!(logs.headers().unwrap(), [grandchild]);
        let source = Source::new(logs, places);
        RetainedTree {
            _dir: dir,
            source,
            cut,
            blobs,
            blob,
            orphan,
            expected,
        }
    }

    #[test]
    fn retained_logs_survive_recursive_parent_removal_and_restart() {
        let tree = retained_tree();
        let actual = crate::transcript::Transcript::cut(
            &tree.cut,
            Arc::new(ext::host::Registry::default()),
            "",
            &tree.source,
            None,
        )
        .unwrap()
        .into_messages();
        assert_eq!(actual, tree.expected);
        assert!(
            !Json::encode(&actual)
                .unwrap()
                .raw()
                .contains("after the frozen cut")
        );
    }

    #[test]
    fn retained_source_cuts_keep_their_payloads_alive_recursively() {
        use ext::blob::BlobStore;
        let tree = retained_tree();
        let logs = tree.source.logs.as_ref();
        let mut pending = vec![tree.cut.source.unwrap()];
        let mut cuts = HashSet::new();
        let mut held = HashSet::new();
        while let Some(cut) = pending.pop() {
            if !cuts.insert(cut) {
                continue;
            }
            for entry in logs.cut(cut).unwrap() {
                let body = entry.unwrap().body;
                pending.extend(body.cuts());
                held.extend(body.blobs().into_iter().filter_map(|held| match held {
                    protocol::Held::Blob(blob) => Some(blob.id),
                    protocol::Held::Stream(_) => None,
                }));
            }
        }
        assert_eq!(cuts.len(), 3);
        tree.blobs.sweep(&|id| held.contains(id)).unwrap();
        assert!(tree.blobs.open(&tree.orphan.id).is_err());
        assert_eq!(tree.source.read(&tree.blob).as_deref(), Some("payload"));
        assert!(references(&tree.cut, &tree.source, tree.blob.id).unwrap());
    }

    fn branch(parent: &ContextCut, logs: &Jsonl) -> ContextCut {
        append(
            ContextCut::default(),
            [
                entry(
                    1,
                    Body::Spawned {
                        intent: SpawnImage {
                            runner: parent.runner,
                            authority: Json::null(),
                            image: parent.clone(),
                        }
                        .to_json()
                        .unwrap(),
                    },
                ),
                entry(
                    2,
                    Body::Ext {
                        ext: "test".into(),
                        state: "position".into(),
                        data: Json::null(),
                    },
                ),
            ],
            logs,
        )
        .unwrap()
    }

    fn append(
        previous: ContextCut,
        entries: impl IntoIterator<Item = Entry>,
        logs: &Jsonl,
    ) -> io::Result<ContextCut> {
        let entries: Vec<_> = entries.into_iter().collect();
        let session = previous
            .source
            .map_or_else(SessionId::new, |cut| cut.session);
        let mut loaded = if previous.source.is_none() {
            logs.create(&Header {
                id: session,
                workspace: std::path::PathBuf::new(),
                parent: None,
            })?
        } else {
            logs.load(session, &mut |_| false, &mut |_| {})?
        };
        for entry in &entries {
            while loaded.seq + 1 < entry.seq.get() {
                loaded.append(Body::Ext {
                    ext: "test".into(),
                    state: "position".into(),
                    data: Json::null(),
                })?;
            }
            loaded.append(entry.body.clone())?;
        }
        freeze(session, loaded.seq, previous, &entries)
    }

    fn remove_source(cut: &LogCut) {
        TEST_STORE.with(|(dir, _, _)| {
            std::fs::remove_dir_all(dir.path().join("sessions").join(cut.session.to_string()))
                .unwrap();
        });
    }
}
