use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use ext::host::{Generations, Prepare, Registry, Sources};
use ext::{Extension, Refused, Registrar, Tier};

use crate::{Limits, Mod};

type Base = dyn Fn() -> Result<Registry, Refused> + Send + Sync;
type Files = BTreeMap<String, String>;

/// Sessions own workspace subscriptions; the index owns only weak references.
pub struct Loader(Arc<Source>);
struct Source {
    compiler: Arc<Compiler>,
    workspaces: Mutex<BTreeMap<PathBuf, Weak<Workspace>>>,
}
struct Compiler {
    home: PathBuf,
    config: sys::config::Exts,
    base: Arc<Base>,
    limits: Limits,
}
struct Workspace {
    compiler: Arc<Compiler>,
    path: PathBuf,
    number: AtomicU64,
    current: Mutex<Arc<Registry>>,
    files: Mutex<Files>,
    dirty: AtomicBool,
    busy: AtomicBool,
    watcher: Mutex<Option<rt::Watcher>>,
    preparing: Mutex<()>,
    opened: Mutex<()>,
}
fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Loader {
    pub fn new(
        home: &Path,
        config: sys::config::Exts,
        base: impl Fn() -> Result<Registry, Refused> + Send + Sync + 'static,
    ) -> Result<Self, Refused> {
        let home = home.canonicalize().map_err(|e| Refused(e.to_string()))?;
        let table = config.table("lua");
        let read = |key, default| {
            table
                .whole(key)
                .map_err(|e| Refused(e.to_string()))
                .map(|v| v.unwrap_or(default))
        };
        let memory = usize::try_from(read("memory_bytes", 4 * 1024 * 1024)?)
            .map_err(|e| Refused(e.to_string()))?;
        let instructions = read("instructions", 100_000)?;
        let timeout = read("timeout_ms", 25)?;
        if memory == 0 || instructions < 1000 || timeout == 0 || timeout > 1000 {
            return Err(Refused(
                "ext.lua: positive limits required; instructions >= 1000 and timeout_ms <= 1000"
                    .into(),
            ));
        }
        Ok(Self(Arc::new(Source {
            compiler: Arc::new(Compiler {
                home,
                config,
                base: Arc::new(base),
                limits: Limits {
                    memory,
                    instructions,
                    slice: Duration::from_millis(timeout),
                },
            }),
            workspaces: Mutex::default(),
        })))
    }
}
impl Extension for Loader {
    const NAME: &'static str = "lua";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.provide::<dyn Sources>(Arc::clone(&self.0) as _);
    }
}
impl Generations for Workspace {
    fn generation(&self) -> u64 {
        self.number.load(Ordering::Acquire)
    }
    fn registry(&self) -> Arc<Registry> {
        Arc::clone(&lock(&self.current))
    }
    fn restore(
        self: Arc<Self>,
        installed: &[protocol::Installed],
    ) -> Result<Arc<Registry>, Refused> {
        let mut registry = (self.compiler.base)()?;
        for extension in installed {
            if let Some(image) = &extension.image {
                registry = registry.with_dyn(Tier::User, &Mod::restore(&extension.name, image)?)?;
            }
        }
        registry = registry.with(&Reload {
            workspace: Arc::downgrade(&self),
            compiler: Arc::clone(&self.compiler),
            path: self.path.clone(),
        })?;
        if registry.installed() != installed {
            return Err(Refused(
                "recorded extension generation differs from this build".into(),
            ));
        }
        Ok(Arc::new(registry))
    }
}
impl Compiler {
    fn scan(&self, workspace: &Path) -> Result<Files, Refused> {
        let mut found = Files::new();
        files(&self.home.join("ext"), &mut found)?;
        files(&workspace.join(".a/ext"), &mut found)?;
        if found.values().map(String::len).sum::<usize>() > 1024 * 1024 {
            return Err(Refused("lua workspace source exceeds 1 MiB".into()));
        }
        Ok(found)
    }
    fn compile(&self, found: &Files, initial: bool) -> Result<Arc<Registry>, Refused> {
        let started = Instant::now();
        let mut registry = (self.base)()?;
        let mut installed = Vec::new();
        for (name, source) in found {
            if self.config.disable.contains(name) {
                continue;
            }
            // A workspace preparation has an aggregate bound as well as per-VM fuel.
            if started.elapsed() > Duration::from_secs(1) {
                return Err(Refused("lua workspace preparation budget exhausted".into()));
            }
            let grants = self
                .config
                .table(name)
                .list("tools")
                .map_err(|e| Refused(e.to_string()))?
                .unwrap_or_default()
                .to_vec();
            let module = match Mod::load(name, source, &grants, self.limits) {
                Ok(module) => module,
                Err(error) if initial => {
                    eprintln!("warning: {error}");
                    continue;
                }
                Err(error) => return Err(error),
            };
            registry = match registry.with_dyn(Tier::User, &module) {
                Ok(registry) => {
                    installed.push(module);
                    registry
                }
                Err(error) if initial => {
                    eprintln!("warning: {error}");
                    let mut registry = (self.base)()?;
                    for module in &installed {
                        registry = registry.with_dyn(Tier::User, module)?;
                    }
                    registry
                }
                Err(error) => return Err(error),
            };
        }
        if started.elapsed() > Duration::from_secs(1) {
            return Err(Refused("lua workspace preparation budget exhausted".into()));
        }
        Ok(Arc::new(registry))
    }
}
impl Workspace {
    fn prepare(
        self: &Arc<Self>,
        compiler: &Arc<Compiler>,
        path: &Path,
        initial: bool,
    ) -> Result<(), Refused> {
        let _preparing = lock(&self.preparing);
        let found = compiler.scan(path)?;
        if !initial && *lock(&self.files) == found {
            return Ok(());
        }
        let registry = compiler.compile(&found, initial)?;
        let registry = Arc::try_unwrap(registry)
            .map_err(|_| Refused("shared registry during preparation".into()))?
            .with(&Reload {
                workspace: Arc::downgrade(self),
                compiler: Arc::clone(compiler),
                path: path.to_path_buf(),
            })?;
        *lock(&self.files) = found;
        *lock(&self.current) = Arc::new(registry);
        self.number.fetch_add(1, Ordering::Release);
        Ok(())
    }

    fn changed(self: &Arc<Self>, compiler: Arc<Compiler>, path: PathBuf, prepare: &Prepare) {
        self.dirty.store(true, Ordering::Release);
        if self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        prepare(Box::new(move || {
            while let Some(workspace) = weak.upgrade() {
                workspace.dirty.store(false, Ordering::Release);
                if let Err(error) = workspace.prepare(&compiler, &path, false) {
                    eprintln!("warning: {error}");
                }
                workspace.busy.store(false, Ordering::Release);
                if !workspace.dirty.load(Ordering::Acquire)
                    || workspace.busy.swap(true, Ordering::AcqRel)
                {
                    break;
                }
            }
        }));
    }
}
impl Sources for Source {
    fn open(&self, path: &Path, prepare: Prepare) -> Result<Arc<dyn Generations>, Refused> {
        let canonical = path.canonicalize().map_err(|e| Refused(e.to_string()))?;
        let path = canonical.as_path();
        // Only weak-index access is global; preparation is serialized per workspace.
        let mut index = lock(&self.workspaces);
        index.retain(|_, workspace| workspace.strong_count() > 0);
        if let Some(workspace) = index.get(path).and_then(Weak::upgrade) {
            drop(index);
            drop(lock(&workspace.opened));
            return Ok(workspace);
        }
        let workspace = Arc::new(Workspace {
            compiler: Arc::clone(&self.compiler),
            path: path.to_path_buf(),
            number: AtomicU64::new(0),
            current: Mutex::new(Arc::new((self.compiler.base)()?)),
            files: Mutex::default(),
            dirty: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            watcher: Mutex::default(),
            preparing: Mutex::default(),
            opened: Mutex::default(),
        });
        let opened = lock(&workspace.opened);
        index.insert(path.to_path_buf(), Arc::downgrade(&workspace));
        drop(index);
        let weak = Arc::downgrade(&workspace);
        let compiler = Arc::clone(&self.compiler);
        let owned_path = path.to_path_buf();
        let home_ext = self.compiler.home.join("ext");
        let local_ext = path.join(".a/ext");
        let local_parent = path.join(".a");
        let mut watcher = rt::Watcher::new(move |paths| {
            if !paths.iter().any(|p| {
                p.starts_with(&home_ext) || p.starts_with(&local_ext) || p == &local_parent
            }) {
                return;
            }
            if let Some(workspace) = weak.upgrade() {
                workspace.changed(Arc::clone(&compiler), owned_path.clone(), &prepare);
            }
        })
        .map_err(|e| Refused(e.to_string()))?;
        // Watching the parent also catches an ext directory created after the session opens.
        for dir in [&self.compiler.home, path] {
            watcher.add(dir).map_err(|e| Refused(e.to_string()))?;
        }
        *lock(&workspace.watcher) = Some(watcher);
        if let Err(error) = workspace.prepare(&self.compiler, path, true) {
            eprintln!("warning: {error}");
        }
        drop(opened);
        Ok(workspace)
    }
}
#[derive(Clone)]
struct Reload {
    workspace: Weak<Workspace>,
    compiler: Arc<Compiler>,
    path: PathBuf,
}
impl ext::Tool for Reload {
    type In = ext::Args;
    type Out = bool;
    const NAME: &'static str = "lua-reload";
    const DESCRIPTION: &'static str = "Reload this workspace's Lua mods.";
    const AUDIENCE: protocol::Audience = protocol::Audience::User;
    async fn call(&self, _: &ext::Args, cx: &ext::Cx) -> Result<bool, String> {
        let workspace = self.workspace.upgrade().ok_or("workspace closed")?;
        let compiler = Arc::clone(&self.compiler);
        let path = self.path.clone();
        let rt = cx.service::<dyn rt::Runtime>().ok_or("no runtime")?;
        rt::unblock(rt, move || workspace.prepare(&compiler, &path, false))
            .await
            .map(|()| true)
            .map_err(|e| e.to_string())
    }
}
impl Extension for Reload {
    const NAME: &'static str = "lua-control";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.tool(self.clone());
    }
}

fn files(dir: &Path, found: &mut Files) -> Result<(), Refused> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Refused(format!("{}: {e}", dir.display()))),
    };
    for (count, entry) in entries.enumerate() {
        if count >= 4096 {
            return Err(Refused("lua directory exceeds 4096 entries".into()));
        }
        let entry = entry.map_err(|e| Refused(e.to_string()))?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "lua")
            || !entry
                .file_type()
                .map_err(|e| Refused(e.to_string()))?
                .is_file()
        {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Source and mod counts bound both compilation and aggregate VM memory.
        if found.len() >= 128 && !found.contains_key(name) {
            return Err(Refused(
                "at most 128 lua mods may be loaded per workspace".into(),
            ));
        }
        let mut source = String::new();
        fs::File::open(&path)
            .and_then(|file| file.take(256 * 1024 + 1).read_to_string(&mut source))
            .map_err(|e| Refused(format!("{}: {e}", path.display())))?;
        if source.len() > 256 * 1024 {
            return Err(Refused(format!("{} exceeds 256 KiB", path.display())));
        }
        found.insert(name.to_owned(), source);
        if found.values().map(String::len).sum::<usize>() > 1024 * 1024 {
            return Err(Refused("lua workspace source exceeds 1 MiB".into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_generations_rebuild_their_own_folds_after_source_files_are_deleted() {
        let root = std::env::temp_dir().join(format!("a-lua-history-{}", std::process::id()));
        fs::create_dir_all(root.join("ext")).unwrap();
        let file = root.join("ext/test.lua");
        fs::write(
            &file,
            "a.fold{name='count',initial=0,apply=function(s,c) return s+c end}",
        )
        .unwrap();
        let loader = Loader::new(&root, sys::config::Exts::default(), || {
            Ok(Registry::default())
        })
        .unwrap();
        let source = loader.0.open(&root, Arc::new(|_| {})).unwrap();
        let installed = source.registry().installed();
        fs::remove_file(file).unwrap();
        let restored = source.restore(&installed).unwrap();
        let mut states = restored.states();
        states.fold("test", "count", &protocol::Json::encode(&2).unwrap());
        states.fold("test", "count", &protocol::Json::encode(&3).unwrap());
        assert_eq!(states.erased("test", "count").unwrap().raw(), "5");
        assert_eq!(restored.installed(), installed);
        drop(restored);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_subscriptions_release_their_registries_after_the_last_owner() {
        let root = std::env::temp_dir().join(format!("a-lua-lifetime-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let loader = Loader::new(&root, sys::config::Exts::default(), || {
            Ok(Registry::default())
        })
        .unwrap();
        let workspace = loader.0.open(&root, Arc::new(|_| {})).unwrap();
        let registry = workspace.registry();
        let weak = Arc::downgrade(&registry);
        drop(registry);
        drop(workspace);
        assert!(weak.upgrade().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_preparation_keeps_the_published_generation_and_reads_do_no_work() {
        let root = std::env::temp_dir().join(format!("a-lua-prepare-{}", std::process::id()));
        fs::create_dir_all(root.join("ext")).unwrap();
        let file = root.join("ext/test.lua");
        fs::write(&file, "a.text{name='test',text='old'}").unwrap();
        let loader = Loader::new(&root, sys::config::Exts::default(), || {
            Ok(Registry::default())
        })
        .unwrap();
        // Queuing work makes publication order explicit, independently of OS delivery.
        let workspace = Arc::new(Workspace {
            compiler: Arc::clone(&loader.0.compiler),
            path: root.clone(),
            number: AtomicU64::new(0),
            current: Mutex::new(Arc::new(Registry::default())),
            files: Mutex::default(),
            dirty: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            watcher: Mutex::default(),
            preparing: Mutex::default(),
            opened: Mutex::default(),
        });
        workspace.prepare(&loader.0.compiler, &root, true).unwrap();
        let old = workspace.registry();
        let before = workspace.generation();
        fs::write(&file, "error('invalid edit')").unwrap();
        let queue = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&queue);
        let prepare: Prepare = Arc::new(move |work| lock(&output).push(work));
        workspace.changed(Arc::clone(&loader.0.compiler), root.clone(), &prepare);
        for _ in 0..1000 {
            assert_eq!(workspace.generation(), before);
        }
        assert_eq!(lock(&queue).len(), 1, "reads cannot schedule preparation");
        lock(&queue).pop().unwrap()();
        assert_eq!(workspace.generation(), before);
        assert!(Arc::ptr_eq(&workspace.registry(), &old));
        fs::write(&file, "a.text{name='test',text='new'}").unwrap();
        workspace.changed(Arc::clone(&loader.0.compiler), root.clone(), &prepare);
        assert!(Arc::ptr_eq(&workspace.registry(), &old));
        lock(&queue).pop().unwrap()();
        assert_eq!(workspace.generation(), before + 1);
        assert_eq!(workspace.registry().system_prompt(), "new");
        assert_eq!(
            old.system_prompt(),
            "old",
            "a call retains its immutable generation"
        );
        drop(workspace);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_events_prepare_generations_without_session_bursts() {
        let root = std::env::temp_dir().join(format!("a-lua-watch-{}", std::process::id()));
        fs::create_dir_all(root.join("ext")).unwrap();
        let loader = Loader::new(&root, sys::config::Exts::default(), || {
            Ok(Registry::default())
        })
        .unwrap();
        let (send, recv) = std::sync::mpsc::channel();
        let workspace = loader
            .0
            .open(
                &root,
                Arc::new(move |work| {
                    send.send(work).unwrap();
                }),
            )
            .unwrap();
        fs::write(
            root.join("ext/test.lua"),
            "a.text{name='test',text='event'}",
        )
        .unwrap();
        let work = recv
            .recv_timeout(Duration::from_secs(5))
            .expect("file-change event");
        work();
        assert_eq!(workspace.registry().system_prompt(), "event");
        drop(workspace);
        fs::remove_dir_all(root).unwrap();
    }
}
