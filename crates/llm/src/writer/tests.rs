//! Ported from rig's completion-writer tests.

use serde_json::{Value, json};

use super::*;
use crate::{Accumulator, Response};

/// One reply written by `write`, then ended with no finish reason on a wire
/// that need not state one.
fn write(
    write: impl FnOnce(&mut Writer) -> Result<(), Error>,
) -> (Vec<Event>, Result<Response, Error>) {
    run(
        Writer::new("test").without_finish_reason(),
        End::default(),
        write,
    )
}

fn run(
    mut writer: Writer,
    end: End,
    write: impl FnOnce(&mut Writer) -> Result<(), Error>,
) -> (Vec<Event>, Result<Response, Error>) {
    let mut events = Vec::new();
    let written = write(&mut writer).and_then(|()| writer.end(end).map(drop));
    writer.drain(&mut events);
    let mut acc = Accumulator::new();
    let outcome = written.and_then(|()| {
        for event in &events {
            acc.push(event.clone())?;
        }
        acc.finish(None)
    });
    (events, outcome)
}

fn parts(outcome: Result<Response, Error>) -> Vec<AssistantPart> {
    outcome.expect("the reply ends").message.content
}

fn item(part: &AssistantPart) -> Option<&Value> {
    part.meta().get("test").and_then(|state| state.get("item"))
}

fn call(id: &str, name: &str) -> Open {
    Open::Call {
        id: id.into(),
        name: name.into(),
    }
}

/// A compact view: `start:<name>`, `args:<json>` or `text:<text>`, `end`.
fn shape(events: &[Event]) -> Vec<String> {
    let mut calls = HashSet::new();
    events
        .iter()
        .filter_map(|event| {
            Some(match event {
                Event::Start { index, block, name } => {
                    if *block == Block::ToolCall {
                        calls.insert(*index);
                    }
                    format!("{index}:start:{}", name.as_deref().unwrap_or_default())
                }
                Event::Delta { index, text } if calls.contains(index) => {
                    format!("{index}:args:{text}")
                }
                Event::Delta { index, text } => format!("{index}:text:{text}"),
                Event::End { index, .. } => format!("{index}:end"),
                _ => return None,
            })
        })
        .collect()
}

fn arguments(parts: &[AssistantPart]) -> Vec<(String, Value)> {
    parts
        .iter()
        .filter_map(|part| match part {
            AssistantPart::ToolCall(call) => {
                Some((call.id.to_string(), Value::Object(call.input.clone())))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn parts_keep_the_order_their_items_opened_in() {
    let (_, outcome) = write(|w| {
        w.open(0, Open::Reasoning { redacted: false }, Value::Null)?;
        w.open(1, Open::Text, Value::Null)?;
        w.open(2, call("call_1", "lookup"), Value::Null)?;
        w.push(2, "{}")?;
        // The call finishes first, the reasoning last: neither moves.
        w.finish(2)?;
        w.push(1, "answer")?;
        w.finish(1)?;
        w.push(0, "think")?;
        w.finish(0)
    });
    let kinds: Vec<_> = parts(outcome)
        .iter()
        .map(|part| match part {
            AssistantPart::Reasoning(_) => "reasoning",
            AssistantPart::Text(_) => "text",
            AssistantPart::ToolCall(_) => "call",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["reasoning", "text", "call"]);
}

#[test]
fn deltas_merge_into_the_item_signatures_concatenate_and_unknown_fields_survive() {
    let (_, outcome) = write(|w| {
        w.open(
            7,
            Open::Reasoning { redacted: false },
            json!({"type": "thinking", "thinking": "", "signature": ""}),
        )?;
        for delta in [
            json!({"type": "thinking_delta", "thinking": "hm"}),
            json!({"type": "signature_delta", "signature": "abc"}),
            json!({"type": "signature_delta", "signature": "def"}),
            json!({"type": "novel_delta", "novel": "x", "list": [1]}),
            json!({"type": "novel_delta", "novel": "y", "list": [2]}),
        ] {
            w.edit(7, |item| merge(item, delta.as_object().unwrap()))?;
        }
        w.push(7, "hm")?;
        w.finish(7)
    });
    let parts = parts(outcome);
    assert_eq!(
        item(&parts[0]),
        Some(&json!({
            "type": "thinking",
            "thinking": "hm",
            "signature": "abcdef",
            "novel": "xy",
            "list": [1, 2],
        }))
    );
    assert!(matches!(&parts[0], AssistantPart::Reasoning(r) if r.text == "hm"));
}

#[test]
fn empty_text_survives_only_with_a_provider_item() {
    let (_, outcome) = write(|w| {
        w.open(0, Open::Text, Value::Null)?;
        w.finish(0)?;
        w.open(1, Open::Text, json!({"thoughtSignature": "c2ln"}))?;
        w.finish(1)?;
        w.open(2, Open::Reasoning { redacted: true }, Value::Null)?;
        w.finish(2)
    });
    let parts = parts(outcome);
    assert_eq!(parts.len(), 2);
    assert!(matches!(&parts[0], AssistantPart::Text(text) if text.text.is_empty()));
    assert!(item(&parts[0]).is_some());
    assert!(matches!(&parts[1], AssistantPart::Reasoning(reasoning) if reasoning.redacted));
}

#[test]
fn a_reused_call_id_is_renamed_and_loses_its_item() {
    let (_, outcome) = write(|w| {
        for index in 0..2 {
            w.open(
                index,
                call("call_1", "lookup"),
                json!({"type": "function_call", "call_id": "call_1"}),
            )?;
            w.push(index, "{}")?;
            w.finish(index)?;
        }
        Ok(())
    });
    let parts = parts(outcome);
    let [
        AssistantPart::ToolCall(first),
        AssistantPart::ToolCall(second),
    ] = parts.as_slice()
    else {
        panic!("both calls are kept: {parts:?}");
    };
    assert_eq!(first.id, "call_1");
    assert!(item(&parts[0]).is_some());
    assert_ne!(second.id, "call_1");
    assert!(item(&parts[1]).is_none());
}

#[test]
fn writing_to_an_item_that_is_not_open_is_an_error() {
    let (_, outcome) = write(|w| w.push(9, "orphan"));
    assert!(outcome.is_err());
}

#[test]
fn a_boundaryless_run_continues_while_the_kind_stays_the_same() {
    let (_, outcome) = write(|w| {
        w.run(Open::Text, "a")?;
        w.run(Open::Text, "b")?;
        w.run(Open::Reasoning { redacted: false }, "r")?;
        w.run(Open::Text, "c")?;
        Ok(())
    });
    let texts: Vec<_> = parts(outcome)
        .into_iter()
        .map(|part| match part {
            AssistantPart::Text(text) => format!("text:{}", text.text),
            AssistantPart::Reasoning(reasoning) => format!("reasoning:{}", reasoning.text),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(texts, ["text:ab", "reasoning:r", "text:c"]);
}

#[test]
fn a_wire_reply_that_names_no_finish_reason_failed() {
    let (_, outcome) = run(Writer::new("test"), End::default(), |w| {
        w.whole(0, Open::Text, json!({"type": "text"}), "hi")
    });
    assert_eq!(outcome.unwrap_err().kind, Kind::Incomplete);
}

#[test]
fn parts_from_the_first_unfinished_item_on_keep_no_state() {
    let end = End {
        reason: Some((Reason::Length, None)),
        ..End::default()
    };
    let (_, outcome) = run(Writer::new("test"), end, |w| {
        w.whole(
            0,
            Open::Reasoning { redacted: false },
            json!({"sig": "a"}),
            "plan",
        )?;
        w.open(1, Open::Reasoning { redacted: false }, json!({"sig": "b"}))?;
        w.push(1, "more")?;
        w.whole(2, Open::Text, json!({"id": "msg_1"}), "answer")
    });
    let parts = parts(outcome);
    let states: Vec<bool> = parts.iter().map(|part| item(part).is_some()).collect();
    assert_eq!(states, [true, false, false], "{parts:?}");
}

#[test]
fn a_call_sent_without_an_id_keeps_its_item_only_where_the_item_has_an_id_slot() {
    let idless = |w: &mut Writer| {
        w.fragment(
            Some(0),
            CallFragment {
                id: None,
                name: Some("lookup"),
                arguments: Some("{}"),
            },
        )?;
        w.edit(0, |item| {
            *item = json!({"functionCall": {"name": "lookup"}});
        })?;
        w.finish(0)
    };
    let end = || End {
        reason: Some((Reason::ToolCalls, None)),
        ..End::default()
    };
    let slotted = parts(run(Writer::new("test").with_call_id_slot(), end(), idless).1);
    assert!(item(&slotted[0]).is_some());
    let unslotted = parts(run(Writer::new("test"), end(), idless).1);
    assert!(item(&unslotted[0]).is_none());
}

#[test]
fn index_less_fragments_with_new_ids_are_new_calls() {
    let fragment = |id: Option<&'static str>, arguments: &'static str| CallFragment {
        id,
        name: id.map(|_| "weather"),
        arguments: Some(arguments),
    };
    let (_, outcome) = write(|w| {
        w.fragment(None, fragment(Some("a1"), r#"{"city":"#))?;
        w.fragment(None, fragment(None, r#""Paris"}"#))?;
        w.fragment(None, fragment(Some("b2"), r#"{"city":"Rome"}"#))?;
        w.fragment(Some(0), fragment(Some("c3"), r#"{"city":"Oslo"}"#))?;
        w.fragment(Some(0), fragment(Some("d4"), r#"{"city":"Bern"}"#))?;
        Ok(())
    });
    assert_eq!(
        arguments(&parts(outcome)),
        [
            ("a1".to_owned(), json!({"city": "Paris"})),
            ("b2".to_owned(), json!({"city": "Rome"})),
            ("c3".to_owned(), json!({"city": "Oslo"})),
            ("d4".to_owned(), json!({"city": "Bern"})),
        ]
    );
}

#[test]
fn a_new_id_under_an_index_continues_a_call_whose_arguments_are_incomplete() {
    // GLM sends a fresh id, and no name, with every later chunk of one call.
    let (_, outcome) = write(|w| {
        for (id, name, arguments) in [("x1", "lookup", r#"{"q":"#), ("x2", "", r#""x"}"#)] {
            w.fragment(
                Some(0),
                CallFragment {
                    id: Some(id),
                    name: Some(name),
                    arguments: Some(arguments),
                },
            )?;
        }
        Ok(())
    });
    let calls: Vec<Value> = arguments(&parts(outcome))
        .into_iter()
        .map(|(_, args)| args)
        .collect();
    assert_eq!(calls, [json!({"q": "x"})]);
}

#[test]
fn index_less_id_less_fragments_open_a_call_once_the_last_is_whole() {
    let (_, outcome) = write(|w| {
        for arguments in [r#"{"q":"a"}"#, r#"{"q":"#, r#""b"}"#] {
            w.fragment(
                None,
                CallFragment {
                    id: None,
                    name: Some("weather"),
                    arguments: Some(arguments),
                },
            )?;
        }
        Ok(())
    });
    let calls: Vec<Value> = arguments(&parts(outcome))
        .into_iter()
        .map(|(_, args)| args)
        .collect();
    assert_eq!(calls, [json!({"q": "a"}), json!({"q": "b"})]);
}

#[test]
fn an_empty_or_null_id_names_no_call() {
    for stated in ["", "null"] {
        let (_, outcome) = write(|w| {
            w.fragment(
                Some(0),
                CallFragment {
                    id: Some(stated),
                    name: Some("lookup"),
                    arguments: Some("{}"),
                },
            )
        });
        let calls = arguments(&parts(outcome));
        assert!(calls[0].0.starts_with("call_"), "{calls:?}");
    }
}

#[test]
fn a_named_call_starts_when_it_opens_and_streams_each_fragment() {
    let (events, outcome) = write(|w| {
        w.open(0, call("call_1", "lookup"), Value::Null)?;
        w.push(0, r#"{"city":"#)?;
        w.open(1, Open::Text, Value::Null)?;
        w.push(1, "checking")?;
        w.push(0, r#""Paris"}"#)?;
        w.finish(0)?;
        w.finish(1)
    });
    assert_eq!(
        shape(&events),
        [
            "0:start:lookup",
            r#"0:args:{"city":"#,
            "1:start:",
            "1:text:checking",
            r#"0:args:"Paris"}"#,
            "0:end",
            "1:end",
        ]
    );
    assert_eq!(arguments(&parts(outcome))[0].1, json!({"city": "Paris"}));
}

#[test]
fn fragments_before_the_name_wait_for_it_then_stream() {
    let (events, _) = write(|w| {
        let fragment = |name, arguments| CallFragment {
            id: Some("call_1"),
            name,
            arguments,
        };
        w.fragment(Some(0), fragment(None, Some(r#"{"x":"#)))?;
        w.fragment(Some(0), fragment(Some("add"), None))?;
        w.fragment(Some(0), fragment(None, Some("1,")))?;
        w.fragment(Some(0), fragment(None, Some(r#""y":2}"#)))?;
        Ok(())
    });
    assert_eq!(
        shape(&events),
        [
            "0:start:add",
            r#"0:args:{"x":"#,
            "0:args:1,",
            r#"0:args:"y":2}"#,
            "0:end"
        ]
    );
}

#[test]
fn parallel_calls_interleave_under_their_own_parts() {
    let (events, _) = write(|w| {
        let fragment = |id, name, arguments| CallFragment {
            id: Some(id),
            name: Some(name),
            arguments: Some(arguments),
        };
        w.fragment(Some(0), fragment("a", "add", r#"{"x":"#))?;
        w.fragment(Some(1), fragment("b", "weather", r#"{"city":"#))?;
        w.fragment(Some(0), fragment("a", "add", "1}"))?;
        w.fragment(Some(1), fragment("b", "weather", r#""Rome"}"#))?;
        Ok(())
    });
    assert_eq!(
        shape(&events),
        [
            "0:start:add",
            r#"0:args:{"x":"#,
            "1:start:weather",
            r#"1:args:{"city":"#,
            "0:args:1}",
            r#"1:args:"Rome"}"#,
            "0:end",
            "1:end",
        ]
    );
}

#[test]
fn a_call_sent_whole_starts_streams_once_and_ends() {
    let (events, _) = write(|w| {
        w.whole(0, call("call_1", "add"), Value::Null, r#"{"x":1}"#)?;
        w.open(1, call("call_2", "noop"), Value::Null)?;
        w.finish(1)
    });
    assert_eq!(
        shape(&events),
        [
            "0:start:add",
            r#"0:args:{"x":1}"#,
            "0:end",
            "1:start:noop",
            "1:args:{}",
            "1:end"
        ]
    );
}

#[test]
fn a_call_that_never_names_a_tool_streams_nothing() {
    let (events, _) = write(|w| {
        w.fragment(
            Some(0),
            CallFragment {
                id: Some("call_1"),
                name: None,
                arguments: Some(r#"{"x":1}"#),
            },
        )?;
        w.open(1, Open::Text, Value::Null)?;
        w.push(1, "done")?;
        w.finish(1)
    });
    assert_eq!(shape(&events), ["1:start:", "1:text:done", "1:end"]);
}

#[test]
fn a_placeholder_null_waits_for_the_real_arguments() {
    let (events, outcome) = write(|w| {
        let fragment = |arguments| CallFragment {
            id: Some("call_1"),
            name: Some("add"),
            arguments: Some(arguments),
        };
        w.fragment(Some(0), fragment("null"))?;
        w.fragment(Some(0), fragment(r#"{"x":"#))?;
        w.fragment(Some(0), fragment("1}"))?;
        Ok(())
    });
    assert_eq!(
        shape(&events),
        ["0:start:add", r#"0:args:{"x":"#, "0:args:1}", "0:end"]
    );
    assert_eq!(arguments(&parts(outcome))[0].1, json!({"x": 1}));
}

#[test]
fn a_started_call_keeps_the_name_its_start_stated() {
    let (events, outcome) = write(|w| {
        let fragment = |name, arguments| CallFragment {
            id: Some("call_1"),
            name,
            arguments,
        };
        w.fragment(Some(0), fragment(Some("add"), Some(r#"{"x":1,"#)))?;
        w.fragment(Some(0), fragment(Some("multiply"), Some(r#""y":2}"#)))?;
        Ok(())
    });
    assert_eq!(
        shape(&events),
        [
            "0:start:add",
            r#"0:args:{"x":1,"#,
            r#"0:args:"y":2}"#,
            "0:end"
        ]
    );
    assert!(matches!(&parts(outcome)[0], AssistantPart::ToolCall(call) if call.name == "add"));
}

#[test]
fn a_placeholder_null_never_replaced_streams_the_ended_arguments() {
    let (events, _) = write(|w| {
        w.fragment(
            Some(0),
            CallFragment {
                id: Some("call_1"),
                name: Some("noop"),
                arguments: Some("null"),
            },
        )
    });
    assert_eq!(shape(&events), ["0:start:noop", "0:args:{}", "0:end"]);
}

#[test]
fn restating_an_opaque_item_keeps_it() {
    let (_, outcome) = write(|w| {
        w.open(
            0,
            Open::Opaque { replay: true },
            json!({"type": "compaction", "summary": "kept"}),
        )?;
        w.restate(0, "ignored")?;
        w.finish(0)
    });
    let parts = parts(outcome);
    assert!(
        matches!(&parts[..], [AssistantPart::Opaque(_)]),
        "{parts:?}"
    );
    assert_eq!(item(&parts[0]).unwrap()["summary"], "kept");
    assert_eq!(parts[0].meta()["test"]["replay"], true);
}

#[test]
fn a_restatement_replaces_what_streamed() {
    let (events, outcome) = write(|w| {
        w.open(0, Open::Text, Value::Null)?;
        w.push(0, "helo")?;
        w.restate(0, "hello")?;
        w.finish(0)
    });
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::End { text: Some(text), .. } if text == "hello"))
    );
    assert!(matches!(&parts(outcome)[0], AssistantPart::Text(text) if text.text == "hello"));
}

#[test]
fn a_stop_with_calls_finishes_as_tool_calls() {
    let end = End {
        reason: Some((Reason::Stop, Some("stop".into()))),
        ..End::default()
    };
    let (_, outcome) = run(Writer::new("test"), end, |w| {
        w.whole(0, call("c", "f"), Value::Null, "{}")
    });
    assert_eq!(outcome.unwrap().finish.reason, Reason::ToolCalls);
}

#[test]
fn an_order_by_index_states_the_message_order() {
    let end = End {
        reason: Some((Reason::Stop, None)),
        ..End::default()
    };
    let mut writer = Writer::new("test");
    writer.order_by_index();
    let (_, outcome) = run(writer, end, |w| {
        w.whole(1, Open::Text, Value::Null, "second")?;
        w.whole(0, Open::Text, Value::Null, "first")
    });
    let texts: Vec<_> = parts(outcome)
        .into_iter()
        .map(|part| match part {
            AssistantPart::Text(text) => text.text,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(texts, ["first", "second"]);
}
