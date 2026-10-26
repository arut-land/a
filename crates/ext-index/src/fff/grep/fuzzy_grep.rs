use super::sink::strip_line_terminators;
use super::types::GrepMatch;
use crate::fff::searcher::lines::LineStep;
use smallvec::SmallVec;
use std::num::NonZeroUsize;

/// A fuzzy grep's matcher and the bounds a matched line must keep to: a
/// typo or two, a dense span, few gaps.
pub(super) struct FuzzyGrep {
    matcher: neo_frizbee::Matcher,
    max_typos: usize,
    needle_len: usize,
    min_score: u16,
    max_match_span: usize,
    max_gaps: usize,
    unique_needle_chars: Vec<u8>,
    min_chars_required: usize,
}

impl FuzzyGrep {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "slice grep receives bounded file buffers and stores compact display coordinates"
    )]
    pub(super) fn new(grep_text: &str, case_insensitive: bool) -> Self {
        // max_typos controls how many *needle* characters can be unmatched.
        // A transposition (e.g. "shcema" -> "schema") costs ~1 typo with
        // default gap penalties. We scale max_typos by needle length:
        //   1-2 chars -> 0 typos (exact subsequence only)
        //   3-5 chars -> 1 typo
        //   6+  chars -> 2 typos
        // Cap at 2: higher values (3+) let the SIMD prefilter pass lines
        // missing key characters entirely (e.g. query "flvencodeX" matching
        // lines without 'l' or 'v'). Quality comes from the post-match filters.
        let max_typos = (grep_text.len() / 3).min(2);
        let scoring = neo_frizbee::Scoring {
            // Use default gap penalties. Higher values (e.g. 20) cause
            // smith-waterman to prefer *dropping needle chars* over paying
            // gap costs, which inflates the typo count and breaks
            // transposition matching ("shcema" -> "schema" becomes 3 typos instead of 1)
            exact_match_bonus: 100,
            prefix_bonus: 0,
            capitalization_bonus: if case_insensitive { 0 } else { 4 },
            ..neo_frizbee::Scoring::default()
        };

        let matcher = neo_frizbee::Matcher::new(
            grep_text,
            &neo_frizbee::Config {
                // The real max_typos, so frizbee's prefilter rejects non-matching
                // lines before full Smith-Waterman scoring.
                max_typos: Some(max_typos as u16),
                sort: neo_frizbee::SortStrategy::Unsorted,
                scoring,
                ..Default::default()
            },
        );

        // Minimum score threshold: 50% of a perfect contiguous match.
        // With default scoring (match_score=12, matching_case_bonus=4 = 16/char),
        // a transposition costs ~5 from a gap, keeping the score well above 50%
        let perfect_score = (grep_text.len() as u16) * 16;
        let min_score = (perfect_score * 50) / 100;

        // Target identifiers are often longer than the query due to delimiters
        // (e.g. query "flvencodepicture" -> "ff_flv_encode_picture_header" from ffmpeg)
        // Allow 3x needle length to accommodate underscore/dot-separated names
        let max_match_span = grep_text.len() * 3;
        let needle_len = grep_text.len();

        // Each delimiter (_, .) in the target creates a gap. A typical C/Rust
        // identifier like "ff_flv_encode_picture_header" has 4-5 underscores.
        // Scale generously so delimiter gaps don't reject valid matches.
        let max_gaps = (needle_len / 3).max(2);

        // If a file doesn't contain enough distinct needle characters just skip it
        let needle_bytes = grep_text.as_bytes();
        let mut unique_needle_chars: Vec<u8> = Vec::new();
        for &b in needle_bytes {
            let lo = b.to_ascii_lowercase();
            let hi = b.to_ascii_uppercase();
            if !unique_needle_chars.contains(&lo) {
                unique_needle_chars.push(lo);
            }
            if lo != hi && !unique_needle_chars.contains(&hi) {
                unique_needle_chars.push(hi);
            }
        }

        // How many distinct needle chars must appear in the file.
        // With max_typos allowed, we need at least (unique_count - max_typos)
        let unique_count = {
            let mut seen = [false; 256];
            for &b in needle_bytes {
                seen[b.to_ascii_lowercase() as usize] = true;
            }
            seen.iter().filter(|&&v| v).count()
        };
        let min_chars_required = unique_count.saturating_sub(max_typos);

        Self {
            matcher,
            max_typos,
            needle_len,
            min_score,
            max_match_span,
            max_gaps,
            unique_needle_chars,
            min_chars_required,
        }
    }

    /// The lines of `file_bytes` the needle fuzzily matches.
    pub(super) fn file(
        &self,
        file_bytes: &[u8],
        max_matches_per_file: Option<NonZeroUsize>,
    ) -> Vec<GrepMatch> {
        if self.min_chars_required > 0 {
            let found = self
                .unique_needle_chars
                .iter()
                .filter(|&&ch| memchr::memchr(ch, file_bytes).is_some())
                .take(self.min_chars_required)
                .count();
            if found < self.min_chars_required {
                return Vec::new();
            }
        }

        // Validate the whole file as UTF-8 once: lines split at `\n` are
        // then valid UTF-8 too, with no per-line check.
        let file_text = std::str::from_utf8(file_bytes).ok();

        let mut stepper = LineStep::new(b'\n', 0, file_bytes.len());
        let estimated_lines = (file_bytes.len() / 40).max(64);
        let mut file_lines: Vec<&str> = Vec::with_capacity(estimated_lines);
        let mut line_meta: Vec<(u64, u64)> = Vec::with_capacity(estimated_lines);

        let mut line_number: u64 = 1;
        while let Some(line_match) = stepper.next_match(file_bytes) {
            let byte_offset = line_match.start() as u64;
            let start = line_match.start();
            let trimmed = strip_line_terminators(&file_bytes[line_match]);

            if !trimmed.is_empty() {
                let line_str = match file_text {
                    Some(text) => Some(&text[start..start + trimmed.len()]),
                    None => std::str::from_utf8(trimmed).ok(),
                };
                if let Some(line_str) = line_str {
                    file_lines.push(line_str);
                    line_meta.push((line_number, byte_offset));
                }
            }

            line_number += 1;
        }

        if file_lines.is_empty() {
            return Vec::new();
        }

        // Single-pass: score + indices in one Smith-Waterman run per line.
        let mut matcher = self.matcher.clone();
        let matches_with_indices = matcher.match_list_indices(&file_lines);
        let mut file_matches: Vec<GrepMatch> = Vec::new();

        for mut match_indices in matches_with_indices {
            if match_indices.score < self.min_score {
                continue;
            }

            let idx = match_indices.index as usize;
            let raw_line = file_lines[idx];

            let display_line = raw_line;

            match_indices.indices.sort_unstable();

            // Minimum matched chars: at least (needle_len - max_typos)
            // characters must appear. This is consistent with the typo
            // budget: each typo can drop one needle char from the alignment.
            let min_matched = self.needle_len.saturating_sub(self.max_typos).max(1);
            if match_indices.indices.len() < min_matched {
                continue;
            }

            let indices = &match_indices.indices;

            if let (Some(&first), Some(&last)) = (indices.first(), indices.last()) {
                // reject widely scattered matches
                let span = (last - first + 1) as usize;
                if span > self.max_match_span {
                    continue;
                }

                // Density check: matched chars / span must be dense enough.
                // Relaxed for perfect subsequence matches (all needle chars
                // present), slightly relaxed for typo matches to handle
                // delimiter-heavy targets
                // (e.g. "ff_flv_encode_picture_header" has span inflated by underscores w/ density ~68%)
                let density = (indices.len() * 100) / span;
                let min_density = if indices.len() >= self.needle_len {
                    45 // Perfect subsequence relaxed (delimiters inflate span)
                } else {
                    65 // Has typos filter out a long string
                };
                if density < min_density {
                    continue;
                }

                // Gap count check: count discontinuities in the indices
                let gap_count = indices.windows(2).filter(|w| w[1] != w[0] + 1).count();
                if gap_count > self.max_gaps {
                    continue;
                }
            }

            let (ln, bo) = line_meta[idx];
            // Frizbee returns byte indices, including every byte of a
            // matched Unicode character, in either matching mode.
            let mut ranges: SmallVec<[(u32, u32); 4]> = SmallVec::new();
            for &index in &match_indices.indices {
                if let Some(last) = ranges.last_mut()
                    && last.1 == index
                {
                    last.1 += 1;
                } else {
                    ranges.push((index, index + 1));
                }
            }
            let col = ranges.first().map_or(0, |r| r.0 as usize);

            file_matches.push(GrepMatch {
                line: ln,
                col,
                byte_offset: bo,
                text: display_line.to_string(),
                ranges,
                score: Some(match_indices.score),
                before: Vec::new(),
                after: Vec::new(),
            });

            if max_matches_per_file.is_some_and(|limit| file_matches.len() >= limit.get()) {
                break;
            }
        }

        file_matches
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transposed_needle_finds_its_line_and_unrelated_lines_do_not_match() {
        let grep = FuzzyGrep::new("shcema", true);
        let found = grep.file(b"fn validate_schema() {}\nhello world foo bar\n", None);
        let lines: Vec<_> = found.iter().map(|m| m.line).collect();
        assert_eq!(lines, [1]);
        assert!(found[0].score.is_some());
    }
}
