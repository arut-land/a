use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use ext::host::{Changed, Generations, Prepare, Registry, Sources, Updates};
use ext::{Extension, Refused, Registrar, Tier};

use crate::{Catalog, Skill, Skills, parse};

/// Composes skills over a base registry and an optional generation source
/// (for example Lua). Sessions retain their immutable generation until rest.
pub struct Loader(Arc<Source>);
#[derive(Clone)]
struct Source {
    home: PathBuf,
    skills: Skills,
    base: Arc<Registry>,
    inner: Option<Arc<dyn Sources>>,
    workspaces: Arc<Mutex<BTreeMap<PathBuf, Weak<Workspace>>>>,
}
struct Workspace {
    source: Arc<Source>,
    path: PathBuf,
    inner: OnceLock<Arc<dyn Generations>>,
    current: Mutex<Published>,
    number: AtomicU64,
    watcher: Mutex<Option<rt::Watcher>>,
    preparing: Mutex<()>,
    subscription: OnceLock<Arc<Changed>>,
    updates: Updates,
}
struct Published {
    registry: Arc<Registry>,
    catalog: Option<Arc<BTreeMap<String, Skill>>>,
    inner_number: u64,
}
fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Loader {
    pub fn new(
        home: &Path,
        skills: Skills,
        base: Registry,
        inner: Option<Arc<dyn Sources>>,
    ) -> Self {
        Self(Arc::new(Source {
            home: home.to_owned(),
            skills,
            base: Arc::new(base),
            inner,
            workspaces: Arc::default(),
        }))
    }
}
impl Extension for Loader {
    const NAME: &'static str = "skills-source";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.provide::<dyn Sources>(self.0.clone());
    }
}
impl Generations for Workspace {
    fn subscribe(&self, changed: Weak<Changed>) {
        self.updates.subscribe(changed);
    }
    fn generation(&self) -> u64 {
        self.number.load(Ordering::Acquire)
    }
    fn registry(&self) -> Arc<Registry> {
        lock(&self.current).registry.clone()
    }
    fn restore(
        self: Arc<Self>,
        installed: &[protocol::Installed],
    ) -> Result<Arc<Registry>, Refused> {
        let recorded = installed
            .iter()
            .find(|extension| extension.name == <Skills as Extension>::NAME)
            .ok_or_else(|| Refused("recorded skills generation is absent".into()))?;
        let catalog = recorded
            .image
            .as_ref()
            .ok_or_else(|| Refused("recorded skill catalog is absent".into()))?
            .parse::<BTreeMap<String, Skill>>()
            .map_err(|e| Refused(e.to_string()))?;
        // Each layer restores only its own image. The inner source sees the
        // underlying installation that this overlay replaced, not its catalog.
        let base = if let Some(inner) = self.inner.get() {
            let underlying = self
                .source
                .base
                .installed()
                .into_iter()
                .find(|extension| extension.name == recorded.name);
            let installed: Vec<_> = installed
                .iter()
                .filter_map(|extension| {
                    if extension.name == recorded.name {
                        underlying.clone()
                    } else {
                        Some(extension.clone())
                    }
                })
                .collect();
            inner.clone().restore(&installed)?
        } else {
            self.source.base.clone()
        };
        let skills = Skills {
            catalog: Arc::new(catalog),
            ..self.source.skills.clone()
        };
        let registry = (*base).clone().with_dyn(Tier::Pre, &Catalog(skills))?;
        if registry.installed() != installed {
            return Err(Refused(
                "recorded skills generation differs from this build".into(),
            ));
        }
        Ok(Arc::new(registry))
    }
}
impl Sources for Source {
    fn open(&self, path: &Path, prepare: Prepare) -> Result<Arc<dyn Generations>, Refused> {
        let path = match path.canonicalize() {
            Ok(path) => path,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_owned(),
            Err(e) => return Err(Refused(e.to_string())),
        };
        let mut index = lock(&self.workspaces);
        index.retain(|_, workspace| workspace.strong_count() > 0);
        if let Some(workspace) = index.get(&path).and_then(Weak::upgrade) {
            return Ok(workspace);
        }
        let workspace = Arc::new(Workspace {
            source: Arc::new(self.clone()),
            path: path.clone(),
            inner: OnceLock::new(),
            current: Mutex::new(Published {
                registry: self.base.clone(),
                catalog: None,
                inner_number: 0,
            }),
            number: AtomicU64::new(0),
            watcher: Mutex::default(),
            preparing: Mutex::default(),
            subscription: OnceLock::new(),
            updates: Updates::default(),
        });
        if let Some(inner) = &self.inner {
            let inner = inner.open(&path, prepare.clone())?;
            let weak = Arc::downgrade(&workspace);
            let changed: Arc<Changed> = Arc::new(move || {
                if let Some(workspace) = weak.upgrade() {
                    workspace.changed();
                }
            });
            let _ = workspace.inner.set(inner.clone());
            inner.subscribe(Arc::downgrade(&changed));
            let _ = workspace.subscription.set(changed);
        }
        let roots = workspace.roots();
        let weak = Arc::downgrade(&workspace);
        let filters = roots.clone();
        let mut watcher = rt::Watcher::new(move |paths| {
            if !paths.iter().any(|path| {
                filters
                    .iter()
                    .any(|root| path.starts_with(root) || root.starts_with(path))
            }) {
                return;
            }
            let weak = weak.clone();
            prepare(Box::new(move || {
                if let Some(workspace) = weak.upgrade() {
                    workspace.changed();
                }
            }));
        })
        .map_err(|e| Refused(e.to_string()))?;
        let mut parents = Vec::new();
        for root in roots {
            if root.starts_with(&workspace.path) && !workspace.path.is_dir() {
                continue;
            }
            let mut parent = root.as_path();
            while !parent.exists() {
                parent = parent
                    .parent()
                    .ok_or_else(|| Refused("skill root has no existing parent".into()))?;
            }
            if !parents.iter().any(|p: &PathBuf| parent.starts_with(p)) {
                watcher.add(parent).map_err(|e| Refused(e.to_string()))?;
                parents.push(parent.to_owned());
            }
        }
        *lock(&workspace.watcher) = Some(watcher);
        workspace.refresh()?;
        if path.is_dir() {
            index.insert(path, Arc::downgrade(&workspace));
        }
        Ok(workspace)
    }
}
impl Workspace {
    fn roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<_> = self
            .source
            .skills
            .dirs
            .iter()
            .map(|dir| self.path.join(dir))
            .collect();
        roots.push(self.source.home.join("skills"));
        roots.push(self.path.join(".a/skills"));
        roots
    }
    fn changed(&self) {
        if let Err(error) = self.refresh() {
            eprintln!("warning: {error}");
        }
    }
    fn refresh(&self) -> Result<(), Refused> {
        // Publication and comparisons share a lock; racing preparations cannot regress.
        let _preparing = lock(&self.preparing);
        let mut catalog = BTreeMap::new();
        for root in self.roots() {
            scan(&root, &mut catalog)?;
        }
        let number = self.inner.get().map_or(0, |inner| inner.generation());
        {
            let current = lock(&self.current);
            if current.inner_number == number && current.catalog.as_deref() == Some(&catalog) {
                return Ok(());
            }
        }
        let base = self
            .inner
            .get()
            .map_or_else(|| self.source.base.clone(), |inner| inner.registry());
        for name in catalog.keys() {
            if base.tools().get(name).is_some() || self.source.skills.reserved.contains(name) {
                return Err(Refused(format!(
                    "skill {name} conflicts with an installed callable"
                )));
            }
        }
        let catalog = Arc::new(catalog);
        let skills = Skills {
            catalog: catalog.clone(),
            ..self.source.skills.clone()
        };
        let registry = (*base).clone().with_dyn(Tier::Pre, &Catalog(skills))?;
        *lock(&self.current) = Published {
            registry: Arc::new(registry),
            catalog: Some(catalog),
            inner_number: number,
        };
        self.number.fetch_add(1, Ordering::Release);
        self.updates.notify();
        Ok(())
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
    use ext::host::Bound;
    use ext::testing::Rig;
    use kernel::{Core, Effect, Event};
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

    fn contexts(effects: &[Effect]) -> Vec<&str> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Append(entry) => match &entry.body {
                    Body::Context { text, .. } => Some(text.as_str()),
                    _ => None,
                },
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
        let source = Arc::new(Source {
            home: rig.dir.clone(),
            skills: Skills::default(),
            base: Arc::new(Registry::default()),
            inner: None,
            workspaces: Arc::default(),
        });
        let workspace = Workspace {
            source,
            path: rig.workspace(),
            inner: OnceLock::new(),
            current: Mutex::new(Published {
                registry: Arc::new(Registry::default()),
                catalog: None,
                inner_number: 0,
            }),
            number: AtomicU64::new(0),
            watcher: Mutex::default(),
            preparing: Mutex::default(),
            subscription: OnceLock::new(),
            updates: Updates::default(),
        };
        workspace.refresh().unwrap();
        let old = workspace.registry();
        let old_binding = Arc::new(Bound::new(old.clone(), Arc::new(rig.cx()), None));
        let mut core = Core::new(old_binding);
        let started = core.step(input("/review"));
        let call = started
            .iter()
            .find_map(|effect| match effect {
                Effect::Append(entry) if matches!(entry.body, Body::ToolCall { .. }) => {
                    Some(CallId(entry.seq))
                }
                _ => None,
            })
            .unwrap();
        assert!(!core.at_rest());
        write("new index", "new instructions");
        workspace.refresh().unwrap();
        let new = workspace.registry();
        assert!(!Arc::ptr_eq(&old, &new));
        let new_binding = Arc::new(Bound::new(new.clone(), Arc::new(rig.cx()), None));
        assert!(core.step(Event::Registry(new_binding.clone())).is_empty());
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
        let workspace = Workspace {
            source: Arc::new(Source {
                home: rig.dir.clone(),
                skills: Skills::default(),
                base: Arc::new(Registry::default()),
                inner: None,
                workspaces: Arc::default(),
            }),
            path: rig.workspace(),
            inner: OnceLock::new(),
            current: Mutex::new(Published {
                registry: Arc::new(Registry::default()),
                catalog: None,
                inner_number: 0,
            }),
            number: AtomicU64::new(0),
            watcher: Mutex::default(),
            preparing: Mutex::default(),
            subscription: OnceLock::new(),
            updates: Updates::default(),
        };
        workspace.refresh().unwrap();
        let old = workspace.registry();
        let number = workspace.generation();
        fs::write(file, "---\nname: [broken").unwrap();
        assert!(workspace.refresh().is_err());
        assert_eq!(workspace.generation(), number);
        assert!(Arc::ptr_eq(&old, &workspace.registry()));
    }
}
