//! Frozen cuts share immutable content nodes, independently of source-log lifetimes.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};

use ext::blob::{BlobStore, Places};
use protocol::publication::{
    ContextImage, ContextInput, ContextLayer, ContextNode, ContextValue, RunnerKind, SpawnImage,
};
use protocol::{BlobOrigin, BlobRef, Body, Entry};

#[derive(Default)]
pub(crate) struct Cuts(BTreeMap<u64, ContextImage>);

impl Cuts {
    pub(crate) fn before(&self, through: u64) -> (u64, ContextImage) {
        self.0
            .range(..=through)
            .next_back()
            .map_or((0, ContextImage::default()), |(&seq, cut)| {
                (seq, cut.clone())
            })
    }

    pub(crate) fn insert(&mut self, through: u64, cut: ContextImage) {
        self.0.insert(through, cut);
    }
}

pub(crate) fn append(
    previous: ContextImage,
    entries: impl IntoIterator<Item = Entry>,
    store: &dyn BlobStore,
) -> io::Result<ContextImage> {
    let mut publications = previous.publications.clone();
    let mut node = ContextNode {
        previous: previous.root,
        presentation: None,
        runner: previous.runner,
        inputs: Vec::new(),
        inherited: Vec::new(),
        edits: Vec::new(),
    };
    for entry in entries {
        publications.apply(&entry);
        let value = if let Body::Spawned { intent } = &entry.body {
            let spawn = SpawnImage::from_json(intent)?;
            node.runner = spawn.runner;
            let at = node.inherited.len();
            node.inherited.push(spawn.image);
            ContextValue::Inherited(at)
        } else {
            if let Some(edit) = crate::transcript::context_edit(&entry) {
                node.edits.push(edit);
            }
            crate::transcript::context_fact(&entry.body).unwrap_or(ContextValue::Position)
        };
        node.inputs.push(ContextInput(entry.seq, value));
        if matches!(entry.body, Body::ModelResult { r#for: None, .. }) {
            node.previous = Some(write(&node, store)?);
            node.inputs.clear();
            node.inherited.clear();
            node.edits.clear();
        }
    }
    if node.inputs.is_empty() {
        return Ok(ContextImage {
            runner: node.runner,
            root: node.previous,
            publications,
        });
    }
    Ok(ContextImage {
        runner: node.runner,
        root: Some(write(&node, store)?),
        publications,
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct PresentationImage {
    pub(crate) installed: Vec<protocol::Installed>,
    pub(crate) extensions: Vec<String>,
}

impl PresentationImage {
    pub(crate) fn of(registry: &ext::host::Registry) -> io::Result<protocol::Json> {
        protocol::Json::encode(&Self {
            installed: registry.installed(),
            extensions: registry
                .extensions()
                .map(|(name, _)| name.to_owned())
                .collect(),
        })
        .map_err(io::Error::other)
    }
}

#[derive(Default)]
pub(crate) struct Presentation {
    registry: std::sync::Weak<ext::host::Registry>,
    root: Option<BlobRef>,
}

impl Presentation {
    pub(crate) fn freeze(
        &mut self,
        registry: &std::sync::Arc<ext::host::Registry>,
        store: &dyn BlobStore,
    ) -> io::Result<BlobRef> {
        if self
            .registry
            .upgrade()
            .is_some_and(|kept| std::sync::Arc::ptr_eq(&kept, registry))
            && let Some(root) = &self.root
        {
            return Ok(root.clone());
        }
        let metadata = PresentationImage::of(registry)?;
        let root = write(
            &ContextNode {
                previous: None,
                presentation: None,
                runner: RunnerKind::Agent,
                inputs: vec![ContextInput(
                    protocol::Seq::new(1).unwrap(),
                    ContextValue::Presentation(metadata),
                )],
                inherited: Vec::new(),
                edits: Vec::new(),
            },
            store,
        )?;
        self.registry = std::sync::Arc::downgrade(registry);
        self.root = Some(root.clone());
        Ok(root)
    }
}

/// Presentation metadata is a separate edge, so history keeps its original projection scope.
pub(crate) fn with_presentation(
    image: ContextImage,
    presentation: BlobRef,
    store: &dyn BlobStore,
) -> io::Result<ContextImage> {
    Ok(ContextImage {
        runner: image.runner,
        publications: image.publications,
        root: Some(write(
            &ContextNode {
                previous: image.root,
                presentation: Some(presentation),
                runner: image.runner,
                inputs: Vec::new(),
                inherited: Vec::new(),
                edits: Vec::new(),
            },
            store,
        )?),
    })
}

pub(crate) fn presentation(image: &ContextImage, places: &Places) -> io::Result<protocol::Json> {
    let root = image
        .root
        .as_ref()
        .ok_or_else(|| io::Error::other("missing presentation cut"))?;
    let node = read(root, places)?;
    let metadata = node
        .presentation
        .ok_or_else(|| io::Error::other("missing presentation witness"))?;
    read(&metadata, places)?
        .inputs
        .into_iter()
        .find_map(|ContextInput(_, value)| {
            if let ContextValue::Presentation(image) = value {
                Some(image)
            } else {
                None
            }
        })
        .ok_or_else(|| io::Error::other("missing presentation witness"))
}

fn write(node: &ContextNode, store: &dyn BlobStore) -> io::Result<BlobRef> {
    let json = node.to_json()?;
    let mut writer = store.writer(json.raw().len() as u64)?;
    writer.write_all(json.raw().as_bytes())?;
    writer.finish("application/vnd.a.context+json", BlobOrigin::Context)
}

fn read(blob: &BlobRef, places: &Places) -> io::Result<ContextNode> {
    let bytes = places.bytes(blob);
    #[cfg(test)]
    let bytes = bytes.or_else(|| {
        crate::lock(&TEST_READERS)
            .iter()
            .find_map(|places| places.bytes(blob))
    });
    let bytes = bytes.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("missing context node {}", blob.id),
        )
    })?;
    ContextNode::from_json(&bytes).map_err(Into::into)
}

pub(crate) fn expand(image: &ContextImage, places: &Places) -> io::Result<Vec<ContextLayer>> {
    expand_with(image, places, None)
}

pub(crate) fn expand_bounded(
    image: &ContextImage,
    places: &Places,
    exchanges: u16,
) -> io::Result<Vec<ContextLayer>> {
    if exchanges == 0 {
        return Ok(Vec::new());
    }
    expand_with(image, places, Some(usize::from(exchanges) + 1))
}

fn expand_with(
    image: &ContextImage,
    places: &Places,
    window: Option<usize>,
) -> io::Result<Vec<ContextLayer>> {
    if window == Some(0) {
        return Ok(Vec::new());
    }
    let mut nodes = Vec::new();
    let mut next = image.root.clone();
    let mut start = None;
    while let Some(blob) = next {
        let mut node = read(&blob, places)?;
        next = node.previous.take();
        nodes.push(node);
        if let Some(window) = window
            && let Some(seq) = suffix_start(&nodes, window, image.runner)
        {
            start = Some(seq);
            break;
        }
    }
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
                let mut inherited = expand_with(&node.inherited[at], places, remaining)?;
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
fn suffix_start(nodes: &[ContextNode], window: usize, runner: RunnerKind) -> Option<protocol::Seq> {
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
    nodes: &[ContextNode],
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

#[cfg(test)]
static TEST_READERS: std::sync::Mutex<Vec<Places>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn register_test_places(places: Places) {
    crate::lock(&TEST_READERS).push(places);
}

#[cfg(test)]
thread_local! {
    static TEST_STORE: (tempfile::TempDir, std::sync::Arc<blob_files::Files>, Places) = {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(blob_files::Files::open(dir.path().join("blobs"), dir.path()).unwrap());
        let tree = std::sync::Arc::new(blob_files::CacheDir::new(dir.path().join("tree"), store.clone()));
        let places = Places::new(store.clone(), tree);
        register_test_places(places.clone());
        (dir, store, places)
    };
}

#[cfg(test)]
pub(crate) fn test_places() -> Places {
    TEST_STORE.with(|(_, _, places)| places.clone())
}

#[cfg(test)]
pub(crate) fn test_append(entries: &[Entry]) -> ContextImage {
    test_append_chunks(entries, 1)
}

#[cfg(test)]
pub(crate) fn test_append_chunks(entries: &[Entry], chunk: usize) -> ContextImage {
    TEST_STORE.with(|(_, store, _)| {
        entries
            .chunks(chunk)
            .fold(ContextImage::default(), |cut, entries| {
                append(cut, entries.iter().cloned(), &**store).unwrap()
            })
    })
}

/// Graph storage is private; a client may fetch only effective inherited payloads.
pub(crate) fn references(
    image: &ContextImage,
    places: &Places,
    blob: protocol::BlobId,
) -> io::Result<bool> {
    Ok(expand(image, places)?.into_iter().flat_map(|ContextLayer(_, inputs)| inputs)
        .flat_map(|ContextInput(_, value)| value.blobs())
        .any(|held| matches!(held, protocol::Held::Blob(reference) if reference.id == blob && reference.origin != BlobOrigin::Context)))
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
                force: false,
            },
        )
    }

    fn spawn(seq: u64, image: ContextImage) -> Entry {
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
    fn forking_a_prefix_reuses_existing_content_nodes() {
        TEST_STORE.with(|(_, store, places)| {
            let parent = append(
                ContextImage::default(),
                [user(1, &"x".repeat(5 << 20))],
                &**store,
            )
            .unwrap();
            let children: Vec<_> = (0..3)
                .map(|_| {
                    append(
                        ContextImage::default(),
                        [spawn(1, parent.clone())],
                        &**store,
                    )
                    .unwrap()
                })
                .collect();
            assert!(children.windows(2).all(|pair| pair[0].root == pair[1].root));
            for child in children {
                let node = read(child.root.as_ref().unwrap(), places).unwrap();
                assert_eq!(node.inherited[0], parent);
                assert!(child.root.unwrap().size < 2048);
            }
        });
    }

    #[test]
    fn appending_a_branch_never_copies_its_settled_parent_prefix() {
        TEST_STORE.with(|(_, store, places)| {
            let parent = append(
                ContextImage::default(),
                [user(1, &"x".repeat(5 << 20))],
                &**store,
            )
            .unwrap();
            let branch = append(
                ContextImage::default(),
                [spawn(1, parent.clone())],
                &**store,
            )
            .unwrap();
            let advanced = append(branch.clone(), [user(2, "private tail")], &**store).unwrap();
            let node = read(advanced.root.as_ref().unwrap(), places).unwrap();
            assert_eq!(node.previous, branch.root);
            assert!(advanced.root.unwrap().size < 2048);
            assert_eq!(
                read(branch.root.as_ref().unwrap(), places)
                    .unwrap()
                    .inherited[0],
                parent
            );
        });
    }

    #[test]
    fn a_frozen_child_ignores_later_parent_edits() {
        TEST_STORE.with(|(_, store, places)| {
            let cut = append(ContextImage::default(), [user(1, "original")], &**store).unwrap();
            let child = append(ContextImage::default(), [spawn(1, cut.clone())], &**store).unwrap();
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
                &**store,
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
                ContextImage::default(),
                [user(1, &"ancestor".repeat(100_000))],
                &**store,
            )
            .unwrap();
            let branch = append(
                ContextImage::default(),
                [spawn(1, parent.clone())],
                &**store,
            )
            .unwrap();
            let branch = append(
                branch,
                [
                    user(2, "older local"),
                    user(3, "recent local"),
                    user(4, "newest local"),
                ],
                &**store,
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
            std::fs::remove_file(store.materialize(&parent.root.unwrap().id).unwrap()).unwrap();
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
                        image: ContextImage::default(),
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
                    context: ContextImage::default(),
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
            let parent = append(ContextImage::default(), entries, &**store).unwrap();
            let child = append(
                ContextImage::default(),
                [spawn(1, parent), user(2, "first"), user(3, "second")],
                &**store,
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
            let mut cut = append(ContextImage::default(), [user(1, "old root")], &**store).unwrap();
            let mut unrequested = None;
            for depth in 0..24 {
                cut = append(
                    ContextImage::default(),
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
                    &**store,
                )
                .unwrap();
                if depth == 12 {
                    unrequested = cut.root.clone();
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
            std::fs::remove_file(store.materialize(&unrequested.unwrap().id).unwrap()).unwrap();
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
            let old = append(ContextImage::default(), [user(1, "old")], &**store).unwrap();
            let mut cut = old.clone();
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
                cut = append(cut, entries, &**store).unwrap();
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
            std::fs::remove_file(store.materialize(&old.root.unwrap().id).unwrap()).unwrap();
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
                ContextImage::default(),
                [
                    entry(
                        1,
                        Body::Spawned {
                            intent: SpawnImage {
                                runner: RunnerKind::ModelOnly,
                                authority: Json::null(),
                                image: ContextImage::default(),
                            }
                            .to_json()
                            .unwrap(),
                        },
                    ),
                    user(2, "unrequested ancestor"),
                ],
                &**store,
            )
            .unwrap();
            let mut cut = old.clone();
            let mut entries = Vec::new();
            for seq in [3, 5, 7, 9] {
                let turn =
                    ext::Conversation::turn::<ext_title::Title>("test", format!("input {seq}"))
                        .request(CallId::new(seq).unwrap());
                let text = protocol::publication::ThreadInput {
                    turn: turn.intent(None).unwrap(),
                    context: ContextImage::default(),
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
            cut = append(cut, entries, &**store).unwrap();
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
            std::fs::remove_file(store.materialize(&old.root.unwrap().id).unwrap()).unwrap();
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
            let base = append(ContextImage::default(), entries, &**store).unwrap();
            let metadata = Presentation::default().freeze(&registry, &**store).unwrap();
            let cut = with_presentation(base, metadata, &**store).unwrap();
            let actual =
                crate::transcript::Transcript::cut(&cut, registry, "system", places, Some(2))
                    .unwrap()
                    .into_messages();
            assert_eq!(actual, expected);
        });
    }

    #[test]
    fn an_admitted_side_cut_pins_its_presentation_before_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let logs = Jsonl::open(dir.path().join("logs")).unwrap();
        let store =
            Arc::new(blob_files::Files::open(dir.path().join("blobs"), dir.path()).unwrap());
        let tree = Arc::new(blob_files::CacheDir::new(
            dir.path().join("tree"),
            store.clone(),
        ));
        let places = Places::new(store.clone(), tree);
        let registry = Arc::new(ext::host::Registry::default());
        let metadata = Presentation::default().freeze(&registry, &*store).unwrap();
        let cut = with_presentation(ContextImage::default(), metadata.clone(), &*store).unwrap();
        let header = Header {
            id: SessionId::new(),
            workspace: dir.path().into(),
            parent: None,
        };
        let mut loaded = logs.create(&header).unwrap();
        loaded.append(user(1, "owner input").body).unwrap();
        loaded
            .append(Body::ModelCall {
                model: "recorded".into(),
                attempt: 0,
                choice: protocol::ToolChoice::None,
                r#for: CallId::new(1),
                purpose: Some("side".into()),
                request: Some(
                    protocol::publication::ThreadImage {
                        turn: Json::null(),
                        authority: SpawnImage {
                            runner: RunnerKind::ModelOnly,
                            authority: Json::null(),
                            image: cut.clone(),
                        }
                        .to_json()
                        .unwrap(),
                    }
                    .to_json()
                    .unwrap(),
                ),
            })
            .unwrap();
        drop(loaded);
        let mut pending: Vec<_> = logs
            .entries(header.id)
            .unwrap()
            .iter()
            .flat_map(|entry| entry.body.blobs())
            .collect();
        let mut live = HashSet::new();
        while let Some(protocol::Held::Blob(blob)) = pending.pop() {
            if live.insert(blob.id) && blob.origin == BlobOrigin::Context {
                pending.extend(read(&blob, &places).unwrap().blobs());
            }
        }
        store.sweep(&|id| live.contains(id)).unwrap();
        assert!(
            store.materialize(&metadata.id).is_ok(),
            "the witness must survive with no delivered child"
        );
        assert_eq!(
            presentation(&cut, &places).unwrap(),
            PresentationImage::of(&registry).unwrap()
        );
    }

    #[test]
    fn admitted_cuts_pin_their_graph_before_a_child_exists() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        let logs = Jsonl::open(&log_dir).unwrap();
        let store = Arc::new(blob_files::Files::open(dir.path().join("blobs"), &log_dir).unwrap());
        let tree = Arc::new(blob_files::CacheDir::new(
            dir.path().join("tree"),
            store.clone(),
        ));
        let places = Places::new(store.clone(), tree);
        let header = Header {
            id: SessionId::new(),
            workspace: dir.path().into(),
            parent: None,
        };
        let mut loaded = logs.create(&header).unwrap();
        loaded
            .append(user(1, "frozen before the child exists").body)
            .unwrap();
        let cut = append(
            ContextImage::default(),
            logs.entries(header.id).unwrap(),
            &*store,
        )
        .unwrap();
        loaded
            .append(Body::Link {
                child: SessionId::new(),
                input: "child".into(),
                intent: SpawnImage {
                    runner: RunnerKind::Agent,
                    authority: Json::null(),
                    image: cut.clone(),
                }
                .to_json()
                .unwrap(),
            })
            .unwrap();
        drop(loaded);
        assert_eq!(
            logs.headers().unwrap().len(),
            1,
            "the child has not been created"
        );
        let mut pending: Vec<_> = logs
            .entries(header.id)
            .unwrap()
            .iter()
            .flat_map(|entry| entry.body.blobs())
            .collect();
        let mut live = HashSet::new();
        while let Some(held) = pending.pop() {
            if let protocol::Held::Blob(blob) = held
                && live.insert(blob.id)
                && blob.origin == BlobOrigin::Context
            {
                pending.extend(read(&blob, &places).unwrap().blobs());
            }
        }
        store.sweep(&|id| live.contains(id)).unwrap();
        assert!(
            expand(&cut, &places).is_ok(),
            "a committed admission must retain its entire graph closure"
        );
    }

    #[test]
    fn unreadable_frozen_context_returns_an_io_error() {
        TEST_STORE.with(|(_, _, places)| {
            let cut = ContextImage {
                runner: RunnerKind::Agent,
                root: Some(BlobRef {
                    id: protocol::BlobId([0xff; 32]),
                    size: 10,
                    media_type: "application/vnd.a.context+json".into(),
                    origin: BlobOrigin::Context,
                }),
                publications: protocol::publication::Publications::default(),
            };
            let error = crate::transcript::Transcript::placed(
                &[spawn(1, cut)],
                Arc::new(ext::host::Registry::default()),
                "",
                places,
            )
            .err()
            .expect("graph read must fail");
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
        });
    }

    #[test]
    fn frozen_context_and_its_payloads_survive_restart_and_source_log_removal() {
        let dir = tempfile::tempdir().unwrap();
        let logs_dir = dir.path().join("logs");
        let logs = Jsonl::open(&logs_dir).unwrap();
        let store = Arc::new(blob_files::Files::open(dir.path().join("blobs"), &logs_dir).unwrap());
        let tree = Arc::new(blob_files::CacheDir::new(
            dir.path().join("tree"),
            store.clone(),
        ));
        let places = Places::new(store.clone(), tree);
        let mut writer = store.writer(7).unwrap();
        writer.write_all(b"payload").unwrap();
        let blob = writer
            .finish(
                "text/plain",
                BlobOrigin::ToolOutput(CallId::new(1).unwrap()),
            )
            .unwrap();
        let header = Header {
            id: SessionId::new(),
            workspace: dir.path().into(),
            parent: None,
        };
        let mut loaded = logs.create(&header).unwrap();
        loaded
            .append(Body::ToolCall {
                invoke: None,
                tool: "read".into(),
                input: Json::null(),
                id: Some("provider-id".into()),
                card: None,
                by: protocol::Audience::Model,
            })
            .unwrap();
        loaded
            .append(Body::ToolResult {
                call: CallId::new(1).unwrap(),
                outcome: ToolOutcome::Done {
                    output: Payload::Blob(Box::new(blob.clone())),
                },
                card: None,
                held: Vec::new(),
            })
            .unwrap();
        let cut = append(
            ContextImage::default(),
            logs.entries(header.id).unwrap(),
            &*store,
        )
        .unwrap();
        let child = append(ContextImage::default(), [spawn(1, cut)], &*store).unwrap();
        let expected = Json::encode(&expand(&child, &places).unwrap()).unwrap();
        drop((loaded, logs));
        std::fs::remove_dir_all(&logs_dir).unwrap();
        let mut live = HashSet::new();
        let mut pending = vec![child.root.clone().unwrap()];
        while let Some(reference) = pending.pop() {
            if !live.insert(reference.id) {
                continue;
            }
            if reference.origin == BlobOrigin::Context {
                pending.extend(
                    read(&reference, &places)
                        .unwrap()
                        .blobs()
                        .into_iter()
                        .filter_map(|held| match held {
                            protocol::Held::Blob(blob) => Some(blob),
                            protocol::Held::Stream(_) => None,
                        }),
                );
            }
        }
        store.sweep(&|id| live.contains(id)).unwrap();
        drop((places, store));
        let store =
            Arc::new(blob_files::Files::open(dir.path().join("blobs"), dir.path()).unwrap());
        let tree = Arc::new(blob_files::CacheDir::new(
            dir.path().join("restored-tree"),
            store.clone(),
        ));
        let places = Places::new(store, tree);
        assert_eq!(
            Json::encode(&expand(&child, &places).unwrap()).unwrap(),
            expected
        );
        assert_eq!(places.read(&blob).as_deref(), Some("payload"));
        assert!(references(&child, &places, blob.id).unwrap());
        assert!(!references(&child, &places, child.root.as_ref().unwrap().id).unwrap());
    }
}
