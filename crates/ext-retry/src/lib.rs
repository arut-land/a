//! `retry`: a model request that failed for a reason that may pass (a rate
//! limit, an overloaded or failing provider, a dropped connection) is sent
//! again, after the wait the provider asks for or a capped, jittered
//! exponential backoff. It answers only the kinds that may pass
//! (`llm::Kind::is_retryable`), and installs after every other extension
//! (`Tier::Post`), so a handler that owns a kind (compaction owns a context
//! overflow) or a user's own policy is asked first. Attempts are counted in
//! the log, and the kernel caps restarts, so a daemon that keeps crashing
//! mid-call cannot retry forever.

use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
use std::time::Duration;

use ext::point::{Failed, Recover, RequestError};
use ext::{Answer, Configured, Extension, PointCx, Refused, Registrar, Table};

/// The `retry` extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    /// Retries per model call; the call fails after the last.
    pub attempts: u16,
    /// Retries of a request that got no reply at all (a timeout, a reset):
    /// fewer, as a provider that does not answer rarely starts to.
    pub transport: u16,
    /// The first wait, doubled for each retry after it.
    pub base: Duration,
    /// The longest backoff.
    pub cap: Duration,
    /// The longest wait a provider may ask for and be waited for.
    pub retry_after_max: Duration,
}

/// opencode's: 2 s doubling to 10 s, ten tries, three on a dead connection,
/// a provider's own wait honoured up to 15 minutes.
impl Default for Retry {
    fn default() -> Self {
        Self {
            attempts: 10,
            transport: 3,
            base: Duration::from_secs(2),
            cap: Duration::from_secs(10),
            retry_after_max: Duration::from_mins(15),
        }
    }
}

impl Retry {
    /// What to do about `failed`: retry what may pass, while attempts last.
    /// `seed` spreads the backoffs of calls failing together.
    pub fn recover(&self, failed: &Failed<'_>, seed: u64) -> Option<Recover> {
        let kind = failed.error.kind;
        let limit = if kind == llm::Kind::Transport {
            self.transport.min(self.attempts)
        } else {
            self.attempts
        };
        if failed.attempt >= limit || !kind.is_retryable() {
            return None;
        }
        let after = match failed.error.retry_after {
            Some(wait) => wait.min(self.retry_after_max),
            None => jitter(
                self.base
                    .saturating_mul(1 << failed.attempt.min(16))
                    .min(self.cap),
                seed,
            ),
        };
        Some(Recover::Retry { after })
    }
}

/// `wait`, give or take a tenth, as `seed` falls: the same for the same
/// call and attempt, so a replay waits as the run did.
fn jitter(wait: Duration, seed: u64) -> Duration {
    let tenth = wait / 10;
    let spread = u32::try_from(seed % 2001).unwrap_or_default();
    wait.saturating_sub(tenth) + tenth * spread / 1000
}

/// What spreads the backoff of the call `cx` is about, at `attempt`.
fn seed(cx: &PointCx<'_>, attempt: u16) -> u64 {
    BuildHasherDefault::<DefaultHasher>::default().hash_one((cx.session.0, cx.call, attempt))
}

/// `[ext.retry] attempts` and `transport`.
impl Configured for Retry {
    fn configure(table: &impl Table) -> Result<Self, Refused> {
        let default = Self::default();
        Ok(Self {
            attempts: table.number("attempts")?.unwrap_or(default.attempts),
            transport: table.number("transport")?.unwrap_or(default.transport),
            ..default
        })
    }
}

impl Extension for Retry {
    const NAME: &'static str = "retry";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let this = *self;
        r.on::<RequestError>(move |cx, failed| {
            Answer::Now(this.recover(&failed, seed(cx, failed.attempt)))
        });
    }
}

#[cfg(test)]
mod tests {
    use llm::{Error, Kind};

    use super::*;

    fn after(retry: &Retry, error: &Error, attempt: u16) -> Option<Duration> {
        let failed = Failed { error, attempt };
        // The middle of the spread: no jitter.
        retry.recover(&failed, 1000).map(|r| match r {
            Recover::Retry { after } => after,
            Recover::Resend => panic!("a retry waits"),
        })
    }

    #[test]
    fn what_may_pass_is_retried_with_a_capped_backoff() {
        let retry = Retry::default();
        let busy = Error::new(Kind::Server, "overloaded");
        let waits: Vec<_> = (0..11).map(|n| after(&retry, &busy, n)).collect();
        let s = |s| Some(Duration::from_secs(s));
        assert_eq!(
            waits,
            [
                s(2),
                s(4),
                s(8),
                s(10),
                s(10),
                s(10),
                s(10),
                s(10),
                s(10),
                s(10),
                None
            ]
        );
        for kind in [Kind::RateLimit, Kind::Incomplete, Kind::Other] {
            assert!(
                after(&retry, &Error::new(kind, "x"), 9).is_some(),
                "{kind:?}"
            );
        }
        // A dead connection, three times only.
        let dead = Error::new(Kind::Transport, "timed out");
        let tries: Vec<_> = (0..4).map(|n| after(&retry, &dead, n).is_some()).collect();
        assert_eq!(tries, [true, true, true, false]);
        for kind in [
            Kind::Auth,
            Kind::Quota,
            Kind::ContextOverflow,
            Kind::InvalidRequest,
        ] {
            assert_eq!(after(&retry, &Error::new(kind, "x"), 0), None, "{kind:?}");
        }
    }

    #[test]
    fn the_providers_wait_is_honoured_up_to_fifteen_minutes() {
        let retry = Retry::default();
        let limited = |wait| Error {
            retry_after: Some(wait),
            ..Error::new(Kind::RateLimit, "slow down")
        };
        let long = Duration::from_mins(7);
        assert_eq!(after(&retry, &limited(long), 3), Some(long));
        let hour = Duration::from_secs(3600);
        assert_eq!(
            after(&retry, &limited(hour), 0),
            Some(retry.retry_after_max)
        );
    }

    /// Backoffs spread a tenth either way, the same for the same seed.
    #[test]
    fn backoffs_are_jittered_the_same_way_twice() {
        let wait = Duration::from_secs(10);
        assert_eq!(jitter(wait, 0), Duration::from_secs(9));
        assert_eq!(jitter(wait, 2000), Duration::from_secs(11));
        assert_eq!(jitter(wait, 1234), jitter(wait, 1234));
        let spread: std::collections::BTreeSet<_> =
            (0..50u64).map(|s| jitter(wait, s * 997)).collect();
        assert!(spread.len() > 40);
    }
}
