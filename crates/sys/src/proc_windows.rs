use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Command, ExitStatus};
#[path = "spawn_windows.rs"]
mod spawn;
pub use spawn::Child;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, FILETIME};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS, GetProcessTimes, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess,
};

use windows_sys::Win32::System::SystemInformation::GetTickCount64;

use super::{ContainedChild, GroupId, OwnedGroup, Passed};
use crate::Signal;
use crate::windows::{Security, ok, owned};

pub type ExitWatch = OwnedHandle;

pub fn passed(handle: &impl AsRawHandle) -> Passed {
    Passed(
        i32::try_from(handle.as_raw_handle() as isize).expect("Windows handle values fit 32 bits"),
    )
}

pub fn duplicate(_number: i32) -> io::Result<File> {
    // Windows startup does not inherit native resources through argv.
    Err(io::ErrorKind::Unsupported.into())
}

pub fn adopt_lock(_file: File, _home: &crate::Home) -> Result<File, super::AdoptLockError> {
    Err(super::AdoptLockError::Unsupported)
}

pub fn spawn_daemon(cmd: &mut Command, _home: &crate::Home, lock: File) -> io::Result<Child> {
    // Windows byte locks belong to processes. The start lock remains held
    // by the client until binding is acknowledged, serializing re-listen.
    drop(lock);
    cmd.arg("--detached")
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    for name in [
        "SystemRoot",
        "WINDIR",
        "USERPROFILE",
        "LOCALAPPDATA",
        "ProgramFiles",
        "TEMP",
        "TMP",
        "COMSPEC",
        "PATHEXT",
    ] {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    let (mut output, writer) = io::pipe()?;
    let input = File::open("NUL")?;
    let error = File::options().write(true).open("NUL")?;
    let mut child = spawn::spawn(
        cmd,
        [&input, &writer, &error],
        None,
        DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
    )?;
    drop(writer);
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let _ = tx.send(output.read_exact(&mut [0]));
    });
    let ready = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon did not acknowledge binding",
            )
        })
        .and_then(std::convert::identity);
    if let Err(error) = ready {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        return Err(error);
    }
    let _ = reader.join();
    Ok(child)
}

pub fn daemon_ready() -> io::Result<()> {
    let mut output = io::stdout().lock();
    output.write_all(&[1])?;
    output.flush()
}

#[allow(clippy::unnecessary_wraps, reason = "Unix leaves the parent's session")]
pub fn detach_session() -> io::Result<()> {
    Ok(())
}

pub fn lead(cmd: &mut Command) {
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

/// Job assignment is a process creation attribute, so no shell can fork
/// before containment. The daemon alone owns each kill-on-close handle.
pub fn spawn_group(cmd: &mut Command, output: File) -> io::Result<ContainedChild> {
    let input = File::open("NUL")?;
    let child = spawn_job(cmd, [&input, &output, &output]);
    drop(output);
    child
}

pub fn spawn_group_captured(cmd: &mut Command, output: File) -> io::Result<ContainedChild> {
    let input = File::open("NUL")?;
    let (reader, writer) = crate::endpoint::imp::process_pipe()?;
    let mut child = spawn_job(cmd, [&input, &writer, &writer])?;
    child.capture = Some(super::OutputCapture {
        reader,
        file: output,
    });
    Ok(child)
}

pub fn spawn_stdio(
    cmd: &mut Command,
) -> io::Result<(ContainedChild, crate::Stream, crate::Stream)> {
    let (input, child_input) = crate::endpoint::imp::process_pipe()?;
    let (output, child_output) = crate::endpoint::imp::process_pipe()?;
    let error = File::options().write(true).open("NUL")?;
    let child = spawn_job(cmd, [&child_input, &child_output, &error])?;
    Ok((child, input, output))
}

fn spawn_job(
    cmd: &Command,
    io: [&dyn std::os::windows::io::AsHandle; 3],
) -> io::Result<ContainedChild> {
    let security = Security::user_only()?;
    let attributes = security.attributes();
    // SAFETY: attributes live through creation, unnamed job cannot be pre-created.
    let job = owned(unsafe { CreateJobObjectW(&raw const attributes, null()) })?;
    // SAFETY: the all-zero limits struct is valid before setting flags.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: the pointer and byte count describe the complete limits struct.
    ok(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            u32::try_from(size_of_val(&limits)).unwrap(),
        )
    })?;
    let child = spawn::spawn(
        cmd,
        io,
        Some(&job),
        CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
    )?;
    let group = OwnedGroup {
        id: GroupId(child.id()),
        handle: job,
    };
    Ok(ContainedChild {
        child,
        group,
        capture: None,
    })
}

pub(super) fn members(job: &OwnedHandle) -> Vec<u32> {
    let mut count = 64;
    loop {
        // Pointer-sized storage aligns JOBOBJECT_BASIC_PROCESS_ID_LIST.
        let mut buffer = vec![0usize; count + 2];
        // SAFETY: the aligned allocation has the advertised writable byte count.
        let success = unsafe {
            QueryInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicProcessIdList,
                buffer.as_mut_ptr().cast(),
                u32::try_from(size_of_val(buffer.as_slice())).unwrap_or(u32::MAX),
                null_mut(),
            )
        };
        if success != 0 {
            // First two DWORDs precede the pointer-sized process array.
            let header = buffer.as_ptr().cast::<u32>();
            // SAFETY: successful query initialized both DWORDs and the listed IDs.
            let len = unsafe { *header.add(1) } as usize;
            let offset = 8 / size_of::<usize>();
            return buffer[offset..offset + len]
                .iter()
                .filter_map(|&pid| pid.try_into().ok())
                .collect();
        }
        if io::Error::last_os_error().raw_os_error() != Some(234) {
            return Vec::new();
        }
        count *= 2;
    }
}

// Kill-on-close jobs cannot survive their daemon; numeric recovery owns none.
pub fn group_members(_id: u32) -> Vec<u32> {
    Vec::new()
}
pub fn group_alive(_id: u32) -> bool {
    false
}
pub fn signal_group(_id: u32, _signal: Signal) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

pub(super) fn terminate_group(job: &OwnedHandle) -> io::Result<()> {
    // SAFETY: the borrowed job remains owned throughout termination.
    ok(unsafe { TerminateJobObject(job.as_raw_handle(), 1) })
}

pub fn process_started(pid: u32) -> Option<u64> {
    // SAFETY: scalar inputs, checked owned result.
    let process =
        owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | 0x0010_0000, 0, pid) })
            .ok()?;
    // Retained handles keep an exited process object queryable. A signalled
    // process is gone even while its creation time remains available.
    // SAFETY: this owned process handle includes SYNCHRONIZE access.
    if unsafe {
        windows_sys::Win32::System::Threading::WaitForSingleObject(process.as_raw_handle(), 0)
    } != windows_sys::Win32::Foundation::WAIT_TIMEOUT
    {
        return None;
    }
    created_at(&process).ok()
}

fn created_at(process: &OwnedHandle) -> io::Result<u64> {
    let mut times = [FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    }; 4];
    let [created, exited, kernel, user] = &mut times;
    // SAFETY: the outputs are distinct FILETIME buffers and process is live.
    ok(unsafe { GetProcessTimes(process.as_raw_handle(), created, exited, kernel, user) })?;
    Ok(u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime))
}

pub fn exit_watch(pid: u32) -> io::Result<OwnedHandle> {
    // SAFETY: scalar inputs; checked handle transferred to the caller.
    owned(unsafe { OpenProcess(0x0010_0000, 0, pid) }).map_err(|error| {
        if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER.cast_signed()) {
            io::ErrorKind::NotFound.into()
        } else {
            error
        }
    })
}

/// The verified kernel object stays owned across both stop attempts.
pub struct RecordedProcess(OwnedHandle);

impl RecordedProcess {
    pub fn open(pid: u32, expected: Option<u64>) -> io::Result<Self> {
        // Holding the query+terminate handle pins identity even if the process
        // exits and its numeric PID is reused before the stop attempt.
        // https://learn.microsoft.com/en-us/windows/win32/procthread/process-handles-and-identifiers
        // SAFETY: scalar inputs, with the checked handle immediately owned.
        let process = owned(unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                0,
                pid,
            )
        })?;
        if expected != Some(created_at(&process)?) {
            return Err(io::Error::other(
                "process is no longer the daemon that recorded it",
            ));
        }
        Ok(Self(process))
    }

    pub fn signal(&self, _signal: Signal) -> io::Result<()> {
        // SAFETY: identity was checked through this same owned handle.
        ok(unsafe { TerminateProcess(self.0.as_raw_handle(), 1) })
    }
}

pub fn signal_process(pid: u32, _signal: Signal) -> io::Result<()> {
    // SAFETY: scalar inputs; checked handle is owned until termination returns.
    let process = owned(unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) })?;
    // SAFETY: callers own this child PID; daemon stop uses RecordedProcess.
    ok(unsafe { TerminateProcess(process.as_raw_handle(), 1) })
}

pub fn exit_signal(_status: ExitStatus) -> Option<String> {
    None
}

pub fn read_boot_id() -> Option<String> {
    // SAFETY: GetTickCount64 has no preconditions.
    let since_boot = unsafe { GetTickCount64() } / 1000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(format!(
        "boot-{}",
        (now.checked_sub(since_boot)? + 30) / 60 * 60
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    #[test]
    fn job_owner() {
        if std::env::var_os("A_TEST_JOB_OWNER").is_none() {
            return;
        }
        let mut command = Command::new("cmd.exe");
        command
            .args(["/D", "/C", "ping -n 60 127.0.0.1 >NUL"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.envs(std::env::vars_os());
        let output = File::options().write(true).open("NUL").unwrap();
        let mut child = spawn_group(&mut command, output).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let members = loop {
            let members = child.group.members();
            if members.len() >= 2 {
                break members;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "shell never launched its child"
            );
            std::thread::yield_now();
        };
        println!(
            "JOB_PIDS {}",
            members
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
        io::stdout().flush().unwrap();
        let mut byte = [0];
        let _ = io::stdin().read_exact(&mut byte);
        let _ = child.group.signal(Signal::Kill);
        let _ = child.wait();
    }

    #[test]
    fn daemon_death_kills_the_whole_job_tree() {
        let mut owner = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "proc::imp::tests::job_owner", "--nocapture"])
            .env("A_TEST_JOB_OWNER", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = BufReader::new(owner.stdout.take().unwrap());
        let members = output.lines().map(Result::unwrap).find_map(|line| {
            line.split_once("JOB_PIDS ")
                .map(|(_, pids)| pids.to_owned())
        });
        let Some(members) = members else {
            let _ = owner.kill();
            let _ = owner.wait();
            panic!("owner never reported its job members");
        };
        let watches: Vec<_> = members
            .split(',')
            .map(|pid| exit_watch(pid.parse().unwrap()).unwrap())
            .collect();
        owner.kill().unwrap();
        owner.wait().unwrap();
        assert!(watches.len() >= 2);
        for watch in watches {
            // SAFETY: owned process handle remains open until the wait completes.
            let status = unsafe { WaitForSingleObject(watch.as_raw_handle(), 5000) };
            assert_eq!(status, 0);
        }
    }

    #[test]
    fn a_shell_never_inherits_an_unrelated_inheritable_pipe() {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        let (mut input, output) = io::pipe().unwrap();
        // SAFETY: the test owns this pipe; only its inheritance bit changes.
        ok(unsafe {
            SetHandleInformation(
                output.as_raw_handle(),
                HANDLE_FLAG_INHERIT,
                HANDLE_FLAG_INHERIT,
            )
        })
        .unwrap();
        let mut command = Command::new("cmd.exe");
        command
            .args(["/D", "/C", "ping -n 60 127.0.0.1 >NUL"])
            .envs(std::env::vars_os());
        let null = File::options().write(true).open("NUL").unwrap();
        let mut child = spawn_group(&mut command, null).unwrap();
        drop(output);
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let _ = tx.send(input.read(&mut [0]));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(5));
        child.group.signal(Signal::Kill).unwrap();
        child.wait().unwrap();
        reader.join().unwrap();
        assert_eq!(result.unwrap().unwrap(), 0);
    }

    #[test]
    fn stop_keeps_one_verified_handle_through_exit_and_a_successor() {
        let mut command = Command::new("cmd.exe");
        command
            .args(["/D", "/C", "ping -n 60 127.0.0.1 >NUL"])
            .envs(std::env::vars_os());
        let mut child = spawn_group(
            &mut command,
            File::options().write(true).open("NUL").unwrap(),
        )
        .unwrap();
        let token = process_started(child.id()).unwrap();
        let missing_rejected = RecordedProcess::open(child.id(), None).is_err();
        let wrong_rejected = RecordedProcess::open(child.id(), Some(token + 1)).is_err();
        let process = RecordedProcess::open(child.id(), Some(token)).unwrap();
        process.signal(Signal::Terminate).unwrap();
        child.wait().unwrap();
        child.group.signal(Signal::Kill).unwrap();
        let mut successor = spawn_group(
            &mut command,
            File::options().write(true).open("NUL").unwrap(),
        )
        .unwrap();
        let _ = process.signal(Signal::Kill);
        let successor_alive = successor.try_wait().unwrap().is_none();
        successor.group.signal(Signal::Kill).unwrap();
        successor.wait().unwrap();
        assert!(missing_rejected && wrong_rejected);
        assert!(successor_alive);
    }

    #[test]
    fn process_identity_is_stable_and_not_a_pid_alone() {
        let token = process_started(std::process::id()).unwrap();
        assert_ne!(token, 0);
        assert_eq!(process_started(std::process::id()), Some(token));
    }
    #[test]
    fn exited_process_objects_retained_by_handles_are_not_live() {
        let mut child = Command::new("cmd.exe")
            .args(["/D", "/C", "exit 0"])
            .spawn()
            .unwrap();
        let retained = exit_watch(child.id()).unwrap();
        child.wait().unwrap();
        assert!(process_started(child.id()).is_none());
        drop(retained);
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use windows_sys::Win32::Foundation::GetHandleInformation;

    #[test]
    fn completed_windows_jobs_need_no_future_spawn_to_be_released() {
        let mut command = Command::new("cmd.exe");
        command
            .args(["/D", "/C", "exit 0"])
            .envs(std::env::vars_os());
        let mut child = spawn_group(
            &mut command,
            File::options().write(true).open("NUL").unwrap(),
        )
        .unwrap();

        child.wait().unwrap();
        let handle = child.group.handle.as_raw_handle();
        drop(child.group);
        let mut flags = 0;
        // SAFETY: this API validates the former handle without adopting or closing it.
        assert_eq!(unsafe { GetHandleInformation(handle, &raw mut flags) }, 0);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(6));
    }
}
