use std::fs::File;
use std::io::{self, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
use windows_sys::Win32::System::JobObjects::IsProcessInJob;
use windows_sys::Win32::System::Threading::GetProcessId;

use super::{Claimed, Given, Group, Handover, Parked};
use crate::windows::ok;
use crate::{Child, ProcessLease, ProcessPipe, RecordedIdentity};

pub fn park(pipe: ProcessPipe) -> io::Result<(Parked, Vec<u8>)> {
    let (pipe, unread) = pipe.surrender()?;
    Ok((Parked(File::from(pipe)), unread))
}

pub fn unpark(pipe: File) -> io::Result<ProcessPipe> {
    crate::Stream::from_pipe(pipe)
}

pub fn give(child: Child, lease: ProcessLease, output: Option<Parked>) -> Given {
    let pid = child.id();
    let process = File::from(child.into_process());
    let job = File::from(lease.into_job());
    let pipe = output.map(|Parked(pipe)| pipe);
    Given {
        group: Group {
            pid,
            output: pipe.as_ref().map(number),
            job: number(&job),
            process: number(&process),
        },
        handles: [Some(job), Some(process), pipe]
            .into_iter()
            .flatten()
            .collect(),
    }
}

pub fn claim(group: &Group) -> io::Result<Claimed> {
    let process = own(group.process)?;
    let job = own(group.job)?;
    let pipe = group.output.map(own).transpose()?;
    // SAFETY: the handle is owned and open.
    if unsafe { GetProcessId(process.as_raw_handle()) } != group.pid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the handed process is not the group's leader",
        ));
    }
    let mut member = 0;
    // SAFETY: both handles are owned; the call fails for one not a job.
    ok(unsafe {
        IsProcessInJob(
            process.as_raw_handle(),
            job.as_raw_handle(),
            &raw mut member,
        )
    })?;
    // SAFETY: the handle is owned and open.
    let piped = |pipe: &File| unsafe { GetFileType(pipe.as_raw_handle()) } == FILE_TYPE_PIPE;
    if member == 0 || pipe.as_ref().is_some_and(|pipe| !piped(pipe)) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the handed handles are not one group's",
        ));
    }
    Ok(Claimed {
        child: Child::handed(OwnedHandle::from(process), group.pid),
        lease: ProcessLease::handed(RecordedIdentity(group.pid), OwnedHandle::from(job)),
        output: pipe.map(crate::Stream::from_pipe).transpose()?,
    })
}

/// A private file holding `bytes`, as on Unix: deleted as its last handle
/// closes, so nothing but the handle reaches it and a crash leaves nothing
/// to read by mistake. One per attempt, so a successor that did not start
/// leaves the next one a full record.
fn record(home: &crate::Home, bytes: &[u8]) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_FLAG_DELETE_ON_CLOSE};
    static ATTEMPT: AtomicU32 = AtomicU32::new(0);
    let dir = home.run_dir();
    crate::fs::private_dir(&dir)?;
    let attempt = ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let mut file = File::options()
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .share_mode(0)
        .create_new(true)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
        .open(dir.join(format!("handover.{}.{attempt}", std::process::id())))?;
    file.write_all(bytes)?;
    Ok(file)
}

pub fn replace(handover: &mut Handover<'_>, mut cmd: std::process::Command) -> io::Error {
    let started = (|| {
        // A starting client waits on the start lock until the successor
        // listens, rather than starting a daemon of its own meanwhile; it
        // is taken before the endpoint goes, and kept until this exits.
        if handover.starting.is_none() {
            let starting = File::options()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(handover.home.run_dir().join("start.lock"))?;
            crate::fs::lock(&starting)?;
            handover.starting = Some(starting);
        }
        // The successor claims the endpoint anew.
        if let Some(retirement) = handover.retirement.take() {
            retirement.retire()?;
        }
        let record = record(handover.home, &handover.record)?;
        let keep: Vec<&File> = std::iter::once(&record).chain(&handover.handles).collect();
        inherit(&keep, true)?;
        cmd.arg("--handover-fd").arg(number(&record).to_string());
        // Byte locks belong to processes: the successor takes the daemon
        // lock as this one lets it go.
        crate::fs::unlock(&handover.lock)?;
        let started = crate::proc::spawn_acknowledged(&mut cmd, handover.home, &keep);
        if started.is_err() {
            inherit(&keep, false)?;
            crate::fs::lock(&handover.lock)?;
        }
        started
    })();
    match started {
        Ok(_) => std::process::exit(0),
        Err(error) => error,
    }
}

pub fn number(file: &File) -> i32 {
    crate::proc::passed(file).0
}

/// Takes the handle `number` names, inherited from the build before and
/// owned by nothing here, no longer inheritable.
pub fn own(number: i32) -> io::Result<File> {
    let handle = number as isize as HANDLE;
    // SAFETY: the call validates the handle value in the kernel.
    ok(unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) }).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "no handed handle under that number",
        )
    })?;
    // SAFETY: the handle is open, inherited for this process alone, and
    // nothing here owns it yet.
    Ok(File::from(unsafe { OwnedHandle::from_raw_handle(handle) }))
}

/// Makes each of `handles` inheritable (`true`) by the child whose handle
/// list names it, or no longer.
fn inherit(handles: &[&File], inherit: bool) -> io::Result<()> {
    let flags = if inherit { HANDLE_FLAG_INHERIT } else { 0 };
    for handle in handles {
        // SAFETY: the handle is owned and open.
        ok(unsafe { SetHandleInformation(handle.as_raw_handle(), HANDLE_FLAG_INHERIT, flags) })?;
    }
    Ok(())
}
