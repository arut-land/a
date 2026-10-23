//! Terminal modes, dimensions and input readiness behind one platform seam.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::Signal;

pub use imp::{InputReader, OutputGuard, is_foreground, output_interrupted, size, suspend};

/// A panic restores modes before the previous hook prints diagnostics, even
/// with a codegen backend that does not unwind through every frame.
pub struct RawMode(Arc<imp::RawMode>);

static RAW: std::sync::Mutex<Option<Arc<imp::RawMode>>> = std::sync::Mutex::new(None);

static LINE: std::sync::Mutex<Option<Arc<imp::LineMode>>> = std::sync::Mutex::new(None);

fn panic_hook() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}

/// Restore all terminal modes before an exit that bypasses Rust destructors.
pub fn restore() {
    if let Some(raw) = RAW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        let _ = raw.restore();
    }
    if let Some(line) = LINE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        let _ = line.restore();
    }
}

pub struct LineMode(Arc<imp::LineMode>);

impl LineMode {
    pub fn enter() -> io::Result<Self> {
        panic_hook();
        let mut held = LINE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.is_some() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        let line = Arc::new(imp::LineMode::enter()?);
        *held = Some(line.clone());
        Ok(Self(line))
    }
}

impl Drop for LineMode {
    fn drop(&mut self) {
        let mut held = LINE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.as_ref().is_some_and(|line| Arc::ptr_eq(line, &self.0)) {
            held.take();
        }
        let _ = self.0.restore();
    }
}

impl RawMode {
    pub fn enter() -> io::Result<Self> {
        panic_hook();
        let mut held = RAW
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.is_some() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        let raw = Arc::new(imp::RawMode::enter()?);
        *held = Some(raw.clone());
        Ok(Self(raw))
    }

    pub fn restore_input(&self) -> io::Result<()> {
        self.0.restore_input()
    }
    pub fn restore(&self) -> io::Result<()> {
        self.0.restore()
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let mut held = RAW
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.as_ref().is_some_and(|raw| Arc::ptr_eq(raw, &self.0)) {
            held.take();
        }
        let _ = self.0.restore();
    }
}

pub const JOB_CONTROL: bool = cfg!(unix);

pub const SUPPORTED: bool = cfg!(any(unix, windows));

pub enum Input {
    Signals(Vec<Signal>),
    Bytes(usize),
    Closed,
}

/// Pausing stdin never pauses signal delivery. The guard wakes the reader
/// when the renderer has reacquired the terminal, without an idle timer.
#[derive(Clone)]
pub struct InputControl {
    paused: Arc<AtomicBool>,
    wake: crate::signals::Wake,
}

impl InputControl {
    fn new(signals: &crate::Signals) -> Self {
        Self {
            paused: Arc::new(AtomicBool::new(false)),
            wake: signals.waker(),
        }
    }

    pub fn pause(&self) -> Option<ResumeInput> {
        if !JOB_CONTROL || self.paused.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(ResumeInput {
            control: self.clone(),
            _output: OutputInterrupt::acquire(),
        })
    }

    fn paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }
}

pub struct ResumeInput {
    control: InputControl,
    _output: OutputInterrupt,
}

/// Hold this guard until the control event has been folded by the renderer.
#[must_use]
pub struct OutputInterrupt(());

impl OutputInterrupt {
    pub fn acquire() -> Self {
        imp::interrupt_output();
        Self(())
    }
}

impl Drop for OutputInterrupt {
    fn drop(&mut self) {
        imp::release_output();
    }
}

impl Drop for ResumeInput {
    fn drop(&mut self) {
        self.control.paused.store(false, Ordering::Release);
        self.control.wake.wake();
    }
}

/// Unbuffered output keeps a frame in one write even when it holds newlines.
#[derive(Default)]
pub struct Stdout {
    restore_until: Option<Instant>,
}

impl Stdout {
    /// Cleanup gets one deadline, including partial writes, independently of
    /// a terminal emulator that has stopped reading.
    pub fn restoring() -> Self {
        Self {
            restore_until: Some(Instant::now() + Duration::from_millis(500)),
        }
    }
}

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        imp::write_stdout(buf, self.restore_until.is_none(), self.restore_until)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::fd::OwnedFd;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use rustix::event::{PollFd, PollFlags, poll};
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use rustix::io::Errno;
    use rustix::io::{FdFlags, fcntl_setfd};
    use rustix::termios::{OptionalActions, Termios, tcgetattr, tcgetwinsize, tcsetattr};

    use super::{Input, InputControl};
    use crate::Signals;

    /// Cooked Unix input already belongs to the terminal driver's job control.
    pub struct LineMode;

    #[allow(clippy::unnecessary_wraps, clippy::unused_self)] // Shared backend contract.
    impl LineMode {
        pub fn enter() -> io::Result<Self> {
            Ok(Self)
        }
        pub fn restore(&self) -> io::Result<()> {
            Ok(())
        }
    }

    pub struct RawMode {
        cooked: Termios,
        output_flags: OFlags,
    }

    struct OutputWake {
        read: OwnedFd,
        write: OwnedFd,
    }
    // A renderer may still be waiting when a termination thread restores
    // modes; process lifetime prevents descriptor reuse under that wait.
    static OUTPUT_WAKE: OnceLock<OutputWake> = OnceLock::new();
    static INTERRUPTED: AtomicUsize = AtomicUsize::new(0);

    fn output_wake() -> io::Result<&'static OutputWake> {
        if OUTPUT_WAKE.get().is_none() {
            let (read, write) = rustix::pipe::pipe()?;
            for fd in [&read, &write] {
                fcntl_setfd(fd, FdFlags::CLOEXEC)?;
                fcntl_setfl(fd, fcntl_getfl(fd)? | OFlags::NONBLOCK)?;
            }
            let _ = OUTPUT_WAKE.set(OutputWake { read, write });
        }
        Ok(OUTPUT_WAKE.get().unwrap())
    }

    pub(super) fn interrupt_output() {
        if INTERRUPTED.fetch_add(1, Ordering::AcqRel) == 0 {
            wake_output();
        }
    }

    fn wake_output() {
        if let Some(wake) = OUTPUT_WAKE.get() {
            let _ = rustix::io::write(&wake.write, &[1]);
        }
    }

    pub fn output_interrupted() -> bool {
        INTERRUPTED.load(Ordering::Acquire) != 0
    }

    fn drain_output_wake() {
        if let Some(wake) = OUTPUT_WAKE.get() {
            let mut buf = [0; 1024];
            while matches!(rustix::io::read(&wake.read, &mut buf), Ok(1..)) {}
        }
    }

    pub(super) fn release_output() {
        if INTERRUPTED.fetch_sub(1, Ordering::AcqRel) == 1 {
            drain_output_wake();
            // A new event may have arrived while its wake byte was drained.
            if output_interrupted() {
                wake_output();
            }
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    impl RawMode {
        pub fn enter() -> io::Result<Self> {
            foreground()?;
            output_wake()?;
            let stdin = rustix::stdio::stdin();
            let output_flags = fcntl_getfl(rustix::stdio::stdout())?;
            let cooked = tcgetattr(stdin)?;
            let mut raw = cooked.clone();
            raw.make_raw();
            let saved = Self {
                cooked,
                output_flags,
            };
            tcsetattr(stdin, OptionalActions::Now, &raw)?;
            fcntl_setfl(rustix::stdio::stdout(), output_flags | OFlags::NONBLOCK)?;
            Ok(saved)
        }

        pub fn restore_input(&self) -> io::Result<()> {
            let _output = OutputGuard::enter()?;
            Ok(tcsetattr(
                rustix::stdio::stdin(),
                OptionalActions::Now,
                &self.cooked,
            )?)
        }

        pub fn restore(&self) -> io::Result<()> {
            let mode = self.restore_input();
            let flags = fcntl_setfl(rustix::stdio::stdout(), self.output_flags);
            mode?;
            Ok(flags?)
        }
    }

    /// Cleanup output must be possible after the shell takes the foreground.
    /// Normal writes let the kernel report background output and recheck
    /// ownership after an interrupted call.
    pub struct OutputGuard {
        mask: libc::sigset_t,
        thread: std::marker::PhantomData<std::rc::Rc<()>>,
    }

    impl OutputGuard {
        pub fn enter() -> io::Result<Self> {
            // SAFETY: writable, initialized signal sets; mask is thread-local.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                let mut old: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&raw mut set);
                libc::sigaddset(&raw mut set, libc::SIGTTOU);
                let error = libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, &raw mut old);
                if error != 0 {
                    return Err(io::Error::from_raw_os_error(error));
                }
                Ok(Self {
                    mask: old,
                    thread: std::marker::PhantomData,
                })
            }
        }
    }

    impl Drop for OutputGuard {
        fn drop(&mut self) {
            // SAFETY: restores this thread's saved mask without changing peers.
            unsafe {
                libc::pthread_sigmask(
                    libc::SIG_SETMASK,
                    &raw const self.mask,
                    std::ptr::null_mut(),
                );
            }
        }
    }

    pub fn is_foreground() -> io::Result<bool> {
        match rustix::termios::tcgetpgrp(rustix::stdio::stdin()) {
            Ok(group) => Ok(group == rustix::process::getpgrp()),
            // A tty supplied without a controlling session has no job owner.
            Err(Errno::NOTTY) => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    // A background resume stops again until the shell assigns the terminal;
    // never take it from the shell with tcsetpgrp.
    fn foreground() -> io::Result<()> {
        while !is_foreground()? {
            stop()?;
        }
        Ok(())
    }

    fn stop() -> io::Result<()> {
        // A process-directed stop may land on another thread: the caller
        // can otherwise re-enter raw mode before the group stop completes.
        // SAFETY: raise directs the uncatchable stop to this thread, and
        // returns only after it has been continued.
        if unsafe { libc::raise(libc::SIGSTOP) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn suspend() -> io::Result<()> {
        stop()?;
        foreground()
    }

    pub fn size() -> io::Result<(u16, u16)> {
        let size = tcgetwinsize(rustix::stdio::stdout())?;
        Ok((size.ws_col, size.ws_row))
    }

    pub fn write_stdout(
        buf: &[u8],
        interruptible: bool,
        deadline: Option<Instant>,
    ) -> io::Result<usize> {
        let stdout = rustix::stdio::stdout();
        loop {
            if interruptible && (output_interrupted() || !is_foreground()?) {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let output = if interruptible {
                None
            } else {
                Some(OutputGuard::enter()?)
            };
            match rustix::io::write(stdout, buf) {
                Ok(n) => return Ok(n),
                Err(Errno::INTR) => continue,
                Err(Errno::AGAIN) => {}
                Err(error) => return Err(error.into()),
            }
            drop(output);
            let Some(wake) = OUTPUT_WAKE.get() else {
                return Err(io::ErrorKind::WouldBlock.into());
            };
            let mut fds = [
                PollFd::new(&stdout, PollFlags::OUT),
                PollFd::new(&wake.read, PollFlags::IN),
            ];
            let timeout = if let Some(deadline) = deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                Some(rustix::event::Timespec {
                    tv_sec: remaining.as_secs().try_into().unwrap_or(i64::MAX),
                    tv_nsec: remaining.subsec_nanos().into(),
                })
            } else {
                None
            };
            let polled = if interruptible {
                &mut fds[..]
            } else {
                &mut fds[..1]
            };
            match poll(polled, timeout.as_ref()) {
                Ok(0) => return Err(io::ErrorKind::TimedOut.into()),
                Ok(_) | Err(Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
            if interruptible && !fds[1].revents().is_empty() {
                drain_output_wake();
            }
        }
    }

    pub struct InputReader {
        signals: Signals,
        control: InputControl,
        open: bool,
    }

    impl InputReader {
        pub fn new(signals: Signals) -> Self {
            Self {
                control: InputControl::new(&signals),
                signals,
                open: true,
            }
        }

        pub fn control(&self) -> InputControl {
            self.control.clone()
        }

        /// A disconnected renderer still needs termination signals handled.
        pub fn close_input(&mut self) {
            self.open = false;
        }

        pub fn read(&mut self, buf: &mut [u8]) -> io::Result<Input> {
            let stdin = rustix::stdio::stdin();
            loop {
                let mut fds = [
                    PollFd::new(&self.signals, PollFlags::IN),
                    PollFd::new(&stdin, PollFlags::IN),
                ];
                let polled = if self.open && !self.control.paused() {
                    &mut fds[..]
                } else {
                    &mut fds[..1]
                };
                match poll(polled, None) {
                    Ok(_) => {}
                    Err(Errno::INTR) => continue,
                    Err(e) => return Err(e.into()),
                }
                if !fds[0].revents().is_empty() {
                    return Ok(Input::Signals(self.signals.take()));
                }
                if self.open && !self.control.paused() && !fds[1].revents().is_empty() {
                    if let Ok(group) = rustix::termios::tcgetpgrp(stdin)
                        && group != rustix::process::getpgrp()
                    {
                        return Ok(Input::Signals(vec![crate::Signal::BackgroundRead]));
                    }
                    match rustix::io::read(stdin, &mut *buf) {
                        Ok(n @ 1..) => return Ok(Input::Bytes(n)),
                        Err(Errno::INTR | Errno::AGAIN) => {}
                        Ok(0) | Err(_) => {
                            self.open = false;
                            return Ok(Input::Closed);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
#[path = "terminal/windows.rs"]
mod imp;

#[cfg(all(test, unix, not(miri)))]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::fd::FromRawFd;
    use std::process::{Command, Stdio};

    fn assert_modes(before: &rustix::termios::Termios, after: &rustix::termios::Termios) {
        use rustix::termios::SpecialCodeIndex as C;
        assert_eq!(after.input_modes, before.input_modes);
        assert_eq!(after.output_modes, before.output_modes);
        assert_eq!(after.control_modes, before.control_modes);
        // BSD sets PENDIN when cooked processing must reconsider queued input.
        let pending = rustix::termios::LocalModes::PENDIN;
        assert_eq!(after.local_modes & !pending, before.local_modes & !pending);
        for code in [
            C::VMIN,
            C::VTIME,
            C::VINTR,
            C::VQUIT,
            C::VSUSP,
            C::VEOF,
            C::VERASE,
            C::VKILL,
        ] {
            assert_eq!(after.special_codes[code], before.special_codes[code]);
        }
        assert_eq!(after.input_speed(), before.input_speed());
        assert_eq!(after.output_speed(), before.output_speed());
    }

    #[test]
    #[allow(clippy::cast_lossless)] // ioctl request types differ between Unix targets.
    fn the_terminal_is_restored_after_a_panic() {
        const NAME: &str = "terminal::tests::the_terminal_is_restored_after_a_panic";
        if std::env::var_os("A_TEST_TTY").is_some() {
            // SAFETY: this dedicated child becomes a session leader and owns
            // its supplied tty; no parent process terminal state is changed.
            unsafe {
                assert_ne!(libc::setsid(), -1);
                assert_ne!(libc::ioctl(0, libc::TIOCSCTTY as _, 0), -1);
            }
            let before = rustix::termios::tcgetattr(rustix::stdio::stdin()).unwrap();
            let output_flags = rustix::fs::fcntl_getfl(rustix::stdio::stdout()).unwrap();
            let _raw = RawMode::enter().unwrap();
            let raw = rustix::termios::tcgetattr(rustix::stdio::stdin()).unwrap();
            assert!(
                !raw.local_modes
                    .contains(rustix::termios::LocalModes::ICANON)
            );
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                previous(info);
                assert_modes(
                    &before,
                    &rustix::termios::tcgetattr(rustix::stdio::stdin()).unwrap(),
                );
                assert_eq!(
                    rustix::fs::fcntl_getfl(rustix::stdio::stdout()).unwrap(),
                    output_flags
                );
                eprintln!("terminal modes restored before process exit");
            }));
            println!("raw input acquired before panic");
            panic!("unwind the owned terminal");
        }

        let mut master = -1;
        let mut slave = -1;
        // SAFETY: openpty initializes these two descriptors; optional name,
        // termios and window size buffers are omitted.
        let opened = unsafe {
            libc::openpty(
                &raw mut master,
                &raw mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0);
        // SAFETY: each successful openpty descriptor is transferred once.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        // Neither descriptor may leak into the test child outside its stdio.
        for file in [&master, &slave] {
            rustix::io::fcntl_setfd(file, rustix::io::FdFlags::CLOEXEC).unwrap();
        }
        let before = rustix::termios::tcgetattr(&slave).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env("A_TEST_TTY", "1")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("raw input acquired before panic")
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("terminal modes restored before process exit")
        );
        assert_modes(&before, &rustix::termios::tcgetattr(&master).unwrap());
    }

    #[test]
    #[allow(clippy::cast_lossless)] // ioctl request types differ between Unix targets.
    fn a_background_continue_never_takes_the_terminal_from_the_shell() {
        const NAME: &str =
            "terminal::tests::a_background_continue_never_takes_the_terminal_from_the_shell";
        match std::env::var("A_TEST_JOB").as_deref() {
            Ok("worker") => {
                // SAFETY: this child creates its own job in the test session.
                assert_eq!(unsafe { libc::setpgid(0, 0) }, 0);
                let signals = crate::Signals::catch(&[Signal::Suspend]).unwrap();
                let raw = RawMode::enter().unwrap();
                println!("raw input acquired");
                assert_eq!(signals.wait(), [Signal::Suspend]);
                drop(raw);
                eprintln!("worker restored raw lease before stopping");
                suspend().unwrap();
                let _raw = RawMode::enter().unwrap();
                println!("raw input reacquired");
            }
            Ok("shell") => {
                // SAFETY: this subprocess owns the supplied controlling tty.
                unsafe {
                    assert_ne!(libc::setsid(), -1);
                    assert_ne!(libc::ioctl(0, libc::TIOCSCTTY as _, 0), -1);
                }
                shell_job_control(NAME);
            }
            _ => {
                let mut master = -1;
                let mut slave = -1;
                // SAFETY: writable descriptor buffers; optional buffers omitted.
                let result = unsafe {
                    libc::openpty(
                        &raw mut master,
                        &raw mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                assert_eq!(result, 0);
                // SAFETY: openpty descriptors transferred exactly once.
                let (master, slave) =
                    unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
                for file in [&master, &slave] {
                    rustix::io::fcntl_setfd(file, rustix::io::FdFlags::CLOEXEC).unwrap();
                }
                let output = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", NAME, "--nocapture"])
                    .env("A_TEST_JOB", "shell")
                    .stderr(Stdio::inherit())
                    .stdin(Stdio::from(slave))
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }

    fn shell_job_control(name: &str) {
        use rustix::process::{Pid, Signal, kill_process};
        use std::io::{BufRead, BufReader};
        use std::os::fd::AsFd;
        struct Job(std::process::Child);
        impl Drop for Job {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let stdin = io::stdin();
        let before = rustix::termios::tcgetattr(&stdin).unwrap();
        let mut child = Job(Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env("A_TEST_JOB", "worker")
            .stdin(Stdio::from(stdin.as_fd().try_clone_to_owned().unwrap()))
            .stdout(Stdio::piped())
            .spawn()
            .unwrap());
        let pid = Pid::from_child(&child.0);
        // Background startup must stop before modifying termios.
        wait_stopped(pid);
        eprintln!("startup stopped before acquiring terminal");
        assert_modes(&before, &rustix::termios::tcgetattr(&stdin).unwrap());
        rustix::termios::tcsetpgrp(&stdin, pid).unwrap();
        kill_process(pid, Signal::CONT).unwrap();
        let stdout = child.0.stdout.take().unwrap();
        let (send, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if send.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        wait_marker(&lines, "raw input acquired");
        kill_process(pid, Signal::TSTP).unwrap();
        wait_stopped(pid);
        eprintln!("worker suspended after restoring terminal");
        assert_modes(&before, &rustix::termios::tcgetattr(&stdin).unwrap());
        let _output = OutputGuard::enter().unwrap();
        let shell = rustix::process::getpgrp();
        rustix::termios::tcsetpgrp(&stdin, shell).unwrap();
        kill_process(pid, Signal::CONT).unwrap();
        wait_stopped(pid);
        eprintln!("background continue stopped without acquiring terminal");
        assert_eq!(rustix::termios::tcgetpgrp(&stdin).unwrap(), shell);
        assert_modes(&before, &rustix::termios::tcgetattr(&stdin).unwrap());
        rustix::termios::tcsetpgrp(&stdin, pid).unwrap();
        kill_process(pid, Signal::CONT).unwrap();
        wait_marker(&lines, "raw input reacquired");
        assert!(child.0.wait().unwrap().success());
        eprintln!("foreground worker exited");
        assert_modes(&before, &rustix::termios::tcgetattr(&stdin).unwrap());
    }

    fn wait_stopped(pid: rustix::process::Pid) {
        use rustix::process::{WaitOptions, waitpid};
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some((_, state)) =
                waitpid(Some(pid), WaitOptions::UNTRACED | WaitOptions::NOHANG).unwrap()
            {
                assert!(
                    state.stopped(),
                    "the job exited instead of stopping: {state:?}"
                );
                return;
            }
            assert!(Instant::now() < deadline, "the job did not stop");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_marker(lines: &std::sync::mpsc::Receiver<String>, marker: &str) {
        loop {
            let line = lines
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|error| panic!("waiting for {marker}: {error}"));
            if line.contains(marker) {
                return;
            }
        }
    }
}
