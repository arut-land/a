//! Validated identities cross config, model-input and discovery boundaries
//! once; transport caches and dispatch cannot accept an unchecked string.
use std::{fmt, ops::Deref, str::FromStr};

macro_rules! name {
    ($name:ident, $valid:expr, $schema:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        impl TryFrom<String> for $name {
            type Error = String;
            fn try_from(text: String) -> Result<Self, Self::Error> {
                if ($valid)(&text) { Ok(Self(text)) }
                else { Err(format!("invalid {}: expected {}", stringify!($name), $schema)) }
            }
        }
        impl FromStr for $name {
            type Err = String;
            fn from_str(text: &str) -> Result<Self, Self::Err> { Self::try_from(text.to_owned()) }
        }
        impl Deref for $name {
            type Target = str;
            fn deref(&self) -> &str { &self.0 }
        }
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str { &self.0 }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
        }
        impl ext::Schema for $name {
            fn schema(out: &mut String) { out.push_str($schema); }
        }
    };
}

name!(
    ServerName,
    |s: &str| !s.is_empty()
        && s.len() <= 64
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
    r#"{"type":"string","minLength":1,"maxLength":64,"pattern":"^[A-Za-z0-9-]+$"}"#
);
// MCP only recommends header-safe names. Keep printable Unicode identities
// intact; the HTTP binding handles their mirrored encoding.
name!(
    ToolName,
    |s: &str| !s.is_empty()
        && s.len() <= 128
        && s.chars().all(|c| !c.is_control() && !c.is_whitespace()),
    r#"{"type":"string","minLength":1,"maxLength":128}"#
);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_are_validated_at_each_untrusted_boundary() {
        for name in ["", "a/b", "a b", &"x".repeat(65)] {
            assert!(name.parse::<ServerName>().is_err());
            assert!(serde_json::from_value::<ServerName>(serde_json::json!(name)).is_err());
        }
        for name in ["", "a\nb", "a b", &"x".repeat(129)] {
            assert!(name.parse::<ToolName>().is_err());
        }
        let tool: ToolName = "工具.echo".parse().unwrap();
        assert_eq!(serde_json::to_value(&tool).unwrap(), "工具.echo");
        let server: ServerName = "docs-1".parse().unwrap();
        assert_eq!(serde_json::to_value(&server).unwrap(), "docs-1");
    }
}
