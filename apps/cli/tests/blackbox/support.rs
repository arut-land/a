//! Portable process collection and isolated daemon homes. PTYs keep their own harness.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use sys::Home;

pub fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

pub fn isolated_root(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "a-acc-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
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
        }
    }
}

impl TestProcess {
    pub fn spawn(command: &mut Command) -> Self {
        Self::from(
            command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        )
    }

    pub fn group(&self) -> &sys::OwnedGroup {
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
        }
    }

    pub fn collect(mut self, deadline: Instant) -> io::Result<Output> {
        let (tx, rx) = mpsc::channel();
        // Both readers start before waiting; either pipe can fill independently.
        let mut streams = [Some(Vec::new()), Some(Vec::new())];
        for (index, reader) in std::mem::take(&mut self.readers).into_iter().enumerate() {
            let Some(mut reader) = reader else { continue };
            streams[index] = None;
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = reader.read_to_end(&mut bytes);
                let _ = tx.send((index, result.map(|_| bytes)));
            });
        }
        drop(tx);
        loop {
            while let Ok((index, result)) = rx.try_recv() {
                streams[index] = Some(result?);
            }
            if let Some(status) = self.child.try_wait()?
                && streams.iter().all(Option::is_some)
            {
                let [stdout, stderr] = streams;
                return Ok(Output {
                    status,
                    stdout: stdout.unwrap(),
                    stderr: stderr.unwrap(),
                });
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "test process collection deadline",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
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
        let _ = TestProcess::spawn(&mut self.command(&["daemon", "stop"])).collect(deadline());
        self.home.endpoint().remove();
        remove_home(self.home.root());
    }
}
