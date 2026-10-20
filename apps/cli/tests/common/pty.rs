//! `a` on a pseudo-terminal, and a small screen model of what it drew: the
//! cursor moves, erases and text the full-screen and inline clients emit
//! (styles and modes are read past).

use std::fs::File;
use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{LocalModes, Winsize, tcgetattr, tcsetwinsize};
use unicode_width::UnicodeWidthChar;

use super::Env;

/// Bytes the child wrote, and when its first full frame ended.
#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    first_frame: Option<Instant>,
}

pub struct Pty {
    master: File,
    pub child: Child,
    output: Arc<Mutex<Output>>,
    started: Instant,
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
        let output = Arc::new(Mutex::new(Output::default()));
        let (mut reader, sink) = (master.try_clone().unwrap(), Arc::clone(&output));
        // Nothing answers the client: it never asks the terminal anything
        // (`no_terminal_queries` in the tui crate), so one that did would
        // hang here, and the test with it.
        std::thread::spawn(move || {
            let mut buf = [0; 8192];
            // EIO once the child and every copy of the slave are gone.
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                let mut out = sink.lock().unwrap_or_else(PoisonError::into_inner);
                out.bytes.extend_from_slice(&buf[..n]);
                if out.first_frame.is_none() && contains(&out.bytes, b"\x1b[?2026l") {
                    out.first_frame = Some(Instant::now());
                }
            }
        });
        Pty {
            master,
            child,
            output,
            started,
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
        let out = self.output.lock().unwrap_or_else(PoisonError::into_inner);
        out.bytes.clone()
    }

    pub fn screen(&self) -> Screen {
        Screen::parse(&self.bytes(), self.cols, self.rows)
    }

    /// From spawning `a` to the end of its first frame.
    pub fn first_frame(&self) -> Option<Duration> {
        let out = self.output.lock().unwrap_or_else(PoisonError::into_inner);
        out.first_frame.map(|t| t - self.started)
    }

    /// The screen once it shows `text`.
    pub fn wait_for(&self, text: &str) -> Screen {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let screen = self.screen();
            if screen.find(text).is_some() {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {text:?} on\n{}",
                screen.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
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

    pub fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "a did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
