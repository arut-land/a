use super::*;

#[test]
fn malformed_scope_graphs_and_positions_are_rejected_before_projection() {
    let input = |seq, value| ContextInput {
        seq: Seq::new(seq).unwrap(),
        value,
    };
    for (scopes, inputs) in [
        (
            vec![RunnerKind::Agent],
            vec![input(1, ContextValue::Inherited(0))],
        ),
        (
            vec![RunnerKind::Agent],
            vec![input(1, ContextValue::Inherited(1))],
        ),
        (vec![RunnerKind::Agent; 2], vec![]),
        (
            vec![RunnerKind::Agent; 2],
            vec![
                input(1, ContextValue::Inherited(1)),
                input(2, ContextValue::Inherited(1)),
            ],
        ),
        (
            vec![RunnerKind::Agent],
            vec![
                input(2, ContextValue::User("later".into())),
                input(1, ContextValue::User("earlier".into())),
            ],
        ),
    ] {
        let image = SpawnImage {
            runner: RunnerKind::Agent,
            authority: Json::null(),
            image: scopes
                .into_iter()
                .enumerate()
                .map(|(scope, runner)| ContextLayer {
                    runner,
                    inputs: if scope == 0 {
                        inputs.clone()
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
        };
        assert!(SpawnImage::from_json(&image.to_json().unwrap()).is_err());
    }
}
