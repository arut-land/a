use super::*;

#[test]
fn statuses_classify_and_read_envelopes() {
    let mut headers = http::HeaderMap::new();
    headers.insert("retry-after-ms", "1500".parse().unwrap());
    let error = Error::from_status(429, &headers, br#"{"error":{"message":"slow down"}}"#);
    assert_eq!(error.kind, Kind::RateLimit);
    assert_eq!(error.message, "slow down");
    assert_eq!(error.retry_after, Some(Duration::from_millis(1500)));
    assert!(error.is_retryable());

    let error = Error::from_status(
        400,
        &http::HeaderMap::new(),
        br#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#,
    );
    assert_eq!(error.kind, Kind::ContextOverflow);
    assert!(!error.is_retryable());

    let quota = Error::from_status(
        429,
        &http::HeaderMap::new(),
        br#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}}"#,
    );
    assert_eq!(quota.kind, Kind::Quota);
}

#[test]
fn should_retry_header_overrides() {
    let mut headers = http::HeaderMap::new();
    headers.insert("x-should-retry", "true".parse().unwrap());
    assert!(Error::from_status(400, &headers, b"bad").is_retryable());
    headers.insert("x-should-retry", "false".parse().unwrap());
    assert!(!Error::from_status(503, &headers, b"busy").is_retryable());
}

#[test]
fn limit_errors_never_retry() {
    let error = Error::limit("reply bytes");
    assert_eq!(error.kind, Kind::Decode);
    assert!(!error.is_retryable());
    let encoded = serde_json::to_string(&error).expect("error encodes");
    assert!(
        !Error::from_json(&encoded)
            .expect("error decodes")
            .is_retryable()
    );
}
