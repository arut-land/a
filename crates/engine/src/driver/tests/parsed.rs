//! A tool call's input, read once.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Deserialize;

use super::*;

/// The reads of [`Counted`] inputs: this test's alone.
static READS: AtomicUsize = AtomicUsize::new(0);

/// A `sleep` input that counts its reads.
struct Counted(u64);

impl<'de> Deserialize<'de> for Counted {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Seconds {
            seconds: u64,
        }
        READS.fetch_add(1, Ordering::SeqCst);
        Seconds::deserialize(d).map(|s| Self(s.seconds))
    }
}

impl ext::Schema for Counted {
    fn schema(out: &mut String) {
        ext::schema::of::<ext_fixtures::Seconds>()
            .as_str()
            .clone_into(out);
    }
}

/// `sleep`, which sleeps for none of its seconds, shown by cards.
struct Counting;

impl Tool for Counting {
    type In = Counted;
    type Out = u64;

    const NAME: &'static str = "sleep";
    const DESCRIPTION: &'static str = "Counts its reads.";

    fn call(&self, input: &Counted, _: &Cx) -> Result<u64, String> {
        Ok(input.0)
    }

    fn present(input: &Counted) -> Option<Card> {
        Some(Card {
            title: format!("sleep {}", input.0),
            ..Card::default()
        })
    }

    fn present_result(input: &Counted, slept: &u64) -> Option<Card> {
        Some(Card {
            title: format!("sleep {}", input.0),
            note: format!("slept {slept}"),
            ..Card::default()
        })
    }
}

struct Reads;

impl Extension for Reads {
    const NAME: &'static str = "reads";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Counting);
    }
}

/// The call's text is read into its tool's type once, at its intent: its
/// card, its run and the card of its outcome are all made of what was read.
#[test]
fn a_tool_input_is_parsed_once() {
    let registry = Registry::default().with(&Reads).unwrap();
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let session = fixture.start(&[]);
    session.submit("sleep 3").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    let cards: Vec<_> = (log.iter())
        .filter_map(|e| match &e.body {
            Body::ToolCall { card, .. } | Body::ToolResult { card, .. } => card.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(cards.len(), 2, "{log:#?}");
    assert_eq!(cards[1].note, "slept 3");
    assert_eq!(READS.load(Ordering::SeqCst), 1);
}
