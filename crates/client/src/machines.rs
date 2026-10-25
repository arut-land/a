//! The machines a client can reach besides its own: the ones the home's
//! `machines.toml` names, then the hosts of the user's ssh configuration.
//! Of the ssh configuration only its `Host` lines are read, for the names
//! they give; what a host is (its address, user and port) is what `ssh -G`
//! says, so ssh's own rules (`Match`, `Include`, tokens, the system file)
//! decide it, never a second parser.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use protocol::MachineId;
use rt::Rt;
use sys::Home;

use crate::Host;

/// A machine a client can reach by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    /// What a person calls it: `a attach <name>:<prefix>`.
    pub name: String,
    pub ssh: Host,
    /// Its machine id, where `machines.toml` pins one: a daemon there that
    /// names another is refused.
    pub pinned: Option<MachineId>,
}

/// Where a machine's ssh host leads, as `ssh -G` resolves it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Route {
    pub user: String,
    pub hostname: String,
    pub port: u16,
}

/// A machine with where it leads; `None` where ssh could not say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Known {
    pub machine: Machine,
    pub route: Option<Route>,
}

/// Which ssh configuration names the hosts.
#[derive(Clone, Debug)]
pub enum SshConfig {
    /// The user's own (`~/.ssh/config`, in `%USERPROFILE%` on Windows),
    /// which ssh reads by itself, after the system's.
    User(PathBuf),
    /// This file alone, as `ssh -F` reads it.
    File(PathBuf),
}

impl SshConfig {
    fn path(&self) -> &Path {
        match self {
            Self::User(path) | Self::File(path) => path,
        }
    }
}

/// Where a client finds the machines it can reach.
#[derive(Clone, Debug)]
pub struct Sources {
    pub machines: PathBuf,
    pub ssh: Option<SshConfig>,
}

impl Sources {
    /// `home`'s `machines.toml` and the user's ssh configuration.
    pub fn of(home: &Home) -> Self {
        Self {
            machines: home.machines_file(),
            ssh: std::env::home_dir().map(|dir| SshConfig::User(dir.join(".ssh").join("config"))),
        }
    }

    /// Every machine named, `machines.toml`'s first and in its order, then
    /// each host of the ssh configuration no machine is named for. Only
    /// reads the files. A host that is a pattern (`*`, `?`, `!`) or whose
    /// name holds a `:`, which separates a machine from a session, is not
    /// one to name.
    pub fn machines(&self) -> io::Result<Vec<Machine>> {
        let mut machines = Vec::new();
        let text = read(&self.machines)?;
        for machine in sys::config::machines(&text).map_err(|e| invalid(&self.machines, &e))? {
            let ssh = machine.ssh.as_deref().unwrap_or(&machine.name);
            let ssh = ssh
                .parse()
                .map_err(|e: String| invalid(&self.machines, &e))?;
            let pinned = machine
                .id
                .map(|id| id.parse())
                .transpose()
                .map_err(|e: &str| invalid(&self.machines, &e))?;
            machines.push(Machine {
                name: machine.name,
                ssh,
                pinned,
            });
        }
        if let Some(config) = &self.ssh {
            for name in hosts(&read(config.path())?) {
                if machines.iter().all(|m| m.name != name)
                    && let Ok(ssh) = name.parse()
                {
                    machines.push(Machine {
                        name,
                        ssh,
                        pinned: None,
                    });
                }
            }
        }
        Ok(machines)
    }

    /// The machine called `name`, if one is.
    pub fn named(&self, name: &str) -> io::Result<Option<Machine>> {
        Ok(self.machines()?.into_iter().find(|m| m.name == name))
    }

    /// Every machine with its route, one per route: a second name for a
    /// route already listed (two `Host` aliases of one host) is left out.
    /// Runs `ssh -G` once per machine, all at once. Blocking.
    pub fn resolve(&self) -> io::Result<Vec<Known>> {
        let machines = self.machines()?;
        let routes: Vec<_> = std::thread::scope(|scope| {
            let resolving: Vec<_> = machines
                .iter()
                .map(|machine| scope.spawn(|| self.route(&machine.ssh)))
                .collect();
            resolving
                .into_iter()
                .map(|thread| thread.join().ok().flatten())
                .collect()
        });
        let mut known: Vec<Known> = Vec::new();
        for (machine, route) in machines.into_iter().zip(routes) {
            if route.is_none() || known.iter().all(|k| k.route != route) {
                known.push(Known { machine, route });
            }
        }
        Ok(known)
    }

    /// Calls `sink` with what [`Sources::resolve`] finds, now and whenever
    /// either file changes, until the watch is dropped.
    pub fn watch(
        &self,
        rt: &Rt,
        sink: impl Fn(io::Result<Vec<Known>>) + Send + Sync + 'static,
    ) -> io::Result<MachineWatch> {
        let files: Vec<PathBuf> = std::iter::once(self.machines.clone())
            .chain(self.ssh.as_ref().map(|ssh| ssh.path().to_owned()))
            .collect();
        let dirs: Vec<PathBuf> = files
            .iter()
            .filter_map(|file| file.parent().map(Path::to_owned))
            .collect();
        let shared = Arc::new(Watched {
            sources: self.clone(),
            rt: rt.clone(),
            sink: Box::new(sink),
            generation: AtomicU64::new(0),
            watcher: Mutex::new(None),
        });
        let spawner = rt.clone();
        let changed = Arc::downgrade(&shared);
        let watcher = rt::Watcher::new(
            Arc::new(move |task| spawner.spawn(task)),
            dirs,
            move |event| {
                if event.affects(&files)
                    && let Some(shared) = changed.upgrade()
                {
                    shared.changed();
                }
            },
        )?;
        *lock(&shared.watcher) = Some(watcher);
        shared.changed();
        Ok(MachineWatch(shared))
    }

    fn route(&self, host: &Host) -> Option<Route> {
        let mut ssh =
            Command::new(std::env::var_os("A_SSH").unwrap_or_else(|| OsString::from("ssh")));
        ssh.arg("-G");
        if let Some(SshConfig::File(file)) = &self.ssh {
            ssh.arg("-F").arg(file);
        }
        let output = ssh
            .arg(host)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        output.status.success().then_some(())?;
        let text = String::from_utf8_lossy(&output.stdout);
        let value = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
                .map(str::to_owned)
        };
        Some(Route {
            user: value("user")?,
            hostname: value("hostname")?,
            port: value("port")?.parse().ok()?,
        })
    }
}

/// The machines as they change; dropping it stops watching.
pub struct MachineWatch(#[allow(dead_code, reason = "held for its watcher")] Arc<Watched>);

struct Watched {
    sources: Sources,
    rt: Rt,
    sink: Box<dyn Fn(io::Result<Vec<Known>>) + Send + Sync>,
    /// Bumped on each change: only the latest change's answer is told, so
    /// a slow resolve never overwrites a newer one.
    generation: AtomicU64,
    watcher: Mutex<Option<rt::Watcher>>,
}

impl Watched {
    fn changed(self: &Arc<Self>) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let watched = Arc::downgrade(self);
        self.rt.spawn(Box::pin(async move {
            let Some(shared) = watched.upgrade() else {
                return;
            };
            let resolving = Arc::clone(&shared);
            let found = rt::unblock_with(
                &*shared.rt,
                rt::BlockingClass::Network,
                64 << 10,
                move || {
                    // A directory made since (`~/.ssh`, say) is watched from now on.
                    if let Some(watcher) = lock(&resolving.watcher).as_mut() {
                        let _ = watcher.refresh();
                    }
                    resolving.sources.resolve()
                },
            )
            .await;
            if shared.generation.load(Ordering::Acquire) == generation {
                (shared.sink)(found);
            }
        }));
    }
}

/// The names `Host` lines give, in order: each word of each line, the
/// patterns and negations left out.
fn hosts(config: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in config.lines() {
        let line = line.trim_start();
        // `Host a b`, `Host=a`, or `host a`, keywords being any case.
        let Some((keyword, rest)) = line.split_once(|c: char| c.is_whitespace() || c == '=') else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
        for word in words(rest) {
            if !word.contains(['*', '?', '!', ':']) && !names.contains(&word) {
                names.push(word);
            }
        }
    }
    names
}

/// A line's words as ssh splits them: on whitespace, a double-quoted word
/// kept whole, a `#` beginning a comment.
fn words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut rest = line.trim_start();
    while !rest.is_empty() && !rest.starts_with('#') {
        let (word, after) = match rest.strip_prefix('"') {
            Some(quoted) => quoted.split_once('"').unwrap_or((quoted, "")),
            None => rest.split_once(char::is_whitespace).unwrap_or((rest, "")),
        };
        if !word.is_empty() {
            words.push(word.to_owned());
        }
        rest = after.trim_start();
    }
    words
}

/// A missing file names nothing.
fn read(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        read => read,
    }
}

fn invalid(path: &Path, why: &dyn std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {why}", path.display()),
    )
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_host_line_names_a_machine_and_never_a_pattern() {
        let config = "\
# Host commented
Host mac2 mac2-alias \"quoted name\" # and a comment
  HostName mac2.local
host=lower
Host *.example.com !nope web? one:two
Match host mac2
  User me
HostName notahost
Host mac2
";
        assert_eq!(
            hosts(config),
            ["mac2", "mac2-alias", "quoted name", "lower"]
        );
    }
}
