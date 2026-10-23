//! Source preparation with injected inventory, capture and waiting operations.
use crate::Refused;

/// The window coalesces editor writes; it is not proof that a writer closed.
/// Compare effective inputs, never notification counts: unrelated activity
/// must not prevent a settled source from making progress.
pub fn prepare<S: PartialEq, T>(
    mut capture: impl FnMut() -> Result<S, Refused>,
    mut wait: impl FnMut(Option<&S>),
    mut compile: impl FnMut(&S) -> Result<T, Refused>,
) -> Result<T, Refused> {
    let mut before = capture();
    loop {
        wait(before.as_ref().ok());
        let after = capture();
        if !same(&before, &after) {
            before = after;
            continue;
        }
        let result = before
            .as_ref()
            .map_err(|e| Refused(e.0.clone()))
            .and_then(&mut compile);
        let checked = capture();
        if same(&before, &checked) {
            return result;
        }
        before = checked;
    }
}

fn same<T: PartialEq>(a: &Result<T, Refused>, b: &Result<T, Refused>) -> bool {
    match (a, b) {
        (Ok(a), Ok(b)) => a == b,
        (Err(a), Err(b)) => a.0 == b.0,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn changes_during_capture_and_failed_compilation_are_retried() {
        let revision = Cell::new(0);
        let captures = Cell::new(0);
        let result = prepare(
            || {
                let n = captures.get();
                captures.set(n + 1);
                if n == 0 {
                    return Err(Refused("torn utf-8".into()));
                }
                Ok(revision.get())
            },
            |_| {},
            |value| {
                if *value == 0 {
                    revision.set(1);
                    Err(Refused("partial source".into()))
                } else {
                    Ok(*value)
                }
            },
        )
        .unwrap();
        assert_eq!(result, 1);
    }

    #[test]
    fn unchanged_invalid_sources_stop_without_idle_polling() {
        let captures = Cell::new(0);
        assert!(
            prepare(
                || {
                    captures.set(captures.get() + 1);
                    Ok(42)
                },
                |_| {},
                |_| Err::<(), _>(Refused("invalid".into())),
            )
            .is_err()
        );
        assert_eq!(captures.get(), 3);
    }
}
