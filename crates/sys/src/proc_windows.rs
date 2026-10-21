use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::os::windows::process::{CommandExt, ProcThreadAttributeList};
use std::process::{Child, Command, ExitStatus, Stdio};
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
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    TerminateProcess,
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
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdout(Stdio::piped());
    for name in [
        "SystemRoot",
        "WINDIR",
        "USERPROFILE",
        "LOCALAPPDATA",
        "TEMP",
        "TMP",
        "COMSPEC",
        "PATHEXT",
    ] {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    let mut child = cmd.spawn()?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("daemon readiness pipe is missing"))?;
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
pub fn spawn_group(cmd: &mut Command) -> io::Result<Child> {
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
    let handles = [job.as_raw_handle()];
    let attributes = ProcThreadAttributeList::build()
        .attribute(PROC_THREAD_ATTRIBUTE_JOB_LIST as usize, &handles)
        .finish()?;
    lead(cmd);
    let child = cmd.spawn_with_attributes(&attributes)?;
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
    let mut times = [FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    }; 4];
    let [created, exited, kernel, user] = &mut times;
    // SAFETY: the outputs are distinct FILETIME buffers and process is live.
    ok(unsafe { GetProcessTimes(process.as_raw_handle(), created, exited, kernel, user) }).ok()?;
    Some(u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime))
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

pub fn signal_process(pid: u32, _signal: Signal) -> io::Result<()> {
    // SAFETY: scalar inputs; checked handle is owned until termination returns.
    let process = owned(unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) })?;
    // SAFETY: the caller validated the daemon's creation time before signalling.
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
        let mut child = spawn_group(&mut command).unwrap();
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
    fn process_identity_is_stable_and_not_a_pid_alone() {
        let token = process_started(std::process::id()).unwrap();
        assert_ne!(token, 0);
        assert_eq!(process_started(std::process::id()), Some(token));
    }
}
