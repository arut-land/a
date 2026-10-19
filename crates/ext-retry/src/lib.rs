//! `retry`: a model request that failed for a reason that may pass (a rate
//! limit, an overloaded or failing provider, a dropped connection) is sent
//! again, after the wait the provider asks for or a capped exponential
//! backoff. Attempts are counted in the log, so a daemon that keeps
//! crashing mid-call cannot retry forever.

use std::time::Duration;

use ext::point::{Failed, Recover, RequestError};
use ext::{Extension, Registrar};

/// The `retry` extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    /// Retries per model call; the call fails after the last.
    pub attempts: u16,
    /// The first wait, doubled for each retry after it.
    pub base: Duration,
    /// The longest wait, the provider's own included.
    pub cap: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            attempts: 4,
            base: Duration::from_millis(500),
            cap: Duration::from_secs(30),
        }
    }
}

impl Retry {
    /// What to do about `failed`: retry what may pass, while attempts last.
    pub fn recover(&self, failed: &Failed<'_>) -> Option<Recover> {
        if failed.attempt >= self.attempts || !failed.error.is_retryable() {
            return None;
        }
        let backoff = self.base.saturating_mul(1 << failed.attempt.min(16));
        let after = failed.error.retry_after.unwrap_or(backoff).min(self.cap);
        Some(Recover::Retry { after })
    }
}

impl Extension for Retry {
    const NAME: &'static str = "retry";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let this = *self;
        r.on::<RequestError>(move |_, failed| this.recover(&failed));
    }
}

#[cfg(test)]
mod tests {
    use llm::{Error, Kind};

    use super::*;

    fn after(retry: &Retry, error: &Error, attempt: u16) -> Option<Duration> {
        let failed = Failed { error, attempt };
        retry.recover(&failed).map(|r| match r {
            Recover::Retry { after } => after,
            Recover::Replace(_) => panic!("a retry sends the same request"),
        })
    }

    #[test]
    fn what_may_pass_is_retried_with_a_capped_backoff() {
        let retry = Retry::default();
        let busy = Error::new(Kind::Server, "overloaded");
        let waits: Vec<_> = (0..5).map(|n| after(&retry, &busy, n)).collect();
        let ms = |ms| Some(Duration::from_millis(ms));
        assert_eq!(waits, [ms(500), ms(1000), ms(2000), ms(4000), None]);
        for kind in [Kind::Transport, Kind::RateLimit, Kind::Incomplete] {
            assert!(
                after(&retry, &Error::new(kind, "x"), 0).is_some(),
                "{kind:?}"
            );
        }
        for kind in [
            Kind::Auth,
            Kind::Quota,
            Kind::ContextOverflow,
            Kind::InvalidRequest,
        ] {
            assert_eq!(after(&retry, &Error::new(kind, "x"), 0), None, "{kind:?}");
        }
        let far = Retry {
            attempts: 40,
            ..retry
        };
        assert_eq!(after(&far, &busy, 39), Some(far.cap));
    }

    #[test]
    fn the_providers_wait_is_honoured_up_to_the_cap() {
        let retry = Retry::default();
        let limited = |wait| Error {
            retry_after: Some(wait),
            ..Error::new(Kind::RateLimit, "slow down")
        };
        let soon = Duration::from_millis(10);
        assert_eq!(after(&retry, &limited(soon), 3), Some(soon));
        let hour = Duration::from_secs(3600);
        assert_eq!(after(&retry, &limited(hour), 0), Some(retry.cap));
    }
}
