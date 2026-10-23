use super::*;
use std::io::Write;
use std::os::windows::io::IntoRawHandle;
use std::os::windows::process::{CommandExt, ProcThreadAttributeList};
use std::process::{Command, Stdio};

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Console::{
    COORD, CTRL_C_EVENT, ClosePseudoConsole, CreatePseudoConsole, ENABLE_LINE_INPUT,
    GenerateConsoleCtrlEvent, GetConsoleCP, GetConsoleMode, GetConsoleOutputCP, HPCON,
    INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT, KEY_EVENT_RECORD, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleCP, SetConsoleCtrlHandler, SetConsoleMode, SetConsoleOutputCP, SetStdHandle,
    WriteConsoleInputW,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    GetThreadIOPendingFlag, GetThreadTimes, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
    WaitForSingleObject,
};

#[test]
fn the_console_output_worker_does_not_wake_while_idle() {
    const NAME: &str =
        "terminal::imp::output::tests::the_console_output_worker_does_not_wake_while_idle";
    if std::env::var_os("A_TEST_STALLED_OUTPUT").is_none() {
        run_in_console(NAME);
        return;
    }
    attach_standard_handles();
    let _raw = crate::terminal::RawMode::enter().unwrap();
    // Completing one operation certifies that thread initialization is over.
    assert_eq!(crate::terminal::Stdout::default().write(b"").unwrap(), 0);
    let before = writer_cpu();
    thread::sleep(Duration::from_millis(250));
    assert_eq!(writer_cpu(), before, "the idle output worker consumed CPU");
}

fn writer_cpu() -> u64 {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: the owner keeps its thread handle open; each FILETIME is writable.
    ok(unsafe {
        GetThreadTimes(
            OUTPUT.get().unwrap().worker.as_raw_handle(),
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    })
    .unwrap();
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    ticks(kernel) + ticks(user)
}

#[test]
fn stalled_console_output_is_cancelled_and_panic_cleanup_has_a_deadline() {
    const NAME: &str = "terminal::imp::output::tests::stalled_console_output_is_cancelled_and_panic_cleanup_has_a_deadline";
    if std::env::var_os("A_TEST_STALLED_OUTPUT").is_none() {
        run_in_console(NAME);
        return;
    }
    attach_standard_handles();
    // SAFETY: this private pseudoconsole must actually change code pages on
    // acquisition, so a missing restoration cannot pass with UTF-8 defaults.
    unsafe {
        assert_ne!(SetConsoleCP(437), 0);
        assert_ne!(SetConsoleOutputCP(437), 0);
    }
    let before = console_state();
    let _raw = crate::terminal::RawMode::enter().unwrap();
    eprintln!("console lease acquired");
    let signals = crate::Signals::catch(&[crate::Signal::Interrupt]).unwrap();
    pause_console_output();
    let (done, result) = mpsc::channel();
    let renderer = thread::spawn(move || {
        let mut out = crate::terminal::Stdout::default();
        let bytes = vec![b'x'; 64 * 1024];
        loop {
            if let Err(error) = out.write_all(&bytes) {
                done.send(error.kind()).unwrap();
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !writer_pending() {
        assert!(
            Instant::now() < deadline,
            "console write never became pending"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        matches!(
            result.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "writer failed before cancellation"
    );
    eprintln!("native console write is pending under output flow control");
    // SAFETY: this pseudoconsole contains only the dedicated test subprocess.
    assert_ne!(unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) }, 0);
    // SAFETY: the caught signal owns its completion event during the wait.
    let ready = unsafe { WaitForSingleObject(signals.as_handle().as_raw_handle(), 5000) };
    assert_eq!(ready, 0);
    assert_eq!(signals.take(), [crate::Signal::Interrupt]);
    eprintln!("native Ctrl-C delivered");
    let interrupt = crate::terminal::OutputInterrupt::acquire();
    assert_eq!(
        result.recv_timeout(Duration::from_secs(2)).unwrap(),
        io::ErrorKind::WouldBlock
    );
    renderer.join().unwrap();
    assert!(
        !OUTPUT.get().unwrap().failed.load(Ordering::Acquire),
        "console did not acknowledge cancellation"
    );
    drop(interrupt);
    eprintln!("Ctrl-C unblocked the renderer");
    pause_console_output();

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("panic cleanup started");
        let started = Instant::now();
        let mut cleanup = crate::terminal::Stdout::restoring();
        let error = cleanup
            .write_all(b"\x1b[0m\x1b[?1049l\x1b[?25h")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(cleanup);
        eprintln!("screen cleanup completed within its deadline");
        previous(info);
        assert_eq!(
            console_state(),
            before,
            "panic did not restore native console state"
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        while writer_pending() {
            assert!(
                Instant::now() < deadline,
                "cleanup left its native write pending"
            );
            thread::sleep(Duration::from_millis(1));
        }
        eprintln!("panic cleanup reached its deadline and restored modes and code pages");
    }));
    assert!(std::panic::catch_unwind(|| panic!("restore a stalled terminal")).is_err());
    // libtest's final status uses ordinary stdout, outside the bounded writer;
    // it must not print into the deliberately paused console.
    std::process::exit(0);
}

fn writer_pending() -> bool {
    let mut pending = 0;
    // SAFETY: the dedicated writer owns this live, queryable thread handle.
    ok(unsafe {
        GetThreadIOPendingFlag(
            OUTPUT.get().unwrap().worker.as_raw_handle(),
            &raw mut pending,
        )
    })
    .unwrap();
    pending != 0
}

fn pause_console_output() {
    let input = io::stdin();
    let mut mode = 0;
    let key = |code| INPUT_RECORD {
        EventType: u16::try_from(KEY_EVENT).unwrap(),
        Event: INPUT_RECORD_0 {
            KeyEvent: KEY_EVENT_RECORD {
                bKeyDown: 1,
                wRepeatCount: 1,
                wVirtualKeyCode: code,
                ..Default::default()
            },
        },
    };
    // An ordinary key releases any prior pause before VK_PAUSE sets it again.
    let records = [key(u16::from(b'A')), key(0x13)];
    let mut count = 0;
    // Pause is intercepted in line mode. Restoring raw input afterward leaves
    // output suspended while keeping the host's API/control service available.
    // SAFETY: this child owns its console, record and writable mode/count.
    unsafe {
        assert_ne!(
            GetConsoleMode(input.as_handle().as_raw_handle(), &raw mut mode),
            0
        );
        assert_ne!(
            SetConsoleMode(input.as_handle().as_raw_handle(), mode | ENABLE_LINE_INPUT),
            0
        );
        assert_ne!(
            WriteConsoleInputW(
                input.as_handle().as_raw_handle(),
                records.as_ptr(),
                2,
                &raw mut count
            ),
            0
        );
        assert_ne!(SetConsoleMode(input.as_handle().as_raw_handle(), mode), 0);
    }
}

fn attach_standard_handles() {
    // ConPTY attaches the console, while libtest's standard handles may still
    // be redirected. Keep diagnostics on the inherited stderr pipe.
    let input = std::fs::File::options()
        .read(true)
        .write(true)
        .open("CONIN$")
        .unwrap();
    let output = std::fs::File::options()
        .read(true)
        .write(true)
        .open("CONOUT$")
        .unwrap();
    // SAFETY: this isolated subprocess transfers both handles to its standard
    // table until process exit, and clears only its inherited Ctrl-C ignore.
    unsafe {
        assert_ne!(SetStdHandle(STD_INPUT_HANDLE, input.into_raw_handle()), 0);
        assert_ne!(SetStdHandle(STD_OUTPUT_HANDLE, output.into_raw_handle()), 0);
        assert_ne!(SetConsoleCtrlHandler(None, 0), 0);
    }
}

fn console_state() -> (u32, u32, u32, u32) {
    let mut input = 0;
    let mut output = 0;
    // SAFETY: the test owns both standard console handles and writable mode buffers.
    unsafe {
        assert_ne!(
            GetConsoleMode(io::stdin().as_handle().as_raw_handle(), &raw mut input),
            0
        );
        assert_ne!(
            GetConsoleMode(io::stdout().as_handle().as_raw_handle(), &raw mut output),
            0
        );
        (input, output, GetConsoleCP(), GetConsoleOutputCP())
    }
}

fn pipe() -> (OwnedHandle, OwnedHandle) {
    let mut read = std::ptr::null_mut();
    let mut write = std::ptr::null_mut();
    // SAFETY: writable handle outputs; no inheritance or custom security.
    ok(unsafe { CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), 4096) }).unwrap();
    (
        crate::windows::owned(read).unwrap(),
        crate::windows::owned(write).unwrap(),
    )
}

fn run_in_console(name: &str) {
    let (input_read, _input_write) = pipe();
    let (output_read, output_write) = pipe();
    let mut console: HPCON = 0;
    // SAFETY: ConPTY duplicates these pipe ends; this fixture owns the console.
    let created = unsafe {
        CreatePseudoConsole(
            COORD { X: 80, Y: 25 },
            input_read.as_raw_handle(),
            output_write.as_raw_handle(),
            0,
            &raw mut console,
        )
    };
    assert_eq!(created, 0);
    drop((input_read, output_write));
    // A ConPTY frontend must keep draining even while its client's writes
    // are paused; otherwise the host's API/control service can deadlock too.
    let drain = thread::spawn(move || {
        let mut output = std::fs::File::from(output_read);
        let _ = io::copy(&mut output, &mut io::sink());
    });
    // SAFETY: this attribute takes the HPCON value, rather than a pointer to it,
    // and the fixture keeps it alive until the child exits.
    let attributes = unsafe {
        ProcThreadAttributeList::build()
            .raw_attribute(
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                console as *const std::ffi::c_void,
                size_of::<HPCON>(),
            )
            .finish()
            .unwrap()
    };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("A_TEST_STALLED_OUTPUT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn_with_attributes(&attributes)
        .unwrap();
    // SAFETY: this child remains owned throughout the bounded process wait.
    let ready = unsafe { WaitForSingleObject(child.as_handle().as_raw_handle(), 20_000) };
    if ready != 0 {
        let _ = child.kill();
    }
    // SAFETY: releases only the pseudoconsole created by this fixture.
    unsafe { ClosePseudoConsole(console) };
    drain.join().unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        ready == 0 && output.status.success(),
        "stalled console fixture: ready={ready}, status={}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}
