//! Where a session or a path is, across machines. Each daemon stores only
//! its own sessions and names only its own paths: a client holding several
//! daemons qualifies a session by its machine, and holds another machine's
//! paths as that machine spelled them.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{MachineId, SessionId};

/// A session on a machine: what a client of several daemons routes by.
/// Never logged by the daemon that stores the session, whose own entries
/// need no machine. Shown as `<machine>:<short id>`; a client that names
/// its machines shows the name in place of the id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address {
    pub machine: MachineId,
    pub session: SessionId,
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.machine, self.session.short())
    }
}

/// A path on the daemon's machine, as that machine spells it. A client
/// shows it and hands it back, never joins, compares or canonicalizes it:
/// the daemon may be on another OS, whose separators and roots differ.
/// Only the machine that made it reads it as a path ([`HostPath::local`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostPath(String);

impl HostPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// This path as the machine that spelled it reads it: the daemon, for
    /// a path in its own store or one its own client sent.
    pub fn local(&self) -> &Path {
        Path::new(&self.0)
    }

    /// A path of this machine as other machines are shown it, a byte that
    /// is not UTF-8 shown as U+FFFD: for what the daemon names of its own
    /// (a session's workspace), never for a path to be read back.
    pub fn lossy(path: &Path) -> Self {
        Self(path.to_string_lossy().into_owned())
    }

    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }
}

/// A path of this machine, which the wire carries as text: one that is not
/// UTF-8 is refused where it enters rather than sent mangled.
impl TryFrom<PathBuf> for HostPath {
    type Error = io::Error;

    fn try_from(path: PathBuf) -> io::Result<Self> {
        path.into_os_string()
            .into_string()
            .map(Self)
            .map_err(|path| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not UTF-8", Path::new(&path).display()),
                )
            })
    }
}

/// Text that is already a host's path, as a person or a test wrote it.
impl From<&str> for HostPath {
    fn from(path: &str) -> Self {
        Self(path.to_owned())
    }
}

impl From<String> for HostPath {
    fn from(path: String) -> Self {
        Self(path)
    }
}

impl fmt::Display for HostPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_path_is_carried_exactly_as_its_machine_spelled_it() {
        // A Windows daemon's path on any client: kept, never re-separated.
        let windows = HostPath::from(r"C:\work\x");
        let json = serde_json::to_string(&windows).unwrap();
        assert_eq!(json, r#""C:\\work\\x""#);
        assert_eq!(serde_json::from_str::<HostPath>(&json).unwrap(), windows);
        assert_eq!(windows.to_string(), r"C:\work\x");
        // The same bytes a path had on the wire.
        let path = PathBuf::from("/w/a b");
        assert_eq!(
            serde_json::to_string(&HostPath::try_from(path.clone()).unwrap()).unwrap(),
            serde_json::to_string(&path).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_is_refused_where_it_enters() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_vec(b"/w/\xff".to_vec()));
        let error = HostPath::try_from(path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn an_address_shows_its_machine_and_short_session() {
        let address = Address {
            machine: MachineId([0; 16]),
            session: SessionId(uuid::Uuid::from_u128(0x1234_5678_9abc)),
        };
        assert_eq!(
            address.to_string(),
            format!("{}:{}", "a".repeat(26), address.session.short())
        );
    }
}
