//! Handing what a daemon owns to the next build of `a` without stopping it:
//! its endpoint, its lock and the process groups its sessions own, each as
//! a live native handle, never a recorded number.
//!
//! On Unix the daemon execs the next build in its own process. The groups
//! stay its children, their leaders unreaped, so the next image adopts each
//! with the kernel's word that it is its child ([`adopt`]), and their output
//! pipes, the listener and the lock cross under the same descriptor numbers
//! with close-on-exec cleared. On Windows the daemon starts the next build
//! inheriting each group's job, leader and pipe through an explicit handle
//! list, and exits once it has taken them: a job dies with its last handle,
//! never in between.
//!
//! What crosses is described by a record the daemon writes ([`record`]) and
//! the next build reads ([`read_record`]); [`VERSION`] names its format, and
//! a build hands over only to one that declares the same.

use std::fs::File;
use std::io;

use serde::{Deserialize, Serialize};

use crate::{Child, ContainedChild, ProcessLease, ProcessPipe};

/// The handover's format, the engine's record with this module's: a daemon
/// hands over only to a build that declares the same, so neither ever reads
/// another's. Bumped whenever either changes.
pub const VERSION: u32 = 1;

/// A process group as the next build finds it: its leader, and the
/// numbers its handles cross under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub pid: u32,
    /// None once nothing of the group writes its pipe any more.
    output: Option<i32>,
    #[cfg(windows)]
    job: i32,
    #[cfg(windows)]
    process: i32,
}

/// A group given up for the next build: how it finds it, the handles it
/// finds it by, kept open here until the handover passes them on, and what
/// was read from its pipe but not yet kept.
pub struct Given {
    pub group: Group,
    pub handles: Vec<File>,
    pub unread: Vec<u8>,
}

/// Gives `child`'s group up, with the read end of its output pipe if
/// anything of it still writes there: from now on this build neither
/// signals nor reaps it, and `lease` no longer stops it.
pub fn give(child: &Child, lease: ProcessLease, output: Option<ProcessPipe>) -> io::Result<Given> {
    imp::give(child, lease, output)
}

/// The group `group` names, handed over by the build before, with its
/// output going on into `file`. A number that does not name this
/// process's own child, or a pipe, is refused.
pub fn adopt(group: &Group, file: File) -> io::Result<ContainedChild> {
    imp::adopt(group, file)
}

/// The handover's record, `bytes`, readable by the next build from the
/// handle returned, which crosses with the others.
pub fn record(home: &crate::Home, bytes: &[u8]) -> io::Result<File> {
    imp::record(home, bytes)
}

/// Reads the record crossing under `number`.
pub fn read_record(number: crate::InheritedNumber) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = imp::own(number.raw())?;
    let mut bytes = Vec::new();
    #[cfg(unix)]
    io::Seek::rewind(&mut file)?;
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The daemon's listener as the next build takes it: on Unix its socket
/// and the claim on its address, by number, which stay open as long as the
/// listener and cross an exec; on Windows nothing, as the next build binds
/// its own.
#[derive(Clone, Copy, Debug)]
pub struct Endpoint {
    #[cfg(unix)]
    socket: crate::Passed,
    #[cfg(unix)]
    claim: crate::Passed,
}

impl Endpoint {
    #[cfg_attr(windows, allow(unused_variables))]
    pub fn of(listener: &crate::Listener) -> Self {
        #[cfg(unix)]
        {
            use crate::Pass;
            Self {
                socket: listener.passed(),
                claim: listener.claim_passed(),
            }
        }
        #[cfg(windows)]
        Self {}
    }
}

impl crate::EndpointRetirement {
    /// What of the endpoint the daemon still lets go of itself once a
    /// successor is to take over: on Unix all of it, should the successor
    /// not start, as it takes the listener on; on Windows nothing, as it
    /// is let go now, for the successor to claim the endpoint anew.
    pub fn for_successor(self) -> io::Result<Option<Self>> {
        #[cfg(unix)]
        return Ok(Some(self));
        #[cfg(windows)]
        self.retire().map(|()| None)
    }
}

/// The next build, as this one starts it: `cmd`, with the handles it is to
/// find open under the numbers they have here.
pub struct Successor<'a> {
    cmd: std::process::Command,
    keep: Vec<&'a File>,
    lock: Option<&'a File>,
    #[cfg(unix)]
    numbers: Vec<i32>,
}

impl<'a> Successor<'a> {
    pub fn new(cmd: std::process::Command) -> Self {
        Self {
            cmd,
            keep: Vec::new(),
            lock: None,
            #[cfg(unix)]
            numbers: Vec::new(),
        }
    }

    /// With the daemon's endpoint and lock, as a client hands them to the
    /// daemon it starts.
    #[must_use]
    #[cfg_attr(windows, allow(unused_variables))]
    pub fn endpoint(mut self, endpoint: Endpoint, lock: &'a File) -> Self {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let Endpoint { socket, claim } = endpoint;
            self.cmd
                .env("A_ENDPOINT_CLAIM_FD", claim.to_string())
                .arg("--listen-fd")
                .arg(socket.to_string())
                .arg("--lock-fd")
                .arg(lock.as_raw_fd().to_string());
            self.numbers.extend([socket.0, claim.0]);
            self.keep.push(lock);
        }
        self.lock = Some(lock);
        self
    }

    /// With the record, named by `--handover-fd`.
    #[must_use]
    pub fn record(mut self, record: &'a File) -> Self {
        self.cmd
            .arg("--handover-fd")
            .arg(imp::number(record).to_string());
        self.keep.push(record);
        self
    }

    #[must_use]
    pub fn handles(mut self, handles: &'a [File]) -> Self {
        self.keep.extend(handles);
        self
    }

    /// Replaces this daemon with the successor: on Unix execs it in this
    /// process; on Windows starts it, lets go of the daemon lock for it,
    /// and exits once it says it serves. It returns only if the successor
    /// did not start, everything as it was.
    pub fn replace(self, home: &crate::Home) -> io::Error {
        imp::replace(self, home)
    }
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::fd::{AsRawFd, FromRawFd};

    use super::{Given, Group};
    use crate::{Child, ContainedChild, OutputCapture, ProcessLease, ProcessPipe};

    #[allow(clippy::unnecessary_wraps, reason = "Windows duplicates handles")]
    pub fn give(
        child: &Child,
        lease: ProcessLease,
        output: Option<ProcessPipe>,
    ) -> io::Result<Given> {
        let pid = child.release();
        // Released, it no longer signals the group as it drops.
        drop(lease);
        Ok(Given {
            group: Group {
                pid,
                output: output.as_ref().map(AsRawFd::as_raw_fd),
            },
            handles: output.into_iter().collect(),
            unread: Vec::new(),
        })
    }

    pub fn adopt(group: &Group, file: File) -> io::Result<ContainedChild> {
        let reader = group.output.map(own).transpose()?;
        if let Some(reader) = &reader
            && !std::os::unix::fs::FileTypeExt::is_fifo(&reader.metadata()?.file_type())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the handed output is not a pipe",
            ));
        }
        let child = Child::adopt(group.pid)?;
        let lease = ProcessLease::new(&child);
        Ok(ContainedChild {
            child,
            group: lease,
            capture: reader.map(|reader| OutputCapture { reader, file }),
        })
    }

    /// An unlinked private file, so nothing but the descriptor reaches it
    /// and a crash leaves nothing to read by mistake.
    pub fn record(home: &crate::Home, bytes: &[u8]) -> io::Result<File> {
        let dir = home.run_dir();
        crate::fs::private_dir(&dir)?;
        let path = dir.join(format!("handover.{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut file = crate::fs::private_file().readable().open(&path)?;
        std::fs::remove_file(&path)?;
        file.write_all(bytes)?;
        Ok(file)
    }

    pub fn number(file: &File) -> i32 {
        file.as_raw_fd()
    }

    pub fn replace(successor: super::Successor<'_>, _home: &crate::Home) -> io::Error {
        use std::os::unix::process::CommandExt;
        let super::Successor {
            mut cmd,
            keep,
            lock,
            numbers,
        } = successor;
        // The next build writes its own record into the lock from the start.
        if let Some(Err(error)) = lock.map(|mut lock| io::Seek::rewind(&mut lock)) {
            return error;
        }
        let numbers: Vec<i32> = (numbers.into_iter())
            .chain(keep.iter().map(|file| file.as_raw_fd()))
            .collect();
        if let Err(error) = inherit(&numbers, true) {
            let _ = inherit(&numbers, false);
            return error;
        }
        let error = cmd.exec();
        let _ = inherit(&numbers, false);
        error
    }

    /// Takes `number`, which nothing in this image owns, as a file closed
    /// on the next exec.
    pub fn own(number: i32) -> io::Result<File> {
        // SAFETY: fcntl validates arbitrary integers in the kernel.
        let flags = unsafe { libc::fcntl(number, libc::F_GETFD) };
        if number < 3 || flags < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no handed descriptor under that number",
            ));
        }
        // SAFETY: fcntl only changes the descriptor's flags.
        if unsafe { libc::fcntl(number, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the number names an open descriptor the previous image
        // passed to this one alone, which nothing here owns yet.
        Ok(unsafe { File::from_raw_fd(number) })
    }

    /// Clears (`true`) or sets close-on-exec on each of `numbers`.
    pub fn inherit(numbers: &[i32], inherit: bool) -> io::Result<()> {
        for &number in numbers {
            // SAFETY: fcntl validates the number and only changes its flags.
            let flags = unsafe { libc::fcntl(number, libc::F_GETFD) };
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            let flags = if inherit {
                flags & !libc::FD_CLOEXEC
            } else {
                flags | libc::FD_CLOEXEC
            };
            // SAFETY: as above.
            if unsafe { libc::fcntl(number, libc::F_SETFD, flags) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
#[path = "handover_windows.rs"]
mod imp;

#[cfg(all(test, unix))]
mod tests;
