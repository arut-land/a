//! Isolated native-console fixtures for terminal consumers.

/// Runs a test in an owned native console when the platform has console records.
/// Other platforms do not execute the callback.
pub fn with_console(name: &str, check: impl FnOnce()) {
    #[cfg(windows)]
    native::with_console(name, check);
    #[cfg(not(windows))]
    let _ = (name, check);
}

/// Replaces the owned console's input queue with UTF-16 key records and a resize.
/// Must be called only inside `with_console`.
pub fn input(text: &str) {
    #[cfg(windows)]
    native::input(text);
    #[cfg(not(windows))]
    let _ = text;
}

#[cfg(windows)]
mod native {
    use std::io;
    use std::os::windows::io::{AsHandle, AsRawHandle, IntoRawHandle};
    use std::process::{Command, Stdio};
    use windows_sys::Win32::System::Console::{
        AllocConsole, FlushConsoleInputBuffer, FreeConsole, INPUT_RECORD, INPUT_RECORD_0,
        KEY_EVENT, KEY_EVENT_RECORD, KEY_EVENT_RECORD_0, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetConsoleCtrlHandler, SetStdHandle, WINDOW_BUFFER_SIZE_EVENT,
        WriteConsoleInputW,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    pub(super) fn with_console(name: &str, check: impl FnOnce()) {
        if std::env::var_os("A_TEST_CONSOLE").is_none() {
            run_console_test(name);
            return;
        }
        let stderr = io::stderr().as_handle().try_clone_to_owned().unwrap();
        // This subprocess owns a fresh console; no developer's terminal or
        // other libtest worker can be affected by mode/control-event changes.
        // SAFETY: process-local console attachment; no pointer arguments.
        unsafe {
            FreeConsole();
            assert_ne!(AllocConsole(), 0);
        }
        // STARTF_USESTDHANDLES preserves libtest's redirected handles during
        // AllocConsole. Give the process table explicit console handles;
        // Windows owns them until this isolated subprocess exits.
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
        // SAFETY: these live handles are transferred to this subprocess's
        // standard-handle table and are never closed before process exit.
        unsafe {
            assert_ne!(SetStdHandle(STD_ERROR_HANDLE, stderr.as_raw_handle()), 0);
            assert_ne!(SetConsoleCtrlHandler(None, 0), 0);
            assert_ne!(SetStdHandle(STD_INPUT_HANDLE, input.into_raw_handle()), 0);
            assert_ne!(SetStdHandle(STD_OUTPUT_HANDLE, output.into_raw_handle()), 0);
        }
        eprintln!("private console allocated");
        check();
    }

    fn run_console_test(name: &str) {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("A_TEST_CONSOLE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        // SAFETY: the child owns this process handle throughout the bounded wait.
        let ready = unsafe { WaitForSingleObject(child.as_handle().as_raw_handle(), 20_000) };
        if ready != 0 {
            let _ = child.kill();
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(ready, 0, "private console subprocess timed out");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    pub(super) fn input(text: &str) {
        let mut records = Vec::new();
        for unit in text.encode_utf16() {
            records.push(INPUT_RECORD {
                EventType: u16::try_from(KEY_EVENT).unwrap(),
                Event: INPUT_RECORD_0 {
                    KeyEvent: KEY_EVENT_RECORD {
                        bKeyDown: 1,
                        wRepeatCount: 1,
                        uChar: KEY_EVENT_RECORD_0 { UnicodeChar: unit },
                        ..Default::default()
                    },
                },
            });
            // Interleave a native resize without splitting the VT sequence.
            if unit == 0x1b {
                records.push(INPUT_RECORD {
                    EventType: u16::try_from(WINDOW_BUFFER_SIZE_EVENT).unwrap(),
                    ..Default::default()
                });
            }
        }
        // Non-character records beyond the native read chunk must not leave a
        // lone Escape waiting for continuation bytes that will never arrive.
        records.extend(std::iter::repeat_n(
            INPUT_RECORD {
                EventType: u16::try_from(KEY_EVENT).unwrap(),
                ..Default::default()
            },
            256,
        ));
        let mut count = 0;
        // SAFETY: the fixture owns this console; records/count are valid.
        unsafe {
            let input = io::stdin().as_handle().as_raw_handle();
            assert_ne!(FlushConsoleInputBuffer(input), 0);
            assert_ne!(
                WriteConsoleInputW(
                    input,
                    records.as_ptr(),
                    u32::try_from(records.len()).unwrap(),
                    &raw mut count
                ),
                0
            );
        }
        assert_eq!(count as usize, records.len());
    }
}
