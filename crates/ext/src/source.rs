//! Source preparation with injected inventory, capture and waiting operations.
use crate::Refused;

/// Retry only unsettled observations and transient I/O, never rejected content.
#[derive(Debug)]
pub enum Error {
    Retry(Refused),
    Rejected(Refused),
}

impl From<Refused> for Error {
    fn from(error: Refused) -> Self {
        Self::Rejected(error)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retry(error) | Self::Rejected(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

/// The window coalesces editor writes; it is not proof that a writer closed.
/// Compare effective inputs, never notification counts: unrelated activity
/// must not prevent a settled source from making progress.
pub fn prepare<S: PartialEq, T>(
    mut capture: impl FnMut() -> Result<S, Error>,
    mut wait: impl FnMut(&S),
    mut compile: impl FnMut(&S) -> Result<T, Refused>,
) -> Result<T, Error> {
    let before = capture()?;
    wait(&before);
    if before != capture()? {
        return Err(Error::Retry(Refused(
            "sources changed during capture".into(),
        )));
    }
    let result = compile(&before);
    if before != capture()? {
        return Err(Error::Retry(Refused(
            "sources changed during compilation".into(),
        )));
    }
    result.map_err(Error::Rejected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn identical_transient_failures_still_require_recovery() {
        for _ in 0..3 {
            assert!(matches!(
                prepare(
                    || Err::<u32, _>(Error::Retry(Refused("sharing violation".into()))),
                    |_| {},
                    |_| Ok(()),
                ),
                Err(Error::Retry(_))
            ));
        }
        assert_eq!(prepare(|| Ok(42), |_| {}, |value| Ok(*value)).unwrap(), 42);
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
