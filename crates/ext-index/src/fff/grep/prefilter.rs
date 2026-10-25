use crate::fff::MAX_FILE_SIZE;
use crate::fff::index::constraints::ConstraintPlan;
use crate::fff::query::{Constraint, ConstraintVec};
use crate::fff::types::{Files, recent_first};

/// What the content index says of a grep: the walked files that may hold
/// a match, and those known to be binary. Slots past the bitsets (files
/// created since the index was built) are always searched.
#[derive(Clone, Copy, Default)]
pub(crate) struct Known<'a> {
    pub(crate) candidates: Option<&'a [u64]>,
    pub(crate) binary: Option<&'a [u64]>,
}

/// The slots of the files a grep reads, most recently modified first.
/// When constraints leave no file and named a file path, the path was
/// likely part of the search text: they are tried again without it.
pub(crate) fn files_to_search(
    files: &Files,
    constraints: &[Constraint<'_>],
    known: Known<'_>,
) -> Vec<usize> {
    let found = prefilter_files(files, constraints, known);
    if !found.is_empty() {
        return found;
    }
    match strip_file_path_constraint_if_present(constraints) {
        Some(stripped) => prefilter_files(files, &stripped, known),
        None => found,
    }
}

fn prefilter_files(files: &Files, constraints: &[Constraint<'_>], known: Known<'_>) -> Vec<usize> {
    let plan = (!constraints.is_empty()).then(|| ConstraintPlan::build(constraints));
    let items = files.items();
    let bit = |bits: &[u64], slot: usize| {
        bits.get(slot / 64)
            .is_some_and(|word| word & (1 << (slot % 64)) != 0)
    };
    let walked = files.walked_count();
    let mut out: Vec<usize> = (0..items.len())
        .filter(|&slot| {
            let file = &items[slot];
            !file.is_deleted()
                && file.size > 0
                && file.size <= MAX_FILE_SIZE
                && known.binary.is_none_or(|bits| !bit(bits, slot))
                && known
                    .candidates
                    .is_none_or(|bits| slot >= walked || bit(bits, slot))
                && plan
                    .as_ref()
                    .is_none_or(|plan| plan.matches(files.path(file), files.name(file)))
        })
        .collect();
    out.sort_unstable_by(|a, b| recent_first(files, &items[*a], &items[*b]));
    out
}

fn strip_file_path_constraint_if_present<'a>(
    constraints: &[Constraint<'a>],
) -> Option<ConstraintVec<'a>> {
    if !constraints
        .iter()
        .any(|c| matches!(c, Constraint::FilePath(_)))
    {
        return None;
    }
    Some(
        constraints
            .iter()
            .filter(|c| !matches!(c, Constraint::FilePath(_)))
            .cloned()
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fff::types::Found;

    #[test]
    fn candidates_binary_and_empty_files_are_left_out_and_new_files_are_kept() {
        let mut files = Files::walked(
            (0..70)
                .map(|i| Found {
                    path: format!("f{i:02}.rs"),
                    size: u64::from(i != 9),
                    modified: 1000 - i,
                })
                .collect(),
        );
        files.upsert(&Found {
            path: "new.rs".into(),
            size: 1,
            modified: 0,
        });
        files.remove("f08.rs");
        let even = [0x5555_5555_5555_5555, 0x55];
        let binary = [1 << 6, 0];
        let known = Known {
            candidates: Some(&even),
            binary: Some(&binary),
        };
        let found: Vec<_> = files_to_search(&files, &[], known)
            .into_iter()
            .map(|slot| files.path(&files.items()[slot]).to_owned())
            .collect();
        let mut expected: Vec<_> = (0..70)
            .filter(|i| i % 2 == 0 && ![6, 8].contains(i))
            .map(|i| format!("f{i:02}.rs"))
            .collect();
        expected.push("new.rs".into());
        assert_eq!(found, expected);
    }
}
