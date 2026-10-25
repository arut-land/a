use aho_corasick::AhoCorasick;
use memchr::memmem::Finder;
use smallvec::SmallVec;

use super::fuzzy_grep::FuzzyGrep;
use super::regex::{RegexMatcher, RegexSink, build_regex};
use super::sink::{SinkState, debug_assert_newline_terminator};
use super::types::{GrepMatch, GrepMode, GrepOptions};
use crate::fff::query::{FFFQuery, FuzzyQuery, GrepConfig, QueryParser};
use crate::fff::searcher::{
    LineTerminator, Searcher, SearcherBuilder, Sink, SinkMatch,
    matcher::{Match, Matcher, NoError},
};

/// A literal needle, found with or without ASCII case.
pub(super) enum NeedleFinder {
    CaseSensitive(Box<Finder<'static>>),
    /// The lowered needle, and an ASCII-case-insensitive searcher for it.
    CaseInsensitive(AhoCorasick, Vec<u8>),
}

impl NeedleFinder {
    fn new(needle: &[u8], case_insensitive: bool) -> Self {
        if !case_insensitive {
            return Self::CaseSensitive(Box::new(Finder::new(needle).into_owned()));
        }
        let lowered = needle.to_ascii_lowercase();
        let searcher = AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build([&lowered])
            .expect("one literal pattern always builds");
        Self::CaseInsensitive(searcher, lowered)
    }

    #[inline]
    fn find(&self, haystack: &[u8]) -> Option<usize> {
        match self {
            Self::CaseSensitive(finder) => finder.find(haystack),
            Self::CaseInsensitive(searcher, _) => searcher.find(haystack).map(|m| m.start()),
        }
    }

    #[inline]
    fn needle(&self) -> &[u8] {
        match self {
            Self::CaseSensitive(finder) => finder.needle(),
            Self::CaseInsensitive(_, needle) => needle,
        }
    }

    /// Compare `haystack` against a slice of the needle with the same case
    /// semantics as `find`.
    #[inline]
    fn eq_fold(&self, haystack: &[u8], needle_seg: &[u8]) -> bool {
        match self {
            Self::CaseSensitive(_) => haystack == needle_seg,
            Self::CaseInsensitive(..) => haystack.eq_ignore_ascii_case(needle_seg),
        }
    }

    /// Collect highlight spans for every needle occurrence within a line.
    #[inline]
    fn for_each_occurrence(&self, haystack: &[u8], mut on_match: impl FnMut(usize)) {
        let mut start_pos = 0usize;
        while let Some(pos) = self.find(&haystack[start_pos..]) {
            on_match(start_pos + pos);
            start_pos += pos + 1;
        }
    }
}

struct PlainTextMatcher<'a> {
    finder: &'a NeedleFinder,
}

impl Matcher for PlainTextMatcher<'_> {
    type Error = NoError;

    #[inline]
    fn find_at(&self, haystack: &[u8], at: usize) -> Result<Option<Match>, NoError> {
        let hay = &haystack[at..];
        let needle_len = self.finder.needle().len();

        Ok(self
            .finder
            .find(hay)
            .map(|pos| Match::new(at + pos, at + pos + needle_len)))
    }

    #[inline]
    fn line_terminator(&self) -> Option<LineTerminator> {
        Some(LineTerminator::byte(b'\n'))
    }
}

struct PlainTextSink<'r> {
    state: SinkState,
    finder: &'r NeedleFinder,
    pattern_len: u32,
    multiline_segment_len: Option<usize>,
}

impl Sink for PlainTextSink<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        searcher: &Searcher,
        sink_match: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        debug_assert_newline_terminator(searcher);
        let line_bytes = sink_match.bytes();
        let (display_bytes, display_len, line_number, byte_offset) =
            SinkState::prepare_line(line_bytes, sink_match);

        let line_content = String::from_utf8_lossy(display_bytes);
        let mut match_byte_offsets: SmallVec<[(u32, u32); 4]> = SmallVec::new();
        let mut col = 0usize;
        let mut first = true;

        if let Some(seg_len) = self.multiline_segment_len {
            // Multiline needle: the match starts on this line, so the needle's
            // first segment must be a suffix of the line. Highlight that suffix.
            let seg = &self.finder.needle()[..seg_len];
            if !seg.is_empty()
                && display_bytes.len() >= seg.len()
                && self
                    .finder
                    .eq_fold(&display_bytes[display_bytes.len() - seg.len()..], seg)
            {
                col = display_bytes.len() - seg.len();
                match_byte_offsets.push((col as u32, display_len));
            }
        } else {
            let pattern_len = self.pattern_len;
            self.finder.for_each_occurrence(display_bytes, |pos| {
                let abs_start = pos as u32;
                let abs_end = (abs_start + pattern_len).min(display_len);
                if first {
                    col = pos;
                    first = false;
                }
                match_byte_offsets.push((abs_start, abs_end));
            });
        }

        let (context_before, context_after) = self.state.extract_context(sink_match);
        Ok(self.state.push_match(
            line_number,
            col,
            byte_offset,
            line_content.into_owned(),
            match_byte_offsets,
            context_before,
            context_after,
        ))
    }
}

enum Pattern {
    Plain {
        finder: NeedleFinder,
        multiline_segment_len: Option<usize>,
    },
    Regex(regex::bytes::Regex),
    Fuzzy(Box<FuzzyGrep>),
}

/// What the content index is asked for a grep's candidates.
pub(crate) enum Prefilter<'a> {
    Literal(&'a str),
    Regex(&'a str),
    Fuzzy(&'a str),
}

/// One grep, its pattern compiled once, that searches any file it is
/// given: what every blocking job of a search shares.
pub(crate) struct Grep {
    pattern: Pattern,
    /// The pattern as searched for: `\n` escapes made newlines.
    text: String,
    searcher: Searcher,
    options: GrepOptions,
    after_context: usize,
    /// Why a regex fell back to literal matching.
    regex_error: Option<String>,
}

impl Grep {
    /// The grep of `query`'s text, or `None` when it has none.
    pub(crate) fn new(query: &FFFQuery<'_>, options: GrepOptions) -> Option<Self> {
        let grep_text = extract_grep_text(query);
        if grep_text.is_empty() {
            return None;
        }
        let case_insensitive = options.casing.is_insensitive_for(&grep_text);

        if options.mode == GrepMode::Fuzzy {
            return Some(Self {
                pattern: Pattern::Fuzzy(Box::new(FuzzyGrep::new(
                    &grep_text,
                    case_insensitive,
                    options.max_matches_per_file,
                ))),
                text: grep_text,
                searcher: Searcher::new(),
                options,
                after_context: 0,
                regex_error: None,
            });
        }

        let (regex, regex_error) = match options.mode {
            GrepMode::Regex => match build_regex(&grep_text, options.casing) {
                Ok(regex) => (Some(regex), None),
                Err(err) => (None, Some(err)),
            },
            GrepMode::Plain | GrepMode::Fuzzy => (None, None),
        };

        let (multiline_segment_len, effective_pattern) = match replace_newline_escapes(&grep_text) {
            Some((replaced, first_newline_pos)) => (Some(first_newline_pos), replaced),
            None => (None, grep_text),
        };
        let is_multiline = multiline_segment_len.is_some();

        // when there is multiple line requested automatically expand the context to include all the lines
        let after_context = if is_multiline && regex.is_none() && options.after_context == 0 {
            effective_pattern.bytes().filter(|&b| b == b'\n').count()
        } else {
            options.after_context
        };

        let searcher = SearcherBuilder::new()
            .line_number(true)
            .multi_line(is_multiline)
            .build();
        let pattern = match regex {
            Some(regex) => Pattern::Regex(regex),
            None => Pattern::Plain {
                finder: NeedleFinder::new(effective_pattern.as_bytes(), case_insensitive),
                multiline_segment_len,
            },
        };
        Some(Self {
            pattern,
            text: effective_pattern,
            searcher,
            options,
            after_context,
            regex_error,
        })
    }

    /// Why the regex asked for was searched as literal text instead.
    pub(crate) fn regex_error(&self) -> Option<&str> {
        self.regex_error.as_deref()
    }

    pub(crate) fn prefilter(&self) -> Prefilter<'_> {
        match self.pattern {
            Pattern::Plain { .. } => Prefilter::Literal(&self.text),
            Pattern::Regex(_) => Prefilter::Regex(&self.text),
            Pattern::Fuzzy(_) => Prefilter::Fuzzy(&self.text),
        }
    }

    /// Every match in a file's content.
    pub(crate) fn file(&self, content: &[u8]) -> Vec<GrepMatch> {
        let state = SinkState {
            matches: Vec::with_capacity(4),
            max_matches: self.options.max_matches_per_file,
            before_context: self.options.before_context,
            after_context: self.after_context,
        };
        match &self.pattern {
            Pattern::Fuzzy(fuzzy) => fuzzy.file(content),
            Pattern::Regex(re) => {
                let matcher = RegexMatcher {
                    regex: re,
                    is_multiline: self.searcher.multi_line(),
                };
                let mut sink = RegexSink { state, re };
                // The sinks never fail and the searcher only reads the slice.
                let _ = self.searcher.search_slice(&matcher, content, &mut sink);
                sink.state.matches
            }
            Pattern::Plain {
                finder,
                multiline_segment_len,
            } => {
                // A whole-file check first: most candidates cannot match,
                // and this skips splitting them into lines.
                if finder.find(content).is_none() {
                    return Vec::new();
                }
                let matcher = PlainTextMatcher { finder };
                let mut sink = PlainTextSink {
                    state,
                    finder,
                    pattern_len: finder.needle().len() as u32,
                    multiline_segment_len: *multiline_segment_len,
                };
                let _ = self.searcher.search_slice(&matcher, content, &mut sink);
                sink.state.matches
            }
        }
    }
}

/// Replace unescaped `\n` escapes with real newlines in a single pass.
///
/// Returns `Some((replaced, first_newline_pos))` when the pattern contained at
/// least one real `\n` escape (the user wants multiline search), where
/// `first_newline_pos` is the byte offset of the first inserted newline in the
/// replaced string. Returns `None` when nothing had to be replaced: `\\n` is
/// preserved as-is (escaped backslash + literal `n`, e.g. `\\nvim-data`).
pub(super) fn replace_newline_escapes(text: &str) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut first_newline_pos: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'n' {
                // Odd number of consecutive backslashes before 'n' -> real \n escape
                let mut backslash_count = 1;
                while backslash_count <= i && bytes[i - backslash_count] == b'\\' {
                    backslash_count += 1;
                }
                if backslash_count % 2 == 1 {
                    first_newline_pos.get_or_insert(result.len());
                    result.push(b'\n');
                    i += 2;
                    continue;
                }
            }
            result.push(bytes[i]);
            i += 1;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }

    let first_newline_pos = first_newline_pos?;
    let replaced = String::from_utf8(result).unwrap_or_else(|_| text.to_string());
    Some((replaced, first_newline_pos))
}

/// A grep query as a person types one: constraints are path globs and
/// segments, never the `?` and `[` code is full of.
pub(crate) fn parse_grep_query(query: &str) -> FFFQuery<'_> {
    QueryParser::new(GrepConfig).parse(query)
}

/// Extract the grep pattern text from the parsed query: all non-constraint
/// tokens joined with spaces, e.g. `"name = *.rs someth"` -> `"name = someth"`
/// with constraint `Extension("rs")`.
fn extract_grep_text(query: &FFFQuery<'_>) -> String {
    if !matches!(query.fuzzy_query, FuzzyQuery::Empty) {
        return query.grep_text();
    }

    // if constraint-only or empty query we use raw_query for backslash-escape handling
    let t = query.raw_query.trim();
    if t.starts_with('\\') && t.len() > 1 {
        let suffix = &t[1..];
        let parser = QueryParser::new(GrepConfig);
        if !parser.parse(suffix).constraints.is_empty() {
            return suffix.to_string();
        }
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grep(query: &str, options: GrepOptions) -> Grep {
        Grep::new(&parse_grep_query(query), options).unwrap()
    }

    #[test]
    fn sinks_stop_when_the_retained_match_limit_is_reached() {
        let options = GrepOptions {
            max_matches_per_file: 1,
            ..GrepOptions::default()
        };
        let found = grep("Needle", options).file(b"Needle();\nfn Needle() {}\nNeedle();\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, 1);
    }

    #[test]
    fn test_replace_newline_escapes() {
        // Single \n → multiline: replaced with a real newline at byte 3
        assert_eq!(
            replace_newline_escapes("foo\\nbar"),
            Some(("foo\nbar".to_string(), 3))
        );
        // \\n → escaped backslash + literal n, NOT multiline
        // (this is what the user types when grepping Rust source with `\\nvim`)
        assert_eq!(replace_newline_escapes("foo\\\\nvim-data"), None);
        // Real-world: source file has literal \\AppData\\Local\\nvim-data
        // (double backslash in the file, so user types double backslash)
        assert_eq!(
            replace_newline_escapes(r#"format!("{}\\AppData\\Local\\nvim-data","#),
            None
        );
        // No \n at all
        assert_eq!(replace_newline_escapes("hello world"), None);
        // \\\\n → even number of backslashes before n → NOT multiline
        assert_eq!(replace_newline_escapes("foo\\\\\\\\nbar"), None);
        // \\\n → 3 backslashes: first two pair up, third + n = \n → multiline,
        // newline lands after "foo" + 2 kept backslashes = byte 5
        assert_eq!(
            replace_newline_escapes("foo\\\\\\nbar"),
            Some(("foo\\\\\nbar".to_string(), 5))
        );
        // Position is for the FIRST newline when there are several
        assert_eq!(
            replace_newline_escapes("a\\nb\\nc"),
            Some(("a\nb\nc".to_string(), 1))
        );
    }

    #[test]
    fn multiline_and_escaped_newline_patterns_match_as_typed() {
        let multi = b"hello unicorn\nrainbow world\n";
        let winpath = b"let p = \"C:\\\\Users\\\\nvim-data\";\n";
        let options = GrepOptions {
            max_matches_per_file: 0,
            ..GrepOptions::default()
        };

        // PlainText + `\n`: needle becomes a real newline, match spans two lines
        let plain = grep("unicorn\\nrainbow", options);
        let found = plain.file(multi);
        assert_eq!(found[0].text, "hello unicorn");
        // Auto after-context: the rest of the matched span is returned
        assert_eq!(found[0].after, ["rainbow world"]);
        // First needle segment highlighted as the line suffix
        assert_eq!(found[0].ranges.as_slice(), [(6, 13)]);
        assert_eq!(found[0].col, 6);
        assert_eq!(plain.file(winpath), []);

        // PlainText + `\\n`: literal backslash + n, must NOT be mangled to newline
        let escaped = grep("\\\\nvim-data", options);
        let found = escaped.file(winpath);
        assert!(found[0].text.contains("\\\\nvim-data"));
        assert_eq!(found[0].after, [""; 0]);

        // Regex + `\n`: goes through the MultiLine searcher strategy
        let regex = grep(
            "unicorn\\nrainbow",
            GrepOptions {
                mode: GrepMode::Regex,
                ..options
            },
        );
        assert!(regex.regex_error().is_none());
        let found = regex.file(multi);
        // Blob is normalized: single-line content + remaining lines as context
        assert_eq!(found[0].text, "hello unicorn");
        assert_eq!(found[0].after, ["rainbow world"]);
        // Highlight clamped to the visible first line
        assert_eq!(found[0].ranges.as_slice(), [(6, 13)]);
    }

    #[test]
    fn an_invalid_regex_is_searched_as_its_literal_text() {
        let options = GrepOptions {
            mode: GrepMode::Regex,
            ..GrepOptions::default()
        };
        let broken = grep("call(", options);
        assert!(broken.regex_error().is_some());
        assert!(matches!(broken.prefilter(), Prefilter::Literal("call(")));
        assert_eq!(broken.file(b"x\nfn call(a)\n")[0].line, 2);
    }

    #[test]
    fn smart_case_ignores_ascii_case_until_the_pattern_has_a_capital() {
        let content = b"Needle\nneedle\nNEEDLE\n";
        let lines = |query| {
            grep(query, GrepOptions::default())
                .file(content)
                .iter()
                .map(|m| m.line)
                .collect::<Vec<_>>()
        };
        assert_eq!(lines("needle"), [1, 2, 3]);
        assert_eq!(lines("Needle"), [1]);
    }
}
