use std::fs::File;
use std::io::{self, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
use windows_sys::Win32::System::JobObjects::IsProcessInJob;
use windows_sys::Win32::System::Threading::GetProcessId;

use super::{Given, Group};
use crate::windows::ok;
use crate::{Child, ContainedChild, OutputCapture, ProcessLease, ProcessPipe, RecordedIdentity};

pub fn give(child: &Child, lease: ProcessLease, output: Option<ProcessPipe>) -> io::Result<Given> {
    let process = inheritable(child.process().try_clone()?)?;
    let job = inheritable(lease.into_job())?;
    let (pipe, unread) = match output {
        Some(output) => {
            let (pipe, unread) = output.surrender()?;
            (Some(inheritable(pipe)?), unread)
        }
        None => (None, Vec::new()),
    };
    Ok(Given {
        group: Group {
            pid: child.id(),
            output: pipe.as_ref().map(number),
            job: number(&job),
            process: number(&process),
        },
        handles: [Some(job), Some(process), pipe]
            .into_iter()
            .flatten()
            .collect(),
        unread,
    })
}

pub fn adopt(group: &Group, file: File) -> io::Result<ContainedChild> {
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
    let child = Child::handed(OwnedHandle::from(process), group.pid);
    Ok(ContainedChild {
        child,
        group: ProcessLease::handed(RecordedIdentity(group.pid), OwnedHandle::from(job)),
        capture: pipe
            .map(|pipe| {
                Ok::<_, io::Error>(OutputCapture {
                    reader: crate::Stream::from_pipe(pipe)?,
                    file,
                })
            })
            .transpose()?,
    })
}

/// A pipe holding `bytes`, written on a thread of its own as the next
/// build reads it.
pub fn record(_home: &crate::Home, bytes: &[u8]) -> io::Result<File> {
    let (reader, mut writer) = io::pipe()?;
    let bytes = bytes.to_vec();
    std::thread::Builder::new()
        .name("a-handover".into())
        .spawn(move || {
            let _ = writer.write_all(&bytes);
        })?;
    inheritable(File::from(OwnedHandle::from(reader)))
}

pub fn replace(successor: super::Successor<'_>, home: &crate::Home) -> io::Error {
    let super::Successor {
        mut cmd,
        keep,
        lock,
        ..
    } = successor;
    let started = (|| {
        // A starting client waits on the start lock until the successor
        // listens, rather than starting a daemon of its own meanwhile.
        let starting = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(home.run_dir().join("start.lock"))?;
        crate::fs::lock(&starting)?;
        // Byte locks belong to processes: the successor takes the daemon
        // lock as this one lets it go.
        if let Some(lock) = lock {
            crate::fs::unlock(lock)?;
        }
        let started = crate::proc::spawn_acknowledged(&mut cmd, home, &keep);
        if let (Err(_), Some(lock)) = (&started, lock) {
            crate::fs::lock(lock)?;
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

/// `handle`, inheritable by the child whose handle list names it.
fn inheritable(handle: impl Into<OwnedHandle>) -> io::Result<File> {
    let handle = handle.into();
    // SAFETY: the handle is owned and open.
    ok(unsafe {
        SetHandleInformation(
            handle.as_raw_handle(),
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        )
    })?;
    Ok(File::from(handle))
}
