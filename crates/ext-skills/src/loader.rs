use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ext::host::{Changed, GenerationLayer, PreparedLayer, Registrations, Registry, Spawn};
use ext::{Extension, Refused, Tier};

use crate::{Catalog, Skill, Skills, parse};

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
        let mut watcher = rt::Watcher::new(spawn, roots.clone(), move |paths| {
            if paths.iter().any(|path| {
                roots
                    .iter()
                    .any(|root| path.starts_with(root) || root.starts_with(path))
            }) {
                changed();
            }
        })
        .map_err(|e| Refused(e.to_string()))?;
        Ok(Box::new(move || {
            watcher.refresh().map_err(|e| Refused(e.to_string()))
        }))
    }

    fn prepare(
        &self,
        workspace: &Path,
        base: &Registry,
        previous: Option<&PreparedLayer>,
        _: bool,
    ) -> Result<PreparedLayer, Refused> {
        let mut catalog = BTreeMap::new();
        for root in self.roots(workspace) {
            scan(&root, &mut catalog)?;
        }
        for name in catalog.keys() {
            if base.tools().get(name).is_some() || self.skills.reserved.contains(name) {
                return Err(Refused(format!(
                    "skill {name} conflicts with an installed callable"
                )));
            }
        }
        if let Some(previous) = previous
            .filter(|p| p.inputs.downcast_ref::<BTreeMap<String, Skill>>() == Some(&catalog))
        {
            return Ok(previous.clone());
        }
        let catalog = Arc::new(catalog);
        Ok(PreparedLayer {
            inputs: catalog.clone(),
            registrations: vec![(Tier::Pre, Arc::new(Catalog(catalog)))],
        })
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
            .parse::<BTreeMap<String, Skill>>()
            .map_err(|e| Refused(e.to_string()))?;
        Ok(vec![(Tier::Pre, Arc::new(Catalog(Arc::new(catalog))))])
    }
}

fn scan(root: &Path, found: &mut BTreeMap<String, Skill>) -> Result<(), Refused> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Refused(format!("{}: {e}", root.display()))),
    };
    let mut paths = Vec::new();
    for entry in entries.take(4097) {
        paths.push(entry.map_err(|e| Refused(e.to_string()))?.path());
    }
    if paths.len() > 4096 {
        return Err(Refused("skill root exceeds 4096 entries".into()));
    }
    paths.sort();
    let mut names = std::collections::BTreeSet::new();
    for directory in paths {
        let path = directory.join("SKILL.md");
        let file = match sys::fs::open_regular_within(root, &path) {
            Ok(sys::fs::RegularFile::Open(file)) => file,
            Ok(sys::fs::RegularFile::Skipped(reason)) => {
                eprintln!("warning: {}: {reason:?}", path.display());
                continue;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(e) => return Err(Refused(format!("{}: {e}", path.display()))),
        };
        let mut source = String::new();
        file.take(64 * 1024 + 1)
            .read_to_string(&mut source)
            .map_err(|e| Refused(format!("{}: {e}", path.display())))?;
        if source.len() > 64 * 1024 {
            return Err(Refused(format!("{} exceeds 64 KiB", path.display())));
        }
        let (name, skill) = parse::parse(&source, &directory)
            .map_err(|e| Refused(format!("{}: {e}", path.display())))?;
        if !names.insert(name.clone()) {
            return Err(Refused(format!(
                "duplicate skill {name} in {}",
                root.display()
            )));
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
    Ok(())
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
            text: text.into(),
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
