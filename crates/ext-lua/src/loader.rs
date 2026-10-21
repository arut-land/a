use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use ext::host::{Changed, GenerationLayer, PreparedLayer, Rebuild, Registrations, Registry, Spawn};
use ext::{Extension, Refused, Registrar, Tier};

use crate::{Limits, Mod};

type Files = BTreeMap<String, String>;

pub struct Loader {
    home: PathBuf,
    config: sys::config::Exts,
    limits: Limits,
}

impl Loader {
    pub fn new(home: &Path, config: sys::config::Exts) -> Result<Self, Refused> {
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
        let mut watcher = rt::Watcher::new(spawn, [home.clone(), local.clone()], move |paths| {
            if paths.iter().any(|path| {
                path.starts_with(&home)
                    || path.starts_with(&local)
                    || home.starts_with(path)
                    || local.starts_with(path)
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
        initial: bool,
    ) -> Result<PreparedLayer, Refused> {
        let mut found = Files::new();
        files(&self.home.join("ext"), &mut found)?;
        files(&workspace.join(".a/ext"), &mut found)?;
        found.retain(|name, _| !self.config.disable.contains(name));
        if let Some(previous) =
            previous.filter(|p| p.inputs.downcast_ref::<Files>() == Some(&found))
        {
            return Ok(previous.clone());
        }
        let started = Instant::now();
        let mut registry = base.clone();
        let mut installed = Vec::new();
        for (name, source) in &found {
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
            inputs: Arc::new(found),
            registrations: installed
                .into_iter()
                .map(|module| (Tier::User, Arc::new(module) as Arc<dyn ext::DynExtension>))
                .collect(),
        })
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
    use ext::host::{Coordinator, Sources};

    fn spawn() -> Spawn {
        let rt: rt::Rt = Arc::new(rt::Threads::default());
        Arc::new(move |task| rt.spawn(task))
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
        let next = loader
            .prepare(&rig.workspace(), &Registry::default(), Some(&first), false)
            .unwrap();
        assert!(Arc::ptr_eq(
            &first.registrations[0].1,
            &next.registrations[0].1
        ));
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
