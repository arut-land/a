//! Compiler checkpoints cross a spawn; mutable transcript and execution state do not.

use super::{Actor, Arc, Body, CallId, Entry, FrameLog, Json, Registry, Transcript};
use ext::blob::Places;
use std::io;

pub(super) fn scope(registry: &Registry) -> Json {
    Json::encode(&(
        "agent",
        registry.installed(),
        registry
            .extensions()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        registry.system_prompt(),
    ))
    .expect("registry compiler scope encodes")
}

pub(crate) struct Seed {
    pub(crate) root: protocol::BlobId,
    pub(crate) prefix: llm::Prefix,
    pub(crate) tail: llm::Messages,
}

pub(super) struct Active {
    pub(super) through: u64,
    pub(super) prefix: llm::Prefix,
    pub(super) tail: Transcript,
}

pub(super) struct Projection {
    scope: Json,
    transcript: Transcript,
}

impl Actor {
    pub(super) async fn fork_seed(
        &mut self,
        image: &protocol::publication::ContextImage,
        call: CallId,
        registry: Arc<Registry>,
        model: &super::Model,
    ) -> io::Result<Option<Seed>> {
        let Some(root) = image.root.as_ref().map(|root| root.id) else {
            return Ok(None);
        };
        let scope = scope(&registry);
        let through = call.get();
        let stable = self
            .core
            .ledger()
            .turn_start()
            .map_or(0, |seq| seq.get() - 1);
        let frames = Arc::clone(&self.live.frames);
        let pending: Vec<_> = self.pending_entries().collect();
        let places = self.live.session.blobs.places();
        let active = self
            .projection
            .model
            .active
            .as_ref()
            .filter(|active| active.through <= through && active.prefix.scope() == Some(&scope))
            .map(|active| Active {
                through: active.through,
                prefix: active.prefix.clone(),
                tail: active.tail.fork_tail(Arc::clone(&registry)),
            });
        let kept = self.projection.model.forks.take();
        let model = model.clone();
        let (kept, seed) = rt::unblock(&*self.live.session.shared.rt, move || {
            let mut kept = kept;
            if let Some(active) = active {
                let entries = entries(&frames, &pending, active.through, through)?;
                if compatible_tail(&entries, active.through) {
                    let mut tail = active.tail;
                    if entries.iter().all(|entry| tail.apply(entry, None)) {
                        return Ok((
                            kept,
                            Some(Seed {
                                root,
                                prefix: active.prefix,
                                tail: tail.into_context(),
                            }),
                        ));
                    }
                }
            }
            let mut projection = kept
                .take()
                .filter(|kept| kept.scope == scope && kept.transcript.through() <= stable)
                .unwrap_or_else(|| Projection {
                    scope: scope.clone(),
                    transcript: Transcript::empty(
                        Arc::clone(&registry),
                        registry.system_prompt(),
                        &places,
                    ),
                });
            let prefix =
                projection.prepare(&frames, &pending, stable, &registry, &places, &model)?;
            let seed = if let Some(prefix) = prefix {
                let suffix = entries(&frames, &pending, stable, through)?;
                if compatible_tail(&suffix, stable) {
                    let mut tail = projection.transcript.fork_tail(registry);
                    suffix
                        .iter()
                        .all(|entry| tail.apply(entry, None))
                        .then(|| Seed {
                            root,
                            prefix,
                            tail: tail.into_context(),
                        })
                } else {
                    None
                }
            } else {
                None
            };
            Ok::<_, io::Error>((Some(projection), seed))
        })
        .await?;
        self.projection.model.forks = kept;
        Ok(seed)
    }
}

impl Projection {
    fn prepare(
        &mut self,
        frames: &Arc<FrameLog>,
        pending: &[Entry],
        stable: u64,
        registry: &Arc<Registry>,
        places: &Places,
        model: &super::Model,
    ) -> io::Result<Option<llm::Prefix>> {
        let suffix = entries(frames, pending, self.transcript.through(), stable)?;
        if !suffix.iter().all(|entry| {
            !matches!(entry.body, Body::Edit { .. }) && self.transcript.apply(entry, None)
        }) {
            self.transcript = replay(frames, pending, stable, registry, places)?;
        }
        self.transcript.normalize_context();
        let restore_frames = Arc::clone(frames);
        let restore_pending = pending.to_vec();
        let restore_registry = Arc::clone(registry);
        let restore_places = places.clone();
        let conversation = self
            .transcript
            .request_before(stable + 1, move || {
                replay(
                    &restore_frames,
                    &restore_pending,
                    stable,
                    &restore_registry,
                    &restore_places,
                )
                .map(Transcript::into_context)
                .map_err(|error| llm::Error::new(llm::Kind::Other, error.to_string()))
            })
            .expect("stable source frontier")
            .with_scope(self.scope.clone());
        let request = model.request(conversation, registry.specs().clone());
        let prefix = model.prefix(&request);
        self.transcript.retain();
        Ok(prefix)
    }
}

fn entries(
    frames: &FrameLog,
    pending: &[Entry],
    after: u64,
    through: u64,
) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    'read: for batch in frames.batches_after(after) {
        for entry in batch? {
            if entry.seq.get() > through {
                break 'read;
            }
            entries.push(entry);
        }
    }
    let committed = entries.last().map_or(after, |entry| entry.seq.get());
    entries.extend(
        pending
            .iter()
            .filter(|entry| entry.seq.get() > committed && entry.seq.get() <= through)
            .cloned(),
    );
    Ok(entries)
}

fn compatible_tail(entries: &[Entry], boundary: u64) -> bool {
    entries.iter().all(|entry| match entry.body {
        Body::Context { .. } | Body::Edit { .. } | Body::Spawned { .. } => false,
        Body::ToolResult { call, .. }
        | Body::Take {
            into: Some(call), ..
        } => call.get() > boundary,
        _ => true,
    })
}

fn replay(
    frames: &FrameLog,
    pending: &[Entry],
    through: u64,
    registry: &Arc<Registry>,
    places: &Places,
) -> io::Result<Transcript> {
    let entries = entries(frames, pending, 0, through)?;
    Transcript::replay(
        |each| {
            for entry in &entries {
                each(entry);
            }
            Ok(())
        },
        Arc::clone(registry),
        registry.system_prompt(),
        places,
    )
}
