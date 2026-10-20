//! Ported from rig's request and replay tests (`completion/tests.rs`,
//! `streaming/tests.rs`).

use super::*;

// ── replay ─────────────────────────────────────────────────────────────

/// The streamed twin of a signed answer sends the text, then an empty part
/// with the signature, which joins the same text. It replays as Gemini's
/// unary reply states it.
#[test]
fn a_trailing_signature_joins_the_text_it_follows() {
    let response = decoded_by(
        "gemini-3-flash-preview",
        &[
            chunk(json!([{"text": "289"}]), None),
            chunk(
                json!([{"text": "", "thoughtSignature": "c2lnbmF0dXJl"}]),
                Some("STOP"),
            ),
        ],
    )
    .expect("the reply decodes");
    let signed = json!({"text": "289", "thoughtSignature": "c2lnbmF0dXJl"});
    let [part @ AssistantPart::Text(text)] = response.message.content.as_slice() else {
        panic!("one answer text: {:?}", response.message.content);
    };
    assert_eq!(text.text, "289");
    assert_eq!(item(part).as_ref(), Some(&signed));
    let body = replayed("gemini-3-flash-preview", &response);
    assert_eq!(body["contents"][1]["parts"], json!([signed]));
}

/// A part that carries only a signature signs the part beside it, whatever
/// its kind: the text or call before it, or the part after it when it comes
/// first.
#[test]
fn a_signature_alone_signs_the_part_beside_it() {
    let model = "gemini-3-flash-preview";
    let after_text = decoded_by(
        model,
        &[
            chunk(json!([{"text": "Answer"}]), None),
            chunk(json!([{"thoughtSignature": "c2ln"}]), Some("STOP")),
        ],
    )
    .expect("the reply decodes");
    assert_eq!(after_text.message.content.len(), 1);
    let body = replayed(model, &after_text);
    assert_eq!(
        body["contents"][1]["parts"],
        json!([{"text": "Answer", "thoughtSignature": "c2ln"}])
    );

    let after_call = decoded_by(
        model,
        &[
            chunk(
                json!([{"functionCall": {"name": "lookup", "args": {"q": 1}, "id": "a"}}]),
                None,
            ),
            chunk(json!([{"thoughtSignature": "c2lnLWNhbGw="}]), Some("STOP")),
        ],
    )
    .expect("the reply decodes");
    let body = replayed(model, &after_call);
    assert_eq!(
        body["contents"][1]["parts"][0]["thoughtSignature"],
        "c2lnLWNhbGw="
    );

    let leading = decoded_by(
        model,
        &[chunk(
            json!([{"thoughtSignature": "c2lnLWxlYWQ="}, {"text": "Answer"}]),
            Some("STOP"),
        )],
    )
    .expect("the reply decodes");
    let body = replayed(model, &leading);
    assert_eq!(
        body["contents"][1]["parts"],
        json!([{"text": "Answer", "thoughtSignature": "c2lnLWxlYWQ="}])
    );
}

/// A reply part that is not an object is no part Gemini takes back.
#[test]
fn a_part_that_is_not_an_object_never_replays() {
    for odd in [json!(null), json!("stray")] {
        let response = decoded(&[chunk(json!([odd.clone(), {"text": "hi"}]), Some("STOP"))])
            .expect("the reply decodes");
        let body = replayed("gemini-2.5-flash", &response);
        assert_eq!(
            body["contents"][1]["parts"],
            json!([{"text": "hi"}]),
            "{odd}"
        );
    }
}

/// Every part kind, an invented kind, and invented fields on known kinds
/// survive decoding and same-model replay, whole and streamed.
#[test]
fn every_part_kind_survives_decode_and_replay() {
    let parts = [
        json!({"thought": true, "text": "why", "thoughtSignature": "c2ln"}),
        json!({"text": "plain", "futureField": 1}),
        json!({"functionCall": {"name": "lookup", "args": {"q": 1}, "id": "c1", "futureField": true}, "thoughtSignature": "c2ln"}),
        json!({"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgo="}}),
        json!({"functionResponse": {"name": "lookup", "response": {"ok": true}}}),
        json!({"fileData": {"mimeType": "application/pdf", "fileUri": "gs://bucket/a.pdf"}}),
        json!({"executableCode": {"language": "PYTHON", "code": "print(1)"}}),
        json!({"codeExecutionResult": {"outcome": "OUTCOME_OK", "output": "1"}}),
        json!({"futureKind": {"x": 1}}),
    ];
    let model = "gemini-3-flash-preview";
    let whole = vec![chunk(json!(parts), Some("STOP"))];
    let mut streamed: Vec<Value> = parts
        .iter()
        .map(|part| chunk(json!([part]), None))
        .collect();
    streamed.push(chunk(json!([]), Some("STOP")));
    for chunks in [whole, streamed] {
        let response = decoded_by(model, &chunks).expect("the reply decodes");
        assert_eq!(response.message.content.len(), parts.len());
        let mut history = vec![
            Message::user("q"),
            Message::Assistant(response.message.clone()),
        ];
        // The model's own call is answered.
        history.push(tool_result("c1", "lookup", Output::Text("ok".into())));
        let body = sent(model, asking(history));
        assert_eq!(body["contents"][1]["parts"], json!(parts));
    }
}

/// An answer signature reaches only the model that issued it: for another
/// model the text goes without it.
#[test]
fn a_text_signature_reaches_no_other_model() {
    let response = decoded_by(
        "gemini-3-flash-preview",
        &[chunk(
            json!([{"text": "the answer", "thoughtSignature": "c2lnbmVk"}]),
            Some("STOP"),
        )],
    )
    .expect("the reply decodes");
    for (model, part) in [
        (
            "gemini-3-flash-preview",
            json!({"text": "the answer", "thoughtSignature": "c2lnbmVk"}),
        ),
        ("gemini-2.5-flash", json!({"text": "the answer"})),
    ] {
        let body = replayed(model, &response);
        assert_eq!(body["contents"][1]["parts"], json!([part]), "{model}");
    }
}

/// A signed answer survives serde, which is how a history persists, and
/// still replays as Gemini sent it.
#[test]
fn a_signed_answer_round_trips_through_serde() {
    let response = decoded_by(
        "gemini-3-flash-preview",
        &[chunk(
            json!([{"text": "the answer", "thoughtSignature": "c2lnbmVk"}]),
            Some("STOP"),
        )],
    )
    .expect("the reply decodes");
    let json = serde_json::to_string(&response).expect("the reply serializes");
    let loaded: Response = serde_json::from_str(&json).expect("the reply loads");
    assert_eq!(loaded, response);
    let body = replayed("gemini-3-flash-preview", &loaded);
    let part = &body["contents"][1]["parts"][0];
    assert_eq!(part["thoughtSignature"], "c2lnbmVk");
    assert_ne!(part.get("thought"), Some(&json!(true)));
}

/// A part rebuilt from its shared fields: text alone, a thought flag on
/// reasoning, a call id only for a model that takes ids, and nothing for
/// redacted reasoning. Gemini 3 also gets Google's placeholder signature on
/// the call.
#[test]
fn shared_fields_rebuild_as_gemini_takes_them() {
    let message = Message::Assistant(Assistant {
        content: vec![
            AssistantPart::Reasoning(Reasoning {
                text: "why".into(),
                ..Reasoning::default()
            }),
            AssistantPart::Reasoning(Reasoning {
                redacted: true,
                ..Reasoning::default()
            }),
            AssistantPart::Text(Text::new("answer")),
            AssistantPart::Text(Text::new("  ")),
            AssistantPart::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "lookup".into(),
                input: crate::Json::encode(&json!({"q": 1})).unwrap(),
                invalid: None,
                provider_executed: false,
                meta: Meta::new(),
            }),
        ],
        origin: None,
    });
    for (model, call) in [
        (
            "gemini-2.5-flash",
            json!({"functionCall": {"name": "lookup", "args": {"q": 1}}}),
        ),
        (
            "gemini-3-flash-preview",
            json!({
                "functionCall": {"name": "lookup", "args": {"q": 1}, "id": "call-1"},
                "thoughtSignature": "skip_thought_signature_validator",
            }),
        ),
    ] {
        let contents = contents_for(model, std::slice::from_ref(&message));
        assert_eq!(
            contents[0]["parts"],
            json!([{"thought": true, "text": "why"}, {"text": "answer"}, call]),
            "{model}"
        );
    }
}

/// A signature that is not base64 is left out of the replayed part: Gemini
/// rejects the whole request over one. Every other field goes as received.
#[test]
fn a_signature_that_is_not_base64_is_left_out() {
    let message = Message::Assistant(Assistant {
        content: vec![
            native(
                "a",
                json!({"text": "a", "thoughtSignature": "not base64!", "extra": 1}),
            ),
            native("b", json!({"text": "b", "thoughtSignature": "c2ln"})),
        ],
        origin: None,
    });
    assert_eq!(
        contents_for("gemini-2.5-flash", &[message])[0]["parts"],
        json!([{"text": "a", "extra": 1}, {"text": "b", "thoughtSignature": "c2ln"}])
    );
}

/// Call ids go only to models that take them; another origin's id is made
/// legal for them.
#[test]
fn call_ids_follow_the_models_that_take_them() {
    for (model, takes) in [
        ("gemini-2.5-flash", false),
        ("gemini-2.0-flash", false),
        ("gemini-3-flash-preview", true),
        ("gemini-3.8-flash", true),
        ("models/gemini-3.8-flash", true),
        ("gemini-flash-latest", true),
        ("gemini-live-3-flash", true),
        ("claude-sonnet-4-5", true),
        ("gpt-oss-120b", true),
        ("gemma-3-27b-it", false),
    ] {
        assert_eq!(
            crate::models::google::takes_call_ids(model),
            takes,
            "{model}"
        );
    }
    let foreign = format!("call.{}|x", "y".repeat(80));
    let fitted = Gemini::default().tool_id(&foreign).expect("refitted");
    assert_eq!(fitted.len(), 64);
    assert!(fitted.starts_with("call_yyy"));
    assert_eq!(Gemini::default().tool_id("call_1"), None);
}

/// A call Gemini sent without an id takes the one the writer issued, on the
/// call and its response alike, for a model that takes ids.
#[test]
fn an_issued_call_id_spells_call_and_response() {
    let model = "gemini-3-flash-preview";
    let response = decoded_by(
        model,
        &[chunk(
            json!([{"functionCall": {"name": "lookup", "args": {}}, "thoughtSignature": "c2ln"}]),
            Some("STOP"),
        )],
    )
    .expect("the reply decodes");
    let id = response
        .message
        .tool_calls()
        .next()
        .expect("a call")
        .id
        .clone();
    let body = replayed(model, &response);
    assert_eq!(
        body["contents"][1]["parts"][0]["functionCall"]["id"],
        json!(id)
    );
    assert_eq!(body["contents"][1]["parts"][0]["thoughtSignature"], "c2ln");
    assert_eq!(
        body["contents"][2]["parts"][0]["functionResponse"]["id"],
        json!(id)
    );
    let body = replayed("gemini-2.5-flash", &response);
    assert!(
        body["contents"][1]["parts"][0]["functionCall"]
            .get("id")
            .is_none(),
        "{body}"
    );
    assert!(
        body["contents"][2]["parts"][0]["functionResponse"]
            .get("id")
            .is_none(),
        "{body}"
    );
}

#[test]
fn current_turn_replay_drops_only_finished_signatures() {
    let signed = |text: &str| {
        Message::Assistant(Assistant {
            content: vec![native(
                text,
                json!({"text": text, "thoughtSignature": "c2ln"}),
            )],
            origin: None,
        })
    };
    let mut contents = contents_for(
        "gemini-2.5-flash",
        &[
            Message::user("one"),
            signed("first"),
            Message::user("two"),
            signed("second"),
        ],
    );
    drop_finished_signatures(&mut contents);
    assert_eq!(contents[1]["parts"], json!([{"text": "first"}]));
    assert_eq!(
        contents[3]["parts"],
        json!([{"text": "second", "thoughtSignature": "c2ln"}])
    );
}

// ── tool results ───────────────────────────────────────────────────────

#[test]
fn a_tool_result_carries_its_images_on_gemini_3() {
    let contents = contents_for(
        "gemini-3-flash-preview",
        &[tool_result(
            "call-123",
            "test_tool",
            Output::Content(vec![
                UserPart::Text(Text::new(r#"{"status": "success"}"#)),
                image("image/png", "iVBORw0KGgo="),
            ]),
        )],
    );
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0]["role"], "user");
    let response = the_function_response(&contents);
    assert_eq!(response["name"], "test_tool");
    assert_eq!(response["id"], "call-123");
    assert_eq!(
        response["response"],
        json!({"result": r#"{"status": "success"}"#})
    );
    assert_eq!(
        response["parts"],
        json!([{"inlineData": {"mimeType": "image/png", "data": "iVBORw0KGgo="}}])
    );
}

#[test]
fn mixed_images_and_values_keep_their_order() {
    let model = "gemini-3-flash-preview";
    let contents = contents_for(
        model,
        &[tool_result(
            "c",
            "ordered_tool",
            Output::Content(vec![
                image("image/png", "first-image"),
                UserPart::Text(Text::new("between-images")),
                image("image/jpeg", "second-image"),
            ]),
        )],
    );
    let response = the_function_response(&contents);
    assert_eq!(response["response"], json!({"result": "between-images"}));
    assert_eq!(response["parts"][0]["inlineData"]["data"], "first-image");
    assert_eq!(response["parts"][1]["inlineData"]["mimeType"], "image/jpeg");

    let cases = [
        (
            Output::Text(r#"{"status":"ok"}"#.into()),
            json!({"result": "{\"status\":\"ok\"}"}),
        ),
        (
            Output::Json(crate::Json::encode(&json!({"status": "ok"})).unwrap()),
            json!({"result": {"status": "ok"}}),
        ),
        // A structured value stays literal, `$ref` and all.
        (
            Output::Json(
                crate::Json::encode(&json!({"literal": {"$ref": "tool_result_image_0"}})).unwrap(),
            ),
            json!({"result": {"literal": {"$ref": "tool_result_image_0"}}}),
        ),
    ];
    for (output, expected) in cases {
        let contents = contents_for(model, &[tool_result("c", "t", output)]);
        assert_eq!(the_function_response(&contents)["response"], expected);
    }
}

/// A function response holds JPEG, PNG and WEBP data only: other media,
/// and every image for Gemini 2, follow in a user content after the
/// responses, and Gemini 2's texts join into one.
#[test]
fn media_a_response_cannot_hold_follow_it() {
    let output = || {
        Output::Content(vec![
            UserPart::Text(Text::new("see")),
            UserPart::Text(Text::new("more")),
            image("image/png", "aW1hZ2U="),
            image("image/gif", "Z2lm"),
            UserPart::Media(Media {
                media_type: "image/png".into(),
                source: Source::Url {
                    url: "https://example.com/image.png".into(),
                },
                meta: Meta::new(),
            }),
        ])
    };
    let history = |model: &str| {
        contents_for(
            model,
            &[tool_result("c", "shot", output()), Message::user("next")],
        )
    };

    let contents = history("gemini-3-flash-preview");
    let response = the_function_response(&contents);
    assert_eq!(response["response"], json!({"result": ["see", "more"]}));
    assert_eq!(response["parts"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        contents[1]["parts"],
        json!([
            {"text": RESULT_IMAGES, "thought": false},
            {"inlineData": {"mimeType": "image/gif", "data": "Z2lm"}, "thought": false},
            {"fileData": {"mimeType": "image/png", "fileUri": "https://example.com/image.png"}, "thought": false},
            {"text": "next", "thought": false},
        ])
    );

    let contents = history("gemini-2.5-flash");
    let response = the_function_response(&contents);
    assert_eq!(response["response"], json!({"result": "see\nmore"}));
    assert!(response.get("parts").is_none(), "{response}");
    assert_eq!(contents[1]["parts"].as_array().map(Vec::len), Some(5));
}

#[test]
fn a_result_that_says_nothing_says_so() {
    for (output, error, expected) in [
        (
            Output::Text(String::new()),
            false,
            json!({"result": NO_OUTPUT}),
        ),
        (
            Output::Content(vec![]),
            true,
            json!({"error": format!("[tool error] {NO_OUTPUT}")}),
        ),
    ] {
        let message = Message::Tool(ToolMessage {
            results: vec![ToolResult {
                id: "c".into(),
                name: "t".into(),
                output,
                error,
                meta: Meta::new(),
            }],
        });
        let contents = contents_for("gemini-2.5-flash", &[message]);
        assert_eq!(the_function_response(&contents)["response"], expected);
    }
}

// ── the request ────────────────────────────────────────────────────────

/// A turn with nothing to send is no content: Gemini rejects one with no
/// parts.
#[test]
fn a_turn_with_nothing_to_send_is_no_content() {
    let empty = Message::Assistant(Assistant {
        content: vec![AssistantPart::Reasoning(Reasoning::default())],
        origin: None,
    });
    let contents = contents_for(
        "gemini-3-flash-preview",
        &[Message::user("q"), empty, Message::user("next")],
    );
    assert!(
        contents
            .iter()
            .all(|content| !content.arr("parts").is_empty()),
        "{contents:?}"
    );
    assert_eq!(contents.len(), 1);
}

/// Gemini takes system text only in its instruction: a later system message
/// joins it, and the user turns it separated become one content.
#[test]
fn a_later_system_message_joins_the_instruction() {
    let body = sent(
        "gemini-2.5-flash",
        asking(vec![
            Message::system("s0"),
            Message::user("a"),
            Message::assistant("ok"),
            Message::user("b"),
            Message::system("later"),
            Message::user("c"),
        ]),
    );
    let roles: Vec<&str> = body["contents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|content| content.str("role"))
        .collect();
    assert_eq!(roles, ["user", "model", "user"]);
    assert_eq!(
        body["systemInstruction"],
        json!({"parts": [{"text": "s0", "thought": false}, {"text": "later", "thought": false}]})
    );
}

#[test]
fn the_request_goes_to_the_models_stream() {
    let gemini = HOSTS[0].0;
    assert_eq!(gemini.key, KeyStyle::Header("x-goog-api-key"));
    let provider = Provider::new(
        gemini.id,
        gemini.base_url,
        Auth::Header {
            name: "x-goog-api-key",
            key: crate::Secret::new("k"),
        },
        Gemini::new(HOSTS[0].1),
    );
    for model in ["gemini-2.5-flash", "models/gemini-2.5-flash"] {
        let request = provider
            .request(model, &asking(vec![Message::user("hi")]))
            .expect("the request builds");
        assert_eq!(
            request.uri(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        assert_eq!(request.headers()["x-goog-api-key"], "k");
        assert_eq!(request.headers()["accept"], "text/event-stream");
    }
}

#[test]
fn tools_choice_and_format_take_gemini_spellings() {
    let request = Request {
        messages: vec![Message::user("hi")].into(),
        tools: vec![
            Tool {
                name: "add".into(),
                description: "Add x and y".into(),
                schema: json!({"type": "object", "properties": {"x": {"type": "number"}}}),
                strict: None,
            },
            Tool {
                name: "now".into(),
                description: String::new(),
                schema: Value::Null,
                strict: None,
            },
        ]
        .into(),
        tool_choice: crate::ToolChoice::Tool("add".into()),
        format: Some(Format::Json {
            schema: Some(json!({"type": "object"})),
            name: None,
            description: None,
        }),
        options: crate::spec::Options::from([(
            KEY.to_owned(),
            json!({"tools": [{"codeExecution": {}}]}),
        )]),
        ..Request::default()
    };
    let body = sent("gemini-2.0-flash", request);
    assert_eq!(
        body["tools"],
        json!([
            {"functionDeclarations": [
                {"name": "add", "description": "Add x and y", "parametersJsonSchema": {"type": "object", "properties": {"x": {"type": "number"}}}},
                {"name": "now"},
            ]},
            {"codeExecution": {}},
        ])
    );
    assert_eq!(
        body["toolConfig"],
        json!({"functionCallingConfig": {"mode": "ANY", "allowed_function_names": ["add"]}})
    );
    assert_eq!(
        body["generationConfig"],
        json!({"responseMimeType": "application/json", "responseJsonSchema": {"type": "object"}})
    );
    let body = sent(
        "gemini-2.0-flash",
        Request {
            tool_choice: crate::ToolChoice::None,
            ..asking(vec![Message::user("hi")])
        },
    );
    assert_eq!(
        body["toolConfig"],
        json!({"functionCallingConfig": {"mode": "NONE"}})
    );
    assert!(body.get("tools").is_none());
}
