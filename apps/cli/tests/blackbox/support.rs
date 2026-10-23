//! Portable process collection and isolated daemon homes. PTYs keep their own harness.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use sys::Home;

/// Tests only target daemons they started in isolated homes.
pub fn signal_process(pid: u32, signal: sys::Signal) -> io::Result<()> {
    #[cfg(unix)]
    {
        let pid = rustix::process::Pid::from_raw(
            i32::try_from(pid).map_err(|_| io::ErrorKind::InvalidInput)?,
        )
        .ok_or(io::ErrorKind::InvalidInput)?;
        let signal = match signal {
            sys::Signal::Terminate => rustix::process::Signal::TERM,
            sys::Signal::Kill => rustix::process::Signal::KILL,
            _ => return Err(io::ErrorKind::InvalidInput.into()),
        };
        rustix::process::kill_process(pid, signal).map_err(Into::into)
    }
    #[cfg(windows)]
    {
        let _ = signal;
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other("test daemon termination failed"))
        }
    }
}

pub fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

pub fn isolated_root(name: &str) -> PathBuf {
    // CI scratch storage avoids slow durable writes in the OS temp directory.
    let base = std::env::var_os("RUNNER_TEMP").map_or_else(std::env::temp_dir, PathBuf::from);
    // The harness stops daemons and retries Windows handle release before removal.
    let root = tempfile::Builder::new()
        .prefix(&format!("a-acc-{name}-"))
        .tempdir_in(base)
        .unwrap()
        .keep();
    fs::canonicalize(root).unwrap()
}

pub fn remove_home(root: &Path) {
    let deadline = deadline();
    loop {
        match fs::remove_dir_all(root) {
            Ok(()) => return,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                assert!(std::thread::panicking(), "cleaning isolated home: {error}");
                return;
            }
        }
    }
}

enum Process {
    Piped(Child),
    Contained(sys::ContainedChild),
}

impl Process {
    #[allow(
        clippy::match_same_arms,
        reason = "sys and std children differ on Windows"
    )]
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self {
            Self::Piped(child) => child.try_wait(),
            Self::Contained(child) => child.try_wait(),
        }
    }
}

pub struct TestProcess {
    child: Process,
    readers: [Option<Box<dyn Read + Send>>; 2],
    command: String,
}

impl From<Child> for TestProcess {
    fn from(mut child: Child) -> Self {
        drop(child.stdin.take());
        let readers = [
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ];
        Self {
            child: Process::Piped(child),
            readers,
            command: "command unavailable (adopted child)".into(),
        }
    }
}

impl TestProcess {
    pub fn spawn(command: &mut Command) -> Self {
        let description = describe(command);
        let mut process = Self::from(
            command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        process.command = description;
        process
    }

    pub fn group(&self) -> &sys::ProcessLease {
        match &self.child {
            Process::Contained(child) => &child.group,
            Process::Piped(_) => panic!("uncontained test process"),
        }
    }

    pub fn contained(command: &mut Command, output: fs::File) -> Self {
        let child = sys::spawn_group(command, output).unwrap();
        Self {
            child: Process::Contained(child),
            readers: [None, None],
            command: describe(command),
        }
    }

    pub fn collect(mut self, deadline: Instant) -> io::Result<Output> {
        let (tx, rx) = mpsc::sync_channel(16);
        // Both readers start before waiting; either pipe can fill independently.
        let mut streams = [Vec::new(), Vec::new()];
        let mut eof = [true, true];
        for (index, reader) in std::mem::take(&mut self.readers).into_iter().enumerate() {
            let Some(mut reader) = reader else { continue };
            eof[index] = false;
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut bytes = [0; 8192];
                loop {
                    let result = match reader.read(&mut bytes) {
                        Ok(n) => Ok(bytes[..n].to_vec()),
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => Err(error),
                    };
                    let done = result.as_ref().map_or(true, Vec::is_empty);
                    if tx.send((index, result)).is_err() || done {
                        break;
                    }
                }
            });
        }
        drop(tx);
        loop {
            // Continuous output must not postpone checking the deadline.
            for _ in 0..16 {
                let Ok((index, result)) = rx.try_recv() else {
                    break;
                };
                let bytes = result?;
                eof[index] = bytes.is_empty();
                streams[index].extend(bytes);
            }
            let status = self.child.try_wait()?;
            if let Some(status) = status
                && eof.iter().all(|&eof| eof)
            {
                let [stdout, stderr] = streams;
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "test process collection deadline: pid={} command={} status={status:?} stdout={} stderr={}\nstdout tail: {}\nstderr tail: {}",
                        match &self.child {
                            Process::Piped(child) => child.id(),
                            Process::Contained(child) => child.id(),
                        },
                        self.command,
                        if eof[0] { "eof" } else { "pending" },
                        if eof[1] { "eof" } else { "pending" },
                        String::from_utf8_lossy(
                            &streams[0][streams[0].len().saturating_sub(4096)..]
                        ),
                        String::from_utf8_lossy(
                            &streams[1][streams[1].len().saturating_sub(4096)..]
                        ),
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn describe(command: &Command) -> String {
    format!(
        "{} {:?} cwd={:?}",
        command.get_program().display(),
        command.get_args().collect::<Vec<_>>(),
        command.get_current_dir(),
    )
}

impl Drop for TestProcess {
    fn drop(&mut self) {
        match &mut self.child {
            Process::Piped(child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            Process::Contained(child) => {
                let _ = child.group.signal(sys::Signal::Kill);
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

pub struct Isolated {
    pub home: Home,
}

impl Isolated {
    pub fn new(name: &str) -> Self {
        let home = Home::from(isolated_root(name));
        fs::write(
            home.config_file(),
            "[daemon]\nagent = \"echo\"\nidle_secs = 60\n",
        )
        .unwrap();
        Self { home }
    }

    pub fn command(&self, args: &[&str]) -> Command {
        self.command_at(Path::new(env!("CARGO_BIN_EXE_a")), args)
    }

    pub fn command_at(&self, exe: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(exe);
        command
            .args(args)
            .env("A_HOME", self.home.root())
            .env_remove("A_HOST")
            .current_dir(self.home.root())
            .stdin(Stdio::null());
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        let output = TestProcess::spawn(&mut self.command(args))
            .collect(deadline())
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    pub fn logs(&self) -> std::collections::BTreeMap<String, Vec<u8>> {
        fs::read_dir(self.home.sessions_dir())
            .unwrap()
            .map(|entry| entry.unwrap().path().join("log.jsonl"))
            .filter(|path| path.is_file())
            .map(|path| {
                (
                    path.parent()
                        .unwrap()
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    fs::read(&path).unwrap(),
                )
            })
            .collect()
    }

    pub fn pid(&self) -> u32 {
        fs::read_to_string(self.home.daemon_pid_file())
            .unwrap()
            .parse::<sys::crash::Record>()
            .unwrap()
            .pid
    }
}

impl Drop for Isolated {
    fn drop(&mut self) {
        if let Err(error) =
            TestProcess::spawn(&mut self.command(&["daemon", "stop"])).collect(deadline())
        {
            eprintln!(
                "stopping isolated daemon at {}: {error}",
                self.home.root().display()
            );
        }
        self.home.endpoint().remove();
        remove_home(self.home.root());
    }
}

#[test]
fn a_collection_timeout_distinguishes_an_exited_child_from_an_open_stream() {
    struct GatedReader(mpsc::Receiver<()>);
    impl Read for GatedReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }

    let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
    command.arg("--version");
    let mut process = TestProcess::spawn(&mut command);
    let Process::Piped(child) = &mut process.child else {
        unreachable!();
    };
    let pid = child.id();
    child.wait().unwrap();
    let (release, gate) = mpsc::channel();
    process.readers = [Some(Box::new(GatedReader(gate))), None];
    let error = process.collect(Instant::now()).unwrap_err();
    drop(release);
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    let message = error.to_string();
    assert!(message.contains(&format!("pid={pid}")), "{message}");
    assert!(message.contains("--version"), "{message}");
    assert!(message.contains("status=Some("), "{message}");
    assert!(message.contains("stdout=pending stderr=eof"), "{message}");
}
