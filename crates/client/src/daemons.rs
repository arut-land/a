//! A client's daemons, one per machine. Each daemon stores and lists only
//! its own sessions, so a session is found on a machine (`mac2:3f9a`) and
//! the set's lists are each daemon's, side by side. A client holding one
//! daemon behaves as one that knows no others.

use std::fmt;
use std::io;
use std::sync::Arc;

use protocol::{Address, MachineId, SessionSummary, Target};
use sys::{Home, Prompt, Staged};

use crate::machines::{Machine, Sources};
use crate::{Daemon, Error, NoMatch, SshLink};

/// One daemon of a set, by the name a person calls its machine.
#[derive(Clone, Debug)]
pub struct Member {
    pub name: String,
    pub daemon: Daemon,
}

impl Member {
    /// What this machine's daemon is called.
    pub const LOCAL: &str = "local";

    /// `machine`'s daemon, over ssh from `home`.
    pub fn ssh(machine: Machine, prompt: Prompt, home: &Home, staged: &Staged) -> Self {
        let daemon = Daemon::new(
            SshLink::new(machine.ssh, prompt, home.clone()),
            staged.clone(),
        );
        Self {
            name: machine.name,
            daemon: Daemon {
                pinned: machine.pinned,
                ..daemon
            },
        }
    }

    /// Its sessions, newest first.
    pub fn sessions(&self) -> Result<Vec<Listed>, Error> {
        let conn = self.daemon.connect(&|| {})?;
        let machine = conn.machine;
        Ok(conn
            .sessions()?
            .into_iter()
            .map(|summary| Listed {
                on: self.name.clone(),
                machine,
                summary,
            })
            .collect())
    }
}

/// The daemons a client holds. The first is its default: where a session
/// starts when no machine is named. Names are unique.
#[derive(Clone, Debug)]
pub struct Daemons(Arc<[Member]>);

impl Daemons {
    /// `daemon` alone, called `name`.
    pub fn one(name: impl Into<String>, daemon: Daemon) -> Self {
        Self(Arc::new([Member {
            name: name.into(),
            daemon,
        }]))
    }

    /// This set with the machine `target` names (`mac2:3f9a`), from
    /// `sources`, where the set lacks it.
    pub fn naming(
        self,
        target: &Target,
        sources: &Sources,
        prompt: Prompt,
        home: &Home,
    ) -> io::Result<Self> {
        let named = match target {
            Target::Prefix(text) | Target::Fork { session: text, .. } => text.split_once(':'),
            _ => None,
        };
        let Some((name, _)) = named.filter(|(name, _)| self.get(name).is_none()) else {
            return Ok(self);
        };
        let Some(machine) = sources.named(name)? else {
            return Ok(self);
        };
        let staged = self.first().daemon.staged.clone();
        Ok(self.with(Member::ssh(machine, prompt, home, &staged)))
    }

    /// This set and `member`, unless a member already has its name.
    #[must_use]
    pub fn with(self, member: Member) -> Self {
        if self.get(&member.name).is_some() {
            return self;
        }
        Self(self.0.iter().cloned().chain([member]).collect())
    }

    pub fn first(&self) -> &Member {
        &self.0[0]
    }

    pub fn get(&self, name: &str) -> Option<&Member> {
        self.0.iter().find(|m| m.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Member> {
        self.0.iter()
    }

    /// Whether it holds more than one: a set is never empty.
    pub fn several(&self) -> bool {
        self.0.len() > 1
    }

    /// The member `target` is on, and the target to ask it for. A session
    /// named as `<machine>:<prefix>` (by the member's name, or its pinned
    /// machine id) is on that member; a fork's source too. A bare prefix is
    /// the one member's, or with several, found in their lists. What makes
    /// a session goes to the first.
    pub fn resolve(&self, target: Target) -> Result<(&Member, Target), Error> {
        match target {
            Target::Prefix(prefix) => match self.qualified(&prefix)? {
                Some((member, prefix)) => Ok((member, Target::Prefix(prefix.to_owned()))),
                None if !self.several() => Ok((self.first(), Target::Prefix(prefix))),
                None => self.find(&prefix),
            },
            Target::Fork { session, seq } => {
                let (member, session) = match self.qualified(&session)? {
                    Some((member, session)) => (member, session.to_owned()),
                    None => (self.first(), session),
                };
                Ok((member, Target::Fork { session, seq }))
            }
            target => Ok((self.first(), target)),
        }
    }

    /// Each member's sessions, newest first, asked of them all at once.
    pub fn sessions(&self) -> Vec<(&Member, Result<Vec<Listed>, Error>)> {
        std::thread::scope(|scope| {
            let asked: Vec<_> = self
                .iter()
                .map(|member| (member, scope.spawn(|| member.sessions())))
                .collect();
            asked
                .into_iter()
                .map(|(member, listing)| {
                    let listed = listing
                        .join()
                        .unwrap_or_else(|_| Err(Error::Refused("listing panicked".into())));
                    (member, listed)
                })
                .collect()
        })
    }

    /// The member `<machine>:` names, with the rest; `None` without one.
    fn qualified<'a>(&self, text: &'a str) -> Result<Option<(&Member, &'a str)>, Error> {
        let Some((machine, rest)) = text.split_once(':') else {
            return Ok(None);
        };
        let pinned = machine.parse::<MachineId>().ok();
        self.iter()
            .find(|m| m.name == machine || pinned.is_some_and(|id| m.daemon.pinned == Some(id)))
            .map(|member| Some((member, rest)))
            .ok_or_else(|| {
                Error::Io(
                    io::ErrorKind::NotFound,
                    format!("no machine named {machine}"),
                )
            })
    }

    /// The one session across the members whose id starts with `prefix`.
    fn find(&self, prefix: &str) -> Result<(&Member, Target), Error> {
        let mut listed = Vec::new();
        for (member, sessions) in self.sessions() {
            // A machine that cannot be reached holds none of them.
            listed.extend(sessions.into_iter().flatten().map(|l| (member, l)));
        }
        match crate::matching(&listed, |(_, l)| Some(l.summary.id), prefix) {
            Ok((member, l)) => Ok((*member, Target::Id(l.summary.id))),
            Err(NoMatch::None) => Err(Error::Io(
                io::ErrorKind::NotFound,
                format!("no session matches {prefix}"),
            )),
            Err(NoMatch::Several) => Err(Error::Io(
                io::ErrorKind::Other,
                format!("{prefix} matches several sessions"),
            )),
        }
    }
}

/// A session as its machine's daemon lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    /// The member's name.
    pub on: String,
    pub machine: MachineId,
    pub summary: SessionSummary,
}

impl Listed {
    pub const fn address(&self) -> Address {
        Address {
            machine: self.machine,
            session: self.summary.id,
        }
    }
}

/// The summary's line behind the machine's name: `mac2:3f9a…  idle …`.
impl fmt::Display for Listed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.on, self.summary)
    }
}
