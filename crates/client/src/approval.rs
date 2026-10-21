/// The one wording rule for approval explanations, including the pipe
/// diagnostic and native bindings. Descriptions remain labelled model text.
pub fn approval_text(p: &protocol::view::Presented<'_>) -> String {
    use protocol::ApprovalReason;
    let Some(reason) = p.reason else {
        return String::new();
    };
    let why = match reason {
        ApprovalReason::Mode { mode } => format!("the {mode} mode requires approval for this tool"),
        ApprovalReason::OperatorRule { tool, input } => match input {
            Some(input) => format!(
                "an operator rule requires approval for {tool} when input contains {input:?}"
            ),
            None => format!("an operator rule requires approval for {tool}"),
        },
        ApprovalReason::OutsideWorkspace { path, workspace } => format!(
            "{} is outside the workspace {}",
            path.display(),
            workspace.display()
        ),
        ApprovalReason::Classifier { reason } => {
            format!("the classifier requested approval: {reason}")
        }
    };
    match p.description {
        Some(description) => format!("Why: {why}\nWhat (model): {description}"),
        None => format!("Why: {why}"),
    }
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
                    assert_eq!(
                        approval_text(&p),
                        "Why: the ask mode requires approval for this tool"
                    );
                    saw_question = true;
                }
            }
        }
        assert!(saw_question);
        let explanation = approval_text(&view.present(&view.items[0]));
        assert_eq!(
            explanation,
            "Why: the ask mode requires approval for this tool\nWhat (model): Lists files in the current directory."
        );
        let snapshot = DaemonMsg::Snapshot(Box::new(view.clone()));
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
