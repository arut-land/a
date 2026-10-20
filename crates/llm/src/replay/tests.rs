use super::*;

/// Fingerprints are stored in session logs: they never change.
#[test]
fn fingerprints_are_stable() {
    let input = serde_json::json!({"b": [true, null], "a": 1});
    let input = &Json::encode(&input).unwrap();
    assert_eq!(fingerprint(Canon::Text("hi")), "cad93eee6ddd9cfe");
    assert_eq!(fingerprint(Canon::Reasoning("x", true)), "fddb7adfc2dd3d73");
    assert_eq!(fingerprint(Canon::Call("f", input)), "0f90db0a3e870785");
    assert_eq!(fingerprint(Canon::Other), "");
}
