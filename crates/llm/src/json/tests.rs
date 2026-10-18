use super::*;

#[test]
fn text_that_states_no_object_has_none() {
    for text in ["not json", "[1, 2", "\"str", "12"] {
        assert_eq!(parse_partial_object(text), None, "{text}");
    }
}
