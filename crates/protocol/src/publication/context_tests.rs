use super::*;

#[test]
fn malformed_scope_graphs_and_positions_are_rejected_before_projection() {
    let input = |scope, seq, value| ContextInput {
        scope,
        seq: Seq::new(seq).unwrap(),
        value,
    };
    for (scopes, inputs) in [
        (vec![], vec![input(0, 1, ContextValue::User("text".into()))]),
        (
            vec![RunnerKind::Agent],
            vec![input(0, 1, ContextValue::Inherited(0))],
        ),
        (
            vec![RunnerKind::Agent],
            vec![input(0, 1, ContextValue::Inherited(1))],
        ),
        (vec![RunnerKind::Agent; 2], vec![]),
        (
            vec![RunnerKind::Agent; 2],
            vec![
                input(0, 1, ContextValue::Inherited(1)),
                input(0, 2, ContextValue::Inherited(1)),
            ],
        ),
        (
            vec![RunnerKind::Agent],
            vec![
                input(0, 2, ContextValue::User("later".into())),
                input(0, 1, ContextValue::User("earlier".into())),
            ],
        ),
    ] {
        let image = SpawnImage {
            runner: RunnerKind::Agent,
            authority: Json::null(),
            image: ContextImage { scopes, inputs },
        };
        assert!(SpawnImage::from_json(&image.to_json().unwrap()).is_err());
    }
}
