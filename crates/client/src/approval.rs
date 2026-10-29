use protocol::{
    CallId, Json,
    view::{Buttons, Presented},
};

/// The review sequence is data: renderers lay out these parts in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalPart {
    Call { text: String },
    Reason { text: String },
    UnverifiedSummary { text: String },
    Controls { call: CallId, title: String },
}

impl ApprovalPart {
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Call { text } | Self::Reason { text } | Self::UnverifiedSummary { text } => {
                Some(text)
            }
            Self::Controls { .. } => None,
        }
    }
}

/// A model summary can only follow complete arguments in a pending review.
pub fn approval_parts(p: &Presented<'_>) -> Vec<ApprovalPart> {
    if p.question.is_some() {
        return Vec::new();
    }
    let Buttons::Approve(call) = p.buttons else {
        return Vec::new();
    };
    let Some((tool, input)) = p.input else {
        return Vec::new();
    };
    let mut parts = vec![ApprovalPart::Call {
        text: review_text(&call_input(tool, input)),
    }];
    if let Some(reason) = p.reason {
        parts.push(ApprovalPart::Reason {
            text: review_text(&reason_text(reason)),
        });
    }
    if let Some(description) = p.description {
        parts.push(ApprovalPart::UnverifiedSummary {
            text: review_text(&format!("Model summary (unverified): {description}")),
        });
    }
    parts.push(ApprovalPart::Controls {
        call,
        title: review_text(&p.title),
    });
    parts
}

/// Text-only review sections, in the same order as interactive renderers.
pub fn approval_text(p: &Presented<'_>) -> String {
    approval_parts(p)
        .iter()
        .filter_map(ApprovalPart::text)
        .collect::<Vec<_>>()
        .join("\n")
}

// Optional model text cannot erase or overwrite the call in a terminal.
fn review_text(text: &str) -> String {
    let mut visible = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_control() && ch != '\n' {
            visible.extend(ch.escape_default());
        } else {
            visible.push(ch);
        }
    }
    visible
}

fn reason_text(reason: &protocol::ApprovalReason) -> String {
    use protocol::ApprovalReason;
    let why = match reason {
        ApprovalReason::Mode { mode } => format!("the {mode} mode requires approval for this tool"),
        ApprovalReason::OperatorRule { tool, input } => match input {
            Some(input) => format!(
                "an operator rule requires approval for {tool} when input contains {input:?}"
            ),
            None => format!("an operator rule requires approval for {tool}"),
        },
        ApprovalReason::OutsideWorkspace { path, workspace } => {
            format!("{path} is outside the workspace {workspace}")
        }
        ApprovalReason::Classifier { reason } => {
            format!("the classifier requested approval: {reason}")
        }
    };
    format!("Why: {why}")
}

/// Complete arguments for review, including literal multiline strings.
/// JSON beside the readable values preserves types and every argument.
fn call_input(tool: &str, input: &Json) -> String {
    use serde_json::Value;
    use std::fmt::Write as _;
    fn strings(value: &Value, path: &str, text: &mut String) {
        match value {
            Value::String(value) => {
                let _ = writeln!(text, "{path}:\n{value}");
            }
            Value::Object(fields) => {
                for (key, value) in fields {
                    strings(value, &format!("{path}[{key:?}]"), text);
                }
            }
            Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    strings(value, &format!("{path}[{index}]"), text);
                }
            }
            _ => {}
        }
    }
    let mut text = format!("Call: {tool}\n");
    let Ok(value) = input.parse::<Value>() else {
        text.push_str(input.raw());
        return text;
    };
    if tool == "edit"
        && let (Some(path), Some(edits)) = (
            value.get("path").and_then(Value::as_str),
            value.get("edits").and_then(Value::as_array),
        )
    {
        let _ = writeln!(text, "--- {path}\n+++ {path}");
        for (index, edit) in edits.iter().enumerate() {
            if let (Some(old), Some(new)) = (
                edit.get("old").and_then(Value::as_str),
                edit.get("new").and_then(Value::as_str),
            ) {
                let _ = writeln!(text, "@@ replacement {} @@", index + 1);
                for (prefix, value) in [("-", old), ("+", new)] {
                    for line in value.split('\n') {
                        let _ = writeln!(text, "{prefix}{line}");
                    }
                }
            }
        }
    }
    strings(&value, "input", &mut text);
    let _ = write!(text, "Arguments (JSON):\n{}", input.raw());
    text
}

#[cfg(test)]
mod approval_tests {
    use super::approval_text;
    use protocol::DaemonMsg;
    use protocol::{
        Decode,
        view::{Buttons, SessionView},
    };

    #[test]
    fn approval_explanations_replay_identically_and_do_not_wait_for_descriptions() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../protocol/tests/vectors/view.json")).unwrap();
        let vector = vectors
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "approval reason precedes its optional description")
            .unwrap();
        let mut view = SessionView::default();
        let mut saw_question = false;
        for value in vector["messages"].as_array().unwrap() {
            let msg = DaemonMsg::from_json(value.to_string().as_bytes()).unwrap();
            view.fold(&msg);
            if let Some(item) = view.items.first() {
                let p = view.present(item);
                if p.reason.is_some() && p.description.is_none() {
                    assert!(matches!(p.buttons, Buttons::Approve(_)));
                    let text = approval_text(&p);
                    assert!(text.starts_with("Call: bash\n"));
                    assert!(text.ends_with("Why: the ask mode requires approval for this tool"));
                    saw_question = true;
                }
            }
        }
        assert!(saw_question);
        let explanation = approval_text(&view.present(&view.items[0]));
        assert!(explanation.ends_with("Why: the ask mode requires approval for this tool\nModel summary (unverified): Lists files in the current directory."));
        let snapshot = DaemonMsg::Snapshot(Box::new(view.snapshot()));
        let encoded = serde_json::to_vec(&snapshot).unwrap();
        let mut replay = SessionView::default();
        replay.fold(&DaemonMsg::from_json(&encoded).unwrap());
        assert_eq!(
            approval_text(&replay.present(&replay.items[0])),
            explanation
        );
        assert_eq!(view.state_hash(), replay.state_hash());
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use protocol::{DaemonMsg, Decode, view::SessionView};

    #[test]
    fn summaries_and_controls_follow_every_effective_argument() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../protocol/tests/vectors/view.json")).unwrap();
        for vector in vectors
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["name"].as_str().unwrap().starts_with("approval reviews "))
        {
            let mut view = SessionView::default();
            for message in vector["messages"].as_array().unwrap() {
                view.fold(&DaemonMsg::from_json(message.to_string().as_bytes()).unwrap());
            }
            let parts = approval_parts(&view.present(&view.items[0]));
            let [
                ApprovalPart::Call { text },
                ApprovalPart::Reason { .. },
                ApprovalPart::UnverifiedSummary { text: summary },
                ApprovalPart::Controls { .. },
            ] = parts.as_slice()
            else {
                panic!("incorrect review order: {parts:?}");
            };
            assert!(!text.contains("original harmless input"));
            assert!(summary.starts_with("Model summary (unverified):"));
            if vector["name"].as_str().unwrap().contains("multiline") {
                assert!(text.contains("printf harmless\nrm -rf /tmp/approval-test-sentinel"));
                assert!(text.contains("60000") && text.contains("every argument"));
            } else {
                for value in [
                    "--- /work/deep/config.ini",
                    "-allow = false",
                    "+allow = true",
                    "-delete = no",
                    "+delete = yes",
                    "\"flag\":true",
                ] {
                    assert!(text.contains(value), "{value}: {text}");
                }
            }
        }
    }
    #[test]
    fn review_text_cannot_erase_the_call_in_a_terminal() {
        let input = Json::encode(&serde_json::json!({"command":"first\nlast"})).unwrap();
        let item = protocol::view::Item::Tool {
            invoke: None,
            call: CallId::new(1).unwrap(),
            tool: "bash".into(),
            input,
            phase: protocol::ledger::Phase::Waiting(protocol::WaitOn::Approval),
            outcome: None,
            reason: None,
            card: None,
            by: protocol::Audience::Model,
        };
        let mut shown = item.present();
        shown.title = "\x1b[3Joverwritten".into();
        shown.description = Some("\x1b[3Jharmless\rconcealed");
        let parts = approval_parts(&shown);
        assert!(matches!(&parts[0], ApprovalPart::Call { text } if text.contains("first\nlast")));
        for part in parts {
            let text = match part {
                ApprovalPart::Controls { title, .. } => title,
                part => part.text().unwrap().to_owned(),
            };
            assert!(!text.chars().any(|ch| ch.is_control() && ch != '\n'));
        }
    }
}
