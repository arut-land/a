//! Console handles, rather than environment variables, establish capabilities.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};

use windows_sys::Win32::System::Console::*;
use windows_sys::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};

use super::Input;
use crate::{Signal, Signals, windows::ok};

pub struct RawMode {
    input: OwnedHandle,
    output: OwnedHandle,
    input_mode: u32,
    output_mode: u32,
    input_cp: u32,
    output_cp: u32,
}

impl RawMode {
    pub fn enter() -> io::Result<Self> {
        let input = io::stdin().as_handle().try_clone_to_owned()?;
        let output = io::stdout().as_handle().try_clone_to_owned()?;
        let mut input_mode = 0;
        let mut output_mode = 0;
        // SAFETY: live owned handles and writable mode buffers.
        unsafe {
            ok(GetConsoleMode(input.as_raw_handle(), &raw mut input_mode))?;
            ok(GetConsoleMode(output.as_raw_handle(), &raw mut output_mode))?;
        }
        let saved = Self {
            input,
            output,
            input_mode,
            output_mode,
            // SAFETY: these queries take no pointers.
            input_cp: unsafe { GetConsoleCP() },
            output_cp: unsafe { GetConsoleOutputCP() },
        };
        // The guard rolls back even a partially successful setup. Keep
        // processed input so Ctrl-C and Ctrl-Break use the control handler.
        // QuickEdit would freeze output while the user selects text.
        unsafe {
            ok(SetConsoleMode(
                saved.output.as_raw_handle(),
                output_mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            ))?;
            ok(SetConsoleMode(
                saved.input.as_raw_handle(),
                (input_mode
                    | ENABLE_EXTENDED_FLAGS
                    | ENABLE_WINDOW_INPUT
                    | ENABLE_VIRTUAL_TERMINAL_INPUT
                    | ENABLE_PROCESSED_INPUT)
                    & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_QUICK_EDIT_MODE),
            ))?;
            ok(SetConsoleCP(65001))?;
            ok(SetConsoleOutputCP(65001))?;
        }
        Ok(saved)
    }

    pub fn restore_input(&self) -> io::Result<()> {
        // SAFETY: input is held open by this guard.
        ok(unsafe { SetConsoleMode(self.input.as_raw_handle(), self.input_mode) })
    }

    pub fn restore(&self) -> io::Result<()> {
        // Attempt every restoration even when a handle has been disconnected.
        // SAFETY: the saved handles remain owned by this guard.
        let results = unsafe {
            [
                SetConsoleMode(self.input.as_raw_handle(), self.input_mode),
                SetConsoleMode(self.output.as_raw_handle(), self.output_mode),
                SetConsoleCP(self.input_cp),
                SetConsoleOutputCP(self.output_cp),
            ]
        };
        if results.contains(&0) {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub fn suspend() -> io::Result<()> {
    Ok(())
}

pub fn size() -> io::Result<(u16, u16)> {
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    // SAFETY: stdout is borrowed for the call; info is writable.
    ok(unsafe {
        GetConsoleScreenBufferInfo(io::stdout().as_handle().as_raw_handle(), &raw mut info)
    })?;
    Ok((
        (info.srWindow.Right - info.srWindow.Left + 1) as u16,
        (info.srWindow.Bottom - info.srWindow.Top + 1) as u16,
    ))
}

pub fn write_stdout(buf: &[u8]) -> io::Result<usize> {
    io::stdout().write(buf)
}

pub struct InputReader {
    signals: Signals,
    open: bool,
    pending: VecDeque<u8>,
    high: Option<u16>,
}

impl InputReader {
    pub fn new(signals: Signals) -> Self {
        Self {
            signals,
            open: true,
            pending: VecDeque::new(),
            high: None,
        }
    }

    pub fn close_input(&mut self) {
        self.open = false;
    }

    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<Input> {
        loop {
            let signals = self.signals.take();
            if !signals.is_empty() {
                return Ok(Input::Signals(signals));
            }
            if !self.pending.is_empty() {
                let n = buf.len().min(self.pending.len());
                for byte in &mut buf[..n] {
                    *byte = self.pending.pop_front().unwrap();
                }
                return Ok(Input::Bytes(n));
            }
            let stdin = io::stdin();
            let handles = [
                self.signals.as_handle().as_raw_handle(),
                stdin.as_handle().as_raw_handle(),
            ];
            // SAFETY: both borrowed handles outlive the infinite, event-driven wait.
            let ready = unsafe {
                WaitForMultipleObjects(if self.open { 2 } else { 1 }, handles.as_ptr(), 0, INFINITE)
            };
            match ready {
                0 => continue,
                1 => {}
                _ => return Err(io::Error::last_os_error()),
            }
            let mut record = INPUT_RECORD::default();
            let mut count = 0;
            // Reading one available record never waits for another. Unlike
            // ReadFile, this retains native resize events alongside VT keys.
            // SAFETY: valid console handle and writable record/count.
            if unsafe { ReadConsoleInputW(handles[1], &raw mut record, 1, &raw mut count) } == 0 {
                self.open = false;
                return Ok(Input::Closed);
            }
            match u32::from(record.EventType) {
                WINDOW_BUFFER_SIZE_EVENT => return Ok(Input::Signals(vec![Signal::Resize])),
                KEY_EVENT => {
                    // SAFETY: EventType identifies the initialized union member.
                    let key = unsafe { record.Event.KeyEvent };
                    if key.bKeyDown == 0 {
                        continue;
                    }
                    // SAFETY: ReadConsoleInputW supplies the Unicode member.
                    let unit = unsafe { key.uChar.UnicodeChar };
                    if unit == 0 {
                        continue;
                    }
                    for _ in 0..key.wRepeatCount {
                        self.character(unit);
                    }
                }
                _ => {}
            }
        }
    }

    fn character(&mut self, unit: u16) {
        if (0xd800..=0xdbff).contains(&unit) {
            if self.high.replace(unit).is_some() {
                self.push(char::REPLACEMENT_CHARACTER);
            }
            return;
        }
        if let Some(high) = self.high.take() {
            if (0xdc00..=0xdfff).contains(&unit) {
                self.push(
                    char::from_u32(
                        0x10000 + ((u32::from(high) - 0xd800) << 10) + u32::from(unit) - 0xdc00,
                    )
                    .unwrap(),
                );
                return;
            }
            self.push(char::REPLACEMENT_CHARACTER);
        }
        self.push(char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER));
    }

    fn push(&mut self, ch: char) {
        self.pending.extend(ch.encode_utf8(&mut [0; 4]).as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn console_state_is_restored_after_a_panic_and_events_preserve_unicode_and_resize() {
        const NAME: &str = "terminal::imp::tests::console_state_is_restored_after_a_panic_and_events_preserve_unicode_and_resize";
        if std::env::var_os("A_TEST_CONSOLE").is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env("A_TEST_CONSOLE", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // This subprocess owns a fresh console; no developer's terminal or
        // other libtest worker can be affected by mode/control-event changes.
        // SAFETY: process-local console attachment; no pointer arguments.
        unsafe {
            FreeConsole();
            assert_ne!(AllocConsole(), 0);
        }
        let mut input_mode = 0;
        let mut output_mode = 0;
        // SAFETY: live standard handles from AllocConsole and writable modes.
        unsafe {
            assert_ne!(
                GetConsoleMode(io::stdin().as_handle().as_raw_handle(), &raw mut input_mode),
                0
            );
            assert_ne!(
                GetConsoleMode(
                    io::stdout().as_handle().as_raw_handle(),
                    &raw mut output_mode
                ),
                0
            );
        }
        // SAFETY: scalar queries for this test's console.
        let cp = unsafe { (GetConsoleCP(), GetConsoleOutputCP()) };
        let signals = Signals::catch(&[Signal::Interrupt, Signal::Terminate]).unwrap();
        let panic = std::panic::catch_unwind(|| {
            let _raw = RawMode::enter().unwrap();
            // SAFETY: scalar queries for the owned console.
            assert_eq!(
                unsafe { (GetConsoleCP(), GetConsoleOutputCP()) },
                (65001, 65001)
            );
            panic!("restore the console on unwind");
        });
        assert!(panic.is_err());
        let mut restored_input = 0;
        let mut restored_output = 0;
        // SAFETY: same handles and writable modes as above.
        unsafe {
            assert_ne!(
                GetConsoleMode(
                    io::stdin().as_handle().as_raw_handle(),
                    &raw mut restored_input
                ),
                0
            );
            assert_ne!(
                GetConsoleMode(
                    io::stdout().as_handle().as_raw_handle(),
                    &raw mut restored_output
                ),
                0
            );
            assert_eq!((GetConsoleCP(), GetConsoleOutputCP()), cp);
        }
        assert_eq!((restored_input, restored_output), (input_mode, output_mode));
        let _raw = RawMode::enter().unwrap();
        let mut reader = InputReader::new(signals);
        let mut records = vec![];
        for unit in "A🙂".encode_utf16() {
            records.push(INPUT_RECORD {
                EventType: KEY_EVENT as u16,
                Event: INPUT_RECORD_0 {
                    KeyEvent: KEY_EVENT_RECORD {
                        bKeyDown: 1,
                        wRepeatCount: 1,
                        uChar: KEY_EVENT_RECORD_0 { UnicodeChar: unit },
                        ..Default::default()
                    },
                },
            });
        }
        records.push(INPUT_RECORD {
            EventType: WINDOW_BUFFER_SIZE_EVENT as u16,
            ..Default::default()
        });
        let mut count = 0;
        // SAFETY: initialized records and writable count, console belongs to this child.
        unsafe {
            assert_ne!(
                FlushConsoleInputBuffer(io::stdin().as_handle().as_raw_handle()),
                0
            );
            assert_ne!(
                WriteConsoleInputW(
                    io::stdin().as_handle().as_raw_handle(),
                    records.as_ptr(),
                    records.len() as u32,
                    &raw mut count
                ),
                0
            );
        }
        let mut got = vec![];
        let mut buf = [0; 32];
        loop {
            match reader.read(&mut buf).unwrap() {
                Input::Bytes(n) => got.extend_from_slice(&buf[..n]),
                Input::Signals(signals) if signals == [Signal::Resize] => break,
                _ => panic!("unexpected console input"),
            }
        }
        assert_eq!(got, "A🙂".as_bytes());
        for (event, signal) in [
            (CTRL_C_EVENT, Signal::Interrupt),
            (CTRL_BREAK_EVENT, Signal::Terminate),
        ] {
            // SAFETY: the fresh console contains only this test subprocess.
            assert_ne!(unsafe { GenerateConsoleCtrlEvent(event, 0) }, 0);
            assert_eq!(reader.signals.wait(), [signal]);
        }
    }
}
