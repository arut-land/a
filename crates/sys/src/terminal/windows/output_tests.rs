use super::*;
use std::io::Write;
use std::os::windows::io::IntoRawHandle;
use std::os::windows::process::{CommandExt, ProcThreadAttributeList};
use std::process::{Command, Stdio};

use windows_sys::Win32::System::Console::{
    COORD, CTRL_C_EVENT, ClosePseudoConsole, CreatePseudoConsole, GenerateConsoleCtrlEvent,
    GetConsoleCP, GetConsoleMode, GetConsoleOutputCP, HPCON, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleCtrlHandler, SetStdHandle,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    GetThreadIOPendingFlag, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, WaitForSingleObject,
};

#[test]
fn stalled_console_output_is_cancelled_and_panic_cleanup_has_a_deadline() {
    const NAME: &str = "terminal::imp::output::tests::stalled_console_output_is_cancelled_and_panic_cleanup_has_a_deadline";
    if std::env::var_os("A_TEST_STALLED_OUTPUT").is_none() {
        run_in_stalled_conpty(NAME);
        return;
    }
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
    let before = console_state();
    let _raw = crate::terminal::RawMode::enter().unwrap();
    let signals = crate::Signals::catch(&[crate::Signal::Interrupt]).unwrap();
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
    loop {
        let mut pending = 0;
        // SAFETY: the dedicated writer owns this live, queryable thread handle.
        ok(unsafe {
            GetThreadIOPendingFlag(
                OUTPUT.get().unwrap().worker.as_raw_handle(),
                &raw mut pending,
            )
        })
        .unwrap();
        if pending != 0 {
            break;
        }
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
    eprintln!("native console write is pending with its output pipe unread");
    // SAFETY: this pseudoconsole contains only the dedicated test subprocess.
    assert_ne!(unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) }, 0);
    // SAFETY: the caught signal owns its completion event during the wait.
    let ready = unsafe { WaitForSingleObject(signals.as_handle().as_raw_handle(), 5000) };
    assert_eq!(ready, 0);
    assert_eq!(signals.take(), [crate::Signal::Interrupt]);
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

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let started = Instant::now();
        let mut cleanup = crate::terminal::Stdout::restoring();
        let error = cleanup.write_all(&vec![b' '; 1024 * 1024]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(cleanup);
        previous(info);
        assert_eq!(
            console_state(),
            before,
            "panic did not restore native console state"
        );
        eprintln!("panic cleanup reached its deadline and restored modes and code pages");
    }));
    assert!(std::panic::catch_unwind(|| panic!("restore a stalled terminal")).is_err());
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

fn run_in_stalled_conpty(name: &str) {
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
    // Closing ConPTY must have an output reader. Start it only after the child
    // has completed or timed out, preserving actual backpressure during tests.
    let drain = thread::spawn(move || {
        let mut output = std::fs::File::from(output_read);
        let _ = io::copy(&mut output, &mut io::sink());
    });
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
