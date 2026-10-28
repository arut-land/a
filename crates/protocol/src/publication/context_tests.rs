use super::*;

#[test]
fn malformed_cut_edges_and_positions_are_rejected_before_projection() {
    for inputs in [
        vec![ContextInput(
            Seq::new(1).unwrap(),
            ContextValue::Inherited(0),
        )],
        vec![
            ContextInput(Seq::new(2).unwrap(), ContextValue::Position),
            ContextInput(Seq::new(1).unwrap(), ContextValue::Position),
        ],
    ] {
        let node = ContextNode {
            previous: None,
            runner: RunnerKind::Agent,
            inputs,
            inherited: Vec::new(),
            edits: Vec::new(),
        };
        assert!(ContextNode::from_json(node.to_json().unwrap().raw().as_bytes()).is_err());
    }
}
