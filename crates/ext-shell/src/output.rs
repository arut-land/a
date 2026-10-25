//! A command's output as the model sees it: decoded and cleaned of what
//! only a terminal would render, and cut to its start and end. The whole
//! is the stream the command writes to, which only a cut output keeps, as
//! a blob.

use std::collections::VecDeque;
use std::fmt::Write as _;

/// The most of the output's start the model sees.
pub const HEAD: usize = 8 * 1024;
/// The most of the output's end the model sees.
pub const TAIL: usize = 24 * 1024;
/// The most lines the model sees, split between start and end.
pub const LINES: usize = 2000;
/// The most output retained per command; reaching it stops the process.
pub const OUTPUT_CAP: u64 = ext::blob::STREAM_CAP;

/// A line longer than this is passed on in pieces, so one without a newline
/// cannot grow without bound.
const LONG_LINE: usize = 64 * 1024;

/// Folds raw output in as it arrives, keeping only what the model is
/// shown; [`Output::finish`] renders it.
#[derive(Default)]
pub struct Output {
    /// The start, until it shows whether the output is text.
    raw: Vec<u8>,
    kind: Kind,
    clean: Clean,
    view: View,
    bytes: u64,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Kind {
    #[default]
    Undecided,
    Text,
    /// A NUL in the first [`HEAD`] bytes: kept whole in the file only.
    Binary,
}

/// What the model is shown of the output.
pub struct Rendered {
    pub text: String,
    pub truncated: bool,
}

/// The output once it has ended, before it is shown.
pub struct Ended {
    view: View,
    kind: Kind,
    bytes: u64,
}

impl Output {
    /// How many bytes were pushed.
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn push(&mut self, mut chunk: &[u8]) {
        self.bytes += chunk.len() as u64;
        if self.kind == Kind::Undecided {
            let take = chunk.len().min(HEAD - self.raw.len());
            self.raw.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            if self.raw.contains(&0) {
                self.kind = Kind::Binary;
                self.raw = Vec::new();
            } else if self.raw.len() == HEAD {
                self.text();
            } else {
                return;
            }
        }
        if self.kind != Kind::Binary {
            self.feed(chunk);
        }
    }

    fn text(&mut self) {
        self.kind = Kind::Text;
        let raw = std::mem::take(&mut self.raw);
        self.feed(&raw);
    }

    fn feed(&mut self, bytes: &[u8]) {
        let Self { clean, view, .. } = self;
        clean.feed(bytes, &mut |piece| view.push(piece));
    }

    /// The output, ended: what is incomplete is cleaned up.
    pub fn finish(mut self) -> Ended {
        if self.kind == Kind::Undecided {
            self.text();
        }
        let Self {
            mut clean,
            mut view,
            kind,
            bytes,
            ..
        } = self;
        clean.finish(&mut |piece| view.push(piece));
        Ended { view, kind, bytes }
    }
}

impl Ended {
    /// Whether the model sees less than the whole, which is then kept.
    pub const fn cut(&self) -> bool {
        matches!(self.kind, Kind::Binary) || self.view.cut
    }

    pub const fn media_type(&self) -> &'static str {
        match self.kind {
            Kind::Binary => "application/octet-stream",
            _ => "text/plain; charset=utf-8",
        }
    }

    /// What the model is shown. Where the whole is read is named apart,
    /// from the reference the result keeps.
    pub fn render(self) -> Rendered {
        let Self { view, kind, bytes } = self;
        if kind == Kind::Binary {
            return Rendered {
                text: format!("[binary output: {bytes} bytes]"),
                truncated: true,
            };
        }
        let truncated = view.cut;
        Rendered {
            text: view.render(),
            truncated,
        }
    }
}

/// The text the model is shown: all of it, or once it outgrows the limits,
/// its start and a sliding window over its end.
#[derive(Default)]
struct View {
    head: String,
    tail: VecDeque<String>,
    tail_bytes: usize,
    bytes: usize,
    lines: usize,
    cut: bool,
}

impl View {
    fn push(&mut self, piece: &str) {
        self.bytes += piece.len();
        let ends = usize::from(piece.ends_with('\n'));
        self.lines += ends;
        if self.cut {
            self.tail_bytes += piece.len();
            self.tail.push_back(piece.to_owned());
            self.trim();
            return;
        }
        self.head.push_str(piece);
        if self.head.len() > HEAD + TAIL || self.lines > LINES {
            self.split();
        }
    }

    /// Starts cutting: the start of the text so far stays as the head and
    /// the rest becomes the tail.
    fn split(&mut self) {
        self.cut = true;
        let all = std::mem::take(&mut self.head);
        let mut lines = all.split_inclusive('\n');
        for (head_lines, line) in lines.by_ref().enumerate() {
            let room = HEAD - self.head.len();
            if line.len() > room || head_lines == LINES / 2 {
                if self.head.is_empty() {
                    let cut = line.floor_char_boundary(room);
                    self.head.push_str(&line[..cut]);
                    self.tail.push_back(line[cut..].to_owned());
                } else {
                    self.tail.push_back(line.to_owned());
                }
                break;
            }
            self.head.push_str(line);
        }
        self.tail.extend(lines.map(str::to_owned));
        self.tail_bytes = all.len() - self.head.len();
        self.trim();
    }

    fn trim(&mut self) {
        while self.tail.len() > LINES / 2 {
            if let Some(first) = self.tail.pop_front() {
                self.tail_bytes -= first.len();
            }
        }
        while self.tail_bytes > TAIL {
            let over = self.tail_bytes - TAIL;
            let Some(first) = self.tail.front_mut() else {
                break;
            };
            if first.len() <= over {
                self.tail_bytes -= first.len();
                self.tail.pop_front();
            } else {
                // Only the start of the oldest line has to go.
                let cut = first.ceil_char_boundary(over);
                first.drain(..cut);
                self.tail_bytes -= cut;
            }
        }
    }

    fn render(self) -> String {
        if !self.cut {
            return self.head;
        }
        let shown_lines = self.head.matches('\n').count() + self.tail.len();
        let shown = self.head.len() + self.tail_bytes;
        let mut text = self.head;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let _ = writeln!(
            text,
            "[… {} lines / {} bytes omitted]",
            self.lines.saturating_sub(shown_lines),
            self.bytes - shown,
        );
        text.extend(self.tail);
        text
    }
}

/// The most output a call reports as it runs: its log holds this much of
/// it, cleaned, for an interrupted call to show.
pub const LIVE_CAP: usize = HEAD + TAIL;

/// The start of the output, cleaned line by line as it arrives, for
/// reporting while the command runs. Binary output is not reported.
#[derive(Default)]
pub struct Live {
    clean: Clean,
    fresh: String,
    taken: usize,
    done: bool,
}

impl Live {
    pub fn push(&mut self, chunk: &[u8]) {
        if self.done {
            return;
        }
        if chunk.contains(&0) {
            self.done = true;
            self.fresh.clear();
            return;
        }
        let Self {
            clean,
            fresh,
            taken,
            done,
        } = self;
        clean.feed(chunk, &mut |piece| {
            let room = LIVE_CAP.saturating_sub(*taken + fresh.len());
            if piece.len() > room {
                fresh.push_str(&piece[..piece.floor_char_boundary(room)]);
                *done = true;
            } else if !*done {
                fresh.push_str(piece);
            }
        });
    }

    /// The text cleaned since the last take, if any.
    pub fn take(&mut self) -> Option<String> {
        if self.fresh.is_empty() {
            return None;
        }
        self.taken += self.fresh.len();
        Some(std::mem::take(&mut self.fresh))
    }
}

/// Decodes UTF-8 across chunk boundaries and drops what only a terminal
/// renders: escape sequences, control characters other than tab and
/// newline, and lines a carriage return overwrote. Passes on whole lines.
#[derive(Default)]
struct Clean {
    parser: vte::Parser,
    line: Line,
}

#[derive(Default)]
struct Line {
    text: String,
    /// A carriage return ended the text so far: the next character starts
    /// the line over, unless it is the newline of a CRLF.
    cr: bool,
}

impl Clean {
    fn feed(&mut self, bytes: &[u8], out: &mut impl FnMut(&str)) {
        let mut sink = Sink {
            line: &mut self.line,
            out,
        };
        self.parser.advance(&mut sink, bytes);
    }

    fn finish(&mut self, out: &mut impl FnMut(&str)) {
        // CAN ends whatever is incomplete: a split character becomes U+FFFD,
        // an unterminated sequence is dropped.
        self.feed(b"\x18", out);
        if !self.line.text.is_empty() {
            out(&self.line.text);
            self.line.text.clear();
        }
    }
}

/// What the parser finds, folded into the line.
struct Sink<'a, F> {
    line: &'a mut Line,
    out: &'a mut F,
}

impl<F: FnMut(&str)> Sink<'_, F> {
    fn push(&mut self, c: char) {
        let line = &mut *self.line;
        if std::mem::take(&mut line.cr) {
            line.text.clear();
        }
        line.text.push(c);
        if line.text.len() >= LONG_LINE {
            (self.out)(&line.text);
            line.text.clear();
        }
    }
}

impl<F: FnMut(&str)> vte::Perform for Sink<'_, F> {
    fn print(&mut self, c: char) {
        if !c.is_control() {
            self.push(c);
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                let line = &mut *self.line;
                line.cr = false;
                line.text.push('\n');
                (self.out)(&line.text);
                line.text.clear();
            }
            b'\r' => self.line.cr = true,
            b'\t' => self.push('\t'),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(chunks: &[&[u8]]) -> String {
        let mut clean = Clean::default();
        let mut text = String::new();
        for chunk in chunks {
            clean.feed(chunk, &mut |piece| text.push_str(piece));
        }
        clean.finish(&mut |piece| text.push_str(piece));
        text
    }

    #[test]
    fn terminal_control_is_dropped_and_progress_lines_collapse() {
        assert_eq!(clean(&[b"\x1b[1;31mred\x1b[0m plain\n"]), "red plain\n");
        assert_eq!(
            clean(&[b"\x1b]0;title\x07a\x1b]8;;http://x\x1b\\b\n"]),
            "ab\n"
        );
        assert_eq!(clean(&[b"\x1b(Bx\x07\x08\ty\n"]), "x\ty\n");
        assert_eq!(clean(&[b"a\r\nb\r\n"]), "a\nb\n");
        assert_eq!(clean(&[b"10%\r50%\r\x1b[K100%\rdone\n"]), "done\n");
        assert_eq!(clean(&[b"last 100%\r"]), "last 100%");
        // Split anywhere: inside an escape, a CRLF, a character.
        let whole = "\x1b[32mgr\u{fc}n\x1b[0m\r\nn\u{e4}chste\n".as_bytes();
        for at in 0..whole.len() {
            let (a, b) = whole.split_at(at);
            assert_eq!(clean(&[a, b]), "gr\u{fc}n\nn\u{e4}chste\n", "split at {at}");
        }
    }

    #[test]
    fn invalid_utf8_becomes_replacement_characters() {
        assert_eq!(clean(&[b"a\xffb\xc3"]), "a\u{FFFD}b\u{FFFD}");
        assert_eq!(clean(&[b"\xe2\x82", b"\xac\n"]), "\u{20ac}\n");
    }

    fn shown(out: Output) -> (bool, Rendered) {
        let ended = out.finish();
        (ended.cut(), ended.render())
    }

    #[test]
    fn short_output_is_shown_whole_and_names_no_file() {
        let mut out = Output::default();
        out.push(b"hello\n");
        let (cut, rendered) = shown(out);
        assert_eq!(rendered.text, "hello\n");
        assert!(!rendered.truncated && !cut);
    }

    #[test]
    fn long_output_shows_its_start_and_end_and_names_the_whole() {
        let mut out = Output::default();
        let lines: Vec<String> = (0..10_000).map(|n| format!("line {n}\n")).collect();
        for chunk in lines.chunks(7) {
            out.push(chunk.concat().as_bytes());
        }
        let (cut, rendered) = shown(out);
        assert!(rendered.truncated && cut);
        let text = rendered.text;
        assert!(text.starts_with("line 0\nline 1\n"), "{}", &text[..40]);
        assert!(text.ends_with("line 9999\n"));
        assert!(text.lines().count() <= LINES + 1);
        assert!(text.len() <= HEAD + TAIL + 200, "{}", text.len());
        let marker = text.lines().find(|l| l.starts_with("[…")).unwrap();
        assert!(
            marker.contains("lines /") && marker.ends_with("omitted]"),
            "{marker}"
        );
    }

    #[test]
    fn one_huge_line_is_cut_too() {
        let mut out = Output::default();
        out.push("é".repeat(100_000).as_bytes());
        let (_, rendered) = shown(out);
        assert!(rendered.truncated);
        assert!(rendered.text.len() <= HEAD + TAIL + 200);
    }

    #[test]
    fn binary_output_is_only_counted() {
        let mut out = Output::default();
        out.push(b"\x7fELF\x00\x01");
        out.push(&[0xff; 9000]);
        let (cut, rendered) = shown(out);
        assert_eq!(rendered.text, "[binary output: 9006 bytes]");
        assert!(cut);
    }

    #[test]
    fn live_output_is_whole_clean_lines_up_to_its_cap() {
        let mut live = Live::default();
        live.push(b"\x1b[1mone\x1b[0m\ntw");
        assert_eq!(live.take().as_deref(), Some("one\n"));
        assert_eq!(live.take(), None);
        live.push(b"o\n");
        live.push(&vec![b'x'; LIVE_CAP]);
        live.push(b"\nmore\n");
        let text = live.take().unwrap();
        assert!(text.starts_with("two\nxxx"), "{text}");
        assert_eq!(text.len(), LIVE_CAP - "one\n".len());
        live.push(b"after the cap\n");
        assert_eq!(live.take(), None);

        let mut binary = Live::default();
        binary.push(b"text\n\0bin");
        binary.push(b"more\n");
        assert_eq!(binary.take(), None);
    }
}
