//! Native spawns own the stdio handles named by the inheritance allowlist.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::os::windows::process::{ExitStatusExt, ProcThreadAttributeList};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::ptr::{null, null_mut};

use super::super::SpawnEnvironment;
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
    /// The process `process` is, `id`, as the build before handed it over.
    pub(crate) const fn handed(process: OwnedHandle, id: u32) -> Self {
        Self { process, id }
    }

    /// Its process handle, for the build it is handed to.
    pub(crate) fn into_process(self) -> OwnedHandle {
        self.process
    }

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
pub(super) struct SpawnPlan<'a> {
    pub command: &'a Command,
    pub io: [&'a dyn AsHandle; 3],
    /// Handles beyond stdio, already inheritable: the child finds each
    /// under the value it has here.
    pub inherited: &'a [&'a std::fs::File],
    pub job: Option<&'a OwnedHandle>,
    pub flags: u32,
    pub environment: SpawnEnvironment,
}

impl SpawnPlan<'_> {
    pub fn spawn(self) -> io::Result<Child> {
        let Self {
            command: cmd,
            io,
            inherited,
            job,
            flags,
            environment: policy,
        } = self;
        let environment = environment_values(cmd, policy)?;
        let program = resolve(cmd.get_program(), &environment, cmd.get_current_dir())?;
        let mut line = command_line(program.as_os_str(), cmd.get_args())?;
        let mut environment = environment_block(environment)?;
        let directory = cmd
            .get_current_dir()
            .map(|dir| {
                let encoded = wide(dir.as_os_str());
                if encoded[..encoded.len() - 1].contains(&0) {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                Ok::<_, io::Error>(encoded)
            })
            .transpose()?;
        let handles = [
            inheritable(io[0])?,
            inheritable(io[1])?,
            inheritable(io[2])?,
        ];
        let stdio = handles.each_ref().map(AsRawHandle::as_raw_handle);
        let allowed: Vec<_> = (stdio.iter().copied())
            .chain(inherited.iter().map(|file| file.as_raw_handle()))
            .collect();
        let jobs = job.map(|job| [job.as_raw_handle()]);
        let attributes = attributes(&allowed, jobs.as_ref())?;
        // SAFETY: zero initializes the reserved fields; required fields follow.
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = u32::try_from(size_of_val(&startup)).unwrap();
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        [
            startup.StartupInfo.hStdInput,
            startup.StartupInfo.hStdOutput,
            startup.StartupInfo.hStdError,
        ] = stdio;
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
                flags
                    | CREATE_SUSPENDED
                    | CREATE_UNICODE_ENVIRONMENT
                    | EXTENDED_STARTUPINFO_PRESENT,
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
}

/// Redirecting stdio does not restrict inheritance. `HANDLE_LIST` is the
/// documented boundary even when `bInheritHandles` is TRUE:
/// <https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute>
fn attributes<'a>(
    allowed: &'a [std::os::windows::io::RawHandle],
    jobs: Option<&'a [std::os::windows::io::RawHandle; 1]>,
) -> io::Result<ProcThreadAttributeList<'a>> {
    // SAFETY: `allowed` outlives the attribute list built over it, and the
    // size is its length in bytes.
    let builder = unsafe {
        ProcThreadAttributeList::build().raw_attribute(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            allowed.as_ptr().cast::<std::ffi::c_void>(),
            size_of_val(allowed),
        )
    };
    match jobs {
        Some(jobs) => builder.attribute(PROC_THREAD_ATTRIBUTE_JOB_LIST as usize, jobs),
        None => builder,
    }
    .finish()
}

fn resolve(
    program: &OsStr,
    environment: &[(OsString, OsString)],
    directory: Option<&Path>,
) -> io::Result<PathBuf> {
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
        let candidate = if candidate.is_absolute() {
            candidate
        } else {
            directory.map_or_else(|| candidate.clone(), |dir| dir.join(&candidate))
        };
        let executable = candidate
            .extension()
            .is_none()
            .then(|| candidate.with_extension("exe"));
        for path in std::iter::once(candidate).chain(executable) {
            if path.is_file() {
                // Verbatim executable names break .NET Framework initialization.
                // Keep them only when no equivalent ordinary Win32 path exists.
                return dunce::canonicalize(path);
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

fn environment_values(
    command: &Command,
    policy: SpawnEnvironment,
) -> io::Result<Vec<(OsString, OsString)>> {
    let inherited = matches!(policy, SpawnEnvironment::InheritSnapshot);
    let mut values: Vec<_> = if inherited {
        std::env::vars_os().collect()
    } else {
        Vec::new()
    };
    for (name, value) in command.get_envs() {
        if inherited {
            let encoded: Vec<_> = name.encode_wide().collect();
            let mut failure = None;
            values.retain(|(key, _)| {
                match ordinal(&key.encode_wide().collect::<Vec<_>>(), &encoded) {
                    Ok(order) => !order.is_eq(),
                    Err(error) => {
                        failure = Some(error);
                        false
                    }
                }
            });
            if let Some(error) = failure {
                return Err(error);
            }
        }
        if let Some(value) = value {
            values.push((name.to_owned(), value.to_owned()));
        }
    }
    Ok(values)
}

fn ordinal(left: &[u16], right: &[u16]) -> io::Result<std::cmp::Ordering> {
    use std::cmp::Ordering;
    let left_len = i32::try_from(left.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
    let right_len = i32::try_from(right.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
    // SAFETY: both buffers have their exact, validated lengths and remain borrowed.
    match unsafe {
        windows_sys::Win32::Globalization::CompareStringOrdinal(
            left.as_ptr(),
            left_len,
            right.as_ptr(),
            right_len,
            1,
        )
    } {
        1 => Ok(Ordering::Less),
        2 => Ok(Ordering::Equal),
        3 => Ok(Ordering::Greater),
        _ => Err(io::Error::last_os_error()),
    }
}

fn environment_block(values: Vec<(OsString, OsString)>) -> io::Result<Vec<u16>> {
    let mut encoded = values
        .into_iter()
        .map(|(key, value)| {
            (
                key.encode_wide().collect::<Vec<_>>(),
                value.encode_wide().collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    for (key, value) in &encoded {
        // Windows snapshots include hidden per-drive current-directory entries.
        let drive = key.len() == 3
            && key[0] == u16::from(b'=')
            && u8::try_from(key[1]).is_ok_and(|b| b.is_ascii_alphabetic())
            && key[2] == u16::from(b':');
        if key.is_empty()
            || key.iter().chain(value).any(|unit| *unit == 0)
            || (key.contains(&u16::from(b'=')) && !drive)
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
    }
    // Stable sorting keeps the last explicit value for equal native names.
    encoded.reverse();
    let mut failure = None;
    encoded.sort_by(|a, b| {
        ordinal(&a.0, &b.0).unwrap_or_else(|error| {
            failure = Some(error);
            std::cmp::Ordering::Equal
        })
    });
    encoded.dedup_by(|a, b| {
        ordinal(&a.0, &b.0).map_or_else(
            |error| {
                failure = Some(error);
                false
            },
            std::cmp::Ordering::is_eq,
        )
    });
    if let Some(error) = failure {
        return Err(error);
    }
    let mut out = Vec::new();
    for (key, value) in encoded {
        out.extend(key);
        out.push(u16::from(b'='));
        out.extend(value);
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

use crate::windows_quote::quote;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStringExt;

    #[test]
    fn an_embedded_nul_cannot_select_a_different_working_directory() {
        let null = std::fs::File::options()
            .read(true)
            .write(true)
            .open("NUL")
            .unwrap();
        let mut command = Command::new(
            PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/cmd.exe"),
        );
        let mut directory = std::env::temp_dir().into_os_string();
        directory.push(OsString::from_wide(&[0, u16::from(b'x')]));
        command
            .args(["/D", "/C", "exit 0"])
            .envs(std::env::vars_os())
            .current_dir(directory);
        let result = SpawnPlan {
            command: &command,
            io: [&null, &null, &null],
            inherited: &[],
            job: None,
            flags: windows_sys::Win32::System::Threading::CREATE_NO_WINDOW,
            environment: SpawnEnvironment::Exact,
        }
        .spawn();
        if let Ok(mut child) = result {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("truncated directory was accepted");
        }
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn environment_names_use_native_case_without_losing_wchar_identity() {
        let first = OsString::from_wide(&[0xd800]);
        let second = OsString::from_wide(&[0xd801]);
        let block = environment_block(vec![
            ("éCHO".into(), "old".into()),
            ("Écho".into(), "new".into()),
            (first, "a".into()),
            (second, "b".into()),
            ("=C:".into(), "C:\\work".into()),
        ])
        .unwrap();
        let entries: Vec<_> = block
            .split(|&unit| unit == 0)
            .filter(|entry| !entry.is_empty())
            .collect();
        assert_eq!(entries.len(), 4);
        let named: Vec<_> = OsStr::new("Écho=new").encode_wide().collect();
        assert!(entries.iter().any(|entry| *entry == named));
        assert!(entries.contains(&&[0xd800, u16::from(b'='), u16::from(b'a')][..]));
        assert!(entries.contains(&&[0xd801, u16::from(b'='), u16::from(b'b')][..]));
        assert_eq!(
            environment_block(vec![("A=B".into(), "x".into())])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
