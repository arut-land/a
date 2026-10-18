//! One module per wire protocol: its encoder, its decoder, and the hosts
//! that speak it with their quirks.

pub mod chat;
pub mod gemini;
pub mod messages;
pub(crate) mod openai;
pub mod responses;
pub(crate) mod schema;

/// The reply state that keeps a provider's own usage object, under `key`.
pub(crate) fn usage_meta(key: &str, usage: Option<&serde_json::Value>) -> crate::Meta {
    usage
        .filter(|usage| usage.is_object())
        .map(|usage| crate::Meta::from([(key.to_owned(), serde_json::json!({ "usage": usage }))]))
        .unwrap_or_default()
}

/// The sum of the counts in an object of counters (`{"web_search": 2}`),
/// or of objects holding them under `key` (`{"web_search": {"num_requests":
/// 2}}`): `None` when it holds none.
pub(crate) fn counters(object: Option<&serde_json::Value>, key: Option<&str>) -> Option<u64> {
    use crate::json::Lenient;
    let counts: Vec<u64> = object?
        .as_object()?
        .values()
        .filter_map(|value| match key {
            Some(key) => value.get(key)?.as_u64_lenient(),
            None => value.as_u64_lenient(),
        })
        .collect();
    (!counts.is_empty()).then(|| counts.iter().sum())
}
