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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use protocol::MachineId;
use rt::Rt;
use rt::watch::{Event, Hub, Subscription, Watch};
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
        let mut machines = self.listed()?;
        for name in self.hosts()? {
            if machines.iter().all(|m| m.name != name)
                && let Some(machine) = Machine::host(name)
            {
                machines.push(machine);
            }
        }
        Ok(machines)
    }

    /// The machine called `name`, if one is: the ssh configuration is read
    /// only when `machines.toml` names none.
    pub fn named(&self, name: &str) -> io::Result<Option<Machine>> {
        if let Some(machine) = self.listed()?.into_iter().find(|m| m.name == name) {
            return Ok(Some(machine));
        }
        Ok(self
            .hosts()?
            .into_iter()
            .find(|host| host == name)
            .and_then(Machine::host))
    }

    /// The machines `machines.toml` names, in its order.
    fn listed(&self) -> io::Result<Vec<Machine>> {
        let text = read(&self.machines)?;
        let invalid = |e: &dyn std::fmt::Display| invalid(&self.machines, e);
        sys::config::machines(&text)
            .map_err(|e| invalid(&e))?
            .into_iter()
            .map(|machine| {
                let ssh = machine.ssh.as_deref().unwrap_or(&machine.name);
                Ok(Machine {
                    ssh: ssh.parse().map_err(|e: String| invalid(&e))?,
                    pinned: (machine.id.map(|id| id.parse()).transpose())
                        .map_err(|e: &str| invalid(&e))?,
                    name: machine.name,
                })
            })
            .collect()
    }

    /// The names the ssh configuration's `Host` lines give, in order, its
    /// `Include`s followed where they stand.
    fn hosts(&self) -> io::Result<Vec<String>> {
        let Some(config) = &self.ssh else {
            return Ok(Vec::new());
        };
        let path = config.path();
        let mut names = Vec::new();
        // Relative includes are in the directory of the user's
        // configuration, `~/.ssh`.
        hosts(path, path.parent().unwrap_or(Path::new(".")), 0, &mut names)?;
        Ok(names)
    }

    /// Every machine with its route, one per route: a second name for a
    /// route already listed (two `Host` aliases of one host) is left out.
    /// Runs `ssh -G` once per machine, all at once. Blocking.
    pub fn resolve(&self) -> io::Result<Vec<Known>> {
        rt::block_on(self.resolve_async(&crate::link::BLOCKING))
    }

    /// Resolves routes concurrently on the supplied runtime's Storage pool.
    pub async fn resolve_async(&self, rt: &Rt) -> io::Result<Vec<Known>> {
        eprintln!("flake resolve start {:?}", std::time::Instant::now());
        let sources = self.clone();
        let machines = rt::unblock(&**rt, move || sources.machines()).await?;
        eprintln!(
            "flake machines loaded {} {:?}",
            machines.len(),
            std::time::Instant::now()
        );
        let resolving: Vec<_> = machines
            .iter()
            .map(|machine| {
                let sources = self.clone();
                let host = machine.ssh.clone();
                rt::spawn(&**rt, rt::unblock(&**rt, move || sources.route(&host)))
            })
            .collect();
        let mut routes = Vec::with_capacity(resolving.len());
        for route in resolving {
            routes.push(route.await);
        }
        let mut known: Vec<Known> = Vec::new();
        for (machine, route) in machines.into_iter().zip(routes) {
            if route.is_none() || known.iter().all(|k| k.route != route) {
                known.push(Known { machine, route });
            }
        }
        eprintln!("flake resolve complete {:?}", std::time::Instant::now());
        Ok(known)
    }

    /// Calls `sink` with what [`Sources::resolve`] finds, now and whenever
    /// either file changes, until the watch is dropped. A directory made
    /// since (`~/.ssh`, say) is followed by `hub` itself.
    pub fn watch(
        &self,
        hub: &Hub,
        sink: impl Fn(io::Result<Vec<Known>>) + Send + Sync + 'static,
    ) -> MachineWatch {
        let files: Arc<[PathBuf]> = std::iter::once(self.machines.clone())
            .chain(self.ssh.as_ref().map(|ssh| ssh.path().to_owned()))
            .collect();
        let shared = Arc::new(Watched {
            sources: self.clone(),
            rt: Arc::clone(hub.runtime()),
            sink: Box::new(sink),
            generation: AtomicU64::new(0),
        });
        let subscriptions = (files.iter())
            .filter_map(|file| file.parent())
            .map(|dir| {
                let (files, changed) = (Arc::clone(&files), Arc::downgrade(&shared));
                hub.subscribe(Watch::entered(dir), move |events| {
                    let ours = events.iter().any(|event| match event {
                        Event::Changed(paths) => paths.iter().any(|p| files.contains(p)),
                        Event::Rescan | Event::Unavailable(_) => true,
                    });
                    if ours && let Some(shared) = changed.upgrade() {
                        shared.changed();
                    }
                })
            })
            .collect();
        shared.changed();
        MachineWatch {
            _watched: shared,
            _subscriptions: subscriptions,
        }
    }

    fn route(&self, host: &Host) -> Option<Route> {
        let mut ssh =
            Command::new(std::env::var_os("A_SSH").unwrap_or_else(|| OsString::from("ssh")));
        ssh.arg("-G");
        if let Some(SshConfig::File(file)) = &self.ssh {
            ssh.arg("-F").arg(file);
        }
        eprintln!("flake ssh start {host:?} {:?}", std::time::Instant::now());
        let output = ssh
            .arg(host)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        eprintln!(
            "flake ssh complete {host:?} {:?}",
            std::time::Instant::now()
        );
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

impl Machine {
    /// A host of the ssh configuration, called by its name.
    fn host(name: String) -> Option<Self> {
        Some(Self {
            ssh: name.parse().ok()?,
            name,
            pinned: None,
        })
    }
}

/// The machines as they change; dropping it stops watching.
pub struct MachineWatch {
    _watched: Arc<Watched>,
    _subscriptions: Vec<Subscription>,
}

struct Watched {
    sources: Sources,
    rt: Rt,
    sink: Box<dyn Fn(io::Result<Vec<Known>>) + Send + Sync>,
    /// Bumped on each change: only the latest change's answer is told, so
    /// a slow resolve never overwrites a newer one.
    generation: AtomicU64,
}

impl Watched {
    fn changed(self: &Arc<Self>) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        eprintln!("flake changed {generation} {:?}", std::time::Instant::now());
        let watched = Arc::downgrade(self);
        self.rt.spawn(Box::pin(async move {
            let Some(shared) = watched.upgrade() else {
                return;
            };
            let found = shared.sources.resolve_async(&shared.rt).await;
            eprintln!(
                "flake delivery {generation} current {} {:?}",
                shared.generation.load(Ordering::Acquire),
                std::time::Instant::now()
            );
            if shared.generation.load(Ordering::Acquire) == generation {
                (shared.sink)(found);
            }
        }));
    }
}

/// How deep `Include`s nest before ssh gives up, as `READCONF_MAX_DEPTH`.
const INCLUDE_DEPTH: u8 = 16;

/// Adds the names the `Host` lines of the file at `path` give to `names`,
/// each once, the patterns and negations left out, and those of each file
/// an `Include` there names (`~` the user's home, a relative path in
/// `base`, a glob its matches in order). A missing file names nothing.
fn hosts(path: &Path, base: &Path, depth: u8, names: &mut Vec<String>) -> io::Result<()> {
    if depth > INCLUDE_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: includes nest too deep", path.display()),
        ));
    }
    for (keyword, words) in read(path)?.lines().filter_map(directive) {
        if keyword.eq_ignore_ascii_case("host") {
            for word in words {
                if !word.contains(['*', '?', '!', ':']) && !names.contains(&word) {
                    names.push(word);
                }
            }
        } else if keyword.eq_ignore_ascii_case("include") {
            for pattern in words {
                for file in included(&pattern, base) {
                    hosts(&file, base, depth + 1, names)?;
                }
            }
        }
    }
    Ok(())
}

/// A line's keyword and its words: `Host a b`, `Host=a`, `host a`, the
/// keyword being any case.
fn directive(line: &str) -> Option<(&str, Vec<String>)> {
    let (keyword, rest) = line
        .trim_start()
        .split_once(|c: char| c.is_whitespace() || c == '=')?;
    let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
    Some((keyword, words(rest)))
}

/// The files an `Include` of `pattern` names, sorted as glob(3) sorts.
fn included(pattern: &str, base: &Path) -> Vec<PathBuf> {
    let path = match pattern.strip_prefix("~/") {
        Some(rest) => match std::env::home_dir() {
            Some(home) => home.join(rest),
            None => return Vec::new(),
        },
        None => base.join(pattern),
    };
    let Some(pattern) = path.to_str() else {
        return Vec::new();
    };
    glob::glob(pattern).map_or_else(|_| Vec::new(), |found| found.flatten().collect())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn names(config: &str, files: &[(&str, &str)]) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        fs::write(dir.path().join("config"), config).unwrap();
        let mut names = Vec::new();
        hosts(&dir.path().join("config"), dir.path(), 0, &mut names).unwrap();
        names
    }

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
            names(config, &[]),
            ["mac2", "mac2-alias", "quoted name", "lower"]
        );
    }

    #[test]
    fn an_include_names_its_files_hosts_where_it_stands() {
        let config = "Host first\nInclude config.d/* missing\nHost last\n";
        let files = [
            ("config.d/b", "Host from-b\n"),
            ("config.d/a", "Host from-a\nInclude nested\n"),
            ("nested", "Host deeper\n"),
        ];
        assert_eq!(
            names(config, &files),
            ["first", "from-a", "deeper", "from-b", "last"]
        );
    }

    #[test]
    fn an_include_of_itself_is_refused_not_followed_forever() {
        let mut found = Vec::new();
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config"), "Include config\n").unwrap();
        let error = hosts(&dir.path().join("config"), dir.path(), 0, &mut found).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
