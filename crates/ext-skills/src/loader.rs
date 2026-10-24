use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ext::host::{Changed, GenerationLayer, PreparedLayer, Registrations, Registry, Spawn};
use ext::source::Error as PrepareError;
use ext::{Extension, Refused, Tier};

use crate::{Catalog, Skill, Skills, parse};

#[derive(Clone, Default, PartialEq, Eq)]
struct Files {
    sources: BTreeMap<String, (PathBuf, sys::source::Source)>,
    invalid: Vec<(PathBuf, Vec<u8>)>,
    duplicates: Vec<String>,
}

impl Files {
    fn is_empty(&self) -> bool {
        self.sources.is_empty() && self.invalid.is_empty() && self.duplicates.is_empty()
    }
}

#[derive(PartialEq)]
struct Inputs {
    files: BTreeMap<String, (PathBuf, sys::source::Source)>,
    catalog: BTreeMap<String, Skill>,
}

pub struct Loader {
    home: PathBuf,
    skills: Skills,
}

impl Loader {
    pub fn new(home: &Path, skills: Skills) -> Self {
        Self {
            home: home.to_owned(),
            skills,
        }
    }

    fn roots(&self, workspace: &Path) -> Vec<PathBuf> {
        let mut roots: Vec<_> = self
            .skills
            .dirs
            .iter()
            .map(|dir| workspace.join(dir))
            .collect();
        roots.push(self.home.join("skills"));
        roots.push(workspace.join(".a/skills"));
        for index in (0..roots.len()).rev() {
            if roots[index + 1..].contains(&roots[index]) {
                roots.remove(index);
            }
        }
        roots
    }
}

impl GenerationLayer for Loader {
    fn name(&self) -> &'static str {
        "skills"
    }

    fn watch(
        &self,
        workspace: &Path,
        spawn: Spawn,
        changed: Arc<Changed>,
    ) -> Result<Box<ext::host::Refresh>, Refused> {
        let roots = self.roots(workspace);
        let mut watcher = rt::Watcher::new(spawn, roots.clone(), move |event| {
            if event.affects(&roots) {
                changed();
            }
        })
        .map_err(|e| Refused(e.to_string()))?;
        Ok(Box::new(move || {
            // Failure is a typed watch event; the watcher owns recovery even
            // when the current source inventory can already be prepared.
            let _ = watcher.refresh();
            Ok(())
        }))
    }

    fn prepare(
        &self,
        workspace: &Path,
        base: &Registry,
        previous: Option<&PreparedLayer>,
        _: bool,
    ) -> Result<PreparedLayer, PrepareError> {
        ext::source::prepare(
            || self.capture(workspace),
            |files: &Files| {
                if !files.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            },
            |files| {
                let catalog = catalog(files)?;
                for name in catalog.keys() {
                    if base.tools().get(name).is_some() || self.skills.reserved.contains(name) {
                        return Err(Refused(format!(
                            "skill {name} conflicts with an installed callable"
                        )));
                    }
                }
                if let Some(previous) = previous.filter(|p| {
                    p.inputs
                        .downcast_ref::<Inputs>()
                        .is_some_and(|p| p.catalog == catalog)
                }) {
                    return Ok(PreparedLayer {
                        inputs: Arc::new(Inputs {
                            files: files.sources.clone(),
                            catalog,
                        }),
                        registrations: previous.registrations.clone(),
                    });
                }
                let registrations = vec![(
                    Tier::Pre,
                    Arc::new(Catalog::new(Arc::new(catalog.clone()))) as Arc<dyn ext::DynExtension>,
                )];
                Ok(PreparedLayer {
                    inputs: Arc::new(Inputs {
                        files: files.sources.clone(),
                        catalog,
                    }),
                    registrations,
                })
            },
        )
    }

    fn delay(&self, duration: std::time::Duration) -> ext::BoxFuture<'static, ()> {
        rt::Reactor::AsyncIo.sleep(duration)
    }

    fn validate(&self, workspace: &Path, prepared: &PreparedLayer) -> Result<bool, PrepareError> {
        let files = self.capture(workspace)?;
        Ok(prepared.inputs.downcast_ref::<Inputs>().is_some_and(|p| {
            files.invalid.is_empty() && files.duplicates.is_empty() && files.sources == p.files
        }))
    }

    fn restore(&self, own_image: &[protocol::Installed]) -> Result<Registrations, Refused> {
        let [recorded] = own_image else {
            return Err(Refused("recorded skills generation is absent".into()));
        };
        if recorded.name != <Skills as Extension>::NAME {
            return Err(Refused("unrecognized skills image".into()));
        }
        let catalog = recorded
            .image
            .as_ref()
            .ok_or_else(|| Refused("recorded skill catalog is absent".into()))?
            .parse::<(uuid::Uuid, BTreeMap<String, Skill>)>()
            .map_err(|e| Refused(e.to_string()))?;
        Ok(vec![(
            Tier::Pre,
            Arc::new(Catalog(Arc::new(catalog.1), catalog.0)),
        )])
    }
}

fn inventory(
    root: &Path,
    found: &mut BTreeMap<String, (PathBuf, PathBuf)>,
    files: &mut Files,
) -> Result<(), PrepareError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(source_error(root, &e)),
    };
    let mut paths = Vec::new();
    for entry in entries.take(4097) {
        paths.push(entry.map_err(|e| source_error(root, &e))?.path());
    }
    if paths.len() > 4096 {
        return Err(Refused("skill root exceeds 4096 entries".into()).into());
    }
    paths.sort();
    for directory in paths {
        let path = directory.join("SKILL.md");
        let prefix = match sys::source::prefix(root, &path, 8256) {
            Ok(Some(prefix)) => prefix,
            Ok(None) => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(e) => return Err(source_error(&path, &e)),
        };
        // Discovery never rejects content. Winners are natively validated
        // below; malformed identity bytes go through the shared debounce.
        let Ok(name) = parse::identity(prefix.bytes()) else {
            let bytes = prefix.checked().map_err(|e| source_error(&path, &e))?;
            if files.invalid.iter().map(|(_, b)| b.len()).sum::<usize>() + bytes.len() > 1024 * 1024
            {
                return Err(Refused("skill identity inventory exceeds 1 MiB".into()).into());
            }
            files.invalid.push((directory, bytes));
            continue;
        };
        if let Some((prior_root, _)) = found.get(&name) {
            if prior_root == root {
                prefix.checked().map_err(|e| source_error(&path, &e))?;
                files.duplicates.push(name);
            }
            continue;
        }
        found.insert(name, (root.to_owned(), directory));
    }
    Ok(())
}

impl Loader {
    fn capture(&self, workspace: &Path) -> Result<Files, PrepareError> {
        self.capture_using(workspace, || {})
    }

    fn capture_using(
        &self,
        workspace: &Path,
        discovered: impl FnOnce(),
    ) -> Result<Files, PrepareError> {
        let mut winners = BTreeMap::new();
        let mut files = Files::default();
        for root in self.roots(workspace).into_iter().rev() {
            inventory(&root, &mut winners, &mut files)?;
        }
        discovered();
        let mut bytes = 0;
        for (name, (root, directory)) in winners {
            let path = directory.join("SKILL.md");
            let source = sys::source::Source::read(&root, &path, 64 * 1024)
                .map_err(|e| source_error(&path, &e))?
                .ok_or_else(|| {
                    PrepareError::Retry(Refused(format!(
                        "{} vanished during capture",
                        path.display()
                    )))
                })?;
            if parse::identity(source.bytes()).is_ok_and(|observed| observed != name) {
                return Err(PrepareError::Retry(Refused(
                    "skill identity changed during capture".into(),
                )));
            }
            bytes += source.len();
            if bytes > 1024 * 1024 {
                return Err(Refused("skill source exceeds 1 MiB".into()).into());
            }
            files.sources.insert(name, (directory, source));
        }
        Ok(files)
    }
}

fn catalog(files: &Files) -> Result<BTreeMap<String, Skill>, Refused> {
    if let Some((directory, bytes)) = files.invalid.first() {
        let error = parse::identity(bytes).expect_err("unidentifiable discovery");
        return Err(Refused(format!(
            "{}: {error}",
            directory.join("SKILL.md").display()
        )));
    }
    if let Some(name) = files.duplicates.first() {
        return Err(Refused(format!("duplicate skill {name}")));
    }
    let mut found = BTreeMap::new();
    for (expected, (directory, source)) in &files.sources {
        let (name, skill) = parse::parse(
            source.text().map_err(|e| Refused(e.to_string()))?,
            directory,
        )
        .map_err(|e| Refused(format!("{}: {e}", directory.display())))?;
        if &name != expected {
            return Err(Refused("skill identity changed during capture".into()));
        }
        found.insert(name, skill);
        if found
            .iter()
            .map(|(name, skill)| name.len() + skill.description.len() + 3)
            .sum::<usize>()
            > 16 * 1024
        {
            return Err(Refused("skill index exceeds 16 KiB".into()));
        }
        if found.len() > 128
            || found
                .values()
                .map(|s| s.body.len() + s.description.len())
                .sum::<usize>()
                > 1024 * 1024
        {
            return Err(Refused("skills exceed 128 entries or 1 MiB".into()));
        }
    }
    Ok(found)
}

fn source_error(path: &Path, error: &std::io::Error) -> PrepareError {
    let retry = sys::source::retryable(error);
    let message = Refused(format!("{}: {error}", path.display()));
    if retry {
        PrepareError::Retry(message)
    } else {
        PrepareError::Rejected(message)
    }
}

#[cfg(test)]
mod tests {
    use ext::host::{Bound, Coordinator, Registry, Sources};
    use ext::testing::Rig;
    use kernel::{Core, Event, StepPlan};
    use protocol::{Body, CallId, Json, Mode, Outcome};
    use serde_json::json;

    use super::*;

    fn input(text: &str) -> Event {
        Event::Input {
            segments: text.into(),
            mode: Mode::Queue,
            origin: None,
            ticket: None,
        }
    }

    fn contexts(effects: &StepPlan) -> Vec<&str> {
        effects
            .appends
            .iter()
            .filter_map(|entry| match &entry.body {
                Body::Context { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn malformed_inventory_is_reobserved_and_recovers_without_another_callback() {
        for malformed in [
            &b"---\nname: rev\xffiew\ndescription: torn\n---\n"[..],
            &b"---\nname: review\ndescription: [torn\n---\n"[..],
            &b"---\nname:"[..],
        ] {
            let rig = Rig::new("skills-inventory-race", Registry::default());
            let directory = rig.workspace().join(".a/skills/review");
            fs::create_dir_all(&directory).unwrap();
            let path = directory.join("SKILL.md");
            fs::write(&path, malformed).unwrap();
            let loader = Loader::new(&rig.dir, Skills::default());
            let mut captures = 0;
            let invalid = ext::source::prepare(
                || {
                    captures += 1;
                    loader.capture(&rig.workspace())
                },
                |_| {},
                catalog,
            );
            assert!(matches!(invalid, Err(PrepareError::Rejected(_))));
            assert_eq!(
                captures, 3,
                "stable content is rejected only after re-observation"
            );
            let mut reobserved = false;
            let attempt = ext::source::prepare(
                || loader.capture(&rig.workspace()),
                |_| {
                    reobserved = true;
                    fs::write(
                        &path,
                        "---\nname: review\ndescription: Review\n---\nsettled",
                    )
                    .unwrap();
                },
                catalog,
            );
            assert!(
                reobserved,
                "discovery must reach the debounce before rejection"
            );
            assert!(matches!(attempt, Err(PrepareError::Retry(_))));
            // The retry is driven by the result, with no filesystem callback.
            let prepared = loader
                .prepare(&rig.workspace(), &Registry::default(), None, false)
                .unwrap();
            assert_eq!(
                prepared.inputs.downcast_ref::<Inputs>().unwrap().catalog["review"].body,
                "settled"
            );
        }
    }

    #[test]
    fn a_name_changed_between_discovery_and_capture_requires_retry() {
        let rig = Rig::new("skills-name-race", Registry::default());
        let directory = rig.workspace().join(".a/skills/package");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("SKILL.md");
        fs::write(&path, "---\nname: before\ndescription: Review\n---\nbody").unwrap();
        let loader = Loader::new(&rig.dir, Skills::default());
        let captured = loader.capture_using(&rig.workspace(), || {
            fs::write(&path, "---\nname: after\ndescription: Review\n---\nbody").unwrap();
        });
        assert!(matches!(captured, Err(PrepareError::Retry(_))));
        let prepared = loader
            .prepare(&rig.workspace(), &Registry::default(), None, false)
            .unwrap();
        assert!(
            prepared
                .inputs
                .downcast_ref::<Inputs>()
                .unwrap()
                .catalog
                .contains_key("after")
        );
    }

    #[test]
    fn invalid_and_rewritten_shadowed_headers_cannot_block_a_winning_edit() {
        let rig = Rig::new("skills-shadowed-headers", Registry::default());
        let home = rig.dir.join("skills/home-package");
        let local = rig.workspace().join(".a/skills/workspace-package");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&local).unwrap();
        let lower = home.join("SKILL.md");
        fs::write(&lower, "---\nname: review\ndescription: [invalid\n---\n").unwrap();
        fs::write(
            local.join("SKILL.md"),
            "---\nname: review\ndescription: Review\n---\nold",
        )
        .unwrap();
        let loader = Loader::new(&rig.dir, Skills::default());
        let previous = loader
            .prepare(&rig.workspace(), &Registry::default(), None, false)
            .unwrap();
        fs::write(
            local.join("SKILL.md"),
            "---\nname: review\ndescription: Review\n---\nsettled",
        )
        .unwrap();
        let mut saves = 0;
        let prepared = ext::source::prepare(
            || {
                saves += 1;
                fs::write(
                    &lower,
                    format!(
                        "---\nname: 'review' # identity only\ndescription: [invalid-{saves}\n---\n"
                    ),
                )
                .unwrap();
                loader.capture(&rig.workspace())
            },
            |_| {},
            catalog,
        )
        .unwrap();
        assert!(saves >= 3);
        assert_eq!(prepared["review"].body, "settled");
        assert!(!loader.validate(&rig.workspace(), &previous).unwrap());
    }

    #[test]
    fn repeated_roots_keep_their_last_precedence_without_duplicate_skills() {
        let rig = Rig::new("skills-repeated-root", Registry::default());
        let directory = rig.workspace().join(".a/skills/review");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("SKILL.md"),
            "---\nname: review\ndescription: Review\n---\nbody",
        )
        .unwrap();
        let loader = Loader::new(
            &rig.dir,
            Skills {
                dirs: vec![".a/skills".into(), "custom".into()],
                ..Skills::default()
            },
        );
        let prepared = loader
            .prepare(&rig.workspace(), &Registry::default(), None, false)
            .unwrap();
        assert_eq!(
            prepared
                .inputs
                .downcast_ref::<Inputs>()
                .unwrap()
                .catalog
                .len(),
            1
        );
        assert_eq!(
            loader.roots(&rig.workspace()).last(),
            Some(&rig.workspace().join(".a/skills"))
        );
    }

    #[test]
    fn shadowed_body_writes_cannot_starve_a_settled_workspace_edit() {
        use std::io::{Seek, SeekFrom, Write};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let rig = Rig::new("skills-shadowed-writes", Registry::default());
        let home = rig.dir.join("skills/different-directory");
        let local = rig.workspace().join(".a/skills/review");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&local).unwrap();
        let header = "---\nname: review\ndescription: Review\n---\n";
        let shadowed = home.join("SKILL.md");
        fs::write(&shadowed, format!("{header}{}", "x".repeat(64))).unwrap();
        fs::write(local.join("SKILL.md"), format!("{header}old")).unwrap();
        let loader = Loader::new(&rig.dir, Skills::default());
        let previous = loader
            .prepare(&rig.workspace(), &Registry::default(), None, false)
            .unwrap();
        fs::write(local.join("SKILL.md"), format!("{header}settled")).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let writes = Arc::new(AtomicUsize::new(0));
        let writer_stop = stop.clone();
        let writer_writes = writes.clone();
        let task = std::thread::spawn(move || {
            let mut file = fs::OpenOptions::new().write(true).open(shadowed).unwrap();
            while !writer_stop.load(Ordering::Acquire) {
                file.seek(SeekFrom::Start(header.len() as u64)).unwrap();
                file.write_all(
                    format!("{:064}", writer_writes.fetch_add(1, Ordering::Relaxed)).as_bytes(),
                )
                .unwrap();
                std::thread::yield_now();
            }
        });
        let result = loader.prepare(
            &rig.workspace(),
            &Registry::default(),
            Some(&previous),
            false,
        );
        stop.store(true, Ordering::Release);
        task.join().unwrap();
        let prepared = result.unwrap();
        assert!(writes.load(Ordering::Relaxed) > 0);
        assert_eq!(
            prepared.inputs.downcast_ref::<Inputs>().unwrap().catalog["review"].body,
            "settled"
        );
        assert!(loader.validate(&rig.workspace(), &prepared).unwrap());
    }

    #[test]
    fn continuous_identical_saves_cannot_starve_another_skill_source() {
        let rig = Rig::new("skills-identical-saves", Registry::default());
        let root = rig.workspace().join(".a/skills");
        for name in ["a", "b"] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        let a = root.join("a/SKILL.md");
        let b = root.join("b/SKILL.md");
        let unchanged = "---\nname: b\ndescription: B\n---\nunchanged";
        fs::write(&a, "---\nname: a\ndescription: A\n---\nold").unwrap();
        fs::write(&b, unchanged).unwrap();
        let loader = Loader::new(&rig.dir, Skills::default());
        let _previous = loader
            .prepare(&rig.workspace(), &Registry::default(), None, false)
            .unwrap();
        fs::write(&a, "---\nname: a\ndescription: A\n---\nsettled").unwrap();
        let prepared = ext::source::prepare(
            || {
                fs::write(root.join("b/save.tmp"), unchanged).unwrap();
                fs::rename(root.join("b/save.tmp"), &b).unwrap();
                loader.capture(&rig.workspace())
            },
            |_| {},
            catalog,
        )
        .unwrap();
        assert_eq!(prepared["a"].body, "settled");
    }

    #[test]
    fn a_changed_file_is_seen_only_at_rest_and_replay_never_reads_it() {
        let rig = Rig::new("skills-generation", Registry::default());
        let directory = rig.workspace().join(".a/skills/review");
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("SKILL.md");
        let write = |description, body| {
            fs::write(
                &file,
                format!("---\nname: review\ndescription: {description}\n---\n{body}"),
            )
            .unwrap();
        };
        write("old index", "old instructions");
        let coordinator = Coordinator::new(
            Registry::default(),
            vec![Arc::new(Loader::new(&rig.dir, Skills::default()))],
        );
        let rt: rt::Rt = Arc::new(rt::Threads::default());
        let workspace = coordinator
            .open(
                &rig.workspace(),
                Arc::new(move |task| rt.spawn(task)),
                Arc::new(|_| {}),
            )
            .unwrap();
        let old = workspace.snapshot().registry;
        let old_binding = Arc::new(Bound::new(old.clone(), Arc::new(rig.cx()), None));
        let mut core = Core::new(old_binding);
        let started = core.step(input("/review"));
        let call = started
            .appends
            .iter()
            .find_map(|entry| {
                matches!(entry.body, Body::ToolCall { .. }).then_some(CallId(entry.seq))
            })
            .unwrap();
        assert!(!core.at_rest());
        write("new index", "new instructions");
        let registrations = Loader::new(&rig.dir, Skills::default())
            .prepare(&rig.workspace(), &Registry::default(), None, false)
            .unwrap();
        let mut registry = Registry::default();
        for (tier, extension) in registrations.registrations {
            registry = registry.with_dyn(tier, &*extension).unwrap();
        }
        let new = Arc::new(registry);
        assert!(!Arc::ptr_eq(&old, &new));
        let new_binding = Arc::new(Bound::new(new.clone(), Arc::new(rig.cx()), None));
        let ignored = core.step(Event::Registry(new_binding.clone()));
        assert!(
            ignored.appends.is_empty()
                && ignored.immediate.is_empty()
                && ignored.admitted.is_empty()
        );
        // Commands and model calls retain the same immutable registry while busy.
        let old_call = Rig::new("skills-old-call", (*old).clone());
        old_call.done("review", &json!({}));
        assert!(old_call.log.lock().unwrap().iter().any(
            |e| matches!(&e.body, Body::Context {text, ..} if text.ends_with("old instructions"))
        ));
        core.step(Event::ToolDone {
            call,
            outcome: Outcome::Done {
                output: Json::encode(&true).unwrap(),
            },
            card: None,
        });
        assert!(core.at_rest());
        core.step(Event::Registry(new_binding));
        let effects = core.step(input("use the skill"));
        assert!(contexts(&effects).iter().any(|s| s.contains("new index")));
        assert!(contexts(&effects).iter().all(|s| !s.contains("old index")
            && !s.contains("old instructions")
            && !s.contains("new instructions")));
        fs::remove_file(&file).unwrap();
        let new_call = Rig::new("skills-new-call", (*new).clone());
        new_call.done("review", &json!({}));
        let before = new_call.log.lock().unwrap().clone();
        assert!(before.iter().any(
            |e| matches!(&e.body, Body::Context {text, ..} if text.ends_with("new instructions"))
        ));
        new_call.restart();
        new_call.done("review", &json!({}));
        assert_eq!(*new_call.log.lock().unwrap(), before);
    }

    #[test]
    fn invalid_edits_keep_the_last_working_generation() {
        let rig = Rig::new("skills-invalid", Registry::default());
        let directory = rig.workspace().join(".a/skills/review");
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("SKILL.md");
        fs::write(&file, "---\nname: review\ndescription: Review\n---\nbody").unwrap();
        let loader = Loader::new(&rig.dir, Skills::default());
        assert!(
            loader
                .prepare(&rig.workspace(), &Registry::default(), None, true)
                .is_ok()
        );
        fs::write(file, "---\nname: [broken").unwrap();
        assert!(
            loader
                .prepare(&rig.workspace(), &Registry::default(), None, false)
                .is_err()
        );
    }
}
