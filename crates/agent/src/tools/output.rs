//! A command's output as the model sees it: decoded and cleaned of what
//! only a terminal would render, cut to its start and end, and saved whole
//! to a private file when cut.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

/// The most of the output's start the model sees.
pub const HEAD: usize = 8 * 1024;
/// The most of the output's end the model sees.
pub const TAIL: usize = 24 * 1024;
/// The most lines the model sees, split between start and end.
pub const LINES: usize = 2000;
/// The most a spill file holds.
pub const SPILL_CAP: u64 = 64 << 20;

/// A line longer than this is passed on in pieces, so one without a newline
/// cannot grow without bound.
const LONG_LINE: usize = 64 * 1024;

/// Folds raw output in as it arrives; [`Output::finish`] renders it.
pub struct Output {
    /// The start, until it shows whether the output is text.
    raw: Vec<u8>,
    kind: Kind,
    clean: Clean,
    view: View,
    pub spill: Spill,
    bytes: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Undecided,
    Text,
    /// A NUL in the first [`HEAD`] bytes: kept whole in the spill file only.
    Binary,
}

/// What the model is shown of the output.
pub struct Rendered {
    pub text: String,
    pub truncated: bool,
    /// The whole output, if it was saved.
    pub full: Option<PathBuf>,
}

impl Output {
    pub fn new(spill: Spill) -> Self {
        Self {
            raw: Vec::new(),
            kind: Kind::Undecided,
            clean: Clean::default(),
            view: View::default(),
            spill,
            bytes: 0,
        }
    }

    pub fn push(&mut self, mut chunk: &[u8]) {
        self.bytes += chunk.len() as u64;
        if self.kind == Kind::Undecided {
            let take = chunk.len().min(HEAD - self.raw.len());
            self.raw.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            if self.raw.contains(&0) {
                self.kind = Kind::Binary;
                let raw = std::mem::take(&mut self.raw);
                self.spill.write(&raw);
            } else if self.raw.len() == HEAD {
                self.text();
            } else {
                return;
            }
        }
        match self.kind {
            Kind::Binary => self.spill.write(chunk),
            _ => self.feed(chunk),
        }
    }

    fn text(&mut self) {
        self.kind = Kind::Text;
        let raw = std::mem::take(&mut self.raw);
        self.feed(&raw);
    }

    fn feed(&mut self, bytes: &[u8]) {
        let Self {
            clean, view, spill, ..
        } = self;
        clean.feed(bytes, &mut |piece| view.push(piece, spill));
    }

    pub fn finish(mut self) -> Rendered {
        if self.kind == Kind::Undecided {
            self.text();
        }
        let Self {
            mut clean,
            mut view,
            mut spill,
            kind,
            bytes,
            ..
        } = self;
        clean.finish(&mut |piece| view.push(piece, &mut spill));
        spill.flush();
        let full = spill.path();
        if kind == Kind::Binary {
            let saved = full
                .as_ref()
                .map_or_else(String::new, |path| format!(", saved to {}", path.display()));
            return Rendered {
                text: format!("[binary output: {bytes} bytes{saved}]"),
                truncated: true,
                full,
            };
        }
        let truncated = view.cut;
        Rendered {
            text: view.render(&spill),
            truncated,
            full,
        }
    }
}

/// The text the model is shown: all of it, or once it outgrows the limits,
/// its start and a sliding window over its end.
#[derive(Default)]
struct View {
    head: String,
    head_lines: usize,
    tail: VecDeque<String>,
    tail_bytes: usize,
    bytes: usize,
    lines: usize,
    cut: bool,
}

impl View {
    fn push(&mut self, piece: &str, spill: &mut Spill) {
        self.bytes += piece.len();
        let ends = usize::from(piece.ends_with('\n'));
        self.lines += ends;
        if self.cut {
            spill.write(piece.as_bytes());
            self.tail_bytes += piece.len();
            self.tail.push_back(piece.to_owned());
            self.trim();
            return;
        }
        self.head.push_str(piece);
        self.head_lines += ends;
        if self.head.len() > HEAD + TAIL || self.head_lines > LINES {
            self.split(spill);
        }
    }

    /// Starts cutting: the whole text so far goes to the spill file, its
    /// start stays as the head and the rest becomes the tail.
    fn split(&mut self, spill: &mut Spill) {
        self.cut = true;
        spill.write(self.head.as_bytes());
        let all = std::mem::take(&mut self.head);
        let mut lines = all.split_inclusive('\n');
        self.head_lines = 0;
        for line in lines.by_ref() {
            let room = HEAD - self.head.len();
            if line.len() > room || self.head_lines == LINES / 2 {
                if self.head.is_empty() {
                    let cut = line.floor_char_boundary(room);
                    self.head.push_str(&line[..cut]);
                    self.tail_bytes += line.len() - cut;
                    self.tail.push_back(line[cut..].to_owned());
                } else {
                    self.tail_bytes += line.len();
                    self.tail.push_back(line.to_owned());
                }
                break;
            }
            self.head.push_str(line);
            self.head_lines += 1;
        }
        for line in lines {
            self.tail_bytes += line.len();
            self.tail.push_back(line.to_owned());
        }
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

    fn render(self, spill: &Spill) -> String {
        if !self.cut {
            return self.head;
        }
        let shown_lines = self.head_lines + self.tail.len();
        let shown = self.head.len() + self.tail_bytes;
        let mut text = self.head;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let where_ = match (spill.path(), &spill.error) {
            (Some(path), _) => format!("full output: {}", path.display()),
            (None, Some(error)) => format!("full output not saved: {error}"),
            (None, None) => "full output not saved".to_owned(),
        };
        let capped = if spill.capped {
            format!(" (its first {} MiB)", SPILL_CAP >> 20)
        } else {
            String::new()
        };
        let _ = writeln!(
            text,
            "[… {} lines / {} bytes omitted; {where_}{capped}]",
            self.lines.saturating_sub(shown_lines),
            self.bytes - shown,
        );
        text.extend(self.tail);
        text
    }
}

/// A private file holding a call's whole output, created on first write
/// and never grown past [`SPILL_CAP`].
pub struct Spill {
    path: PathBuf,
    file: Option<BufWriter<File>>,
    written: u64,
    pub capped: bool,
    pub error: Option<String>,
}

impl Spill {
    /// A spill to `path`, or a free name next to it if that is taken.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            written: 0,
            capped: false,
            error: None,
        }
    }

    /// Where the output was saved, once anything was.
    pub fn path(&self) -> Option<PathBuf> {
        self.file.is_some().then(|| self.path.clone())
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if self.error.is_some() || self.capped || bytes.is_empty() {
            return;
        }
        if self.file.is_none() {
            match create_private(&self.path) {
                Ok((file, path)) => (self.file, self.path) = (Some(BufWriter::new(file)), path),
                Err(e) => {
                    self.error = Some(e.to_string());
                    return;
                }
            }
        }
        let room = usize::try_from(SPILL_CAP - self.written).unwrap_or(usize::MAX);
        let bytes = if bytes.len() > room {
            self.capped = true;
            &bytes[..room]
        } else {
            bytes
        };
        if let Some(file) = &mut self.file
            && let Err(e) = file.write_all(bytes)
        {
            self.error = Some(e.to_string());
        }
        self.written += bytes.len() as u64;
    }

    fn flush(&mut self) {
        if let Some(file) = &mut self.file
            && let Err(e) = file.flush()
        {
            self.error = Some(e.to_string());
        }
    }
}

/// Creates a file only its owner can read, in a directory only its owner
/// can enter, never opening one that exists: at `path`, or at `path` with
/// a number added when that is taken.
pub fn create_private(path: &Path) -> io::Result<(File, PathBuf)> {
    let mut dir = std::fs::DirBuilder::new();
    dir.recursive(true);
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        dir.mode(0o700);
        open.mode(0o600);
    }
    if let Some(parent) = path.parent() {
        dir.create(parent)?;
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path.extension().unwrap_or_default().to_string_lossy();
    for n in 0..100 {
        let path = match n {
            0 => path.to_path_buf(),
            n => path.with_file_name(format!("{stem}-{n}.{ext}")),
        };
        match open.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{} and its alternatives exist", path.display()),
    ))
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
    /// The start of a character split across chunks.
    partial: Vec<u8>,
    esc: Esc,
    line: String,
    /// A carriage return ended the text so far: the next character starts
    /// the line over, unless it is the newline of a CRLF.
    cr: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Esc {
    #[default]
    No,
    Started,
    /// `ESC` and intermediate bytes, as in `ESC ( B`.
    Intermediate,
    /// `ESC [`: up to a final byte.
    Csi,
    /// `ESC ]`: up to BEL or `ESC \`.
    Osc,
    OscTail,
}

impl Clean {
    fn feed(&mut self, bytes: &[u8], out: &mut impl FnMut(&str)) {
        let mut joined;
        let mut rest = if self.partial.is_empty() {
            bytes
        } else {
            joined = std::mem::take(&mut self.partial);
            joined.extend_from_slice(bytes);
            &joined[..]
        };
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    self.chars(text, out);
                    return;
                }
                Err(e) => {
                    let (valid, after) = rest.split_at(e.valid_up_to());
                    self.chars(std::str::from_utf8(valid).unwrap_or_default(), out);
                    let Some(bad) = e.error_len() else {
                        self.partial = after.to_vec();
                        return;
                    };
                    self.char('\u{FFFD}', out);
                    rest = &after[bad..];
                }
            }
        }
    }

    fn chars(&mut self, text: &str, out: &mut impl FnMut(&str)) {
        for c in text.chars() {
            self.char(c, out);
        }
    }

    fn char(&mut self, c: char, out: &mut impl FnMut(&str)) {
        match self.esc {
            Esc::No => {}
            Esc::Started => {
                self.esc = match c {
                    '[' => Esc::Csi,
                    ']' => Esc::Osc,
                    ' '..='/' => Esc::Intermediate,
                    _ => Esc::No,
                };
                return;
            }
            Esc::Intermediate => {
                if !(' '..='/').contains(&c) {
                    self.esc = Esc::No;
                }
                return;
            }
            Esc::Csi => {
                if ('@'..='~').contains(&c) {
                    self.esc = Esc::No;
                }
                return;
            }
            Esc::Osc => {
                match c {
                    '\x07' => self.esc = Esc::No,
                    '\x1b' => self.esc = Esc::OscTail,
                    _ => {}
                }
                return;
            }
            Esc::OscTail => {
                self.esc = if c == '\\' { Esc::No } else { Esc::Osc };
                return;
            }
        }
        match c {
            '\x1b' => self.esc = Esc::Started,
            '\n' => {
                self.cr = false;
                self.line.push('\n');
                out(&self.line);
                self.line.clear();
            }
            '\r' => self.cr = true,
            c if c.is_control() && c != '\t' => {}
            c => {
                if std::mem::take(&mut self.cr) {
                    self.line.clear();
                }
                self.line.push(c);
                if self.line.len() >= LONG_LINE {
                    out(&self.line);
                    self.line.clear();
                }
            }
        }
    }

    fn finish(&mut self, out: &mut impl FnMut(&str)) {
        if !std::mem::take(&mut self.partial).is_empty() {
            self.char('\u{FFFD}', out);
        }
        if !self.line.is_empty() {
            out(&self.line);
            self.line.clear();
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

    fn spill() -> (PathBuf, Spill) {
        let dir = std::env::temp_dir().join(format!(
            "a-output-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("1.log");
        (dir, Spill::new(path))
    }

    #[test]
    fn short_output_is_shown_whole_and_saves_nothing() {
        let (dir, spill) = spill();
        let mut out = Output::new(spill);
        out.push(b"hello\n");
        let rendered = out.finish();
        assert_eq!(rendered.text, "hello\n");
        assert!(!rendered.truncated && rendered.full.is_none());
        assert!(!dir.exists());
    }

    #[test]
    fn long_output_shows_its_start_and_end_and_saves_the_rest() {
        let (dir, spill) = spill();
        let mut out = Output::new(spill);
        let lines: Vec<String> = (0..10_000).map(|n| format!("line {n}\n")).collect();
        for chunk in lines.chunks(7) {
            out.push(chunk.concat().as_bytes());
        }
        let rendered = out.finish();
        let full = rendered.full.clone().unwrap();
        let saved = std::fs::read_to_string(&full).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!((mode(&dir), mode(&full)), (0o700, 0o600));
        }
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(rendered.truncated);
        assert_eq!(saved, lines.concat());
        let text = rendered.text;
        assert!(text.starts_with("line 0\nline 1\n"), "{}", &text[..40]);
        assert!(text.ends_with("line 9999\n"));
        assert!(text.lines().count() <= LINES + 1);
        assert!(text.len() <= HEAD + TAIL + 200, "{}", text.len());
        let marker = text.lines().find(|l| l.starts_with("[…")).unwrap();
        assert!(marker.contains("lines /") && marker.contains(&full.display().to_string()));
    }

    #[test]
    fn one_huge_line_is_cut_too() {
        let (dir, spill) = spill();
        let mut out = Output::new(spill);
        out.push("é".repeat(100_000).as_bytes());
        let rendered = out.finish();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(rendered.truncated);
        assert!(rendered.text.len() <= HEAD + TAIL + 200);
    }

    #[test]
    fn binary_output_is_only_saved() {
        let (dir, spill) = spill();
        let mut out = Output::new(spill);
        out.push(b"\x7fELF\x00\x01");
        out.push(&[0xff; 9000]);
        let rendered = out.finish();
        let saved = std::fs::read(rendered.full.as_ref().unwrap()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(saved.len(), 9006);
        assert!(
            rendered
                .text
                .starts_with("[binary output: 9006 bytes, saved to "),
            "{}",
            rendered.text
        );
    }

    #[test]
    fn a_taken_spill_name_is_never_reused() {
        let (dir, _) = spill();
        let path = dir.join("2.log");
        let (_, first) = create_private(&path).unwrap();
        let (_, second) = create_private(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(first, path);
        assert_eq!(second, dir.join("2-1.log"));
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
