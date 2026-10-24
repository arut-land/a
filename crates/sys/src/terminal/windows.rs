//! Console handles, rather than environment variables, establish capabilities.

use std::collections::VecDeque;
use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};

use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT,
    CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT,
    ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT,
    ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    ENABLE_WINDOW_INPUT, GetConsoleCP, GetConsoleMode, GetConsoleOutputCP,
    GetConsoleScreenBufferInfo, GetNumberOfConsoleInputEvents, INPUT_RECORD, KEY_EVENT,
    ReadConsoleInputW, SetConsoleCP, SetConsoleCtrlHandler, SetConsoleMode, SetConsoleOutputCP,
    WINDOW_BUFFER_SIZE_EVENT,
};
use windows_sys::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};

use super::{Input, InputControl};
use crate::{Signal, Signals, windows::ok};

#[path = "windows/output.rs"]
mod output;
pub(super) use output::{interrupt_output, release_output};
pub use output::{output_interrupted, write_stdout};

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
        output::initialize()?;
        let saved = Self {
            input,
            output,
            input_mode,
            output_mode,
            // SAFETY: these queries take no pointers.
            input_cp: unsafe { GetConsoleCP() },
            // SAFETY: scalar query for the attached console.
            output_cp: unsafe { GetConsoleOutputCP() },
        };
        // The guard rolls back even a partially successful setup. Keep
        // processed input so Ctrl-C and Ctrl-Break use the control handler.
        // QuickEdit would freeze output while the user selects text.
        // SAFETY: both owned console handles remain live; all arguments are scalar.
        unsafe {
            ok(SetConsoleMode(
                saved.output.as_raw_handle(),
                output_mode
                    | ENABLE_PROCESSED_OUTPUT
                    | ENABLE_VIRTUAL_TERMINAL_PROCESSING
                    | DISABLE_NEWLINE_AUTO_RETURN,
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

#[allow(clippy::unnecessary_wraps)] // Shared job-control contract.
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
        (info.srWindow.Right - info.srWindow.Left + 1).cast_unsigned(),
        (info.srWindow.Bottom - info.srWindow.Top + 1).cast_unsigned(),
    ))
}

pub struct InputReader {
    signals: Signals,
    control: InputControl,
    open: bool,
    pending: VecDeque<u8>,
    high: Option<u16>,
    resized: bool,
}

impl InputReader {
    pub fn new(signals: Signals) -> Self {
        Self {
            control: InputControl::new(&signals),
            signals,
            open: true,
            pending: VecDeque::new(),
            high: None,
            resized: false,
        }
    }

    pub fn control(&self) -> InputControl {
        self.control.clone()
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
            // A console that can no longer be read is closed input, as at
            // EOF: the caller keeps receiving signals.
            if self.open && !self.control.paused() && self.drain().is_err() {
                self.open = false;
                return Ok(Input::Closed);
            }
            if std::mem::take(&mut self.resized) {
                return Ok(Input::Signals(vec![Signal::Resize]));
            }
            if !self.pending.is_empty() {
                let len = buf.len().min(self.pending.len());
                for byte in &mut buf[..len] {
                    *byte = self.pending.pop_front().unwrap();
                }
                let more = !self.pending.is_empty();
                return Ok(Input::Bytes { len, more });
            }
            let stdin = io::stdin();
            let handles = [
                self.signals.as_handle().as_raw_handle(),
                stdin.as_handle().as_raw_handle(),
            ];
            // SAFETY: both borrowed handles outlive the infinite, event-driven wait.
            let ready = unsafe {
                WaitForMultipleObjects(
                    if self.open && !self.control.paused() {
                        2
                    } else {
                        1
                    },
                    handles.as_ptr(),
                    0,
                    INFINITE,
                )
            };
            match ready {
                0..=1 => {}
                _ => return Err(io::Error::last_os_error()),
            }
        }
    }

    fn drain(&mut self) -> io::Result<()> {
        let mut available = 0;
        // SAFETY: a borrowed console handle and writable count. This reader
        // exclusively consumes console records, so draining cannot race a read.
        ok(unsafe {
            GetNumberOfConsoleInputEvents(
                io::stdin().as_handle().as_raw_handle(),
                &raw mut available,
            )
        })?;
        while available != 0 {
            let mut records = [INPUT_RECORD::default(); 128];
            let mut count = 0;
            // SAFETY: initialized writable records/count and a console handle.
            // Only already available records are requested, so this never waits
            // for the suffix of an escape sequence to arrive.
            ok(unsafe {
                ReadConsoleInputW(
                    io::stdin().as_handle().as_raw_handle(),
                    records.as_mut_ptr(),
                    available.min(128),
                    &raw mut count,
                )
            })?;
            available -= count;
            for record in &records[..count as usize] {
                match u32::from(record.EventType) {
                    WINDOW_BUFFER_SIZE_EVENT => self.resized = true,
                    KEY_EVENT => {
                        // SAFETY: EventType identifies the initialized member.
                        let key = unsafe { record.Event.KeyEvent };
                        // SAFETY: ReadConsoleInputW supplies the Unicode member.
                        let unit = unsafe { key.uChar.UnicodeChar };
                        if key.bKeyDown != 0 && unit != 0 {
                            for _ in 0..key.wRepeatCount {
                                self.character(unit);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
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

/// Each redirected side remains byte-oriented; only real console handles
/// participate in mode and code-page ownership.
pub struct LineMode {
    input_cp: Option<u32>,
    control: bool,
    output: Option<(OwnedHandle, u32, u32)>,
}

impl LineMode {
    pub fn enter() -> io::Result<Self> {
        let mut saved = Self {
            input_cp: None,
            control: false,
            output: None,
        };
        // LINE's lock is held until this snapshot is published. A control
        // callback during setup waits for it before restoring the console.
        // SAFETY: process-lifetime function; removal is owned by the guard.
        ok(unsafe { SetConsoleCtrlHandler(Some(line_control), 1) })?;
        saved.control = true;
        let mut mode = 0;
        // SAFETY: standard handles are borrowed for the call and mode is writable.
        unsafe {
            if GetConsoleMode(io::stdin().as_handle().as_raw_handle(), &raw mut mode) != 0 {
                saved.input_cp = Some(GetConsoleCP());
                ok(SetConsoleCP(65001))?;
            }
            if GetConsoleMode(io::stdout().as_handle().as_raw_handle(), &raw mut mode) != 0 {
                let output = io::stdout().as_handle().try_clone_to_owned()?;
                saved.output = Some((output, mode, GetConsoleOutputCP()));
                ok(SetConsoleMode(
                    saved.output.as_ref().unwrap().0.as_raw_handle(),
                    mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                ))?;
                ok(SetConsoleOutputCP(65001))?;
            }
        }
        Ok(saved)
    }
}

unsafe extern "system" fn line_control(event: u32) -> i32 {
    if matches!(
        event,
        CTRL_C_EVENT
            | CTRL_BREAK_EVENT
            | CTRL_CLOSE_EVENT
            | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT
    ) {
        super::restore();
    }
    // Let the next handler preserve the console's native termination status.
    0
}

impl LineMode {
    pub fn restore(&self) -> io::Result<()> {
        let mut failed = false;
        // SAFETY: saved console handle remains owned through these calls.
        unsafe {
            if let Some(cp) = self.input_cp {
                failed |= SetConsoleCP(cp) == 0;
            }
            if let Some((handle, mode, cp)) = &self.output {
                failed |= SetConsoleMode(handle.as_raw_handle(), *mode) == 0;
                failed |= SetConsoleOutputCP(*cp) == 0;
            }
        }
        if failed {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for LineMode {
    fn drop(&mut self) {
        let _ = self.restore();
        if self.control {
            // SAFETY: removes only this guard's registered process callback.
            unsafe {
                SetConsoleCtrlHandler(Some(line_control), 0);
            }
        }
    }
}

pub struct OutputGuard;
#[allow(clippy::unnecessary_wraps)] // Shared output-ownership contract.
impl OutputGuard {
    pub fn enter() -> io::Result<Self> {
        Ok(Self)
    }
}

#[allow(clippy::unnecessary_wraps)] // A console has no Unix foreground jobs.
pub fn is_foreground() -> io::Result<bool> {
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, FlushConsoleInputBuffer, GenerateConsoleCtrlEvent,
        INPUT_RECORD_0, KEY_EVENT_RECORD, KEY_EVENT_RECORD_0, STD_INPUT_HANDLE, SetStdHandle,
        WriteConsoleInputW,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    #[test]
    fn console_state_is_restored_after_a_panic_and_events_preserve_unicode_and_resize() {
        const NAME: &str = "terminal::imp::tests::console_state_is_restored_after_a_panic_and_events_preserve_unicode_and_resize";
        super::super::testing::with_console(NAME, || {
            assert_panic_restores_state();
            eprintln!("panic restoration passed");
            let signals = Signals::catch(&[Signal::Interrupt, Signal::Terminate]).unwrap();
            assert_input_events(signals);
        });
    }

    #[test]
    fn a_failed_console_read_closes_input_and_still_delivers_signals() {
        const NAME: &str =
            "terminal::imp::tests::a_failed_console_read_closes_input_and_still_delivers_signals";
        super::super::testing::with_console(NAME, || {
            let signals = Signals::catch(&[Signal::Interrupt]).unwrap();
            let _raw = super::super::RawMode::enter().unwrap();
            let mut reader = InputReader::new(signals);
            // Console record queries fail on a handle that is not a console.
            let nul = std::fs::File::open("NUL").unwrap().into_raw_handle();
            // SAFETY: the subprocess owns its standard-handle table; the
            // leaked handle stays valid until it exits.
            assert_ne!(unsafe { SetStdHandle(STD_INPUT_HANDLE, nul) }, 0);
            let mut buf = [0; 32];
            assert!(matches!(reader.read(&mut buf).unwrap(), Input::Closed));
            // SAFETY: the fresh console contains only this test subprocess.
            assert_ne!(unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) }, 0);
            assert!(matches!(
                reader.read(&mut buf).unwrap(),
                Input::Signals(signals) if signals == [Signal::Interrupt]
            ));
        });
    }

    fn assert_panic_restores_state() {
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
        for raw in [true, false] {
            let panic = std::panic::catch_unwind(|| {
                let _raw = raw.then(|| super::super::RawMode::enter().unwrap());
                let _line = (!raw).then(|| super::super::LineMode::enter().unwrap());
                // SAFETY: scalar queries for the owned console.
                let cp = unsafe { (GetConsoleCP(), GetConsoleOutputCP()) };
                assert_eq!(cp, (65001, 65001));
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
        }
    }

    fn assert_input_events(signals: Signals) {
        let _raw = super::super::RawMode::enter().unwrap();
        let mut reader = InputReader::new(signals);
        let mut records = vec![];
        for unit in "A🙂".encode_utf16() {
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
        }
        records.push(INPUT_RECORD {
            EventType: u16::try_from(WINDOW_BUFFER_SIZE_EVENT).unwrap(),
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
                    u32::try_from(records.len()).unwrap(),
                    &raw mut count
                ),
                0
            );
        }
        let mut got = vec![];
        let mut buf = [0; 32];
        let mut resized = false;
        while got.len() < "A🙂".len() || !resized {
            match reader.read(&mut buf).unwrap() {
                Input::Bytes { len, .. } => got.extend_from_slice(&buf[..len]),
                Input::Signals(signals) if signals == [Signal::Resize] => resized = true,
                _ => panic!("unexpected console input"),
            }
        }
        eprintln!("Unicode and resize passed");
        assert_eq!(got, "A🙂".as_bytes());
        for (event, signal) in [
            (CTRL_C_EVENT, Signal::Interrupt),
            (CTRL_BREAK_EVENT, Signal::Terminate),
        ] {
            // SAFETY: the fresh console contains only this test subprocess.
            assert_ne!(unsafe { GenerateConsoleCtrlEvent(event, 0) }, 0);
            // SAFETY: the signal source owns this event throughout the wait.
            let ready =
                unsafe { WaitForSingleObject(reader.signals.as_handle().as_raw_handle(), 5000) };
            assert_eq!(ready, 0, "control event {event} did not reach its handler");
            assert_eq!(reader.signals.take(), [signal]);
            eprintln!("control event {event} passed");
        }
    }
}
