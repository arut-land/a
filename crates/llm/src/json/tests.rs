use super::*;

#[test]
fn complete_arguments_at_the_byte_limit_keep_their_fields() {
    let mut text = serde_json::json!({"a": 1}).to_string();
    text.extend(std::iter::repeat_n(
        ' ',
        DecodeLimits::default().argument_bytes - text.len(),
    ));
    assert_eq!(
        parse_partial_arguments(&text).get("a"),
        Some(&Value::from(1))
    );
}

/// `at` finds what `Value::pointer` finds for a pointer that escapes
/// nothing, `null` aside.
#[test]
fn at_reads_a_pointer_as_value_pointer_does() {
    let value = serde_json::json!({"a": {"b": [10, {"c": null}, 12]}, "": 1, "n": null});
    for pointer in [
        "",
        "/a",
        "/a/b",
        "/a/b/0",
        "/a/b/2",
        "/a/b/3",
        "/a/b/00",
        "/a/b/+1",
        "/a/b/-1",
        "/a/b/1/c",
        "/a/x",
        "/",
        "a",
        "/n",
        "/a/b/0/x",
        "/a/b/1/c/d",
    ] {
        let expected = value.pointer(pointer).filter(|value| !value.is_null());
        assert_eq!(value.at(pointer), expected, "{pointer}");
    }
}

#[test]
fn text_that_states_no_object_has_none() {
    for text in ["not json", "[1, 2", "\"str", "12"] {
        assert_eq!(parse_partial_object(text), None, "{text}");
    }
}
mod partial_arguments {
    use super::super::parse_partial_arguments;
    use serde_json::{Value, json};

    fn parsed(text: &str) -> Value {
        Value::Object(parse_partial_arguments(text))
    }

    #[test]
    fn cut_off_text_keeps_what_it_states() {
        for (text, expected) in [
            ("", json!({})),
            ("   ", json!({})),
            ("{", json!({})),
            (r#"{"path"#, json!({})),
            (r#"{"path":"#, json!({})),
            (r#"{"path": "no"#, json!({"path": "no"})),
            (r#"{"path": "notes.md","#, json!({"path": "notes.md"})),
            (r#"{"path": "notes.md", "con"#, json!({"path": "notes.md"})),
            (r#"{"a": {"b": [1, 2"#, json!({"a": {"b": [1, 2]}})),
            (r#"{"a": [{"b": "c"}, {"d"#, json!({"a": [{"b": "c"}, {}]})),
            (r#"{"a": 1, "b": 2,"#, json!({"a": 1, "b": 2})),
            (r#"{"line": "one\ntw"#, json!({"line": "one\ntw"})),
            (r#"{"quote": "say \"hi"#, json!({"quote": "say \"hi"})),
        ] {
            assert_eq!(parsed(text), expected, "{text}");
        }
    }

    #[test]
    fn escapes_cut_short_keep_the_string_before_them() {
        for (text, expected) in [
            (r#"{"a": "x\"#, json!({"a": "x"})),
            (r#"{"a": "x\u"#, json!({"a": "x"})),
            (r#"{"a": "x\u00"#, json!({"a": "x"})),
            (r#"{"a": "x\u00e"#, json!({"a": "x"})),
            (r#"{"a": "x\u00e9"#, json!({"a": "xé"})),
            (r#"{"a": "x\\u00"#, json!({"a": "x\\u00"})),
        ] {
            assert_eq!(parsed(text), expected, "{text}");
        }
    }

    #[test]
    fn numbers_and_literals_cut_short_are_dropped_until_valid() {
        for (text, expected) in [
            (r#"{"n": 1"#, json!({"n": 1})),
            (r#"{"n": 1."#, json!({})),
            (r#"{"n": -"#, json!({})),
            (r#"{"n": 1e"#, json!({})),
            (r#"{"n": 1.5"#, json!({"n": 1.5})),
            (r#"{"b": tru"#, json!({})),
            (r#"{"b": true"#, json!({"b": true})),
            (r#"{"v": nul"#, json!({})),
            (r#"{"a": 1, "v": nul"#, json!({"a": 1})),
        ] {
            assert_eq!(parsed(text), expected, "{text}");
        }
    }

    #[test]
    fn raw_control_characters_and_stray_backslashes_are_repaired() {
        assert_eq!(
            parsed("{\"text\": \"line one\nline two\ttabbed\"}"),
            json!({"text": "line one\nline two\ttabbed"})
        );
        assert_eq!(
            parsed(r#"{"path": "C:\Users\me"}"#),
            json!({"path": "C:\\Users\\me"})
        );
        assert_eq!(
            parsed("{\"text\": \"cut\nshort"),
            json!({"text": "cut\nshort"})
        );
    }

    #[test]
    fn a_complete_object_parses_as_serde_does() {
        let text = r#"{"path": "a.md", "content": "x\ny \"q\" \u00e9", "n": [1, 2.5, -3e2], "o": {"t": true, "f": null}}"#;
        assert_eq!(
            parsed(text),
            serde_json::from_str::<Value>(text).expect("valid json")
        );
    }

    #[test]
    fn a_top_level_that_is_not_an_object_is_empty() {
        for text in [
            "[1, 2]", "[1, 2", "\"str\"", "\"str", "12", "true", "null", "not json",
        ] {
            assert_eq!(parsed(text), json!({}), "{text}");
        }
    }

    #[test]
    fn every_prefix_parses_and_the_whole_text_parses_exactly() {
        for whole in [
            json!({"path": "notes/today.md", "content": "Line one.\nLine \"two\" \\ é ✓\n\tIndented."}),
            json!({"x": 1, "y": -2.5, "z": 3e10, "flags": [true, false, null], "nested": {"a": [{"b": "c"}]}}),
            json!({"query": "", "limit": 0, "filters": {}, "tags": []}),
        ] {
            for text in [
                whole.to_string(),
                serde_json::to_string_pretty(&whole).expect("json"),
            ] {
                for (cut, _) in text.char_indices() {
                    // Never fails, and always an object.
                    let _ = parse_partial_arguments(&text[..cut]);
                }
                assert_eq!(parsed(&text), whole);
            }
        }
    }
}

#[test]
fn partial_json_checkpoint_memory_is_bounded() {
    let limits = DecodeLimits {
        argument_bytes: 128 * 1024,
        depth: 8,
        ..Default::default()
    };
    let text = format!(r#"{{"a":[{}"#, "0,".repeat(20_000));
    let cuts = checkpoints(&text, limits).expect("bounded input");
    assert_eq!(cuts.len(), CHECKPOINTS);
    assert_eq!(cuts.capacity(), CHECKPOINTS);
    assert!(cuts.iter().all(|(_, stack, _)| stack.len() <= limits.depth));
    assert!(
        cuts.iter()
            .map(|(_, stack, _)| stack.capacity())
            .sum::<usize>()
            <= CHECKPOINTS * limits.depth
    );
    assert!(checkpoints(&"[".repeat(limits.depth + 1), limits).is_none());
    assert!(checkpoints(&" ".repeat(limits.argument_bytes + 1), limits).is_none());
    assert!(parse_partial_object(r#"{"a":[1,2,3],"b":"cut"#).is_some());
}

#[test]
fn partial_candidates_spend_their_copy_budget_before_allocating() {
    let text = r#"{"a": 1,"b":"cut"#;
    let limits = DecodeLimits {
        reply_bytes: text.len() - 1,
        ..Default::default()
    };
    assert!(partial_object(text, limits).is_none());
    assert!(partial_object(text, DecodeLimits::default()).is_some());
}

#[test]
fn argument_repair_charges_only_the_bytes_it_adds() {
    let limit = DecodeLimits::default().argument_bytes;
    let mut text = String::from("{\"value\":\"\n\"}");
    text.extend(std::iter::repeat_n(' ', limit - text.len() - 1));
    assert_eq!(parse_partial_arguments(&text)["value"], "\n");
    text.push(' ');
    assert!(parse_partial_arguments(&text).is_empty());
}
