//! Native spawns own the stdio handles named by the inheritance allowlist.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::os::windows::process::{ExitStatusExt, ProcThreadAttributeList};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::ptr::{null, null_mut};

use crate::windows::{ok, owned, wide};
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentProcess, GetExitCodeProcess, INFINITE, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, TerminateProcess, WaitForSingleObject,
};

pub struct Child {
    process: OwnedHandle,
    id: u32,
}

impl Child {
    pub fn id(&self) -> u32 {
        self.id
    }
    pub fn exit_watch(&self) -> io::Result<OwnedHandle> {
        self.process.try_clone()
    }
    pub fn kill(&mut self) -> io::Result<()> {
        // SAFETY: this owned handle identifies the process created here.
        ok(unsafe { TerminateProcess(self.process.as_raw_handle(), 1) })
    }
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.wait_for(INFINITE)?
            .ok_or_else(|| io::Error::other("infinite process wait timed out"))
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait_for(0)
    }
    fn wait_for(&self, timeout: u32) -> io::Result<Option<ExitStatus>> {
        // SAFETY: the owned process handle has synchronization rights.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), timeout) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: the handle remains open and code is a writable output.
                ok(unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &raw mut code) })?;
                Ok(Some(ExitStatus::from_raw(code)))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }
}

fn inheritable(source: &dyn AsHandle) -> io::Result<OwnedHandle> {
    let mut handle = null_mut();
    // SAFETY: the pseudo-process handles are valid; source remains borrowed
    // until its independently owned, inheritable duplicate is created.
    ok(unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source.as_handle().as_raw_handle(),
            GetCurrentProcess(),
            &raw mut handle,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    })?;
    owned(handle)
}

/// The engine supplies an explicit environment, never the daemon's secrets.
/// Stdio is supplied separately so no hidden std duplication can change the list.
pub(super) fn spawn(
    cmd: &Command,
    io: [&dyn AsHandle; 3],
    job: Option<&OwnedHandle>,
    flags: u32,
    inherit_environment: bool,
) -> io::Result<Child> {
    let mut environment: Vec<_> = if inherit_environment {
        std::env::vars_os().collect()
    } else {
        Vec::new()
    };
    for (name, value) in cmd.get_envs() {
        environment.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        if let Some(value) = value {
            environment.push((name.to_owned(), value.to_owned()));
        }
    }
    let program = resolve(cmd.get_program(), &environment)?;
    let mut line = command_line(program.as_os_str(), cmd.get_args())?;
    let mut environment = environment_block(environment)?;
    let directory = cmd.get_current_dir().map(|dir| wide(dir.as_os_str()));
    let handles = [
        inheritable(io[0])?,
        inheritable(io[1])?,
        inheritable(io[2])?,
    ];
    let allowed = handles.each_ref().map(AsRawHandle::as_raw_handle);
    let jobs = job.map(|job| [job.as_raw_handle()]);
    // Redirecting stdio does not restrict inheritance. HANDLE_LIST is the
    // documented boundary even when bInheritHandles is TRUE:
    // https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute
    let builder = ProcThreadAttributeList::build()
        .attribute(PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize, &allowed);
    let builder = if let Some(jobs) = &jobs {
        builder.attribute(PROC_THREAD_ATTRIBUTE_JOB_LIST as usize, jobs)
    } else {
        builder
    };
    let attributes = builder.finish()?;
    // SAFETY: zero initializes the reserved fields; required fields follow.
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = u32::try_from(size_of_val(&startup)).unwrap();
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    [
        startup.StartupInfo.hStdInput,
        startup.StartupInfo.hStdOutput,
        startup.StartupInfo.hStdError,
    ] = allowed;
    startup.lpAttributeList = attributes.as_ptr().cast_mut().cast();
    // SAFETY: PROCESS_INFORMATION is an output struct of scalar fields.
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let program = wide(program.as_os_str());
    // SAFETY: all buffers and inherited handles live through CreateProcessW;
    // line is writable and NUL-terminated, attributes borrow the exact arrays.
    ok(unsafe {
        CreateProcessW(
            program.as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            1,
            flags | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_mut_ptr().cast(),
            directory.as_ref().map_or(null(), Vec::as_ptr),
            &raw const startup.StartupInfo,
            &raw mut process,
        )
    })?;
    let handle = owned(process.hProcess)?;
    let thread = owned(process.hThread)?;
    let mut child = Child {
        process: handle,
        id: process.dwProcessId,
    };
    let admitted = (|| {
        if let Some(job) = job {
            let mut member = 0;
            // SAFETY: both handles are owned; the BOOL output is writable.
            ok(unsafe {
                windows_sys::Win32::System::JobObjects::IsProcessInJob(
                    child.process.as_raw_handle(),
                    job.as_raw_handle(),
                    &raw mut member,
                )
            })?;
            if member == 0 {
                return Err(io::Error::other("child was not assigned to its job"));
            }
        }
        // SAFETY: the owned primary thread is suspended from creation.
        if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    if let Err(error) = admitted {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(child)
}

fn resolve(program: &OsStr, environment: &[(OsString, OsString)]) -> io::Result<PathBuf> {
    let program = Path::new(program);
    let paths = environment
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map_or_default(|(_, value)| std::env::split_paths(value).collect::<Vec<_>>());
    let dirs = if program.components().count() > 1 || program.is_absolute() {
        vec![PathBuf::new()]
    } else {
        paths
    };
    for dir in dirs {
        let candidate = dir.join(program);
        for path in [candidate.clone(), candidate.with_extension("exe")] {
            if path.is_file() {
                return std::fs::canonicalize(path);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "executable {} not found in the command environment",
            program.display()
        ),
    ))
}

fn environment_block(values: Vec<(OsString, OsString)>) -> io::Result<Vec<u16>> {
    let mut sorted = std::collections::BTreeMap::new();
    for (key, value) in values {
        sorted.insert(key.to_string_lossy().to_uppercase(), (key, value));
    }
    let mut out = Vec::new();
    for (_, (key, value)) in sorted {
        if key
            .encode_wide()
            .chain(value.encode_wide())
            .any(|unit| unit == 0)
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        out.extend(key.encode_wide());
        out.push(u16::from(b'='));
        out.extend(value.encode_wide());
        out.push(0);
    }
    if out.is_empty() {
        out.push(0);
    }
    out.push(0);
    Ok(out)
}

fn command_line<'a>(
    program: &'a OsStr,
    args: impl Iterator<Item = &'a OsStr>,
) -> io::Result<Vec<u16>> {
    let mut out = Vec::new();
    for arg in std::iter::once(program).chain(args) {
        if !out.is_empty() {
            out.push(u16::from(b' '));
        }
        let arg: Vec<_> = arg.encode_wide().collect();
        if arg.contains(&0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        out.extend(quote(&arg));
    }
    out.push(0);
    Ok(out)
}

#[path = "windows_quote.rs"]
mod quoting;
use quoting::quote;
