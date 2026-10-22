use serde::Deserialize;
use serde_json::{Value, json};

use crate::spec::Response;

/// A reply holding every kind of part, output and source.
fn every_part() -> Value {
    let meta = json!({"messages": {"item": {"type": "thinking"}, "fp": "00"}});
    json!({
        "message": {
            "content": [
                {"type": "text", "text": "hi", "meta": meta},
                {"type": "reasoning", "text": "hmm", "redacted": true, "meta": meta},
                {"type": "tool_call", "id": "c1", "name": "bash", "input": {"command": "ls"},
                 "invalid": "{", "provider_executed": true, "meta": meta},
                {"type": "tool_result", "id": "c1", "name": "bash",
                 "output": {"type": "text", "value": "out"}, "error": true, "meta": meta},
                {"type": "tool_result", "id": "c2", "name": "search",
                 "output": {"type": "json", "value": {"hits": [1, 2]}}},
                {"type": "tool_result", "id": "c3", "name": "fetch",
                 "output": {"type": "content", "value": [
                     {"type": "text", "text": "page"},
                     {"type": "media", "media_type": "image/png",
                      "source": {"type": "data", "data": "AAAA"}, "meta": meta},
                 ]}},
                {"type": "media", "media_type": "image/png", "source": {"type": "url", "url": "http://x"}},
                {"type": "media", "media_type": "application/pdf", "source": {"type": "file", "id": "f1"}},
                {"type": "opaque", "meta": meta},
            ],
            "origin": {"protocol": "anthropic.messages", "provider": "anthropic", "model": "claude"},
        },
        "finish": {"reason": "tool_calls", "raw": "tool_use", "message": "why"},
        "usage": {
            "input": 1, "output": 2, "cache_read": 3, "cache_write": 4, "cache_write_long": 5,
            "tool_input": 6, "audio_input": 7, "reasoning": 8, "audio_output": 9,
            "hosted_tool_calls": 10,
        },
        "id": "r1",
        "model": "claude",
        "meta": {"messages": {"usage": {"input_tokens": 1}}},
    })
}

/// `value` with the value at each path through it removed and replaced by
/// one of every JSON type.
fn mutations(value: &Value) -> Vec<(String, Value)> {
    let replacements = [
        Value::Null,
        json!(true),
        json!(1),
        json!(-1),
        json!(1.5),
        json!("text"),
        json!("stop"),
        json!([]),
        json!({}),
    ];
    let mut out = Vec::new();
    let mut paths = Vec::new();
    collect(value, &mut String::new(), &mut paths);
    for path in paths {
        let (parent, key) = path.rsplit_once('/').expect("a path");
        let mut removed = value.clone();
        match removed.pointer_mut(parent) {
            Some(Value::Object(map)) => {
                map.remove(key);
            }
            Some(Value::Array(items)) => {
                items.remove(key.parse().expect("an index"));
            }
            _ => unreachable!(),
        }
        out.push((format!("{path} removed"), removed));
        for replacement in &replacements {
            // serde also reads a variant by its index and a struct from an
            // array of its fields; nothing writes either.
            let struct_as_array =
                replacement.is_array() && value.pointer(&path).is_some_and(Value::is_object);
            if path.ends_with("/type") && replacement.is_number() || struct_as_array {
                continue;
            }
            let mut replaced = value.clone();
            *replaced.pointer_mut(&path).expect("the path") = replacement.clone();
            out.push((format!("{path} = {replacement}"), replaced));
        }
    }
    out
}

fn collect(value: &Value, at: &mut String, paths: &mut Vec<String>) {
    let children: Vec<(String, &Value)> = match value {
        Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v)).collect(),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v))
            .collect(),
        _ => return,
    };
    for (key, child) in children {
        let len = at.len();
        at.push('/');
        at.push_str(&key);
        paths.push(at.clone());
        collect(child, at, paths);
        at.truncate(len);
    }
}

#[test]
fn reads_what_the_derived_deserializer_reads() {
    let whole = every_part();
    let read = Response::from_json(&whole.to_string()).expect("a reply");
    assert_eq!(Response::deserialize(&whole).ok(), Some(read.clone()));
    assert_eq!(serde_json::to_value(&read).expect("a value"), whole);
    let mut checked = 0;
    for (mutation, value) in mutations(&whole) {
        let derived = Response::deserialize(&value).ok();
        assert_eq!(
            Response::from_json(&value.to_string()).ok(),
            derived,
            "{mutation}"
        );
        checked += 1;
    }
    assert!(checked > 1000, "{checked}");
}

#[test]
fn reads_a_minimal_reply() {
    let value = json!({
        "message": {"content": [{"type": "reasoning"}]},
        "finish": {"reason": "stop"},
        "usage": {},
    });
    let read = Response::from_json(&value.to_string()).expect("a reply");
    assert_eq!(Response::deserialize(&value).ok(), Some(read));
}

/// A logged reply's provider state is kept as the text the log wrote, one
/// string per protocol, and is a tree only when its protocol reads it.
#[test]
fn provider_state_is_parsed_only_where_read() {
    let reply = Response::from_json(&every_part().to_string()).expect("a reply");
    let state = r#"{"fp":"00","item":{"type":"thinking"}}"#;
    let part = &reply.message.content[0];
    assert_eq!(part.meta().raw("messages"), Some(state));
    assert_eq!(part.meta().get("messages").unwrap()["fp"], json!("00"));
    // Written out again as that text.
    let written = serde_json::to_value(&reply).expect("a reply writes");
    assert_eq!(
        written["message"]["content"][0]["meta"],
        every_part()["message"]["content"][0]["meta"]
    );
}

/// An origin names one of the four protocols; a log naming a fifth is not
/// this crate's, and no code path can mistake one for its own.
#[test]
fn an_origin_names_a_protocol_the_crate_speaks() {
    let mut reply = every_part();
    reply["message"]["origin"]["protocol"] = json!("short");
    let error = Response::from_json(&reply.to_string()).expect_err("no such protocol");
    assert!(error.to_string().contains("short"), "{error}");
    reply["message"]["origin"]["protocol"] = json!("openai.chat");
    let read = Response::from_json(&reply.to_string()).expect("a reply");
    let origin = read.message.origin.expect("an origin");
    assert_eq!(origin.protocol, crate::ProtocolKey::OpenaiChat);
    assert_eq!(origin.protocol.as_str(), "openai.chat");
}

#[test]
fn persistent_messages_preserve_all_roles_parts_and_provider_state() {
    let reply = Response::from_json(&every_part().to_string()).unwrap();
    let messages: crate::Messages = [
        crate::Message::system("system"),
        crate::Message::user("user"),
        crate::Message::Assistant(reply.message),
        crate::Message::Tool(crate::ToolMessage {
            results: vec![crate::ToolResult {
                id: "c1".into(),
                name: "bash".into(),
                output: crate::Output::Json(crate::Json::encode(&json!({"x":1})).unwrap()),
                error: false,
                meta: crate::Meta::new(),
            }],
        }),
    ]
    .into_iter()
    .collect();
    let encoded = serde_json::to_string(&messages).unwrap();
    assert_eq!(crate::Messages::from_json(&encoded).unwrap(), messages);
}
