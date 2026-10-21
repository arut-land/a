use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Command, ExitStatus};
#[path = "spawn_windows.rs"]
mod spawn;
pub use spawn::Child;
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};

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

use super::Passed;
use crate::Signal;
use crate::windows::{Security, ok, owned};

pub type ExitWatch = OwnedHandle;

pub fn passed(handle: &impl AsRawHandle) -> Passed {
    Passed(
        i32::try_from(handle.as_raw_handle() as isize).expect("Windows handle values fit 32 bits"),
    )
}

pub fn adopt_lock(_passed: Passed) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "Windows daemon startup claims its own lock",
    ))
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

struct Job {
    id: u32,
    handle: OwnedHandle,
}
static JOBS: OnceLock<Mutex<Vec<Job>>> = OnceLock::new();

fn jobs() -> std::sync::MutexGuard<'static, Vec<Job>> {
    JOBS.get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Job assignment is a process creation attribute, so no shell can fork
/// before containment. The daemon alone owns each kill-on-close handle.
pub fn spawn_group(cmd: &mut Command, output: File) -> io::Result<Child> {
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
    let input = File::open("NUL")?;
    let child = spawn::spawn(
        cmd,
        [&input, &output, &output],
        Some(&job),
        CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
    )?;
    drop(output);
    let mut jobs = jobs();
    jobs.retain(|job| !members(&job.handle).is_empty());
    jobs.push(Job {
        id: child.id(),
        handle: job,
    });
    Ok(child)
}

fn members(job: &OwnedHandle) -> Vec<u32> {
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

pub fn group_members(id: u32) -> Vec<u32> {
    jobs()
        .iter()
        .find(|job| job.id == id)
        .map_or_else(Vec::new, |job| members(&job.handle))
}

pub fn group_alive(id: u32) -> bool {
    !group_members(id).is_empty()
}

pub fn signal_group(id: u32, _signal: Signal) -> io::Result<()> {
    if let Some(job) = jobs().iter().find(|job| job.id == id) {
        // SAFETY: this process owns the job; all its children are ours.
        ok(unsafe { TerminateJobObject(job.handle.as_raw_handle(), 1) })?;
    }
    Ok(())
}

pub fn process_started(pid: u32) -> Option<u64> {
    // SAFETY: scalar inputs, checked owned result.
    let process = owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) }).ok()?;
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
            let members = group_members(child.id());
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
        let _ = signal_group(child.id(), Signal::Kill);
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
        signal_group(child.id(), Signal::Kill).unwrap();
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
        signal_group(child.id(), Signal::Kill).unwrap();
        let mut successor = spawn_group(
            &mut command,
            File::options().write(true).open("NUL").unwrap(),
        )
        .unwrap();
        let _ = process.signal(Signal::Kill);
        let successor_alive = successor.try_wait().unwrap().is_none();
        signal_group(successor.id(), Signal::Kill).unwrap();
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
}
