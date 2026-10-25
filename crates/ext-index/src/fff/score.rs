//! Fuzzy file search: frizbee's typo-resistant Smith-Waterman over every
//! path, then FFF's bonuses for where the match landed (the file's name,
//! an exact name, an entry point like `mod.rs`, a path-shaped query that
//! lines up with the path's end).
//!
//! FFF's frecency, git status and query-history boosts are left out: no
//! signal feeds them here yet.

use std::path::MAIN_SEPARATOR;

use neo_frizbee::Scoring;
use smallvec::SmallVec;

use crate::fff::index::constraints::apply_constraints;
use crate::fff::match_offsets::char_indices_to_byte_offsets;
use crate::fff::query::{FFFQuery, FuzzyQuery};
use crate::fff::types::{Files, recent_first};

/// How one file scored, and why.
#[derive(Debug, Clone, Default)]
pub(crate) struct Score {
    pub(crate) total: i32,
    pub(crate) base_score: i32,
    pub(crate) filename_bonus: i32,
    pub(crate) special_filename_bonus: i32,
    pub(crate) path_alignment_bonus: i32,
    pub(crate) exact_match: bool,
    pub(crate) match_type: &'static str,
}

/// The best `limit` files for a query, and how many matched at all.
#[derive(Debug, Default)]
pub(crate) struct Searched {
    pub(crate) items: Vec<(usize, Score)>,
    pub(crate) total_matched: usize,
}

/// The typos a query may make: a short query that may make many matches
/// everything.
pub(crate) fn max_typos(query: &FFFQuery<'_>) -> u16 {
    let effective = match &query.fuzzy_query {
        FuzzyQuery::Text(t) => *t,
        FuzzyQuery::Parts(parts) if !parts.is_empty() => parts[0],
        _ => query.raw_query.trim(),
    };
    (effective.len() as u16 / 4).clamp(2, 6)
}

/// The best `limit` of `files` for `query`, best first. A query of
/// constraints alone lists the files that pass them, most recently
/// modified first.
pub(crate) fn search(files: &Files, query: &FFFQuery<'_>, limit: usize) -> Searched {
    let items = files.items();
    let live = (0..items.len()).filter(|&slot| !items[slot].is_deleted());
    let working = apply_constraints(files, live.clone(), &query.constraints)
        .unwrap_or_else(|| live.collect());
    let Some(fuzzy_parts) = fuzzy_parts(query) else {
        let mut working = working;
        let total_matched = working.len();
        let recent = |a: &usize, b: &usize| recent_first(files, &items[*a], &items[*b]);
        if limit < working.len() {
            working.select_nth_unstable_by(limit, recent);
            working.truncate(limit);
        }
        working.sort_unstable_by(recent);
        let items = working
            .into_iter()
            .map(|slot| {
                let score = Score {
                    match_type: "recent",
                    ..Score::default()
                };
                (slot, score)
            })
            .collect();
        return Searched {
            items,
            total_matched,
        };
    };
    let paths: Vec<&str> = working
        .iter()
        .map(|&slot| files.path(&items[slot]))
        .collect();
    let results = score_matches(files, &working, &paths, fuzzy_parts, max_typos(query));
    sort_and_take(files, results, limit)
}

fn fuzzy_parts<'q>(query: &'q FFFQuery<'q>) -> Option<&'q [&'q str]> {
    match &query.fuzzy_query {
        FuzzyQuery::Text(t) if t.len() >= 2 => Some(std::slice::from_ref(t)),
        FuzzyQuery::Parts(parts) if !parts.is_empty() => Some(parts.as_slice()),
        _ => None,
    }
}

fn config(parts: &[&str], max_typos: u16) -> neo_frizbee::Config {
    let has_uppercase = parts.iter().any(|p| p.chars().any(char::is_uppercase));
    neo_frizbee::Config {
        max_typos: Some(max_typos),
        sort: neo_frizbee::SortStrategy::Unsorted,
        scoring: Scoring {
            capitalization_bonus: if has_uppercase { 8 } else { 0 },
            matching_case_bonus: if has_uppercase { 4 } else { 0 },
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Each part matched against every path: the paths every part matched,
/// their scores averaged. `Match::index` is into `paths`.
fn match_fuzzy_parts(
    fuzzy_parts: &[&str],
    paths: &[&str],
    options: &neo_frizbee::Config,
) -> Vec<neo_frizbee::Match> {
    let valid_parts: SmallVec<[&str; 4]> = fuzzy_parts
        .iter()
        .copied()
        .filter(|p| p.len() >= 2)
        .collect();

    if valid_parts.is_empty() {
        return vec![];
    }

    let reverse_pair = valid_parts.len() == 2 && valid_parts[1].len() > valid_parts[0].len();
    let first_part = usize::from(reverse_pair);
    let mut first_options = *options;
    if reverse_pair {
        first_options.max_typos = options
            .max_typos
            .map(|t| t.min(valid_parts[1].len() as u16));
    }

    // Narrow two-part queries with the longer part before scoring the shorter part.
    let first_part_matches =
        neo_frizbee::Matcher::new(valid_parts[first_part], &first_options).match_list(paths);

    if valid_parts.len() == 1 {
        return first_part_matches;
    }

    let total_parts = valid_parts.len() as u32;
    let mut matches = first_part_matches;
    for (part_index, part) in valid_parts
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != first_part)
    {
        let mut part_options = *options;
        if part_index > 0 {
            part_options.max_typos = options.max_typos.map(|t| t.min(part.len() as u16));
        }

        // Match only the files that survived the previous round.
        let survivors: Vec<&str> = matches.iter().map(|m| paths[m.index as usize]).collect();
        let sub_matches = neo_frizbee::Matcher::new(*part, &part_options).match_list(&survivors);

        if sub_matches.is_empty() {
            return vec![];
        }

        // Map sub_matches back to original indices, average scores across all parts.
        matches = sub_matches
            .into_iter()
            .map(|sm| {
                let prev = &matches[sm.index as usize];
                let sum = u32::from(prev.score).saturating_add(u32::from(sm.score));
                let avg = sum / total_parts;

                neo_frizbee::Match {
                    index: prev.index,
                    score: avg.min(u32::from(u16::MAX)) as u16,
                    end_col: if reverse_pair {
                        sm.end_col
                    } else {
                        prev.end_col
                    },
                    exact: prev.exact && sm.exact,
                }
            })
            .collect();
    }

    matches
}

/// Every match of the parts in `paths` (the paths of `working`'s slots),
/// scored.
fn score_matches(
    files: &Files,
    working: &[usize],
    paths: &[&str],
    fuzzy_parts: &[&str],
    max_typos: u16,
) -> Vec<(usize, Score)> {
    // Users type `/` regardless of platform. Checking the OS separator alone
    // would miss forward-slash queries on Windows.
    let query_contains_path_separator = fuzzy_parts
        .iter()
        .any(|p| p.contains('/') || p.contains(MAIN_SEPARATOR));
    let options = config(fuzzy_parts, max_typos);
    let path_matches = match_fuzzy_parts(fuzzy_parts, paths, &options);

    let main_needle = fuzzy_parts[0].as_bytes();
    let main_needle_len = main_needle.len() as u16;
    let items = files.items();

    // The matches that ended in a directory are matched again against the
    // file's name alone: a name that matches well still earns its bonus.
    let mut fallback_indices: Vec<u32> = Vec::new();
    let filename_fallback_matches = if query_contains_path_separator || path_matches.len() > 15_000
    {
        vec![]
    } else {
        let mut fallback_filenames: Vec<&str> = Vec::new();
        for (i, path_match) in path_matches.iter().enumerate() {
            let file = &items[working[path_match.index as usize]];
            let filename_start = file.name_offset() as u16;
            let match_start_approx = path_match.end_col.saturating_sub(main_needle_len - 1);
            if match_start_approx < filename_start {
                fallback_indices.push(i as u32);
                fallback_filenames.push(files.name(file));
            }
        }
        if fallback_filenames.is_empty() {
            vec![]
        } else {
            neo_frizbee::Matcher::new(fuzzy_parts[0], &options).match_list(&fallback_filenames)
        }
    };

    // path-match index -> position in filename_fallback_matches (u32::MAX = none)
    let mut fallback_by_match: Vec<u32> = Vec::new();
    if !filename_fallback_matches.is_empty() {
        fallback_by_match.resize(path_matches.len(), u32::MAX);
        for (position, m) in filename_fallback_matches.iter().enumerate() {
            fallback_by_match[fallback_indices[m.index as usize] as usize] = position as u32;
        }
    }

    path_matches
        .iter()
        .enumerate()
        .map(|(match_idx, path_match)| {
            let slot = working[path_match.index as usize];
            let file = &items[slot];
            let base_score = i32::from(path_match.score);

            let filename_start = file.name_offset() as u16;
            let match_start_approx = path_match.end_col.saturating_sub(main_needle_len - 1);

            let end_col_filename_match = match_start_approx >= filename_start;
            let simd_filename_match = if end_col_filename_match {
                None
            } else {
                fallback_by_match
                    .get(match_idx)
                    .and_then(|&position| filename_fallback_matches.get(position as usize))
            };

            let is_filename_match = end_col_filename_match || simd_filename_match.is_some();
            let name = files.name(file);
            let fname_len = name.len();

            let is_exact_filename = simd_filename_match.is_some_and(|m| m.exact)
                || (end_col_filename_match
                    && main_needle_len as usize == fname_len
                    && main_needle.eq_ignore_ascii_case(name.as_bytes()));

            let mut has_special_filename_bonus = false;
            let filename_bonus = if is_exact_filename {
                base_score / 5 * 2 // 40% bonus for exact filename match
            } else if is_filename_match {
                // 16% bonus for fuzzy filename match that landed in the filename region.
                // For fallback matches (where the path match landed in a directory segment),
                // scale the bonus by the quality of the filename match — a contiguous match
                // like "rename" in "rename.ts" gets the full bonus, while a scattered
                // subsequence like r-e-n-a-m-e in "generateSessionName.ts" gets much less.
                let max_bonus = (base_score / 6).min(30);
                if let Some(fm) = simd_filename_match {
                    let max_possible = i32::from(main_needle_len) * 16;
                    let quality = i32::from(fm.score).min(max_possible);
                    max_bonus * quality / max_possible
                } else {
                    max_bonus
                }
            } else if (5..=11).contains(&fname_len) && is_special_entry_point_file(name) {
                // 5% bonus for special file but not as much as file name to avoid situations
                // when you have /user_service/server.rs and /user_service/server/mod.rs
                has_special_filename_bonus = true;
                base_score * 5 / 100
            } else {
                0
            };

            let path_alignment_bonus = if query_contains_path_separator && main_needle.len() > 10 {
                let common_suffix =
                    common_suffix_len_ignore_ascii_case(files.path(file).as_bytes(), main_needle);
                let needle_len = main_needle.len();
                let coverage = common_suffix * 100 / needle_len;
                if common_suffix > 10 && coverage >= 30 {
                    base_score * coverage as i32 / 100
                } else {
                    0
                }
            } else {
                0
            };

            let total = base_score
                .saturating_add(filename_bonus)
                .saturating_add(path_alignment_bonus);

            let score = Score {
                total,
                base_score,
                filename_bonus,
                special_filename_bonus: if has_special_filename_bonus {
                    filename_bonus
                } else {
                    0
                },
                path_alignment_bonus,
                exact_match: is_exact_filename || path_match.exact,
                match_type: if is_exact_filename {
                    "exact_filename"
                } else if is_filename_match {
                    "fuzzy_filename"
                } else if path_match.exact {
                    "exact_path"
                } else {
                    "fuzzy_path"
                },
            };

            (slot, score)
        })
        .collect()
}

fn common_suffix_len_ignore_ascii_case(path: &[u8], needle: &[u8]) -> usize {
    path.iter()
        .rev()
        .zip(needle.iter().rev())
        .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
        .count()
}

fn is_special_entry_point_file(filename: &str) -> bool {
    matches!(
        filename,
        "mod.rs"
            | "lib.rs"
            | "main.rs"
            | "index.js"
            | "index.jsx"
            | "index.ts"
            | "index.tsx"
            | "index.mjs"
            | "index.cjs"
            | "index.vue"
            | "__init__.py"
            | "__main__.py"
            | "main.go"
            | "main.c"
            | "index.php"
            | "main.rb"
            | "index.rb"
    )
}

/// The best `limit` by total, ties to the more recently modified and
/// then by path, so a page is the same however often it is asked.
fn sort_and_take(files: &Files, mut results: Vec<(usize, Score)>, limit: usize) -> Searched {
    let total_matched = results.len();
    let items = files.items();
    let compare = |a: &(usize, Score), b: &(usize, Score)| {
        b.1.total
            .cmp(&a.1.total)
            .then_with(|| recent_first(files, &items[a.0], &items[b.0]))
    };
    if limit < total_matched {
        results.select_nth_unstable_by(limit, compare);
        results.truncate(limit);
    }
    results.sort_unstable_by(compare);
    Searched {
        items: results,
        total_matched,
    }
}

/// Where in each of `paths` the query's parts matched, as byte ranges:
/// what a client highlights.
pub(crate) fn match_ranges(query: &FFFQuery<'_>, paths: &[&str]) -> Vec<SmallVec<[(u32, u32); 4]>> {
    let parts: SmallVec<[&str; 4]> = match &query.fuzzy_query {
        FuzzyQuery::Text(text) if text.len() >= 2 => smallvec::smallvec![*text],
        FuzzyQuery::Parts(parts) => parts.iter().copied().filter(|p| p.len() >= 2).collect(),
        _ => SmallVec::new(),
    };

    let mut ranges_by_item = vec![SmallVec::new(); paths.len()];
    if parts.is_empty() || paths.is_empty() {
        return ranges_by_item;
    }
    let config = config(&parts, max_typos(query));

    for (idx, part) in parts.iter().copied().enumerate() {
        let mut part_config = config;
        if idx > 0 {
            part_config.max_typos = config.max_typos.map(|t| t.min(part.len() as u16));
        }

        let mut matcher = neo_frizbee::Matcher::new(part, &part_config);
        for mut matched in matcher.match_list_indices(paths) {
            let item_idx = matched.index as usize;
            let Some(path) = paths.get(item_idx) else {
                continue;
            };

            matched.indices.sort_unstable();
            ranges_by_item[item_idx].extend(char_indices_to_byte_offsets(path, &matched.indices));
        }
    }

    for ranges in &mut ranges_by_item {
        *ranges = merge_byte_offsets(std::mem::take(ranges));
    }

    ranges_by_item
}

fn merge_byte_offsets(mut ranges: SmallVec<[(u32, u32); 4]>) -> SmallVec<[(u32, u32); 4]> {
    if ranges.len() <= 1 {
        return ranges;
    }

    ranges.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut merged = 0;
    for index in 0..ranges.len() {
        let (start, end) = ranges[index];
        if end <= start {
            continue;
        }

        if merged > 0 && start <= ranges[merged - 1].1 {
            let last = &mut ranges[merged - 1];
            last.1 = last.1.max(end);
            continue;
        }

        ranges[merged] = (start, end);
        merged += 1;
    }

    ranges.truncate(merged);
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fff::query::QueryParser;
    use crate::fff::types::Found;

    fn make_files(paths: &[&str]) -> Files {
        Files::walked(
            paths
                .iter()
                .map(|path| Found {
                    path: (*path).to_owned(),
                    size: 1,
                    modified: 0,
                })
                .collect(),
        )
    }

    fn search_in(files: &Files, query: &str) -> Vec<(String, Score)> {
        let parsed = QueryParser::default().parse(query);
        search(files, &parsed, 100)
            .items
            .into_iter()
            .map(|(slot, score)| (files.path(&files.items()[slot]).to_owned(), score))
            .collect()
    }

    #[test]
    fn longer_second_part_preserves_matches_and_first_part_positions() {
        let paths = [
            "src/controller.rs",
            "src/contrller.rs",
            "test/src.rs",
            "日本語/controller.tsx",
            "src/utils.rs",
            "src/controller/controller.rs",
        ];
        for parts in [
            ["src", "controller"],
            ["rs", "contrller"],
            ["rs", "日本語"],
            ["no", "absent"],
            ["src", "src/controller.rs"],
        ] {
            for max_typos in [0, 2, 4] {
                let options = neo_frizbee::Config {
                    max_typos: Some(max_typos),
                    sort: neo_frizbee::SortStrategy::Unsorted,
                    ..Default::default()
                };
                let first = match_fuzzy_parts(&parts[..1], &paths, &options);
                let mut second_options = options;
                second_options.max_typos = Some(max_typos.min(parts[1].len() as u16));
                let second = match_fuzzy_parts(&parts[1..], &paths, &second_options);
                let mut expected: Vec<_> = first
                    .iter()
                    .filter_map(|a| {
                        let b = second.iter().find(|b| b.index == a.index)?;
                        Some((
                            a.index,
                            ((u32::from(a.score) + u32::from(b.score)) / 2) as u16,
                            a.end_col,
                            a.exact && b.exact,
                        ))
                    })
                    .collect();
                let mut actual: Vec<_> = match_fuzzy_parts(&parts, &paths, &options)
                    .iter()
                    .map(|m| (m.index, m.score, m.end_col, m.exact))
                    .collect();
                expected.sort_unstable();
                actual.sort_unstable();
                assert_eq!(actual, expected, "{parts:?}, typos={max_typos}");
            }
        }
    }

    #[test]
    fn a_page_is_the_head_of_the_full_ranking() {
        let paths: Vec<String> = (0..500)
            .map(|i| format!("src/group_{}/controller_{i}.rs", i % 7))
            .collect();
        let files = make_files(&paths.iter().map(String::as_str).collect::<Vec<_>>());
        let parsed = QueryParser::default().parse("controller");
        let full = search(&files, &parsed, usize::MAX);
        let page = search(&files, &parsed, 20);
        assert_eq!(page.total_matched, full.total_matched);
        let slots = |s: &Searched| s.items.iter().map(|(slot, _)| *slot).collect::<Vec<_>>();
        assert_eq!(slots(&page), slots(&full)[..20]);
    }

    #[test]
    fn a_query_of_constraints_alone_lists_recent_files_first() {
        let mut files = make_files(&["a.rs", "b.rs", "c.txt"]);
        files.upsert(Found {
            path: "b.rs".into(),
            size: 1,
            modified: 10,
        });
        let found: Vec<_> = search_in(&files, "*.rs")
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(found, ["b.rs", "a.rs"]);
    }

    #[test]
    fn merge_offsets_reuses_storage() {
        let ranges: SmallVec<[(u32, u32); 4]> = smallvec::smallvec![(4, 6), (0, 2), (1, 3), (6, 8)];
        assert_eq!(merge_byte_offsets(ranges).as_slice(), [(0, 3), (4, 8)]);
    }

    #[test]
    fn test_filename_match_ranks_above_path_only_match() {
        let files = make_files(&["src/username/handler.rs", "src/username/username.rs"]);

        let results = search_in(&files, "usrnmea");

        assert!(
            results.len() >= 2,
            "both files should match, got {}",
            results.len()
        );
        assert_eq!(
            results[0].0, "src/username/username.rs",
            "filename match should rank first"
        );
        assert!(
            results[0].1.filename_bonus > 0,
            "username.rs should have filename bonus"
        );
        assert_eq!(
            results[1].1.filename_bonus, 0,
            "handler.rs should have no filename bonus"
        );
    }

    #[test]
    fn test_exact_filename_beats_fuzzy_filename() {
        let files = make_files(&["src/user_name_handler.rs", "src/username.rs"]);

        let results = search_in(&files, "username.rs");

        assert!(results.len() >= 2);
        assert_eq!(
            results[0].0, "src/username.rs",
            "exact filename should rank first"
        );
        assert_eq!(results[0].1.match_type, "exact_filename");
        assert!(results[0].1.filename_bonus > results[1].1.filename_bonus);
    }

    #[test]
    fn test_same_length_filename_no_false_exact() {
        let files = make_files(&["src/item_sync/file.rs", "src/models/item.rs"]);

        let results = search_in(&files, "item.rs");

        assert!(results.len() >= 2);
        assert_eq!(results[0].0, "src/models/item.rs");
        assert_eq!(results[0].1.match_type, "exact_filename");
        assert_ne!(
            results[1].1.match_type, "exact_filename",
            "file.rs should not get exact_filename"
        );
    }

    #[test]
    fn test_path_separator_disables_filename_bonus() {
        let files = make_files(&["src/controllers/user.rs"]);

        let results = search_in(&files, "src/user");

        assert!(!results.is_empty());
        assert_eq!(
            results[0].1.filename_bonus, 0,
            "path-like query should not get filename bonus"
        );
    }

    /// Regression: full-path query should rank the near-exact path match first.
    /// <https://x.com/mbarneyjr/status/2043474268390817861>
    #[test]
    fn test_full_path_query_prefers_closer_filename_match() {
        let files = make_files(&[
            "test-utils/completion/condition-key/yaml_partial-svc-colon.yml",
            "test-utils/test-cases/completion/condition-key/yaml_partial-svc.yml",
            "test-utils/action-value/yaml_inline_partial-svc-colon.yml",
            "test-utils/completion/action-value/yaml_array_partial-svc-colon.yml",
            "test-utils/completion/action-value/yaml_array_partial-svc.yml",
            "test-utils/completion/condition-key/yaml_global-tag-keys.yml",
            "test-utils/test-cases/completion/condition-key/yaml_partial.yml",
        ]);

        let results = search_in(
            &files,
            "t-utils/test-cases/completion/condition-key/yaml_partial-svc.yml",
        );

        assert_eq!(
            results[0].0,
            "test-utils/test-cases/completion/condition-key/yaml_partial-svc.yml"
        );
    }

    #[test]
    fn test_path_longer_than_512_bytes_does_not_panic_and_matches() {
        let mut long_path = String::new();
        while long_path.len() < 600 {
            long_path.push_str("deeply_nested_directory_segment/");
        }
        long_path.push_str("needle_file.rs");

        let files = make_files(&[long_path.as_str(), "src/other.rs"]);
        let results = search_in(&files, "needle");

        assert!(results.iter().any(|(p, _)| p == &long_path));
    }

    #[test]
    fn test_typo_resistant_long_query() {
        let files = make_files(&[
            "src/pricing/bid_comparison_supplier_part_cost_modifiers.rs",
            "src/pricing/bid_evaluation_handler.rs",
            "src/pricing/supplier_contract_terms.rs",
            "src/models/purchase_order.rs",
            "src/models/inventory_item.rs",
            "src/controllers/user_controller.rs",
            "src/controllers/admin_controller.rs",
            "src/services/notification_service.rs",
            "src/services/email_dispatcher.rs",
            "src/utils/string_helpers.rs",
            "src/utils/date_formatter.rs",
            "src/db/migration_runner.rs",
            "src/db/connection_pool.rs",
            "src/config/app_settings.rs",
            "src/config/feature_flags.rs",
        ]);
        let target = "bid_comparison_supplier_part_cost_modifiers";
        for query in [
            "bid_comparison",
            // A smushed query with two typos: comparsion, modfiers.
            "bidcomparsionsupplierpartcostmodfiers",
            "bidcomp",
            "supplierpartcost",
        ] {
            let results = search_in(&files, query);
            assert!(
                results.iter().any(|(p, _)| p.contains(target)),
                "{query} found {results:?}"
            );
        }
    }

    #[test]
    fn highlights_are_byte_ranges_of_each_path() {
        let parsed = QueryParser::default().parse("mainrs");
        let ranges = match_ranges(&parsed, &["src/main.rs", "lib/other.rs"]);
        assert_eq!(ranges[0].as_slice(), [(4, 8), (9, 11)]);
    }
}
