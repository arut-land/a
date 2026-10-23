//! `a` on a pseudo-terminal, and a small screen model of what it drew: the
//! cursor moves, erases and text the full-screen and inline clients emit
//! (styles and modes are read past).

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rustix::io::{FdFlags, fcntl_setfd};
use rustix::process::{Pid, WaitOptions, waitpid};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{LocalModes, Winsize, tcgetattr, tcsetwinsize};
use unicode_width::UnicodeWidthChar;

use super::Env;

/// Bytes the child wrote, and when its first full frame ended.
#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    first_frame: Option<Instant>,
    /// Reading stops once a frame starts past this many bytes.
    deaf_after: Option<usize>,
    /// Nothing more is read, as of a hung emulator.
    deaf: bool,
}

/// What the child wrote, and a wake for every read of it.
#[derive(Default)]
struct Sink {
    output: Mutex<Output>,
    wrote: Condvar,
}

impl Sink {
    fn lock(&self) -> MutexGuard<'_, Output> {
        self.output.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct Pty {
    master: File,
    pub child: Child,
    sink: Arc<Sink>,
    started: Instant,
    /// Reaped by [`Pty::wait`]: nothing is signalled or waited for again.
    reaped: bool,
    cols: u16,
    rows: u16,
}

impl Env {
    /// `a` with `args` on a `cols` × `rows` terminal.
    pub fn tty(&self, args: &[&str], cols: u16, rows: u16) -> Pty {
        Self::tty_of(self.cmd(args), cols, rows)
    }

    /// `cmd` on a `cols` × `rows` terminal.
    pub fn tty_of(mut cmd: Command, cols: u16, rows: u16) -> Pty {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        // posix_openpt has no close-on-exec flag: a copy left in the client
        // (or the daemon it starts) would keep the terminal from ever
        // hanging up, and the client running past the test.
        fcntl_setfd(&master, FdFlags::CLOEXEC).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        let slave = File::options()
            .read(true)
            .write(true)
            .open(name.to_str().unwrap())
            .unwrap();
        tcsetwinsize(
            &slave,
            Winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .unwrap();
        let stdio = || Stdio::from(slave.try_clone().unwrap());
        let started = Instant::now();
        let child = cmd
            .env("TERM", "xterm-256color")
            .stdin(stdio())
            .stdout(stdio())
            .stderr(stdio())
            .spawn()
            .unwrap();
        drop(slave);
        let master = File::from(master);
        let sink = Arc::new(Sink::default());
        let (mut reader, written) = (master.try_clone().unwrap(), Arc::clone(&sink));
        // Nothing answers the client: it never asks the terminal anything
        // (`no_terminal_queries` in the tui crate), so one that did would
        // hang here, and the test with it.
        std::thread::spawn(move || {
            let mut buf = [0; 8192];
            // EIO once the child and every copy of the slave are gone.
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                let mut out = written.lock();
                out.bytes.extend_from_slice(&buf[..n]);
                if out.first_frame.is_none() && contains(&out.bytes, b"\x1b[?2026l") {
                    out.first_frame = Some(Instant::now());
                }
                out.deaf = out
                    .deaf_after
                    .is_some_and(|from| contains(&out.bytes[from..], b"\x1b[?2026h"));
                let deaf = out.deaf;
                drop(out);
                written.wrote.notify_all();
                if deaf {
                    return;
                }
            }
        });
        Pty {
            master,
            child,
            sink,
            started,
            reaped: false,
            cols,
            rows,
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

impl Pty {
    pub fn bytes(&self) -> Vec<u8> {
        self.sink.lock().bytes.clone()
    }

    /// From spawning `a` to the end of its first frame.
    pub fn first_frame(&self) -> Option<Duration> {
        self.sink.lock().first_frame.map(|t| t - self.started)
    }

    /// The screen once it shows `text`: looked at again as the child
    /// writes, never on a timer.
    pub fn wait_for(&self, text: &str) -> Screen {
        self.wait_until(text, |screen| screen.find(text).is_some())
    }

    pub fn wait_until(&self, what: &str, done: impl Fn(&Screen) -> bool) -> Screen {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut out = self.sink.lock();
        loop {
            let screen = Screen::parse(&out.bytes, self.cols, self.rows);
            if done(&screen) {
                return screen;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                left > Duration::ZERO,
                "timed out waiting for {what:?} on\n{}",
                screen.text()
            );
            out = self
                .sink
                .wrote
                .wait_timeout(out, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Sends `bytes`, and reads nothing more once the frame they draw has
    /// begun, as a hung emulator would: a frame larger than the terminal
    /// buffers then blocks the client in its write for good.
    pub fn go_deaf_drawing(&mut self, bytes: &[u8]) {
        let mut out = self.sink.lock();
        out.deaf_after = Some(out.bytes.len());
        drop(out);
        self.send(bytes);
        let mut out = self.sink.lock();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !out.deaf {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(left > Duration::ZERO, "no frame drawn");
            out = self
                .sink
                .wrote
                .wait_timeout(out, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    pub fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }

    /// Output from the shell while the client is stopped, through the tty's
    /// output side rather than the master that sends keyboard input.
    pub fn shell_output(&self, bytes: &[u8]) {
        let name = ptsname(&self.master, Vec::new()).unwrap();
        File::options()
            .write(true)
            .open(name.to_str().unwrap())
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    /// A left click at a screen cell, as an SGR mouse report.
    pub fn click(&mut self, (x, y): (u16, u16)) {
        let report = format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", x + 1, y + 1, x + 1, y + 1);
        self.send(report.as_bytes());
    }

    /// Whether the terminal is in cooked mode: echoing and by lines.
    pub fn cooked(&self) -> bool {
        let mode = tcgetattr(&self.master).unwrap();
        mode.local_modes
            .contains(LocalModes::ECHO | LocalModes::ICANON)
    }

    /// The child's exit status, waited for (on a helper thread, as a wait
    /// has no timeout) rather than polled; a child that is still running
    /// after 10 s fails the test.
    pub fn wait(&mut self) -> ExitStatus {
        let pid = Pid::from_child(&self.child);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(waitpid(Some(pid), WaitOptions::empty()));
        });
        let (_, status) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("a did not exit")
            .unwrap()
            .expect("the child was reaped by someone else");
        self.reaped = true;
        ExitStatus::from_raw(status.as_raw())
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The text grid a terminal would show.
pub struct Screen {
    rows: Vec<Vec<char>>,
    /// Rows scrolled off the top, oldest first.
    pub scrollback: Vec<String>,
    cursor: (usize, usize),
}

impl Screen {
    fn parse(bytes: &[u8], cols: u16, rows: u16) -> Self {
        let (width, height) = (usize::from(cols), usize::from(rows));
        let mut screen = Self {
            rows: vec![vec![' '; width]; height],
            scrollback: Vec::new(),
            cursor: (0, 0),
        };
        let (mut x, mut y) = (0usize, 0usize);
        let text = String::from_utf8_lossy(bytes);
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => match chars.next() {
                    Some('[') => {
                        let mut params = String::new();
                        let mut fin = ' ';
                        for c in chars.by_ref() {
                            if ('\x40'..='\x7e').contains(&c) {
                                fin = c;
                                break;
                            }
                            params.push(c);
                        }
                        let n: Vec<usize> =
                            params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
                        let n0 = n[0].max(1);
                        match fin {
                            'H' => {
                                y = (n0 - 1).min(height - 1);
                                x = (n.get(1).copied().unwrap_or(1).max(1) - 1).min(width - 1);
                            }
                            'A' => y = y.saturating_sub(n0),
                            'B' => y = (y + n0).min(height - 1),
                            'C' => x = (x + n0).min(width - 1),
                            'D' => x = x.saturating_sub(n0),
                            'G' => x = (n0 - 1).min(width - 1),
                            'J' if params.starts_with('2') => {
                                screen.rows.iter_mut().for_each(|r| r.fill(' '));
                            }
                            'J' => {
                                screen.rows[y][x..].fill(' ');
                                screen.rows[y + 1..].iter_mut().for_each(|r| r.fill(' '));
                            }
                            'K' => screen.rows[y][x..].fill(' '),
                            _ => {}
                        }
                    }
                    Some(']') => {
                        while let Some(c) = chars.next() {
                            if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                                break;
                            }
                        }
                    }
                    _ => {}
                },
                '\r' => x = 0,
                '\n' => {
                    if y + 1 == height {
                        let top = screen.rows.remove(0);
                        screen
                            .scrollback
                            .push(top.iter().collect::<String>().trim_end().to_owned());
                        screen.rows.push(vec![' '; width]);
                    } else {
                        y += 1;
                    }
                }
                '\x08' => x = x.saturating_sub(1),
                c if c.is_control() => {}
                c => {
                    let cw = c.width().unwrap_or(0);
                    if cw == 0 {
                        continue;
                    }
                    if x + cw > width {
                        x = 0;
                        if y + 1 < height {
                            y += 1;
                        }
                    }
                    screen.rows[y][x] = c;
                    if cw == 2 && x + 1 < width {
                        screen.rows[y][x + 1] = '\0';
                    }
                    x = (x + cw).min(width);
                }
            }
        }
        screen.cursor = (x.min(width - 1), y);
        screen
    }

    pub fn row(&self, y: usize) -> String {
        let row: String = self.rows[y].iter().filter(|&&c| c != '\0').collect();
        row.trim_end().to_owned()
    }

    pub fn text(&self) -> String {
        (0..self.rows.len())
            .map(|y| self.row(y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Where `text` starts on screen, as (column, row).
    pub fn find(&self, text: &str) -> Option<(u16, u16)> {
        (0..self.rows.len()).find_map(|y| {
            let row = self.row(y);
            let at = row.find(text)?;
            let x = row[..at]
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum::<usize>();
            Some((u16::try_from(x).ok()?, u16::try_from(y).ok()?))
        })
    }
}
