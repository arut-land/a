use super::*;
use protocol::{Epoch, Json, Mode, SessionId};
fn spawn_image(image: protocol::publication::ContextImage) -> Json {
    let intent = crate::tests::spawn_intent(1, "");
    let mut intent = crate::driver::spawn::SpawnIntent::from_json(&intent).unwrap();
    intent.image = image;
    intent.into_json().unwrap()
}

#[test]
fn a_prefix_preserves_on_stop_context() {
    let entries = [entry(
        1,
        Body::Decision {
            call: None,
            by: "review".into(),
            d: Verdict::OnStop {
                text: "continue with the missing verification".into(),
            },
        },
    )];
    let registry = Arc::new(Registry::default());
    let expected = Transcript::of(&entries, registry.clone(), "").into_context();
    let image = crate::context::test_append(&entries);
    let intent = spawn_image(image);
    let actual = Transcript::of([entry(1, Body::Spawned { intent })], registry, "").into_messages();
    assert_eq!(actual, expected);
}
fn entry(seq: u64, body: Body) -> Entry {
    Entry::new(Seq::new(seq).unwrap(), Epoch(0), body)
}
#[test]
fn a_prefix_projects_edits_and_publications_without_executable_ownership() {
    let entries = vec![
        entry(
            1,
            Body::Input {
                segments: "old".into(),
                mode: Mode::Queue,
                taken: true,
                origin: None,
                force: false,
            },
        ),
        entry(
            2,
            Body::Context {
                text: "skill".into(),
                publication: Some(protocol::publication::Publication {
                    namespace: "skills".into(),
                    key: Json::encode("review").unwrap(),
                }),
            },
        ),
        entry(
            3,
            Body::Edit {
                target: Seq::new(1).unwrap(),
                upto: None,
                replace: Some(Json::encode("effective").unwrap()),
            },
        ),
        entry(
            4,
            Body::ToolCall {
                invoke: None,
                tool: "task".into(),
                input: Json::null(),
                id: Some("pending".into()),
                card: None,
                by: protocol::Audience::Model,
            },
        ),
        entry(
            5,
            Body::Watch {
                child: SessionId::new(),
            },
        ),
        entry(
            6,
            Body::Edit {
                target: Seq::new(5).unwrap(),
                upto: None,
                replace: Some(Json::encode("ownership note").unwrap()),
            },
        ),
    ];
    let image = crate::context::test_append(&entries);
    assert!(
        crate::context::expand(&image, &crate::context::test_places())
            .unwrap()
            .iter()
            .flat_map(|ContextLayer(_, inputs)| inputs)
            .all(|input| !matches!(input.1, ContextValue::ToolCall(..)))
    );
    let registry = Arc::new(Registry::default());
    let expected = Transcript::of(&entries, registry.clone(), "").into_context();
    let intent = spawn_image(image);
    let child = [entry(1, Body::Spawned { intent })];
    let actual = Transcript::of(&child, registry, "").into_messages();
    assert_eq!(actual, expected);
    assert_eq!(
        actual,
        Messages::from_iter([
            Message::user("skill"),
            Message::user("effective"),
            Message::user("ownership note")
        ])
    );
    let ledger = protocol::ledger::Ledger::of(child.iter());
    assert!(ledger.open_calls().next().is_none());
    assert!(ledger.armed().next().is_none());
    assert!(
        ledger
            .publications()
            .contains("skills", &Json::encode("review").unwrap())
    );
}
#[test]
fn pending_exchanges_never_become_child_work() {
    let call = |id: &str| {
        llm::AssistantPart::ToolCall(llm::ToolCall {
            id: id.into(),
            name: "task".into(),
            input: Json::encode(&serde_json::json!({})).unwrap(),
            invalid: None,
            provider_executed: false,
            meta: llm::Meta::new(),
        })
    };
    let assistant = Assistant {
        content: vec![call("complete"), call("pending")],
        origin: None,
    };
    let reply = llm::Response {
        message: assistant,
        finish: llm::Finish::new(llm::Reason::ToolCalls, None),
        usage: llm::Usage::default(),
        id: None,
        model: None,
        meta: llm::Meta::new(),
    };
    let mut entries = vec![
        response_entry(1, &reply),
        tool_entry(2, "complete"),
        entry(
            3,
            Body::Assistant {
                text: "progress".into(),
            },
        ),
        entry(
            4,
            Body::ToolResult {
                call: CallId::new(2).unwrap(),
                outcome: Outcome::Done {
                    output: Json::encode("answer").unwrap().into(),
                },
                card: None,
                held: Vec::new(),
            },
        ),
        tool_entry(5, "pending"),
    ];
    entries.push(response_entry(6, &reply));
    entries.push(entry(
        7,
        Body::Input {
            segments: "pending input".into(),
            mode: Mode::Queue,
            taken: false,
            origin: None,
            force: false,
        },
    ));
    let image = crate::context::test_append(&entries);
    let intent = spawn_image(image);
    let child = [entry(
        1,
        Body::Spawned {
            intent: intent.clone(),
        },
    )];
    let ledger = protocol::ledger::Ledger::of(&child);
    assert!(ledger.open_calls().next().is_none());
    assert!(ledger.armed().next().is_none());
    let messages = Transcript::of(
        [entry(1, Body::Spawned { intent })],
        Arc::new(Registry::default()),
        "",
    )
    .into_messages();
    assert_eq!(messages.len(), 3);
    let Message::Assistant(assistant) = &messages[0] else {
        panic!("{messages:?}")
    };
    assert_eq!(
        assistant
            .tool_calls()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["complete"]
    );
    let Message::Tool(tool) = &messages[2] else {
        panic!("{messages:?}")
    };
    assert_eq!(
        tool.results
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        ["complete"]
    );
}

fn recursive_scope(
    image: &Vec<ContextLayer>,
    scope: usize,
    registry: &Arc<Registry>,
) -> Transcript {
    let mut transcript = Transcript::empty(registry.clone(), "", &Places::default());
    let Some(ContextLayer(runner, inputs)) = image.get(scope) else {
        return transcript;
    };
    transcript.model_only = *runner == RunnerKind::ModelOnly;
    for ContextInput(seq, value) in inputs {
        if let ContextValue::Inherited(child) = *value {
            let messages = recursive_scope(image, child, registry).into_context();
            transcript.inherit(&messages, *seq);
        } else {
            transcript.apply_input(*seq, value.clone(), None);
        }
    }
    transcript
}

fn recursive_projection(entries: &[Entry], registry: &Arc<Registry>) -> Transcript {
    let mut transcript = Transcript::empty(registry.clone(), "", &Places::default());
    let edits = context_edits(entries);
    for e in entries {
        match edits
            .iter()
            .rev()
            .find(|(from, to, _)| (*from..=*to).contains(&e.seq.get()))
        {
            Some((from, _, Some(text))) if *from == e.seq.get() => {
                transcript.messages.push(Message::user(text.clone()));
                transcript.last = e.seq.get();
            }
            Some(_) => {}
            None => {
                if let Body::Spawned { intent } = &e.body {
                    let image = protocol::publication::SpawnImage::from_json(intent).unwrap();
                    let messages = recursive_scope(
                        &crate::context::expand(&image.image, &crate::context::test_places())
                            .unwrap(),
                        0,
                        registry,
                    )
                    .into_context();
                    for message in messages.iter() {
                        transcript.messages.push(message.clone());
                    }
                    transcript.context = transcript.messages.len();
                    transcript.last = e.seq.get();
                } else {
                    transcript.apply(e, None);
                }
            }
        }
    }
    transcript
}

#[test]
fn normalized_context_equals_recursive_projection() {
    let registry = Arc::new(Registry::default());
    let mut entries = vec![entry(
        1,
        Body::Assistant {
            text: "ancestor".into(),
        },
    )];
    for depth in 0..12 {
        let intent = spawn_image(crate::context::test_append(&entries));
        entries = vec![
            entry(1, Body::Spawned { intent }),
            entry(
                2,
                Body::Watch {
                    child: SessionId::new(),
                },
            ),
            entry(
                3,
                Body::Assistant {
                    text: format!("layer {depth}"),
                },
            ),
            entry(
                4,
                Body::Edit {
                    target: Seq::new(2).unwrap(),
                    upto: Some(Seq::new(3).unwrap()),
                    replace: Some(Json::encode("replacement").unwrap()),
                },
            ),
            entry(
                5,
                Body::Context {
                    text: format!("context {depth}"),
                    publication: None,
                },
            ),
            entry(
                6,
                Body::Edit {
                    target: Seq::new(3).unwrap(),
                    upto: None,
                    replace: Some(Json::encode("overlap").unwrap()),
                },
            ),
        ];
        let expected = recursive_projection(&entries, &registry).into_context();
        assert_eq!(
            Transcript::of(&entries, registry.clone(), "").into_context(),
            expected
        );
        let image = crate::context::test_append(&entries);
        assert!(
            crate::context::expand(&image, &crate::context::test_places())
                .unwrap()
                .iter()
                .flat_map(|ContextLayer(_, inputs)| inputs)
                .all(|input| !matches!(&input.1, ContextValue::Assistant(text) if text.is_empty()))
        );
        let child = entry(
            1,
            Body::Spawned {
                intent: spawn_image(image),
            },
        );
        let recovered = Transcript::of([&child], registry.clone(), "").into_context();
        let mut live = Transcript::of(std::iter::empty::<Entry>(), registry.clone(), "");
        live.apply(&child, None);
        assert_eq!(recovered, expected);
        assert_eq!(live.into_context(), expected);
    }
}

#[test]
fn provider_state_and_publication_identity_survive_normalization() {
    let mut meta = llm::Meta::new();
    meta.insert(
        "messages",
        &serde_json::json!({"signature":"opaque signature", "encrypted":"state"}),
    );
    let assistant = Assistant {
        content: vec![llm::AssistantPart::Opaque(llm::Opaque { meta })],
        origin: Some(llm::Origin {
            protocol: llm::ProtocolKey::AnthropicMessages,
            provider: "provider".into(),
            model: "model".into(),
        }),
    };
    let response = llm::Response {
        message: assistant.clone(),
        finish: llm::Finish::new(llm::Reason::Stop, None),
        usage: llm::Usage::default(),
        id: None,
        model: None,
        meta: llm::Meta::new(),
    };
    let key = Json::encode(&serde_json::json!({"skill":"review", "revision":3})).unwrap();
    let publication = || {
        Some(protocol::publication::Publication {
            namespace: "skills".into(),
            key: key.clone(),
        })
    };
    let mut entries = vec![
        entry(
            1,
            Body::Context {
                text: "removed".into(),
                publication: publication(),
            },
        ),
        entry(
            2,
            Body::Context {
                text: "effective".into(),
                publication: publication(),
            },
        ),
        response_entry(3, &response),
        entry(
            4,
            Body::Edit {
                target: Seq::new(1).unwrap(),
                upto: None,
                replace: None,
            },
        ),
    ];
    for _ in 0..3 {
        entries = vec![entry(
            1,
            Body::Spawned {
                intent: spawn_image(crate::context::test_append(&entries)),
            },
        )];
        let messages = Transcript::of(&entries, Arc::new(Registry::default()), "").into_context();
        assert_eq!(
            messages,
            Messages::from_iter([
                Message::user("effective"),
                Message::Assistant(assistant.clone())
            ])
        );
        let ledger = protocol::ledger::Ledger::of(&entries);
        assert!(ledger.publications().contains("skills", &key));
    }
    entries.push(entry(
        2,
        Body::Edit {
            target: Seq::new(1).unwrap(),
            upto: None,
            replace: Some(Json::encode("summary").unwrap()),
        },
    ));
    let child = [entry(
        1,
        Body::Spawned {
            intent: spawn_image(crate::context::test_append(&entries)),
        },
    )];
    assert!(
        protocol::ledger::Ledger::of(&child)
            .publications()
            .is_empty()
    );
}

fn recorded_provider_cases() -> [(llm::Provider, &'static str); 4] {
    use llm::protocols::{chat, gemini, messages, responses};

    let url = "http://127.0.0.1:9";
    [
        (
            llm::Provider::new(
                "deepseek",
                url,
                llm::Auth::None,
                chat::Chat::new(chat::HOSTS[0].1),
            ),
            "deepseek/reasoning_matrix/text_streamed.yaml",
        ),
        (
            llm::Provider::new(
                "anthropic",
                url,
                llm::Auth::None,
                messages::Messages::new(messages::HOSTS[0].1),
            ),
            "anthropic/raw_stream_capture_matrix/terminal_raw_round_trips_for_thinking_stream.yaml",
        ),
        (
            llm::Provider::new(
                "openai",
                url,
                llm::Auth::None,
                responses::Responses::new(responses::HOSTS[0].1),
            ),
            "openai/gpt_5_6_reasoning/streaming_metadata.yaml",
        ),
        (
            llm::Provider::new(
                "gemini",
                url,
                llm::Auth::None,
                gemini::Gemini::new(gemini::HOSTS[0].1),
            ),
            "gemini/history_survival_matrix/streaming.yaml",
        ),
    ]
}

#[test]
fn incremental_graph_cuts_preserve_provider_bytes_across_segment_edits() {
    for (provider, cassette) in recorded_provider_cases() {
        let response = recorded_response(&provider, cassette);
        let entries = [
            entry(
                1,
                Body::Input {
                    segments: "original".into(),
                    mode: Mode::Queue,
                    taken: true,
                    origin: None,
                    force: false,
                },
            ),
            response_entry(2, &response),
            entry(
                3,
                Body::Assistant {
                    text: "removed in a later segment".into(),
                },
            ),
            response_entry(4, &response),
            edit_entry(5, 1, None, Some("edited across the result boundary")),
            edit_entry(6, 3, None, None),
            entry(
                7,
                Body::Context {
                    text: "published context".into(),
                    publication: None,
                },
            ),
            edit_entry(8, 1, Some(3), Some("latest overlapping edit")),
        ];
        for end in 1..=entries.len() {
            let registry = Arc::new(Registry::default());
            let direct = Transcript::of(&entries[..end], registry.clone(), "").into_context();
            for chunk in [1, 2, end] {
                let cut = crate::context::test_append_chunks(&entries[..end], chunk);
                let projected = Transcript::cut(
                    &cut,
                    Arc::clone(&registry),
                    "",
                    &crate::context::test_places(),
                    None,
                )
                .unwrap()
                .into_context();
                assert_eq!(projected, direct, "{cassette} prefix {end}");
                assert_eq!(
                    provider_bytes(&provider, projected),
                    provider_bytes(&provider, direct.clone()),
                    "{cassette} prefix {end}"
                );
            }
        }
    }
}

fn provider_bytes(provider: &llm::Provider, messages: Messages) -> Vec<u8> {
    let mut request = llm::Request {
        model: "recorded".into(),
        messages: messages.into(),
        ..llm::Request::default()
    };
    request.generation.max_output_tokens = std::num::NonZeroU32::new(64);
    provider
        .request("recorded", &request)
        .unwrap()
        .body()
        .to_vec()
}

fn recorded_response(provider: &llm::Provider, cassette: &str) -> llm::Response {
    struct Recorded(String);
    impl llm::http::Http for Recorded {
        fn send(
            &self,
            _: http::Request<llm::http::Payload>,
        ) -> llm::http::BoxFuture<'static, io::Result<llm::http::Response>> {
            let response = http::Response::builder()
                .status(200)
                .body(llm::http::body([self.0.clone().into_bytes()]))
                .unwrap();
            Box::pin(std::future::ready(Ok(response)))
        }
    }
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/rig/cassettes")
            .join(cassette),
    )
    .unwrap();
    let doc: serde_yaml_ng::Value = serde::Deserialize::deserialize(
        serde_yaml_ng::Deserializer::from_str(&text).next().unwrap(),
    )
    .unwrap();
    rt::block_on(llm::generate(
        &Recorded(doc["then"]["body"].as_str().unwrap().into()),
        provider,
        &llm::Request {
            model: "recorded".into(),
            generation: llm::Generation {
                max_output_tokens: std::num::NonZeroU32::new(64),
                ..llm::Generation::default()
            },
            messages: vec![Message::user("fixture")].into(),
            ..llm::Request::default()
        },
    ))
    .unwrap()
}

fn edit_entry(seq: u64, target: u64, upto: Option<u64>, replace: Option<&str>) -> Entry {
    entry(
        seq,
        Body::Edit {
            target: Seq::new(target).unwrap(),
            upto: upto.map(|seq| Seq::new(seq).unwrap()),
            replace: replace.map(|text| Json::encode(text).unwrap()),
        },
    )
}

fn response_entry(seq: u64, response: &llm::Response) -> Entry {
    entry(
        seq,
        Body::ModelResult {
            call: CallId::new(seq).unwrap(),
            outcome: Outcome::Done {
                output: Json::encode(response).unwrap(),
            },
            r#for: None,
            failure: None,
        },
    )
}

fn tool_entry(seq: u64, id: &str) -> Entry {
    entry(
        seq,
        Body::ToolCall {
            invoke: None,
            tool: "task".into(),
            input: Json::null(),
            id: Some(id.into()),
            card: None,
            by: protocol::Audience::Model,
        },
    )
}

#[test]
fn identical_local_sequences_and_provider_ids_remain_in_their_scopes() {
    let registry = Arc::new(Registry::default());
    let reply = llm::Response {
        message: Assistant {
            content: vec![llm::AssistantPart::ToolCall(llm::ToolCall {
                id: "same-id".into(),
                name: "task".into(),
                input: Json::null(),
                invalid: None,
                provider_executed: false,
                meta: llm::Meta::new(),
            })],
            origin: None,
        },
        finish: llm::Finish::new(llm::Reason::ToolCalls, None),
        usage: llm::Usage::default(),
        id: None,
        model: None,
        meta: llm::Meta::new(),
    };
    let exchange = |text: &str| {
        vec![
            response_entry(2, &reply),
            tool_entry(3, "same-id"),
            entry(
                4,
                Body::ToolResult {
                    call: CallId::new(3).unwrap(),
                    outcome: Outcome::Done {
                        output: Json::encode(text).unwrap().into(),
                    },
                    card: None,
                    held: Vec::new(),
                },
            ),
        ]
    };
    let mut entries = exchange("ancestor answer");
    let mut expected = Transcript::of(&entries, registry.clone(), "").into_context();
    for text in ["child answer", "grandchild answer"] {
        let mut child = vec![entry(
            1,
            Body::Spawned {
                intent: spawn_image(crate::context::test_append(&entries)),
            },
        )];
        child.extend(exchange(text));
        let own = Transcript::of(exchange(text), registry.clone(), "").into_context();
        expected = expected.iter().chain(own.iter()).cloned().collect();
        let actual = Transcript::of(&child, registry.clone(), "").into_context();
        assert_eq!(actual, expected);
        entries = child;
    }
    let image = crate::context::test_append(&entries);
    let image = crate::context::expand(&image, &crate::context::test_places()).unwrap();
    assert_eq!(image.len(), 3);
    assert_eq!(
        image
            .iter()
            .filter(|layer| layer.1.iter().any(|i| i.0.get() == 3))
            .count(),
        3
    );
}

#[test]
fn model_only_inputs_keep_their_interpretation_across_agent_scopes() {
    let turn = ext::Conversation::turn::<ext_title::Title>("test", "side input")
        .per_call()
        .key("turn")
        .instructions("side instructions")
        .request(CallId::new(1).unwrap());
    let mut intent =
        crate::driver::spawn::SpawnIntent::from_json(&crate::tests::spawn_intent(1, "")).unwrap();
    intent.request.runner = ext::Runner::ModelOnly {
        role: ext::ModelRole::Session,
        context: ext::ContextPolicy {
            exchanges: 8,
            bytes: 65536,
        },
    };
    let entries = vec![
        entry(
            1,
            Body::Spawned {
                intent: intent.into_json().unwrap(),
            },
        ),
        entry(
            2,
            Body::Input {
                segments: turn.intent(None).unwrap().raw().into(),
                mode: Mode::Queue,
                taken: false,
                origin: None,
                force: false,
            },
        ),
        entry(
            3,
            Body::Take {
                input: Seq::new(2).unwrap(),
                into: None,
            },
        ),
    ];
    let registry = Arc::new(Registry::default());
    let expected = turn.messages;
    assert_eq!(
        Transcript::of(&entries, registry.clone(), "").into_context(),
        expected
    );
    let mut image = crate::context::test_append(&entries);
    for _ in 0..3 {
        let child = entry(
            1,
            Body::Spawned {
                intent: spawn_image(image),
            },
        );
        let mut live = Transcript::empty(registry.clone(), "", &Places::default());
        live.apply(&child, None);
        assert_eq!(live.into_context(), expected);
        assert_eq!(
            Transcript::of([&child], registry.clone(), "").into_context(),
            expected
        );
        image = crate::context::test_append(std::slice::from_ref(&child));
    }
}

#[test]
fn batch_replay_preserves_inherited_layers_and_latest_overlapping_edits() {
    use crate::store::LogStore;

    let home = tempfile::tempdir().unwrap();
    let store = crate::store::Jsonl::open(home.path()).unwrap();
    let mut loaded = store
        .create(&crate::store::Header {
            id: SessionId::new(),
            workspace: home.path().to_path_buf(),
            parent: None,
        })
        .unwrap();
    let ancestor = crate::context::test_append(&[entry(
        1,
        Body::Assistant {
            text: "ancestor".into(),
        },
    )]);
    let parent = crate::context::test_append(&[
        entry(
            1,
            Body::Spawned {
                intent: spawn_image(ancestor),
            },
        ),
        entry(
            2,
            Body::Assistant {
                text: "parent".into(),
            },
        ),
    ]);
    let mut entries = vec![entry(
        1,
        Body::Spawned {
            intent: spawn_image(parent),
        },
    )];
    entries.extend((2..=4097).map(|seq| {
        entry(
            seq,
            Body::Assistant {
                text: "history".repeat(64),
            },
        )
    }));
    entries.push(entry(4098, Body::Interrupt { origin: None }));
    for (from, to, replacement) in [
        (100, 300, Some("earlier")),
        (200, 400, Some("later")),
        (300, 350, None),
        (4099, 4099, None),
    ] {
        entries.push(entry(
            entries.len() as u64 + 1,
            Body::Edit {
                target: Seq::new(from).unwrap(),
                upto: Some(Seq::new(to).unwrap()),
                replace: replacement.map(|text| Json::encode(text).unwrap()),
            },
        ));
    }
    loaded.writer.append_raw(&entries).unwrap();
    let through = entries.len() as u64;
    drop(entries);
    let frames = crate::outbox::FrameLog::new(loaded.lines, through);
    let mut passes = 0;
    let transcript = Transcript::replay(
        |each| {
            passes += 1;
            for batch in frames.batches_after(0) {
                for entry in batch? {
                    each(&entry);
                }
            }
            Ok(())
        },
        Arc::new(Registry::default()),
        "system",
        &Places::default(),
    )
    .unwrap();
    assert_eq!(passes, 2);
    assert_eq!(transcript.through(), through);
    let messages = transcript.into_messages();
    assert_eq!(messages.len(), 3800);
    let expected = [
        Message::system("system"),
        Message::assistant("ancestor"),
        Message::assistant("parent"),
    ];
    assert_eq!(
        &messages.iter().take(3).cloned().collect::<Vec<_>>(),
        &expected
    );
    assert_eq!(messages.get(101), Some(&Message::user("earlier")));
    assert_eq!(messages.get(102), Some(&Message::user("later")));
    assert_eq!(
        messages.last(),
        Some(&Message::assistant("history".repeat(64)))
    );
}

/// Runs through the hosted perf harness's long-log filter, away from shared dev hosts.
#[test]
#[ignore = "a measurement; run alone or on a hosted runner"]
fn bench_loading_a_long_log_transcript_reconstruction() {
    let binary = std::env::current_exe().unwrap();
    let output_dir = tempfile::tempdir().unwrap();
    let metrics = output_dir.path().join("reconstruction.json");
    let commands = ["staged_transcript_reconstruction", "streaming_transcript_reconstruction"]
        .map(|name| format!("{binary:?} --exact transcript::context_image_tests::{name} --ignored --nocapture --test-threads=1"));
    let output = std::process::Command::new("hyperfine")
        .args([
            "-N",
            "--runs",
            "5",
            "--warmup",
            "1",
            "--metrics",
            "all",
            "--show-output",
        ])
        .arg("--export-json")
        .arg(&metrics)
        .args(commands)
        .output()
        .unwrap();
    eprintln!("{}", String::from_utf8_lossy(&output.stdout));
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success());
    eprintln!("{}", std::fs::read_to_string(metrics).unwrap());
}

#[test]
#[ignore = "a measurement; run alone or on a hosted runner"]
fn staged_transcript_reconstruction() {
    measure_reconstruction(Reconstruction::Staged);
}

#[test]
#[ignore = "a measurement; run alone or on a hosted runner"]
fn streaming_transcript_reconstruction() {
    measure_reconstruction(Reconstruction::Streaming);
}

#[derive(Clone, Copy)]
enum Reconstruction {
    Staged,
    Streaming,
}

fn measure_reconstruction(pipeline: Reconstruction) {
    use crate::store::LogStore;
    use std::time::{Duration, Instant};

    let home = tempfile::tempdir().unwrap();
    let store = crate::store::Jsonl::open(home.path()).unwrap();
    let mut loaded = store
        .create(&crate::store::Header {
            id: SessionId::new(),
            workspace: home.path().to_path_buf(),
            parent: None,
        })
        .unwrap();
    for start in (1..=16384).step_by(256) {
        let entries: Vec<_> = (start..start + 256)
            .map(|seq| {
                entry(
                    seq,
                    Body::Assistant {
                        text: "history ".repeat(512),
                    },
                )
            })
            .collect();
        loaded.writer.append_raw(&entries).unwrap();
    }
    loaded
        .writer
        .append_raw(&[entry(
            16385,
            Body::Edit {
                target: Seq::new(4097).unwrap(),
                upto: Some(Seq::new(8192).unwrap()),
                replace: Some(Json::encode("summary").unwrap()),
            },
        )])
        .unwrap();
    let frames = crate::outbox::FrameLog::new(loaded.lines, 16385);
    let registry = Arc::new(Registry::default());
    let staged = matches!(pipeline, Reconstruction::Staged);
    sys::release();
    let before = resident_kib();
    let mut sampled_peak = before;
    let mut elapsed = Duration::ZERO;
    let transcript = if staged {
        let start = Instant::now();
        let entries = frames.entries().unwrap();
        elapsed += start.elapsed();
        sampled_peak = sampled_peak.max(resident_kib());
        let start = Instant::now();
        let image = crate::context::test_append(&entries);
        elapsed += start.elapsed();
        sampled_peak = sampled_peak.max(resident_kib());
        let start = Instant::now();
        let transcript = project(
            crate::context::expand(&image, &crate::context::test_places()).unwrap(),
            Arc::clone(&registry),
            "",
            &Places::default(),
        );
        elapsed += start.elapsed();
        sampled_peak = sampled_peak.max(resident_kib());
        transcript
    } else {
        let start = Instant::now();
        let transcript = Transcript::replay(
            |each| {
                for batch in frames.batches_after(0) {
                    for entry in batch? {
                        each(&entry);
                    }
                }
                Ok(())
            },
            Arc::clone(&registry),
            "",
            &Places::default(),
        )
        .unwrap();
        elapsed += start.elapsed();
        sampled_peak = sampled_peak.max(resident_kib());
        transcript
    };
    sys::release();
    let retained = resident_kib().saturating_sub(before);
    let messages = transcript.into_messages();
    assert_eq!(messages.len(), 12289);
    assert_eq!(messages.get(4096), Some(&Message::user("summary")));
    eprintln!(
        "transcript staged={staged}: 16385 entries, elapsed={elapsed:?}, sampled_peak_delta_kib={}, retained_result_delta_kib={retained}",
        sampled_peak.saturating_sub(before)
    );
}

fn resident_kib() -> u64 {
    let output = std::process::Command::new(std::env::var("A_PERF_PYTHON").unwrap())
        .args([
            std::env::var("A_PERF_PROCESS").unwrap(),
            std::process::id().to_string(),
            "rss".into(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse::<u64>()
        .unwrap()
}

#[test]
fn forked_provider_bytes_need_no_projection_of_their_settled_ancestry() {
    for (provider, cassette) in recorded_provider_cases() {
        let response = recorded_response(&provider, cassette);
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(blob_files::Files::open(dir.path().join("blobs"), dir.path()).unwrap());
        let places = Places::new(
            store.clone(),
            Arc::new(blob_files::CacheDir::new(
                dir.path().join("tree"),
                store.clone(),
            )),
        );
        let registry = Arc::new(Registry::default());
        let source = [taken_text(1, "settled input"), response_entry(2, &response)];
        let cut = crate::context::append(
            protocol::publication::ContextImage::default(),
            source.clone(),
            &*store,
        )
        .unwrap();
        let local = taken_text(3, "owner private tail");
        let image = crate::context::append(cut, [local.clone()], &*store).unwrap();
        let child = [
            entry(
                1,
                Body::Spawned {
                    intent: spawn_image(image.clone()),
                },
            ),
            taken_text(2, "child private input"),
        ];
        let expected = provider_bytes(
            &provider,
            Transcript::placed(&child, Arc::clone(&registry), "system", &places)
                .unwrap()
                .into_messages(),
        );
        let canonical = Transcript::placed(&source, Arc::clone(&registry), "system", &places)
            .unwrap()
            .into_context();
        let request = llm::Request {
            model: "recorded".into(),
            messages: llm::Conversation::retained(llm::Resident::default(), canonical, || {
                Err(llm::Error::new(llm::Kind::Other, "unrequested ancestor"))
            }),
            generation: llm::Generation {
                max_output_tokens: std::num::NonZeroU32::new(64),
                ..llm::Generation::default()
            },
            ..llm::Request::default()
        };
        provider.request(&request.model, &request).unwrap();
        let prefix = request
            .messages
            .prefix()
            .expect("recorded completed prefix");
        let tail = Transcript::placed(&[local], Arc::clone(&registry), "", &places)
            .unwrap()
            .into_context();
        let root = image.root.as_ref().unwrap().id;
        std::fs::remove_dir_all(dir.path().join("blobs")).unwrap();
        assert!(
            crate::context::expand(&image, &places).is_err(),
            "the ordinary path must need the removed nodes"
        );
        let transcript = Transcript::replay_seeded(
            &child,
            registry,
            "system",
            &places,
            crate::driver::prefix::Seed { root, prefix, tail },
        )
        .unwrap();
        let mut request = request;
        request.messages = transcript
            .request_before(3, || {
                Err(llm::Error::new(llm::Kind::Other, "unrequested ancestor"))
            })
            .unwrap();
        assert_eq!(
            provider
                .request(&request.model, &request)
                .unwrap()
                .body()
                .to_vec(),
            expected,
            "{cassette}"
        );
    }
}

fn taken_text(seq: u64, text: &str) -> Entry {
    entry(
        seq,
        Body::Input {
            segments: text.into(),
            mode: Mode::Queue,
            taken: true,
            origin: None,
            force: false,
        },
    )
}
