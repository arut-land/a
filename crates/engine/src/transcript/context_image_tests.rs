use super::*;
use protocol::{Epoch, Json, Mode, SessionId};
fn spawn_image(image: ContextImage) -> Json {
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
    let image = context_image(&entries);
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
    let image = context_image(&entries);
    assert!(
        image
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
                    output: Json::encode("answer").unwrap(),
                },
                card: None,
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
    let image = context_image(&entries);
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

fn recursive_scope(image: &ContextImage, scope: usize, registry: &Arc<Registry>) -> Transcript {
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
                    let messages = recursive_scope(&image.image, 0, registry).into_context();
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
        let intent = spawn_image(context_image(&entries));
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
        let image = context_image(&entries);
        assert!(
            image
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
                intent: spawn_image(context_image(&entries)),
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
            intent: spawn_image(context_image(&entries)),
        },
    )];
    assert!(
        protocol::ledger::Ledger::of(&child)
            .publications()
            .is_empty()
    );
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
                        output: Json::encode(text).unwrap(),
                    },
                    card: None,
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
                intent: spawn_image(context_image(&entries)),
            },
        )];
        child.extend(exchange(text));
        let own = Transcript::of(exchange(text), registry.clone(), "").into_context();
        expected = expected.iter().chain(own.iter()).cloned().collect();
        let actual = Transcript::of(&child, registry.clone(), "").into_context();
        assert_eq!(actual, expected);
        entries = child;
    }
    let image = context_image(&entries);
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
    let mut image = context_image(&entries);
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
        image = context_image(std::slice::from_ref(&child));
    }
}
