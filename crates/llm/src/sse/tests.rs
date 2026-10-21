use super::{SseFramer, events};
use crate::protocol::Flow;

/// Every event a framer dispatches over one whole body.
fn sse_all(body: &[u8]) -> Vec<(String, String)> {
    events(&mut SseFramer::new(), body)
}

fn message(data: &str) -> (String, String) {
    ("message".into(), data.into())
}

#[test]
fn every_line_ending_terminates_a_line() {
    let expected = vec![message("one")];
    assert_eq!(sse_all(b"data: one\n\n"), expected);
    assert_eq!(sse_all(b"data: one\r\n\r\n"), expected);
    // Bare CR terminates a line; a trailing lone CR cannot — the `\n` of a
    // `\r\n` pair may still arrive, so the blank line stays pending.
    assert_eq!(sse_all(b"data: one\r\rdata: two\r\r"), expected);
}

#[test]
fn a_leading_bom_is_not_data() {
    assert_eq!(
        sse_all("\u{feff}data: one\n\n".as_bytes()),
        vec![message("one")]
    );
}

#[test]
fn a_bom_split_across_chunks_is_still_ignored() {
    let mut framer = SseFramer::new();
    assert_eq!(events(&mut framer, &[0xef]).len(), 0);
    assert_eq!(events(&mut framer, &[0xbb]).len(), 0);
    assert_eq!(events(&mut framer, &[0xbf]).len(), 0);
    let events = events(&mut framer, b"data: one\n\n");
    assert_eq!(events.first().map(|(_, data)| data.as_str()), Some("one"));
}

#[test]
fn a_leading_colon_is_a_comment() {
    assert_eq!(
        sse_all(b": keep-alive\ndata: one\n\n"),
        vec![message("one")]
    );
}

#[test]
fn id_and_retry_lines_are_not_data() {
    assert_eq!(
        sse_all(b"id: 7\nretry: 500\ndata: one\n\ndata: two\n\n"),
        vec![message("one"), message("two")]
    );
}

#[test]
fn a_blank_line_without_data_dispatches_nothing_and_resets_the_type() {
    assert_eq!(
        sse_all(b"event: ping\n\ndata: one\n\n"),
        vec![message("one")]
    );
}

#[test]
fn a_truncated_trailing_event_waits_for_its_blank_line() {
    let mut framer = SseFramer::new();
    let first = events(&mut framer, b"data: one\n\nevent: message_stop\ndata: {}");
    assert_eq!(first.len(), 1);
    let rest = events(&mut framer, b"\n\n");
    assert_eq!(rest, [("message_stop".to_owned(), "{}".to_owned())]);
}

/// The two cassette-derived shapes: a body whose last event has no trailing
/// blank line delivers one event fewer than it spells, on any chunking.
#[test]
fn a_body_whose_last_event_lacks_its_blank_line_delivers_one_event_fewer() {
    let body = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}"
    );
    assert_eq!(sse_all(body.as_bytes()).len(), 1);
    let terminated = format!("{body}\n\n");
    assert_eq!(sse_all(terminated.as_bytes()).len(), 2);
}

#[test]
fn splitting_a_body_at_every_offset_yields_the_same_events() {
    let body = concat!(
        "\u{feff}: hello\r\n",
        "event: a\r\ndata: {\"n\":1}\r\n\r\n",
        "id: x\ndata: line one\ndata: line two\n\n",
        "\rdata: after a bare cr\r\r",
        "retry: 12\nevent: b\ndata: last\n\n",
    )
    .as_bytes();
    let whole = sse_all(body);
    assert_eq!(whole.len(), 4);
    for split in 0..=body.len() {
        let (head, tail) = body.split_at(split);
        let mut framer = SseFramer::new();
        let mut split_events = events(&mut framer, head);
        split_events.extend(events(&mut framer, tail));
        assert_eq!(split_events, whole, "split at {split}");
    }
}

/// The reply's end stops the framing: no event after it is handed over.
#[test]
fn a_done_event_stops_the_framing() {
    let mut framer = SseFramer::new();
    let mut seen = Vec::new();
    let flow = framer.push(b"data: one\n\ndata: two\n\n", |frame| {
        seen.push(frame.data.to_owned());
        Ok(Flow::Done)
    });
    assert!(matches!(flow, Ok(Flow::Done)));
    assert_eq!(seen, ["one"]);
}

#[test]
fn frame_limits_cover_unterminated_lines_and_multiline_events() {
    use super::DecodeLimits;
    use crate::Kind;
    let limits = DecodeLimits {
        frame_bytes: 32,
        ..Default::default()
    };
    for chunks in [
        vec![vec![b'x'; 33]],
        vec![vec![b'x'; 16], vec![b'x'; 17]],
        vec![
            b"data: 1234567890\n".to_vec(),
            b"data: 1234567890\n\n".to_vec(),
        ],
    ] {
        let mut framer = SseFramer::with_limits(limits);
        let mut failure = None;
        for chunk in chunks {
            if let Err(error) = framer.push(&chunk, |_| Ok(Flow::More)) {
                failure = Some(error);
                break;
            }
        }
        assert_eq!(failure.expect("bounded frame").kind, Kind::Decode);
        assert!(framer.buffer.len() <= limits.frame_bytes);
        assert!(framer.data.len() <= limits.frame_bytes);
    }
    let mut framer = SseFramer::with_limits(limits);
    assert_eq!(events(&mut framer, &b"data: x\n\n".repeat(100)).len(), 100);
    let mut framer = SseFramer::with_limits(limits);
    assert_eq!(
        framer
            .push(&b"data: x\n\n".repeat(100), |_| Ok(Flow::Done))
            .unwrap(),
        Flow::Done
    );
}

#[test]
fn frame_bytes_include_both_bytes_of_crlf_and_replayed_bom_prefixes() {
    let mut framer = SseFramer::with_limits(super::DecodeLimits {
        frame_bytes: 11,
        ..Default::default()
    });
    assert!(framer.push(b"data: x\r\n\r\n", |_| Ok(Flow::More)).is_ok());
    let mut framer = SseFramer::with_limits(super::DecodeLimits {
        frame_bytes: 10,
        ..Default::default()
    });
    assert!(framer.push(b"data: x\r\n\r\n", |_| Ok(Flow::More)).is_err());
    let mut framer = SseFramer::with_limits(super::DecodeLimits {
        frame_bytes: 1,
        ..Default::default()
    });
    assert!(
        framer
            .push(&[0xef, 0xbb, b'x'], |_| Ok(Flow::More))
            .is_err()
    );
    assert_eq!(framer.buffer.len(), 0);
}
