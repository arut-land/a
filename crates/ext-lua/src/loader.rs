use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use ext::host::{Changed, GenerationLayer, PreparedLayer, Rebuild, Registrations, Registry, Spawn};
use ext::source::Error as PrepareError;
use ext::{Extension, Refused, Registrar, Tier};

use crate::{Limits, Mod};

type Files = BTreeMap<String, sys::source::Source>;

pub struct Loader {
    home: PathBuf,
    config: sys::config::Exts,
    limits: Limits,
}

impl Loader {
    pub fn new(home: &Path, config: sys::config::Exts) -> Result<Self, Refused> {
        // A fresh home may not have been created yet; source directories are optional.
        let home = home
            .canonicalize()
            .or_else(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    std::path::absolute(home)
                } else {
                    Err(error)
                }
            })
            .map_err(|e| Refused(e.to_string()))?;
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
        Ok(Self {
            home,
            config,
            limits: Limits {
                memory,
                instructions,
                slice: Duration::from_millis(timeout),
            },
        })
    }
}

impl GenerationLayer for Loader {
    fn name(&self) -> &'static str {
        "lua"
    }
    fn watch(
        &self,
        workspace: &Path,
        spawn: Spawn,
        changed: Arc<Changed>,
    ) -> Result<Box<ext::host::Refresh>, Refused> {
        let home = self.home.join("ext");
        let local = workspace.join(".a/ext");
        let roots = [home, local];
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
        initial: bool,
    ) -> Result<PreparedLayer, PrepareError> {
        ext::source::prepare(
            || self.capture(workspace),
            |files: &Files| {
                if !files.is_empty() {
                    std::thread::sleep(Duration::from_millis(100));
                }
            },
            |found| self.compile(found, base, previous, initial),
        )
    }

    fn delay(&self, duration: Duration) -> ext::BoxFuture<'static, ()> {
        rt::Reactor::AsyncIo.sleep(duration)
    }

    fn validate(&self, workspace: &Path, prepared: &PreparedLayer) -> Result<bool, PrepareError> {
        Ok(prepared.inputs.downcast_ref::<Files>() == Some(&self.capture(workspace)?))
    }

    fn restore(&self, own_image: &[protocol::Installed]) -> Result<Registrations, Refused> {
        own_image
            .iter()
            .map(|extension| {
                let image = extension
                    .image
                    .as_ref()
                    .ok_or_else(|| Refused("recorded lua source is absent".into()))?;
                Ok((
                    Tier::User,
                    Arc::new(Mod::restore(&extension.name, image)?) as Arc<dyn ext::DynExtension>,
                ))
            })
            .collect()
    }
    fn controls(&self, rebuild: Weak<Rebuild>) -> Registrations {
        vec![(Tier::User, Arc::new(Reload { rebuild }))]
    }
}

#[derive(Clone)]
struct Reload {
    rebuild: Weak<Rebuild>,
}
impl ext::Tool for Reload {
    type In = ext::Args;
    type Out = bool;
    const NAME: &'static str = "lua-reload";
    const DESCRIPTION: &'static str = "Reload this workspace's Lua mods.";
    const AUDIENCE: protocol::Audience = protocol::Audience::User;
    async fn call(&self, _: &ext::Args, cx: &ext::Cx) -> Result<bool, String> {
        let rebuild = self.rebuild.upgrade().ok_or("workspace closed")?;
        let rt = cx.service::<dyn rt::Runtime>().ok_or("no runtime")?;
        rt::unblock_with(rt, rt::BlockingClass::Cpu, rt::STORAGE_BYTES, move || {
            rebuild()
        })
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

impl Loader {
    fn capture(&self, workspace: &Path) -> Result<Files, PrepareError> {
        let mut paths = BTreeMap::new();
        inventory(&self.home.join("ext"), &mut paths)?;
        inventory(&workspace.join(".a/ext"), &mut paths)?;
        paths.retain(|name, _| !self.config.disable.contains(name));
        let mut found = Files::new();
        for (name, (root, path)) in paths {
            let source = sys::source::Source::read(&root, &path, 256 * 1024)
                .map_err(|e| source_error(&path, &e))?
                .ok_or_else(|| Refused(format!("{} is not a regular source", path.display())))?;
            found.insert(name, source);
            if found.values().map(sys::source::Source::len).sum::<usize>() > 1024 * 1024 {
                return Err(Refused("lua workspace source exceeds 1 MiB".into()).into());
            }
        }
        Ok(found)
    }

    fn compile(
        &self,
        found: &Files,
        base: &Registry,
        previous: Option<&PreparedLayer>,
        initial: bool,
    ) -> Result<PreparedLayer, Refused> {
        if let Some(previous) = previous.filter(|p| p.inputs.downcast_ref::<Files>() == Some(found))
        {
            return Ok(previous.clone());
        }
        let started = Instant::now();
        let mut registry = base.clone();
        let mut installed = Vec::new();
        for (name, source) in found {
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
            let module = match Mod::load(
                name,
                source.text().map_err(|e| Refused(e.to_string()))?,
                &grants,
                self.limits,
            ) {
                Ok(module) => module,
                Err(error) if initial => {
                    ext_logging::emit(ext_logging::Fact::ExtensionRejected {
                        module: ext_logging::workspace(Path::new(name)),
                    });
                    eprintln!("warning: {error}");
                    continue;
                }
                Err(error) => return Err(error),
            };
            match registry.clone().with_dyn(Tier::User, &module) {
                Ok(next) => {
                    installed.push(module);
                    registry = next;
                }
                Err(error) if initial => {
                    ext_logging::emit(ext_logging::Fact::ExtensionRejected {
                        module: ext_logging::workspace(Path::new(name)),
                    });
                    eprintln!("warning: {error}");
                }
                Err(error) => return Err(error),
            }
        }
        if started.elapsed() > Duration::from_secs(1) {
            return Err(Refused("lua workspace preparation budget exhausted".into()));
        }
        Ok(PreparedLayer {
            inputs: Arc::new(found.clone()),
            registrations: installed
                .into_iter()
                .map(|module| (Tier::User, Arc::new(module) as Arc<dyn ext::DynExtension>))
                .collect(),
        })
    }
}

fn inventory(
    dir: &Path,
    found: &mut BTreeMap<String, (PathBuf, PathBuf)>,
) -> Result<(), PrepareError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(source_error(dir, &e)),
    };
    for (count, entry) in entries.enumerate() {
        if count >= 4096 {
            return Err(Refused("lua directory exceeds 4096 entries".into()).into());
        }
        let entry = entry.map_err(|e| source_error(dir, &e))?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "lua")
            || !entry
                .file_type()
                .map_err(|e| source_error(dir, &e))?
                .is_file()
        {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Source and mod counts bound both compilation and aggregate VM memory.
        if found.len() >= 128 && !found.contains_key(name) {
            return Err(Refused("at most 128 lua mods may be loaded per workspace".into()).into());
        }
        found.insert(name.to_owned(), (dir.to_owned(), path));
    }
    Ok(())
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
    use super::*;
    use ext::host::{Coordinator, Sources};

    fn spawn() -> Spawn {
        let rt: rt::Rt = Arc::new(rt::Threads::default());
        Arc::new(move |task| rt.spawn(task))
    }

    #[test]
    fn shadowed_and_nested_sources_cannot_invalidate_workspace_inputs() {
        let rig = ext::testing::Rig::new("lua-precedence", Registry::default());
        let home = rig.dir.join("ext");
        let local = rig.workspace().join(".a/ext");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(local.join("cache")).unwrap();
        fs::write(home.join("test.lua"), "home").unwrap();
        fs::write(
            local.join("test.lua"),
            "a.text{name='test',text='workspace'}",
        )
        .unwrap();
        let loader = Loader::new(&rig.dir, sys::config::Exts::default()).unwrap();
        let captured = loader.capture(&rig.workspace()).unwrap();
        for n in 0..10 {
            fs::write(home.join("test.lua"), format!("home {n}")).unwrap();
            fs::write(local.join("cache/active.lua"), format!("cache {n}")).unwrap();
            assert_eq!(loader.capture(&rig.workspace()).unwrap(), captured);
        }
        let layer = loader
            .prepare(&rig.workspace(), &Registry::default(), None, false)
            .unwrap();
        let mut registry = Registry::default();
        for (tier, extension) in layer.registrations {
            registry = registry.with_dyn(tier, &*extension).unwrap();
        }
        assert_eq!(registry.system_prompt(), "workspace");
    }

    #[test]
    fn continuous_identical_saves_cannot_starve_another_lua_source() {
        let rig = ext::testing::Rig::new("lua-identical-saves", Registry::default());
        let directory = rig.dir.join("ext");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("a.lua"), "a.text{name='a',text='old'}").unwrap();
        let unchanged = "a.text{name='b',text='unchanged'}";
        fs::write(directory.join("b.lua"), unchanged).unwrap();
        let loader = Loader::new(&rig.dir, sys::config::Exts::default()).unwrap();
        let previous = loader
            .prepare(&rig.workspace(), &Registry::default(), None, false)
            .unwrap();
        fs::write(directory.join("a.lua"), "a.text{name='a',text='settled'}").unwrap();
        let prepared = ext::source::prepare(
            || {
                // Every capture observes a new native identity, without a timing race.
                fs::write(directory.join("save.lua.tmp"), unchanged).unwrap();
                fs::rename(directory.join("save.lua.tmp"), directory.join("b.lua")).unwrap();
                loader.capture(&rig.workspace())
            },
            |_| {},
            |files| loader.compile(files, &Registry::default(), Some(&previous), false),
        )
        .unwrap();
        assert!(loader.validate(&rig.workspace(), &prepared).unwrap());
        let registry = prepared
            .registrations
            .into_iter()
            .fold(Registry::default(), |registry, (tier, module)| {
                registry.with_dyn(tier, &*module).unwrap()
            });
        assert!(registry.system_prompt().contains("settled"));
    }

    #[test]
    fn missing_optional_source_directories_prepare_an_empty_layer() {
        let rig = ext::testing::Rig::new("lua-missing-home", Registry::default());
        let home = rig.dir.join("home");
        let loader = Loader::new(&home, sys::config::Exts::default()).unwrap();
        let layer = loader
            .prepare(&rig.workspace(), &Registry::default(), None, true)
            .unwrap();
        assert!(layer.registrations.is_empty());
        assert!(!home.exists());
    }

    #[test]
    fn unchanged_rejected_sources_do_not_poison_other_layers_rebuilds() {
        let rig = ext::testing::Rig::new("lua-preparation-cache", Registry::default());
        fs::create_dir_all(rig.dir.join("ext")).unwrap();
        fs::write(
            rig.dir.join("ext/good.lua"),
            "a.text{name='good',text='cached'}",
        )
        .unwrap();
        let bad = rig.dir.join("ext/bad.lua");
        fs::write(&bad, "error('invalid source')").unwrap();
        let loader = Loader::new(&rig.dir, sys::config::Exts::default()).unwrap();
        let first = loader
            .prepare(&rig.workspace(), &Registry::default(), None, true)
            .unwrap();
        assert_eq!(first.registrations.len(), 1);
        fs::write(
            rig.dir.join("ext/good.lua"),
            "a.text{name='good',text='cached'}",
        )
        .unwrap();
        let next = loader
            .prepare(&rig.workspace(), &Registry::default(), Some(&first), false)
            .unwrap();
        assert!(Arc::ptr_eq(
            &first.registrations[0].1,
            &next.registrations[0].1
        ));
        assert!(loader.validate(&rig.workspace(), &next).unwrap());
        fs::write(&bad, "error('different invalid source')").unwrap();
        assert!(
            loader
                .prepare(&rig.workspace(), &Registry::default(), Some(&first), false)
                .is_err()
        );
        fs::remove_file(bad).unwrap();
        let recovered = loader
            .prepare(&rig.workspace(), &Registry::default(), Some(&first), false)
            .unwrap();
        assert_eq!(recovered.registrations.len(), 1);
    }

    #[test]
    fn partial_and_deleted_sources_reattach_the_same_fold_value_when_restored() {
        use std::io::Write;
        let rig = ext::testing::Rig::new("lua-fold-retention", Registry::default());
        let directory = rig.dir.join("ext");
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("test.lua");
        let full = "a.fold{name='count',initial=0,apply=function(s,c) return s+c end}";
        fs::write(&file, full).unwrap();
        let loader = Loader::new(&rig.dir, sys::config::Exts::default()).unwrap();
        let registry = |layer: PreparedLayer| {
            layer
                .registrations
                .into_iter()
                .fold(Registry::default(), |registry, (tier, module)| {
                    registry.with_dyn(tier, &*module).unwrap()
                })
        };
        let prepare = || {
            registry(
                loader
                    .prepare(&rig.workspace(), &Registry::default(), None, false)
                    .unwrap(),
            )
            .states()
        };
        let log = [protocol::Entry::new(
            protocol::Seq::new(1).unwrap(),
            protocol::Epoch::default(),
            protocol::Body::Ext {
                ext: "test".into(),
                state: "count".into(),
                data: protocol::Json::encode(&42).unwrap(),
            },
        )];
        let mut states = prepare();
        states.replay(&log);
        let mut writer = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&file)
            .unwrap();
        writer
            .write_all(b"a.text{name='prefix',text='partial'}")
            .unwrap();
        writer.flush().unwrap();
        let mut partial = prepare(); // The writer remains open beyond the debounce.
        partial.adopt(&states).unwrap();
        assert!(!partial.registered("test", "count"));
        drop(writer);
        fs::remove_file(&file).unwrap();
        let mut absent = prepare();
        absent.adopt(&partial).unwrap();
        assert!(!absent.registered("test", "count"));
        fs::write(&file, full).unwrap();
        let mut restored = prepare();
        restored.replay(&log);
        restored.adopt(&absent).unwrap();
        assert_eq!(restored.erased("test", "count").unwrap().raw(), "42");
        restored.fold("test", "count", &protocol::Json::encode(&1).unwrap());
        assert_eq!(restored.erased("test", "count").unwrap().raw(), "43");
    }

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
        let loader = Loader::new(&root, sys::config::Exts::default()).unwrap();
        let source = Coordinator::new(Registry::default(), vec![Arc::new(loader)])
            .open(&root, spawn(), Arc::new(|_| {}))
            .unwrap();
        let installed = source.snapshot().registry.installed();
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
        let loader = Loader::new(&root, sys::config::Exts::default()).unwrap();
        let workspace = Coordinator::new(Registry::default(), vec![Arc::new(loader)])
            .open(&root, spawn(), Arc::new(|_| {}))
            .unwrap();
        let registry = workspace.snapshot().registry;
        let weak = Arc::downgrade(&registry);
        drop(registry);
        drop(workspace);
        assert!(weak.upgrade().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_events_prepare_generations_without_session_bursts() {
        let root = std::env::temp_dir().join(format!("a-lua-watch-{}", std::process::id()));
        fs::create_dir_all(root.join("ext")).unwrap();
        let loader = Loader::new(&root, sys::config::Exts::default()).unwrap();
        let (send, recv) = std::sync::mpsc::channel();
        let workspace = Coordinator::new(Registry::default(), vec![Arc::new(loader)])
            .open(
                &root,
                spawn(),
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
        assert_eq!(workspace.snapshot().registry.system_prompt(), "event");
        drop(workspace);
        fs::remove_dir_all(root).unwrap();
    }
}
