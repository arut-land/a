//! Large files are searched with fixed I/O buffers and bounded DFA caches.
//! Reverse searches recover a match's start by seeking, without retaining
//! the bytes between its endpoints (a regex span or line can be arbitrarily long).

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::ops::Range;

use regex_automata::{Anchored, MatchKind, hybrid::dfa, nfa::thompson, util::start};

use super::sink::{MAX_LINE_DISPLAY_LEN, excerpt, strip_line_terminators};
use super::types::{GrepMatch, GrepMode, GrepOptions};
use smallvec::SmallVec;

const BUFFER: usize = 64 << 10;

pub(super) struct StreamRegex {
    forward: dfa::DFA,
    reverse: dfa::DFA,
    multiline: bool,
}

impl StreamRegex {
    pub(super) fn new(pattern: &str, insensitive: bool, multiline: bool) -> io::Result<Self> {
        let mut hir = regex_syntax::ParserBuilder::new()
            .unicode(false)
            .utf8(false)
            .case_insensitive(insensitive)
            .multi_line(true)
            .build()
            .parse(pattern)
            .map_err(io::Error::other)?;
        if !multiline {
            hir = without_newlines(hir);
        }
        let forward_nfa = thompson::Compiler::new()
            .configure(thompson::Config::new().which_captures(thompson::WhichCaptures::None))
            .build_from_hir(&hir)
            .map_err(io::Error::other)?;
        let reverse_nfa = thompson::Compiler::new()
            .configure(
                thompson::Config::new()
                    .reverse(true)
                    .which_captures(thompson::WhichCaptures::None),
            )
            .build_from_hir(&hir)
            .map_err(io::Error::other)?;
        let forward = dfa::DFA::builder()
            .build_from_nfa(forward_nfa)
            .map_err(io::Error::other)?;
        let reverse = dfa::DFA::builder()
            .configure(dfa::Config::new().match_kind(MatchKind::All))
            .build_from_nfa(reverse_nfa)
            .map_err(io::Error::other)?;
        Ok(Self {
            forward,
            reverse,
            multiline,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the streaming cursor, match spans and display coordinates advance together"
    )]
    pub(super) fn search(
        &self,
        file: &mut File,
        limit: usize,
        options: GrepOptions,
        stopped: &dyn Fn() -> bool,
    ) -> io::Result<Vec<GrepMatch>> {
        let mut reader = BufReader::with_capacity(BUFFER, file);
        let mut forward = self.forward.create_cache();
        let mut reverse = self.reverse.create_cache();
        let mut found = Vec::new();
        let mut from = 0;
        let mut line = 1;
        loop {
            if found.len() >= limit || stopped() {
                break;
            }
            let Some(end) = self.end(&mut reader, &mut forward, from, None, stopped)? else {
                break;
            };
            let start = self.start(&mut reader, &mut reverse, from, end, stopped)?;
            if stopped() {
                break;
            }
            let (line_start, number) = locate(&mut reader, from, start, line, stopped)?;
            let display_line_end = line_end(&mut reader, start, stopped)?;
            let first_end = end;
            let end = if self.multiline && options.mode == GrepMode::Regex {
                self.adjacent(
                    &mut reader,
                    (&mut forward, &mut reverse),
                    start..end,
                    stopped,
                )?
            } else {
                end
            };
            let display = Display::read(&mut reader, start, line_start..display_line_end)?;
            let mut ranges: SmallVec<[(u32, u32); 4]> =
                [display.range(start..first_end)].into_iter().collect();
            let mut cursor = if options.mode == GrepMode::Plain {
                start + 1
            } else {
                first_end.max(start + 1)
            };
            while cursor < display.end && !stopped() {
                let Some(next_end) = self.end(
                    &mut reader,
                    &mut forward,
                    cursor,
                    Some(display.end),
                    stopped,
                )?
                else {
                    break;
                };
                let next_start =
                    self.start(&mut reader, &mut reverse, cursor, next_end, stopped)?;
                if next_start >= display.end {
                    break;
                }
                ranges.push(display.range(next_start..next_end));
                cursor = if options.mode == GrepMode::Plain {
                    next_start + 1
                } else {
                    next_end.max(next_start + 1)
                };
            }
            let (_, last_line) = locate(
                &mut reader,
                line_start,
                end.saturating_sub(1).max(start),
                number,
                stopped,
            )?;
            found.push(GrepMatch {
                line: number,
                col: usize::try_from(start - line_start).map_err(io::Error::other)?,
                byte_offset: line_start,
                text: display.text,
                ranges,
                score: None,
                before: context_before(&mut reader, line_start, options.before_context, stopped)?,
                after: context_after(
                    &mut reader,
                    display_line_end,
                    if options.mode == GrepMode::Regex {
                        usize::try_from(last_line - number).map_err(io::Error::other)?
                            + options.after_context
                    } else {
                        options.after_context
                    },
                    stopped,
                )?,
            });
            // A grep reports a matching line once, including empty regexes.
            let next = if options.mode == GrepMode::Plain {
                display_line_end
            } else {
                line_end(&mut reader, end.saturating_sub(1).max(start), stopped)?
            };
            if next == from {
                break;
            }
            let (_, next_line) = locate(&mut reader, from, next, line, stopped)?;
            from = next;
            line = next_line;
            reader.seek(SeekFrom::Start(from))?;
            if reader.fill_buf()?.is_empty() {
                break;
            }
        }
        Ok(found)
    }

    /// Slice grep coalesces adjacent multiline regex hits into one blob.
    /// Keep that behavior using offsets, without retaining the blob's bytes.
    fn adjacent(
        &self,
        reader: &mut BufReader<&mut File>,
        caches: (&mut dfa::Cache, &mut dfa::Cache),
        first: Range<u64>,
        stopped: &dyn Fn() -> bool,
    ) -> io::Result<u64> {
        let (forward, reverse) = caches;
        let mut end = first.end;
        let mut cursor = end.max(first.start + 1);
        let mut last_line_end = line_end(reader, end.saturating_sub(1).max(first.start), stopped)?;
        while !stopped() {
            let Some(next_end) = self.end(reader, forward, cursor, None, stopped)? else {
                break;
            };
            let start = self.start(reader, reverse, cursor, next_end, stopped)?;
            if start > last_line_end
                && locate(reader, last_line_end, start, 0, stopped)?.0 > last_line_end
            {
                break;
            }
            end = next_end;
            last_line_end = line_end(reader, end.saturating_sub(1).max(start), stopped)?;
            cursor = end.max(start + 1);
            if byte_at(reader, Some(cursor))?.is_none() {
                break;
            }
        }
        Ok(end)
    }

    fn end(
        &self,
        reader: &mut BufReader<&mut File>,
        cache: &mut dfa::Cache,
        from: u64,
        until: Option<u64>,
        stopped: &dyn Fn() -> bool,
    ) -> io::Result<Option<u64>> {
        let previous = byte_at(reader, from.checked_sub(1))?;
        let mut state = self
            .forward
            .start_state(cache, &start::Config::new().look_behind(previous))
            .map_err(io::Error::other)?;
        reader.seek(SeekFrom::Start(from))?;
        let mut position = from;
        let mut matched = None;
        loop {
            if stopped() {
                return Ok(None);
            }
            let bytes = reader.fill_buf()?;
            if bytes.is_empty() {
                state = self
                    .forward
                    .next_eoi_state(cache, state)
                    .map_err(io::Error::other)?;
                if state.is_match() {
                    matched = Some(position);
                }
                return Ok(matched);
            }
            let mut consumed = 0;
            for &byte in bytes {
                if until.is_some_and(|until| position > until) {
                    return Ok(matched);
                }
                state = self
                    .forward
                    .next_state(cache, state, byte)
                    .map_err(io::Error::other)?;
                if state.is_match() {
                    matched = Some(position);
                }
                if !self.multiline && byte == b'\n' {
                    if matched.is_some() {
                        return Ok(matched);
                    }
                    state = self
                        .forward
                        .start_state(cache, &start::Config::new().look_behind(Some(b'\n')))
                        .map_err(io::Error::other)?;
                } else if state.is_dead() {
                    return Ok(matched);
                }
                position += 1;
                consumed += 1;
            }
            reader.consume(consumed);
        }
    }

    fn start(
        &self,
        reader: &mut BufReader<&mut File>,
        cache: &mut dfa::Cache,
        from: u64,
        end: u64,
        stopped: &dyn Fn() -> bool,
    ) -> io::Result<u64> {
        let after = byte_at(reader, Some(end))?;
        let mut state = self
            .reverse
            .start_state(
                cache,
                &start::Config::new()
                    .anchored(Anchored::Yes)
                    .look_behind(after),
            )
            .map_err(io::Error::other)?;
        let mut position = end;
        let mut matched = end;
        let mut bytes = vec![0; BUFFER];
        while position > from && !stopped() {
            let first = position.saturating_sub(BUFFER as u64).max(from);
            let len = usize::try_from(position - first).expect("bounded I/O buffer");
            reader.seek(SeekFrom::Start(first))?;
            reader.read_exact(&mut bytes[..len])?;
            for &byte in bytes[..len].iter().rev() {
                state = self
                    .reverse
                    .next_state(cache, state, byte)
                    .map_err(io::Error::other)?;
                if state.is_match() {
                    matched = position;
                }
                if state.is_dead() {
                    return Ok(matched);
                }
                position -= 1;
            }
        }
        state = self
            .reverse
            .next_eoi_state(cache, state)
            .map_err(io::Error::other)?;
        if state.is_match() {
            matched = from;
        }
        Ok(matched)
    }
}

/// The visible part of a line and its coordinate system, including omission markers.
struct Display {
    text: String,
    start: u64,
    end: u64,
    prefix: usize,
    visible: usize,
}

impl Display {
    fn read(reader: &mut BufReader<&mut File>, matched: u64, line: Range<u64>) -> io::Result<Self> {
        let start = matched
            .saturating_sub((MAX_LINE_DISPLAY_LEN / 4) as u64)
            .max(line.start);
        let end = line.end.min(start + MAX_LINE_DISPLAY_LEN as u64);
        let mut bytes = vec![0; usize::try_from(end - start).expect("bounded excerpt")];
        reader.seek(SeekFrom::Start(start))?;
        reader.read_exact(&mut bytes)?;
        let mut left = 0;
        while left < bytes.len() && bytes[left] & 0xc0 == 0x80 {
            left += 1;
        }
        let raw = &bytes[left..];
        let right = match std::str::from_utf8(raw) {
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            _ => raw.len(),
        };
        let start = start + left as u64;
        let end = start + right as u64;
        let text = String::from_utf8_lossy(strip_line_terminators(&raw[..right]));
        let prefix = if start > line.start { "…" } else { "" };
        let suffix = if end < line.end { "…" } else { "" };
        Ok(Self {
            start,
            end,
            prefix: prefix.len(),
            visible: text.len(),
            text: format!("{prefix}{text}{suffix}"),
        })
    }

    fn range(&self, span: Range<u64>) -> (u32, u32) {
        let coordinate = |at: u64| {
            let visible = at.saturating_sub(self.start).min(self.visible as u64);
            u32::try_from(visible).expect("bounded excerpt")
                + u32::try_from(self.prefix).expect("ellipsis")
        };
        (coordinate(span.start), coordinate(span.end))
    }
}

fn byte_at(reader: &mut BufReader<&mut File>, at: Option<u64>) -> io::Result<Option<u8>> {
    let Some(at) = at else { return Ok(None) };
    reader.seek(SeekFrom::Start(at))?;
    Ok(reader.fill_buf()?.first().copied())
}

fn locate(
    reader: &mut BufReader<&mut File>,
    from: u64,
    to: u64,
    mut line: u64,
    stopped: &dyn Fn() -> bool,
) -> io::Result<(u64, u64)> {
    reader.seek(SeekFrom::Start(from))?;
    let mut position = from;
    let mut start = from;
    while position < to && !stopped() {
        let bytes = reader.fill_buf()?;
        let len = usize::try_from((to - position).min(BUFFER as u64)).expect("bounded I/O buffer");
        if len == 0 {
            break;
        }
        for newline in memchr::memchr_iter(b'\n', &bytes[..len]) {
            line += 1;
            start = position + newline as u64 + 1;
        }
        position += len as u64;
        reader.consume(len);
    }
    Ok((start, line))
}

fn line_end(
    reader: &mut BufReader<&mut File>,
    at: u64,
    stopped: &dyn Fn() -> bool,
) -> io::Result<u64> {
    reader.seek(SeekFrom::Start(at))?;
    let mut position = at;
    while !stopped() {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            break;
        }
        if let Some(end) = memchr::memchr(b'\n', bytes) {
            return Ok(position + end as u64 + 1);
        }
        let len = bytes.len();
        position += len as u64;
        reader.consume(len);
    }
    Ok(position)
}

fn context_before(
    reader: &mut BufReader<&mut File>,
    mut at: u64,
    count: usize,
    stopped: &dyn Fn() -> bool,
) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut bytes = vec![0; BUFFER];
    for _ in 0..count {
        if at == 0 || stopped() {
            break;
        }
        let end = at - 1;
        at = end;
        loop {
            let first = at.saturating_sub(BUFFER as u64);
            reader.seek(SeekFrom::Start(first))?;
            reader.read_exact(
                &mut bytes[..usize::try_from(at - first).expect("bounded I/O buffer")],
            )?;
            if let Some(nl) = memchr::memrchr(
                b'\n',
                &bytes[..usize::try_from(at - first).expect("bounded I/O buffer")],
            ) {
                at = first + nl as u64 + 1;
                break;
            }
            at = first;
            if at == 0 || stopped() {
                break;
            }
        }
        lines.push(context_line(reader, at..end)?);
    }
    lines.reverse();
    Ok(lines)
}

fn context_after(
    reader: &mut BufReader<&mut File>,
    mut at: u64,
    count: usize,
    stopped: &dyn Fn() -> bool,
) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    for _ in 0..count {
        if stopped() || byte_at(reader, Some(at))?.is_none() {
            break;
        }
        let end = line_end(reader, at, stopped)?;
        lines.push(context_line(reader, at..end)?);
        at = end;
    }
    Ok(lines)
}

fn context_line(reader: &mut BufReader<&mut File>, span: Range<u64>) -> io::Result<String> {
    reader.seek(SeekFrom::Start(span.start))?;
    let mut bytes = vec![
        0;
        usize::try_from((span.end - span.start).min(MAX_LINE_DISPLAY_LEN as u64))
            .expect("bounded excerpt")
    ];
    reader.read_exact(&mut bytes)?;
    let suffix = if span.end - span.start > bytes.len() as u64 {
        "…"
    } else {
        ""
    };
    Ok(format!(
        "{}{suffix}",
        String::from_utf8_lossy(strip_line_terminators(&bytes))
    ))
}

/// Fuzzy matching has a bounded accepted span. Overlap by that span so a
/// window boundary cannot cut an accepted match; retain one hit per line.
pub(super) fn fuzzy(
    file: &mut File,
    grep: &super::fuzzy_grep::FuzzyGrep,
    span: usize,
    limit: usize,
    stopped: &dyn Fn() -> bool,
) -> io::Result<Vec<GrepMatch>> {
    let overlap = span.max(MAX_LINE_DISPLAY_LEN);
    let mut bytes = vec![0; BUFFER.max(overlap * 2)];
    let mut retained = 0;
    let mut offset = 0u64;
    let mut line = 1;
    let mut column = 0;
    let mut last_line = None;
    let mut found = Vec::new();
    while found.len() < limit && !stopped() {
        let read = file.read(&mut bytes[retained..])?;
        let len = retained + read;
        if len == 0 {
            break;
        }
        let search_len = match std::str::from_utf8(&bytes[..len]) {
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            _ => len,
        };
        for mut hit in grep.file(&bytes[..search_len], None) {
            hit.line += line - 1;
            if last_line.is_some_and(|line| hit.line <= line) {
                continue;
            }
            last_line = Some(hit.line);
            if hit.byte_offset == 0 {
                hit.col += column;
                hit.byte_offset = offset.saturating_sub(column as u64);
                if column > 0 {
                    hit.text.insert(0, '…');
                    for (a, b) in &mut hit.ranges {
                        *a += 3;
                        *b += 3;
                    }
                }
            } else {
                hit.byte_offset += offset;
            }
            excerpt(&mut hit);
            found.push(hit);
            if found.len() >= limit {
                break;
            }
        }
        if read == 0 {
            break;
        }
        let mut advance = len.saturating_sub(overlap);
        while advance > 0 && bytes[advance] & 0xc0 == 0x80 {
            advance -= 1;
        }
        let prefix = &bytes[..advance];
        if let Some(nl) = memchr::memrchr(b'\n', prefix) {
            column = prefix.len() - nl - 1;
        } else {
            column += prefix.len();
        }
        line += memchr::memchr_iter(b'\n', prefix).count() as u64;
        offset += advance as u64;
        bytes.copy_within(advance..len, 0);
        retained = len - advance;
    }
    Ok(found)
}

// Slice search excludes cross-line matches even when a class contains LF.
// Compile that same rule into the streaming automaton, preserving text anchors.
fn without_newlines(hir: regex_syntax::hir::Hir) -> regex_syntax::hir::Hir {
    use regex_syntax::hir::{
        Class, ClassBytes, ClassBytesRange, ClassUnicode, ClassUnicodeRange, Hir, HirKind,
    };
    match hir.into_kind() {
        HirKind::Empty => Hir::empty(),
        HirKind::Look(look) => Hir::look(look),
        HirKind::Literal(lit) => {
            if lit.0.contains(&b'\n') {
                Hir::fail()
            } else {
                Hir::literal(lit.0)
            }
        }
        HirKind::Class(mut class) => {
            match &mut class {
                Class::Bytes(bytes) => {
                    bytes.difference(&ClassBytes::new([ClassBytesRange::new(b'\n', b'\n')]));
                }
                Class::Unicode(chars) => {
                    chars.difference(&ClassUnicode::new([ClassUnicodeRange::new('\n', '\n')]));
                }
            }
            Hir::class(class)
        }
        HirKind::Repetition(mut repetition) => {
            repetition.sub = Box::new(without_newlines(*repetition.sub));
            Hir::repetition(repetition)
        }
        HirKind::Capture(mut capture) => {
            capture.sub = Box::new(without_newlines(*capture.sub));
            Hir::capture(capture)
        }
        HirKind::Concat(parts) => Hir::concat(parts.into_iter().map(without_newlines).collect()),
        HirKind::Alternation(parts) => {
            Hir::alternation(parts.into_iter().map(without_newlines).collect())
        }
    }
}
