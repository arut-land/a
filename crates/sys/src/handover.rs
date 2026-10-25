//! Handing what a daemon owns to the next build of `a` without stopping it:
//! its endpoint, its lock and the process groups its sessions own, each as
//! a live native handle, never a recorded number.
//!
//! On Unix the daemon execs the next build in its own process. The groups
//! stay its children, their leaders unreaped, so the next image claims each
//! with the kernel's word that it is its child, and their output pipes, the
//! listener and the lock cross under the same descriptor numbers with
//! close-on-exec cleared. On Windows the daemon starts the next build
//! inheriting each group's job, leader and pipe through an explicit handle
//! list, and exits once it serves: a job dies with its last handle, never in
//! between.
//!
//! A group is given up in two steps: [`park`] stops its pipe between two
//! reads, which [`Parked::unpark`] undoes, and [`give`] then releases it for
//! good. What crosses is the record the daemon writes ([`Handover`]); the
//! next build reads it ([`read_record`]) and claims every group it names at
//! once, typed and owned ([`Claimed`]): one it does not adopt is killed and
//! reaped as it drops. [`VERSION`] names the format, and a build hands over
//! only to one that declares the same.

use std::fs::File;
use std::io;

use serde::{Deserialize, Serialize};

use crate::{Child, ContainedChild, OutputCapture, ProcessLease, ProcessPipe};

/// The handover's format, this module's record with what its owner says of
/// each group: a daemon hands over only to a build that declares the same,
/// so neither ever reads another's. Bumped whenever either changes; the
/// engine keeps an encoded record of each version, so a change that is not
/// bumped fails its test.
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

/// A group's output pipe, stopped between two reads for a handover: what
/// was read but not yet kept comes back with it ([`park`]).
pub struct Parked(File);

/// Stops `pipe` for a handover. On Windows a read in flight is cancelled,
/// and what it or one before it read but nobody took yet is returned, so
/// nothing written is lost.
pub fn park(pipe: ProcessPipe) -> io::Result<(Parked, Vec<u8>)> {
    imp::park(pipe)
}

impl Parked {
    /// The pipe again, the handover called off.
    pub fn unpark(self) -> io::Result<ProcessPipe> {
        imp::unpark(self.0)
    }
}

/// A group given up for the next build: how it finds it, and the handles it
/// finds it by, kept open here until the handover passes them on.
pub struct Given {
    pub group: Group,
    pub handles: Vec<File>,
}

/// Gives `child`'s group up, with its parked output pipe if anything of it
/// still writes there: from now on this build neither signals nor reaps
/// it, and `lease` no longer stops it.
pub fn give(child: Child, lease: ProcessLease, output: Option<Parked>) -> Given {
    imp::give(child, lease, output)
}

/// A group the build before handed over, claimed at once and validated: a
/// number that does not name this process's own child, a job holding it or
/// a pipe is refused. Dropped unadopted, the group is killed and reaped on
/// Unix, and on Windows its job closes, which kills it.
pub struct Claimed {
    child: Child,
    lease: ProcessLease,
    output: Option<ProcessPipe>,
}

impl std::fmt::Debug for Claimed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Claimed")
            .field("pid", &self.pid())
            .finish_non_exhaustive()
    }
}

impl Claimed {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The group, owned again, its output going on into `file`.
    pub fn adopt(self, file: File) -> ContainedChild {
        ContainedChild {
            child: self.child,
            group: self.lease,
            capture: self.output.map(|reader| OutputCapture { reader, file }),
        }
    }
}

/// What crosses for each group: where it is, and what its owner says of it.
#[derive(Serialize, Deserialize)]
struct Entry {
    group: Group,
    owner: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
struct Record {
    groups: Vec<Entry>,
}

/// The record of `groups`, each with what its owner says of it.
pub fn encode<'a>(groups: impl IntoIterator<Item = (&'a Group, &'a serde_json::Value)>) -> Vec<u8> {
    let groups = (groups.into_iter())
        .map(|(group, owner)| Entry {
            group: group.clone(),
            owner: owner.clone(),
        })
        .collect();
    serde_json::to_vec(&Record { groups }).unwrap_or_default()
}

/// Reads the record crossing under `number` and claims every group it
/// names, each with what its owner said of it, or why it could not be
/// claimed (its handles then closed).
pub fn read_record(
    number: crate::InheritedNumber,
) -> io::Result<Vec<(io::Result<Claimed>, serde_json::Value)>> {
    use std::io::Read;
    let mut file = imp::own(number.raw())?;
    let mut bytes = Vec::new();
    #[cfg(unix)]
    io::Seek::rewind(&mut file)?;
    file.read_to_end(&mut bytes)?;
    let record: Record = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    Ok((record.groups.into_iter())
        .map(|Entry { group, owner }| (imp::claim(&group), owner))
        .collect())
}

/// Lets TERM, INT and HUP through again in an image the build before
/// exec'd into, which blocked them across the exec so none came between
/// the images unhandled: called once the daemon catches them.
pub fn shutdown_signals_caught() -> io::Result<()> {
    #[cfg(unix)]
    {
        let set = crate::signals::shutdown_set();
        // SAFETY: the set is initialized; the old mask is not asked for.
        let error = unsafe {
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &raw const set, std::ptr::null_mut())
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
    }
    Ok(())
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

/// A daemon becoming the next build: its endpoint, lock and given groups,
/// and the record of them, passed on by [`Handover::replace`].
pub struct Handover<'a> {
    home: &'a crate::Home,
    #[cfg(unix)]
    endpoint: Endpoint,
    retirement: Option<crate::EndpointRetirement>,
    lock: File,
    handles: Vec<File>,
    record: Vec<u8>,
    /// On Windows, the start lock, held from the first attempt on.
    #[cfg(windows)]
    starting: Option<File>,
}

impl<'a> Handover<'a> {
    pub fn new(
        home: &'a crate::Home,
        (endpoint, retirement): (Endpoint, crate::EndpointRetirement),
        lock: File,
        handles: Vec<File>,
        record: Vec<u8>,
    ) -> Self {
        #[cfg(windows)]
        let _ = endpoint;
        Self {
            home,
            #[cfg(unix)]
            endpoint,
            retirement: Some(retirement),
            lock,
            handles,
            record,
            #[cfg(windows)]
            starting: None,
        }
    }

    /// Replaces this daemon with `cmd`, the next build: on Unix execs it in
    /// this process; on Windows starts it, lets go of the endpoint and the
    /// daemon lock for it under the start lock, and exits once it says it
    /// serves. It returns only if the successor did not start, and may be
    /// tried again with another.
    pub fn replace(&mut self, cmd: std::process::Command) -> io::Error {
        imp::replace(self, cmd)
    }

    /// Lets go of what a handover that did not happen still holds: the
    /// endpoint retired, the daemon lock released as this drops.
    pub fn abandon(mut self) -> io::Result<()> {
        self.retirement
            .take()
            .map_or(Ok(()), crate::EndpointRetirement::retire)
    }
}

/// The next build, as this one starts it: `cmd`, with the handles it is to
/// find open under the numbers they have here.
#[cfg(unix)]
pub(crate) struct Successor<'a> {
    cmd: std::process::Command,
    keep: Vec<&'a File>,
    numbers: Vec<i32>,
}

#[cfg(unix)]
impl<'a> Successor<'a> {
    pub(crate) fn new(cmd: std::process::Command) -> Self {
        Self {
            cmd,
            keep: Vec::new(),
            numbers: Vec::new(),
        }
    }

    /// With the daemon's listener and lock, as a client hands them to the
    /// daemon it starts.
    #[must_use]
    pub(crate) fn endpoint(mut self, endpoint: Endpoint, lock: &'a File) -> Self {
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
        self
    }

    /// With the record, named by `--handover-fd`.
    #[must_use]
    pub(crate) fn record(mut self, record: &'a File) -> Self {
        self.cmd
            .arg("--handover-fd")
            .arg(imp::number(record).to_string());
        self.keep.push(record);
        self
    }

    #[must_use]
    pub(crate) fn handles(mut self, handles: &'a [File]) -> Self {
        self.keep.extend(handles);
        self
    }

    /// Replaces this process's image with the successor's; returns only if
    /// that fails, everything as it was. TERM, INT and HUP stay blocked
    /// across the exec, so none comes between the images unhandled; the
    /// next image lets them through once it catches them
    /// ([`shutdown_signals_caught`]).
    pub(crate) fn exec(self) -> io::Error {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let Self {
            mut cmd,
            keep,
            numbers,
        } = self;
        let numbers: Vec<i32> = (numbers.into_iter())
            .chain(keep.iter().map(|file| file.as_raw_fd()))
            .collect();
        if let Err(error) = imp::inherit(&numbers, true) {
            let _ = imp::inherit(&numbers, false);
            return error;
        }
        // SAFETY: the hook runs after std resets the signal mask, in this
        // process just before execve, and only calls sigprocmask on a set
        // built on its stack.
        unsafe {
            cmd.pre_exec(|| {
                let set = crate::signals::shutdown_set();
                if libc::sigprocmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let error = cmd.exec();
        let _ = imp::inherit(&numbers, false);
        error
    }
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::fd::{AsRawFd, FromRawFd};

    use super::{Claimed, Given, Group, Handover, Parked, Successor};
    use crate::{Child, ProcessLease, ProcessPipe};

    #[allow(clippy::unnecessary_wraps, reason = "Windows cancels a read in flight")]
    pub fn park(pipe: ProcessPipe) -> io::Result<(Parked, Vec<u8>)> {
        Ok((Parked(pipe), Vec::new()))
    }

    #[allow(clippy::unnecessary_wraps, reason = "Windows rewraps the handle")]
    pub fn unpark(pipe: File) -> io::Result<ProcessPipe> {
        Ok(pipe)
    }

    pub fn give(child: Child, lease: ProcessLease, output: Option<Parked>) -> Given {
        let pid = child.release();
        // Released, neither signals the group as it drops.
        drop((child, lease));
        let output = output.map(|Parked(pipe)| pipe);
        Given {
            group: Group {
                pid,
                output: output.as_ref().map(AsRawFd::as_raw_fd),
            },
            handles: output.into_iter().collect(),
        }
    }

    pub fn claim(group: &Group) -> io::Result<Claimed> {
        let output = group.output.map(own).transpose()?;
        if let Some(output) = &output
            && !std::os::unix::fs::FileTypeExt::is_fifo(&output.metadata()?.file_type())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the handed output is not a pipe",
            ));
        }
        let child = Child::adopt(group.pid)?;
        let lease = ProcessLease::new(&child);
        Ok(Claimed {
            child,
            lease,
            output,
        })
    }

    /// An unlinked private file, so nothing but the descriptor reaches it
    /// and a crash leaves nothing to read by mistake.
    fn record(home: &crate::Home, bytes: &[u8]) -> io::Result<File> {
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

    pub fn replace(handover: &mut Handover<'_>, cmd: std::process::Command) -> io::Error {
        let record = match record(handover.home, &handover.record) {
            Ok(record) => record,
            Err(error) => return error,
        };
        // The next build writes its own record into the lock from the start.
        if let Err(error) = io::Seek::rewind(&mut &handover.lock) {
            return error;
        }
        Successor::new(cmd)
            .endpoint(handover.endpoint, &handover.lock)
            .record(&record)
            .handles(&handover.handles)
            .exec()
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
