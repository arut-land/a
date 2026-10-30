//! Draft values remain local until the user submits one complete answer.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use protocol::{FieldKind, Json, Prompt, Question, QuestionAnswer, Response, Seq};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuestionDraft {
    pub values: BTreeMap<String, String>,
}

impl QuestionDraft {
    pub fn new(question: &Question) -> Self {
        let mut values = BTreeMap::new();
        for (p, prompt) in question.prompts.values().enumerate() {
            if question.prompts.len() > 1 {
                values.insert(format!("p{p}"), "accept".into());
            }
            if let Prompt::Form { fields, .. } = prompt {
                for (f, field) in fields.iter().enumerate() {
                    if let Some(default) = field.default_text() {
                        values.insert(format!("f{p}_{f}"), default);
                    } else if field.required
                        && !matches!(
                            field.kind,
                            FieldKind::Choice {
                                selection: protocol::Selection::One,
                                ..
                            }
                        )
                    {
                        values.insert(format!("f{p}_{f}"), String::new());
                    }
                }
            }
        }
        Self { values }
    }

    pub fn answer(
        &self,
        id: Seq,
        question: &Question,
        action: &str,
    ) -> Result<QuestionAnswer, String> {
        let mut responses = BTreeMap::new();
        for (p, (key, prompt)) in question.prompts.iter().enumerate() {
            let action = if action == "accept" {
                self.values
                    .get(&format!("p{p}"))
                    .map_or(action, String::as_str)
            } else {
                action
            };
            let response = match action {
                "accept" => {
                    let mut values = BTreeMap::new();
                    if let Prompt::Form { fields, .. } = prompt {
                        for (f, field) in fields.iter().enumerate() {
                            let Some(value) = self.values.get(&format!("f{p}_{f}")) else {
                                continue;
                            };
                            let value = match &field.kind {
                                FieldKind::String { .. }
                                | FieldKind::Choice {
                                    selection: protocol::Selection::One,
                                    ..
                                } => Json::encode(value).map_err(|e| e.to_string())?,
                                _ => {
                                    let value: serde_json::Value = serde_json::from_str(value)
                                        .map_err(|e| format!("{}: {e}", field.name))?;
                                    Json::encode(&value).map_err(|e| e.to_string())?
                                }
                            };
                            values.insert(field.name.clone(), value);
                        }
                    }
                    if matches!(prompt, Prompt::Form { .. }) {
                        Response::Submit(values)
                    } else if matches!(prompt, Prompt::Continue { .. }) {
                        Response::Continue(protocol::ContinueAnswer::Proceed)
                    } else {
                        Response::Accept(values)
                    }
                }
                "decline" if matches!(prompt, Prompt::Continue { .. }) => {
                    Response::Continue(protocol::ContinueAnswer::Decline)
                }
                "decline" => Response::Decline,
                "cancel" => prompt.cancel(),
                _ => return Err("unknown question action".into()),
            };
            responses.insert(key.clone(), response);
        }
        let answer = QuestionAnswer {
            question: id,
            responses,
        };
        question.validate(&answer)?;
        Ok(answer)
    }
}

pub fn question_text(question: &Question) -> String {
    let mut text = format!("Requested by {}\n", question.source);
    for prompt in question.prompts.values() {
        text.push_str(prompt.message());
        text.push('\n');
        match prompt {
            Prompt::Form { fields, .. } => {
                for field in fields {
                    if field.sensitive {
                        let _ = writeln!(
                            text,
                            "Warning: {} is requesting sensitive information. Secrets belong in a secure browser flow; you can decline.",
                            question.source
                        );
                    }
                    text.push_str(field.title.as_deref().unwrap_or(&field.name));
                    text.push_str(if field.required {
                        " (required)"
                    } else {
                        " (optional)"
                    });
                    if let Some(description) = &field.description {
                        text.push_str(": ");
                        text.push_str(description);
                    }
                    if let FieldKind::Choice { options, .. } = &field.kind
                        && !field.sensitive
                    {
                        let _ = write!(
                            text,
                            " {:?}",
                            options
                                .iter()
                                .map(|o| (&o.value, &o.title))
                                .collect::<Vec<_>>()
                        );
                    }
                    text.push('\n');
                }
            }
            Prompt::Url {
                url, destination, ..
            } => {
                let _ = writeln!(text, "Destination: {}", destination.origin);
                if let Some(raw) = &destination.raw_origin {
                    let _ = writeln!(text, "Raw origin: {raw}");
                }
                if destination.suspicious {
                    text.push_str("Warning: internationalized or confusable host. Inspect both spellings before opening.\n");
                }
                text.push_str(url);
                text.push('\n');
            }
            _ => {}
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_prompt_in_a_batch_can_have_its_own_response() {
        let question = Question {
            source: "fixture".into(),
            prompts: ["a", "b"]
                .into_iter()
                .map(|key| {
                    (
                        key.into(),
                        Prompt::Confirm {
                            message: key.into(),
                        },
                    )
                })
                .collect(),
        };
        let mut draft = QuestionDraft::new(&question);
        draft.values.insert("p1".into(), "decline".into());
        let id = Seq::new(1).unwrap();
        let answer = draft.answer(id, &question, "accept").unwrap();
        assert_eq!(answer.responses["a"], Response::accept());
        assert_eq!(answer.responses["b"], Response::Decline);
        assert!(
            draft
                .answer(id, &question, "cancel")
                .unwrap()
                .responses
                .values()
                .all(|r| *r == Response::Cancel)
        );
    }

    #[test]
    fn an_empty_choice_is_a_value_only_after_the_user_includes_it() {
        let question = Question {
            source: "fixture".into(),
            prompts: BTreeMap::from([(
                "form".into(),
                Prompt::Form {
                    message: "Choose".into(),
                    fields: vec![protocol::Field {
                        name: "choice".into(),
                        title: None,
                        description: None,
                        required: true,
                        sensitive: false,
                        default: None,
                        kind: FieldKind::Choice {
                            options: vec![protocol::Choice {
                                value: String::new(),
                                title: "Empty".into(),
                            }],
                            selection: protocol::Selection::One,
                        },
                    }],
                },
            )]),
        };
        let mut draft = QuestionDraft::new(&question);
        let id = Seq::new(1).unwrap();
        assert!(draft.answer(id, &question, "accept").is_err());
        draft.values.insert("f0_0".into(), String::new());
        let answer = draft.answer(id, &question, "accept").unwrap();
        let Response::Submit(fields) = &answer.responses["form"] else {
            panic!("form answer");
        };
        assert_eq!(fields["choice"].parse::<String>().unwrap(), "");
    }
}
