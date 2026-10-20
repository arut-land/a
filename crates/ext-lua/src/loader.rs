use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ext::host::{Registry, Sources};
use ext::{Extension, Refused, Registrar, Tier};

use crate::{Limits, Mod};

type Base = dyn Fn() -> Result<Registry, Refused> + Send + Sync;
type Files = BTreeMap<String, String>;

struct Generation {
    looked: Instant,
    files: Files,
    registry: Arc<Registry>,
}

/// A workspace-scoped source. It keeps an unchanged generation shared across
/// sessions, and a failed edit cannot displace the last working generation.
pub struct Loader(Arc<Source>);

struct Source {
    home: PathBuf,
    config: sys::config::Exts,
    base: Arc<Base>,
    limits: Limits,
    poll: Duration,
    workspaces: Mutex<BTreeMap<PathBuf, Generation>>,
}

impl Loader {
    pub fn new(
        home: PathBuf,
        config: sys::config::Exts,
        base: impl Fn() -> Result<Registry, Refused> + Send + Sync + 'static,
    ) -> Result<Self, Refused> {
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
        let poll = Duration::from_millis(read("poll_ms", 250)?);
        Ok(Self(Arc::new(Source {
            home,
            config,
            base: Arc::new(base),
            limits: Limits {
                memory,
                instructions,
                slice: Duration::from_millis(timeout),
            },
            poll,
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

fn files(dir: &Path, found: &mut Files) -> Result<(), Refused> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Refused(format!("{}: {e}", dir.display()))),
    };
    for entry in entries {
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
    }
    Ok(())
}

impl Sources for Source {
    fn registry(&self, workspace: &Path) -> Result<Arc<Registry>, Refused> {
        let mut generations = self
            .workspaces
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(last) = generations.get(workspace)
            && last.looked.elapsed() < self.poll
        {
            return Ok(Arc::clone(&last.registry));
        }
        let mut found = Files::new();
        let scanned = files(&self.home.join("ext"), &mut found)
            .and_then(|()| files(&workspace.join(".a/ext"), &mut found));
        if let Err(why) = scanned {
            if let Some(last) = generations.get(workspace) {
                eprintln!("warning: {why}");
                return Ok(Arc::clone(&last.registry));
            }
            return Err(why);
        }
        if let Some(last) = generations.get_mut(workspace) {
            last.looked = Instant::now();
            if last.files == found {
                return Ok(Arc::clone(&last.registry));
            }
        }
        let mut registry = (self.base)()?;
        let mut installed = Vec::new();
        for (name, source) in &found {
            if self.config.disable.contains(name) {
                continue;
            }
            let grants = self
                .config
                .table(name)
                .list("tools")
                .map_err(|e| Refused(e.to_string()))?
                .unwrap_or_default()
                .to_vec();
            let loaded = Mod::load(name, source, &grants, self.limits).and_then(|module| {
                // Registry consumes itself, so validate the contribution before
                // applying it to the generation being assembled.
                Registry::default().with_dyn(Tier::User, &module)?;
                Ok(module)
            });
            match loaded {
                Ok(module) => match registry.with_dyn(Tier::User, &module) {
                    Ok(next) => {
                        registry = next;
                        installed.push(module);
                    }
                    Err(why) => {
                        eprintln!("warning: {why}");
                        if let Some(last) = generations.get_mut(workspace) {
                            last.files = found;
                            return Ok(Arc::clone(&last.registry));
                        }
                        registry = (self.base)()?;
                        for module in &installed {
                            registry = registry.with_dyn(Tier::User, module)?;
                        }
                    }
                },
                Err(why) => {
                    eprintln!("warning: {why}");
                    if let Some(last) = generations.get_mut(workspace) {
                        last.files = found;
                        return Ok(Arc::clone(&last.registry));
                    }
                }
            }
        }
        let registry = Arc::new(registry);
        generations.insert(
            workspace.to_owned(),
            Generation {
                looked: Instant::now(),
                files: found,
                registry: Arc::clone(&registry),
            },
        );
        Ok(registry)
    }
}
