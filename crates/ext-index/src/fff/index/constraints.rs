//! Constraint-based prefiltering for search queries: extensions, globs,
//! path segments, path suffixes and exclusions.

use globset::GlobMatcher;
use smallvec::SmallVec;

use crate::fff::query::Constraint;
use crate::fff::types::Files;

/// Stored/canonical paths use `/`; also accept `\` so a Windows user typing
/// a native separator in a query still matches.
#[inline]
fn is_path_sep(b: u8) -> bool {
    b == b'/' || b == b'\\'
}

#[inline]
fn path_slice_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).all(|(x, y)| {
        if is_path_sep(*x) && is_path_sep(*y) {
            true
        } else {
            x.eq_ignore_ascii_case(y)
        }
    })
}

/// Path ends with suffix at a path-separator boundary (case-insensitive).
#[inline]
pub(crate) fn path_ends_with_suffix(path: &str, suffix: &str) -> bool {
    let path_bytes = path.as_bytes();
    let suffix_bytes = suffix.as_bytes();
    if path_bytes.len() < suffix_bytes.len() {
        return false;
    }

    let start = path.len() - suffix.len();

    // Multi-byte UTF-8 may put `start` inside a char.
    if !path.is_char_boundary(start) {
        return false;
    }

    if !path_slice_eq(&path_bytes[start..], suffix_bytes) {
        return false;
    }

    // Exact or preceded by a separator. Scan backward past any multi-byte
    // continuation bytes to find the preceding ASCII byte.
    if start == 0 {
        return true;
    }
    let mut i = start;
    while i > 0 {
        i -= 1;
        if path_bytes[i] < 128 {
            return is_path_sep(path_bytes[i]);
        }
    }
    false
}

#[inline]
pub(crate) fn file_has_extension(file_name: &str, ext: &str) -> bool {
    let name_bytes = file_name.as_bytes();
    let ext_bytes = ext.as_bytes();
    if name_bytes.len() <= ext_bytes.len() + 1 {
        return false;
    }
    let start = name_bytes.len() - ext_bytes.len() - 1;
    if start > 0 && !file_name.is_char_boundary(start) {
        return false;
    }
    name_bytes.get(start) == Some(&b'.') && name_bytes[start + 1..].eq_ignore_ascii_case(ext_bytes)
}

/// Matches multi-segment queries like `libswscale/aarch64`.
#[inline]
pub(crate) fn path_contains_segment(path: &str, segment: &str) -> bool {
    let path_bytes = path.as_bytes();
    let segment_bytes = segment.as_bytes();
    let segment_len = segment_bytes.len();

    if path_bytes.len() > segment_len
        && is_path_sep(path_bytes[segment_len])
        && path.is_char_boundary(segment_len)
        && path_slice_eq(&path_bytes[..segment_len], segment_bytes)
    {
        return true;
    }

    if path_bytes.len() < segment_len + 2 {
        return false;
    }

    for i in 0..path_bytes.len().saturating_sub(segment_len + 1) {
        if is_path_sep(path_bytes[i]) {
            let start = i + 1;
            let end = start + segment_len;
            if end < path_bytes.len()
                && is_path_sep(path_bytes[end])
                && path.is_char_boundary(start)
                && path.is_char_boundary(end)
                && path_slice_eq(&path_bytes[start..end], segment_bytes)
            {
                return true;
            }
        }
    }
    false
}

/// The slots of `slots` whose files pass `constraints`; `None` when
/// there are none, so every file passes.
///
/// Constraint semantics:
/// - All `Extension` constraints OR together (file matches if ANY extension hits).
/// - Every other constraint kind ANDs (file matches only if ALL hold),
///   evaluated in order with short-circuit on first failure.
pub(crate) fn apply_constraints(
    files: &Files,
    slots: impl Iterator<Item = usize>,
    constraints: &[Constraint<'_>],
) -> Option<Vec<usize>> {
    if constraints.is_empty() {
        return None;
    }
    let plan = ConstraintPlan::build(constraints);
    let items = files.items();
    Some(
        slots
            .filter(|&slot| plan.matches(files.path(&items[slot]), files.name(&items[slot])))
            .collect(),
    )
}

/// Preprocessed constraints for the per-file evaluator. Globs are
/// compiled once, in the order the evaluator meets them.
pub(crate) struct ConstraintPlan<'q, 'c> {
    /// OR semantics — file passes if ANY extension matches. Empty = no ext filter.
    extensions: SmallVec<[&'q str; 8]>,
    /// AND semantics — file passes only if ALL match.
    rest: SmallVec<[&'c Constraint<'q>; 8]>,
    /// `None` slot = compile failure -> never matches; preserves index alignment.
    globs: Vec<Option<GlobMatcher>>,
}

impl<'q, 'c> ConstraintPlan<'q, 'c> {
    pub(crate) fn build(constraints: &'c [Constraint<'q>]) -> Self {
        let mut extensions = SmallVec::new();
        let mut rest: SmallVec<[&'c Constraint<'q>; 8]> = SmallVec::new();
        for c in constraints {
            match c {
                Constraint::Extension(ext) => extensions.push(*ext),
                _ => rest.push(c),
            }
        }
        let mut globs = Vec::new();
        for c in &rest {
            walk_globs(c, &mut |pattern| globs.push(compile_one(pattern)));
        }
        Self {
            extensions,
            rest,
            globs,
        }
    }

    /// Whether the file at relative `path`, named `name`, passes.
    #[inline]
    pub(crate) fn matches(&self, path: &str, name: &str) -> bool {
        if !self.extensions.is_empty()
            && !self
                .extensions
                .iter()
                .any(|ext| file_has_extension(name, ext))
        {
            return false;
        }
        let mut glob_idx = 0;
        self.rest
            .iter()
            .all(|c| self.evaluate(path, name, c, &mut glob_idx, false))
    }

    fn evaluate(
        &self,
        path: &str,
        name: &str,
        constraint: &Constraint<'_>,
        glob_idx: &mut usize,
        negate: bool,
    ) -> bool {
        let raw = match constraint {
            Constraint::Glob(_) => {
                let m = self
                    .globs
                    .get(*glob_idx)
                    .and_then(Option::as_ref)
                    .is_some_and(|p| p.is_match(path));
                *glob_idx += 1;
                m
            }
            // Reachable only via `Not(Extension(_))` — bare extensions are split out
            // up front.
            Constraint::Extension(ext) => file_has_extension(name, ext),
            Constraint::PathSegment(segment) => path_contains_segment(path, segment),
            Constraint::FilePath(suffix) => path_ends_with_suffix(path, suffix),
            // Only meaningful under negation (used as exclude filter).
            Constraint::Text(text) => contains_ignore_ascii_case(path, text),
            Constraint::Not(inner) => {
                return self.evaluate(path, name, inner, glob_idx, !negate);
            }
            // Pass-throughs — handled at higher levels.
            Constraint::FileType(_) => true,
        };
        if negate { !raw } else { raw }
    }
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

/// Visit every Glob (including ones nested under Not) in constraint walk order.
/// Order matters: `glob_idx` in the per-item evaluator increments by one per Glob node.
fn walk_globs<F: FnMut(&str)>(c: &Constraint<'_>, f: &mut F) {
    match c {
        Constraint::Glob(p) => f(p),
        Constraint::Not(inner) => walk_globs(inner, f),
        _ => {}
    }
}

pub(crate) fn compile_one(pattern: &str) -> Option<GlobMatcher> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(pattern.contains('/'))
        .build()
        .ok()
        .map(|glob| glob.compile_matcher())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passing<'a>(paths: &[&'a str], constraints: &[Constraint<'_>]) -> Vec<&'a str> {
        let files = Files::walked(
            paths
                .iter()
                .map(|path| crate::fff::types::Found {
                    path: (*path).to_owned(),
                    size: 1,
                    modified: 0,
                })
                .collect(),
        );
        let passed = apply_constraints(&files, 0..files.items().len(), constraints).unwrap();
        let mut found: Vec<_> = passed
            .into_iter()
            .map(|slot| {
                let path = files.path(&files.items()[slot]);
                *paths.iter().find(|p| **p == path).unwrap()
            })
            .collect();
        found.sort_by_key(|path| paths.iter().position(|p| p == path));
        found
    }

    #[test]
    fn test_file_has_extension() {
        assert!(file_has_extension("file.rs", "rs"));
        assert!(file_has_extension("file.RS", "rs")); // case-insensitive
        assert!(file_has_extension("file.test.rs", "rs"));
        assert!(file_has_extension("a.rs", "rs"));

        assert!(!file_has_extension("file.tsx", "rs"));
        assert!(!file_has_extension("rs", "rs")); // too short
        assert!(!file_has_extension(".rs", "rs")); // just extension
        assert!(!file_has_extension("file.rsx", "rs")); // different extension
        assert!(!file_has_extension("filers", "rs")); // no dot
    }

    #[test]
    fn test_path_contains_segment() {
        // Segment at start
        assert!(path_contains_segment("src/lib.rs", "src"));
        assert!(path_contains_segment("SRC/lib.rs", "src")); // case-insensitive

        // Segment in middle
        assert!(path_contains_segment("app/src/lib.rs", "src"));
        assert!(path_contains_segment("app/SRC/lib.rs", "src"));

        // Multiple levels
        assert!(path_contains_segment("core/workflow/src/main.rs", "src"));
        assert!(path_contains_segment(
            "core/workflow/src/main.rs",
            "workflow"
        ));
        assert!(path_contains_segment("core/workflow/src/main.rs", "core"));

        // Should not match partial segments
        assert!(!path_contains_segment("source/lib.rs", "src"));
        assert!(!path_contains_segment("mysrc/lib.rs", "src"));

        // Should not match filename
        assert!(!path_contains_segment("lib/src", "src"));

        // Multi-segment constraints
        assert!(path_contains_segment(
            "libswscale/aarch64/input.S",
            "libswscale/aarch64"
        ));
        assert!(path_contains_segment(
            "foo/libswscale/aarch64/input.S",
            "libswscale/aarch64"
        ));
        assert!(path_contains_segment(
            "foo/LibSwscale/AArch64/input.S",
            "libswscale/aarch64"
        )); // case-insensitive
        assert!(!path_contains_segment(
            "xlibswscale/aarch64/input.S",
            "libswscale/aarch64"
        )); // partial match at start
        assert!(!path_contains_segment(
            "foo/libswscale/aarch64x/input.S",
            "libswscale/aarch64"
        )); // partial match at end
        assert!(path_contains_segment(
            "crates/fff-core/src/grep.rs",
            "fff-core/src"
        ));

        // Edge cases
        assert!(!path_contains_segment("", "src"));
        assert!(!path_contains_segment("src", "src")); // no trailing slash
    }

    #[test]
    fn test_path_contains_segment_accepts_backslash() {
        assert!(path_contains_segment("src\\lib.rs", "src"));
        assert!(path_contains_segment(
            "app\\modules\\src\\services\\x.lua",
            "src"
        ));
        assert!(path_contains_segment("app\\SRC\\x.lua", "src"));

        assert!(path_contains_segment(
            "foo\\libswscale\\aarch64\\input.S",
            "libswscale/aarch64"
        ));
        assert!(path_contains_segment(
            "crates\\fff-core\\src\\grep.rs",
            "fff-core/src"
        ));

        assert!(!path_contains_segment("mysrc\\lib.rs", "src"));
        assert!(!path_contains_segment(
            "xlibswscale\\aarch64\\in.S",
            "libswscale/aarch64"
        ));
    }

    #[test]
    fn test_path_ends_with_suffix() {
        // Exact match
        assert!(path_ends_with_suffix(
            "libswscale/input.c",
            "libswscale/input.c"
        ));

        // Suffix match at / boundary
        assert!(path_ends_with_suffix(
            "foo/libswscale/input.c",
            "libswscale/input.c"
        ));

        // Deep nesting
        assert!(path_ends_with_suffix(
            "a/b/c/libswscale/input.c",
            "libswscale/input.c"
        ));

        // No boundary — partial directory name
        assert!(!path_ends_with_suffix(
            "xlibswscale/input.c",
            "libswscale/input.c"
        ));

        // Case insensitive
        assert!(path_ends_with_suffix(
            "foo/LibSwscale/Input.C",
            "libswscale/input.c"
        ));

        // Single file name
        assert!(path_ends_with_suffix("input.c", "input.c"));
        assert!(!path_ends_with_suffix("xinput.c", "input.c"));

        // Suffix longer than path
        assert!(!path_ends_with_suffix("input.c", "foo/input.c"));

        // Simple path
        assert!(path_ends_with_suffix("src/main.rs", "src/main.rs"));
        assert!(path_ends_with_suffix("crates/src/main.rs", "src/main.rs"));
    }

    #[test]
    fn test_path_ends_with_suffix_accepts_backslash() {
        assert!(path_ends_with_suffix(
            "app\\modules\\src\\services\\handler.lua",
            "services/handler.lua"
        ));
        assert!(path_ends_with_suffix(
            "foo\\libswscale\\input.c",
            "libswscale/input.c"
        ));
        assert!(!path_ends_with_suffix(
            "xlibswscale\\input.c",
            "libswscale/input.c"
        ));
    }

    #[test]
    fn test_path_ends_with_suffix_does_not_panic_on_unicode_suffix() {
        assert!(!path_ends_with_suffix("유니코드_파일_테스트.csv", "트.c"));
        assert!(path_ends_with_suffix(
            "data/유니코드_파일_테스트.csv",
            "유니코드_파일_테스트.csv"
        ));
    }

    #[test]
    fn test_path_ends_with_suffix_unicode_apostrophe_mismatch() {
        assert!(!path_ends_with_suffix(
            "dir/\u{2019}bar/file.txt",
            "'bar/file.txt"
        ));
    }

    #[test]
    fn test_path_ends_with_suffix_unicode_space_mismatch() {
        assert!(!path_ends_with_suffix(
            "dir/\u{202f}am/file.txt",
            " am/file.txt"
        ));
    }

    #[test]
    fn test_path_contains_segment_does_not_panic_on_unicode_segment() {
        assert!(!path_contains_segment("문서/notes.txt", "문x"));
        assert!(path_contains_segment("프로젝트/문서/notes.txt", "문서"));
    }

    #[test]
    fn test_path_contains_segment_unicode_no_panic() {
        assert!(!path_contains_segment(
            "Library/Cloud/Project\u{2019}s Folder/books.ttl",
            "Project's Folder"
        ));
    }

    #[test]
    fn test_file_has_extension_unicode_no_panic() {
        assert!(!file_has_extension("cat\u{00e9}.rs", "s"));
    }

    #[test]
    fn test_file_has_extension_unicode_filename() {
        assert!(file_has_extension("운영-가이드.md", "md"));
        assert!(file_has_extension("테스트.csv", "csv"));
        assert!(!file_has_extension("테스트.csv", "md"));
    }

    #[test]
    fn test_apply_constraints_file_path_with_unicode_suffix() {
        let paths = ["data/유니코드_파일_테스트.csv"];
        let exact = [Constraint::FilePath("유니코드_파일_테스트.csv")];
        assert_eq!(passing(&paths, &exact).len(), 1);
        let mismatch = [Constraint::FilePath("트.c")];
        assert_eq!(passing(&paths, &mismatch), [""; 0]);
    }

    #[test]
    fn test_unicode_path_no_panic_real_korean_cases() {
        // Real Korean paths that caused panics
        let path1 = "Downloads/(커리큘럼) hermes agent_정승현님 - 1차 커리큘럼 (강사님 작성).csv";
        let path2 = "hermes-agent-lecture-materials/세부_커리큘럼_최종.csv";
        let path3 = "projects/fastcampus-hermes-agent-curriculum/chapters/part-02-Hermes-설치-및-기본-사용/section-02-doctor로-설치-상태-검증/research/03-fix가-자동-수정하는-것과-못하는-것.md";

        // These must not panic regardless of segment/suffix used
        assert!(!path_contains_segment(path1, "작성"));
        assert!(!path_ends_with_suffix(path1, "작성.csv"));
        assert!(!path_contains_segment(path2, "최종"));
        assert!(!path_ends_with_suffix(path2, "최종.csv"));
        assert!(!path_contains_segment(path3, "수정"));
        assert!(!path_ends_with_suffix(path3, "것.md"));

        // Positive cases should still work
        assert!(path_contains_segment(
            path2,
            "hermes-agent-lecture-materials"
        ));
        assert!(path_ends_with_suffix(
            path1,
            "(커리큘럼) hermes agent_정승현님 - 1차 커리큘럼 (강사님 작성).csv"
        ));
        assert!(path_ends_with_suffix(path2, "세부_커리큘럼_최종.csv"));
    }

    #[test]
    fn test_negated_glob_excludes_matching_files() {
        let paths = ["src/main.rs", "src/lib.ts", "include/fff.h"];
        // Not(Glob("**/*.rs")) should exclude .rs files
        let constraints = [Constraint::Not(Box::new(Constraint::Glob("**/*.rs")))];
        assert_eq!(
            passing(&paths, &constraints),
            ["src/lib.ts", "include/fff.h"]
        );
    }

    #[test]
    fn literal_punctuation_matches_with_and_without_an_extension() {
        for (pattern, literal, neighbor) in [
            ("{a[,]b.rs,a[,]b.rs/**}", "a,b.rs", "axb.rs"),
            (
                "{pkg/[{]a[,]b[}],pkg/[{]a[,]b[}]/**}",
                "pkg/{a,b}/a.rs",
                "pkg/XaXbY/a.rs",
            ),
        ] {
            let compiled = compile_one(pattern).unwrap();
            assert!(compiled.is_match(literal), "{pattern}");
            assert!(!compiled.is_match(neighbor), "{pattern}");
            for constraints in [
                vec![Constraint::Glob(pattern)],
                vec![Constraint::Glob(pattern), Constraint::Extension("rs")],
            ] {
                assert_eq!(
                    passing(&[literal, neighbor], &constraints),
                    [literal],
                    "{pattern}"
                );
            }
        }
    }

    #[test]
    fn path_glob_boundaries_match_negated_or_not() {
        let paths = [
            "src/a.rs",
            "src/foo.rs",
            "src/nested/foo.rs",
            "src/deep/nested/foo.rs",
            "lib/foo.rs",
            "lib/nested/foo.rs",
            "src/a,b.rs",
            "src/nested/a,b.rs",
            "src/.hidden/foo.rs",
        ];
        for (pattern, expected) in [
            ("src/*.rs", vec![0, 1, 6]),
            ("src/?.rs", vec![0]),
            ("src/*/foo.rs", vec![2, 8]),
            ("src/**/*.rs", vec![0, 1, 2, 3, 6, 7, 8]),
            ("src/**/", vec![]),
            ("**/[^a].rs", vec![]),
            ("**/[^b].rs", vec![0]),
            ("src/*a[,]b.rs", vec![6]),
            ("{src/*.rs,lib/**}", vec![0, 1, 4, 5, 6]),
            ("{src,lib}/*.rs", vec![0, 1, 4, 6]),
            ("**/*.{rs,ts}", (0..paths.len()).collect()),
            ("*.rs", (0..paths.len()).collect()),
        ] {
            for negate in [false, true] {
                let constraint = if negate {
                    Constraint::Not(Box::new(Constraint::Glob(pattern)))
                } else {
                    Constraint::Glob(pattern)
                };
                let expected_paths: Vec<_> = paths
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| expected.contains(index) != negate)
                    .map(|(_, path)| *path)
                    .collect();
                assert_eq!(
                    passing(&paths, &[constraint]),
                    expected_paths,
                    "{pattern}, negate={negate}"
                );
            }
        }
    }

    #[test]
    fn test_glob_with_extension() {
        let paths = [
            "src/main.rs",
            "src/lib.ts",
            "tests/foo.rs",
            "docs/readme.md",
        ];
        let mixed = [Constraint::Extension("rs"), Constraint::Glob("src/**")];
        assert_eq!(passing(&paths, &mixed), ["src/main.rs"]);
        let pure_glob = [Constraint::Glob("src/**")];
        assert_eq!(passing(&paths, &pure_glob), ["src/main.rs", "src/lib.ts"]);
    }

    #[test]
    fn test_negated_glob_with_extension() {
        let paths = ["src/main.rs", "vendor/foo.rs", "vendor/foo.ts"];
        let constraints = [
            Constraint::Extension("rs"),
            Constraint::Not(Box::new(Constraint::Glob("vendor/**"))),
        ];
        assert_eq!(passing(&paths, &constraints), ["src/main.rs"]);
    }
}
