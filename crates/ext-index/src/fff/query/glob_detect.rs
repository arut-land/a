//! Glob wildcard detection: the characters globset treats as wildcards.

#[inline]
pub fn has_wildcards(s: &str) -> bool {
    s.bytes().any(|b| matches!(b, b'*' | b'?' | b'[' | b'{'))
}
