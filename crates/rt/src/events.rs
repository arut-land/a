//! Observability that costs nothing when off. Emitters are generic over a
//! sink and write `if E::ENABLED { sink.emit(|| ..) }`: with [`Noop`] the
//! branch is a constant `false`, so the event is never built and the code
//! compiles away. Other sinks (fan-out, tracing, OTLP) are further impls.

use std::io::Write;

/// Something worth knowing happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// What happened, such as `attach` or `step`.
    pub name: &'static str,
    /// The session it happened in, if any.
    pub session: Option<String>,
    /// The particulars, for a person to read.
    pub detail: String,
}

pub trait EventSink: Send + Sync + 'static {
    /// Whether this sink keeps events. Checked before building one.
    const ENABLED: bool;

    /// Takes the event `event` builds. Only called when [`Self::ENABLED`].
    fn emit(&self, event: impl FnOnce() -> Event);
}

/// Drops every event, at no cost.
#[derive(Clone, Copy, Debug, Default)]
pub struct Noop;

impl EventSink for Noop {
    const ENABLED: bool = false;

    #[inline(always)]
    fn emit(&self, _event: impl FnOnce() -> Event) {}
}

/// Writes debugging events to stderr; never the typed daemon log.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stderr;

impl EventSink for Stderr {
    const ENABLED: bool = true;

    fn emit(&self, event: impl FnOnce() -> Event) {
        let Event {
            name,
            session,
            detail,
        } = event();
        let session = session.map_or_else(String::new, |s| format!(" session={s}"));
        let _ = writeln!(std::io::stderr().lock(), "event {name}{session} {detail}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emit<E: EventSink>(sink: &E, built: &mut bool) {
        if E::ENABLED {
            sink.emit(|| {
                *built = true;
                Event {
                    name: "test",
                    session: None,
                    detail: String::new(),
                }
            });
        }
    }

    #[test]
    fn a_disabled_sink_never_builds_an_event() {
        let mut built = false;
        emit(&Noop, &mut built);
        assert!(!built);
        emit(&Stderr, &mut built);
        assert!(built);
    }
}
