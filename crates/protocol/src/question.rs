//! Typed user questions shared by extensions, policy and every client.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Json, Seq};

/// The prompts asked together at one durable wait.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub source: String,
    pub prompts: BTreeMap<String, Prompt>,
}

/// Only question waits allocate continuation storage; ordinary call phases stay small.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionWait {
    pub question: Question,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<Json>,
}

/// Each prompt has its own response, including in a multi-input round.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Prompt {
    Confirm {
        message: String,
    },
    Form {
        message: String,
        fields: Vec<Field>,
    },
    Url {
        message: String,
        url: String,
        destination: Destination,
    },
    Continue {
        message: String,
    },
}

/// Display metadata computed when an external URL enters the question boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Destination {
    pub origin: String,
    pub raw_origin: Option<String>,
    pub suspicious: bool,
}

impl Prompt {
    pub fn cancel(&self) -> Response {
        if matches!(self, Self::Continue { .. }) {
            Response::Continue(ContinueAnswer::Cancel)
        } else {
            Response::Cancel
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Confirm { message }
            | Self::Form { message, .. }
            | Self::Url { message, .. }
            | Self::Continue { message } => message,
        }
    }
}

/// Absence of an optional field is distinct from its empty or false value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub required: bool,
    pub sensitive: bool,
    pub default: Option<Json>,
    pub kind: FieldKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    String {
        min: Option<u64>,
        max: Option<u64>,
        format: Option<String>,
    },
    Number {
        integer: bool,
        min: Option<serde_json::Number>,
        max: Option<serde_json::Number>,
    },
    Boolean,
    Choice {
        options: Vec<Choice>,
        selection: Selection,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    One,
    Many { min: Option<u64>, max: Option<u64> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub value: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    pub question: Seq,
    pub responses: BTreeMap<String, Response>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Accept(BTreeMap<String, Json>),
    Submit(BTreeMap<String, Json>),
    Continue(ContinueAnswer),
    Decline,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinueAnswer {
    Proceed,
    Decline,
    Cancel,
}

impl Response {
    pub fn accept() -> Self {
        Self::Accept(BTreeMap::new())
    }
}

impl crate::Decode for QuestionAnswer {
    fn from_json(bytes: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(bytes)
    }
}

impl QuestionAnswer {
    pub fn confirm(question: Seq, response: Response) -> Self {
        Self {
            question,
            responses: BTreeMap::from([("confirm".into(), response)]),
        }
    }

    pub fn accepted(&self) -> bool {
        self.responses.values().all(|r| {
            matches!(
                r,
                Response::Accept(_)
                    | Response::Submit(_)
                    | Response::Continue(ContinueAnswer::Proceed)
            )
        })
    }
}

impl Question {
    pub fn confirm(message: String) -> Self {
        Self {
            source: "authorization".into(),
            prompts: BTreeMap::from([("confirm".into(), Prompt::Confirm { message })]),
        }
    }

    /// Validates an untrusted answer against the exact question still pending.
    pub fn validate(&self, answer: &QuestionAnswer) -> Result<(), String> {
        if self.prompts.len() != answer.responses.len() {
            return Err("answer must address every prompt".into());
        }
        for (key, prompt) in &self.prompts {
            let response = answer
                .responses
                .get(key)
                .ok_or("answer has an unknown prompt")?;
            let values = match response {
                Response::Decline | Response::Cancel
                    if !matches!(prompt, Prompt::Continue { .. }) =>
                {
                    continue;
                }
                Response::Continue(_) if matches!(prompt, Prompt::Continue { .. }) => continue,
                Response::Submit(values) if matches!(prompt, Prompt::Form { .. }) => values,
                Response::Accept(values)
                    if !matches!(prompt, Prompt::Form { .. } | Prompt::Continue { .. })
                        && values.is_empty() =>
                {
                    continue;
                }
                _ => return Err("response does not match the prompt type".into()),
            };
            let Prompt::Form { fields, .. } = prompt else {
                unreachable!()
            };
            if values
                .keys()
                .any(|name| !fields.iter().any(|f| &f.name == name))
            {
                return Err("answer has an unknown field".into());
            }
            for field in fields {
                match values.get(&field.name) {
                    Some(value) => field
                        .validate(value)
                        .map_err(|e| format!("{}: {e}", field.name))?,
                    None if field.required => return Err(format!("{}: required", field.name)),
                    None => {}
                }
            }
        }
        Ok(())
    }
}

impl Field {
    pub fn default_text(&self) -> Option<String> {
        let value = self.default.as_ref()?;
        match &self.kind {
            FieldKind::String { .. }
            | FieldKind::Choice {
                selection: Selection::One,
                ..
            } => value.parse().ok(),
            _ => Some(value.raw().into()),
        }
    }

    pub fn validate(&self, value: &Json) -> Result<(), String> {
        let value: Value = value.parse().map_err(|e| e.to_string())?;
        let valid = match &self.kind {
            FieldKind::Boolean => value.is_boolean(),
            FieldKind::String { min, max, format } => value.as_str().is_some_and(|s| {
                let length = s.chars().count() as u64;
                min.is_none_or(|n| length >= n)
                    && max.is_none_or(|n| length <= n)
                    && format.as_ref().is_none_or(|f| valid_format(f, s))
            }),
            FieldKind::Number { integer, min, max } => value.as_number().is_some_and(|n| {
                (!integer || n.as_f64().is_some_and(|n| n.fract() == 0.0))
                    && min
                        .as_ref()
                        .is_none_or(|m| compare_number(n, m).is_some_and(|o| !o.is_lt()))
                    && max
                        .as_ref()
                        .is_none_or(|m| compare_number(n, m).is_some_and(|o| !o.is_gt()))
            }),
            FieldKind::Choice { options, selection } => {
                let contains = |v: &Value| {
                    v.as_str()
                        .is_some_and(|s| options.iter().any(|o| o.value == s))
                };
                match selection {
                    Selection::One => contains(&value),
                    Selection::Many { min, max } => value.as_array().is_some_and(|vs| {
                        let length = vs.len() as u64;
                        min.is_none_or(|n| length >= n)
                            && max.is_none_or(|n| length <= n)
                            && vs.iter().all(contains)
                            && vs.iter().enumerate().all(|(at, v)| !vs[..at].contains(v))
                    }),
                }
            }
        };
        if valid {
            Ok(())
        } else {
            Err("value does not match the requested field".into())
        }
    }
}

fn valid_format(format: &str, value: &str) -> bool {
    match format {
        "email" => email(value),
        "uri" => uri(value),
        "date" => date(value),
        "date-time" => datetime(value),
        _ => false,
    }
}

fn date(value: &str) -> bool {
    let mut parts = value.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if ![year, month, day]
        .iter()
        .all(|v| v.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    let (Ok(y), Ok(m), Ok(d)) = (
        year.parse::<u32>(),
        month.parse::<u32>(),
        day.parse::<u32>(),
    ) else {
        return false;
    };
    let max = match m {
        4 | 6 | 9 | 11 => 30,
        2 => {
            if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                29
            } else {
                28
            }
        }
        1..=12 => 31,
        _ => 0,
    };
    year.len() == 4 && month.len() == 2 && day.len() == 2 && d > 0 && d <= max
}

fn datetime(value: &str) -> bool {
    if !value.is_ascii() {
        return false;
    }
    let Some((day, time)) = value.split_once(['T', 't']) else {
        return false;
    };
    if !date(day) || time.len() < 9 || time.as_bytes()[2] != b':' || time.as_bytes()[5] != b':' {
        return false;
    }
    let clock = [&time[..2], &time[3..5], &time[6..8]];
    if clock.iter().zip([23, 59, 60]).any(|(n, max)| {
        !n.bytes().all(|b| b.is_ascii_digit()) || n.parse::<u32>().map_or(true, |n| n > max)
    }) {
        return false;
    }
    let tail = &time[8..];
    let zone = if let Some(fraction) = tail.strip_prefix('.') {
        let length = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if length == 0 {
            return false;
        }
        &fraction[length..]
    } else {
        tail
    };
    if matches!(zone, "Z" | "z") {
        return true;
    }
    zone.len() == 6
        && matches!(zone.as_bytes()[0], b'+' | b'-')
        && zone.as_bytes()[3] == b':'
        && zone[1..3]
            .bytes()
            .chain(zone[4..].bytes())
            .all(|b| b.is_ascii_digit())
        && zone[1..3].parse::<u32>().is_ok_and(|n| n <= 23)
        && zone[4..].parse::<u32>().is_ok_and(|n| n <= 59)
}

fn compare_number(a: &serde_json::Number, b: &serde_json::Number) -> Option<std::cmp::Ordering> {
    let integer = |n: &serde_json::Number| {
        n.as_i64()
            .map(i128::from)
            .or_else(|| n.as_u64().map(i128::from))
    };
    match (integer(a), integer(b)) {
        (Some(a), Some(b)) => Some(a.cmp(&b)),
        _ => a.as_f64()?.partial_cmp(&b.as_f64()?),
    }
}

fn uri(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once(':') else {
        return false;
    };
    if scheme.is_empty()
        || !scheme.bytes().enumerate().all(|(i, c)| {
            c.is_ascii_alphabetic() || i > 0 && (c.is_ascii_digit() || b"+-.".contains(&c))
        })
    {
        return false;
    }
    let mut bytes = rest.bytes();
    while let Some(c) = bytes.next() {
        if c == b'%' {
            if !bytes.next().is_some_and(|b| b.is_ascii_hexdigit())
                || !bytes.next().is_some_and(|b| b.is_ascii_hexdigit())
            {
                return false;
            }
        } else if !c.is_ascii_alphanumeric() && !b"-._~:/?#[]@!$&'()*+,;=".contains(&c) {
            return false;
        }
    }
    true
}

fn email(value: &str) -> bool {
    let Some((local, host)) = value.rsplit_once('@') else {
        return false;
    };
    if local.is_empty() || !local.is_ascii() || !host.is_ascii() {
        return false;
    }
    if let Some(quoted) = local.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        let mut escaped = false;
        for c in quoted.bytes() {
            if escaped {
                if !(32..=126).contains(&c) {
                    return false;
                }
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' || !(32..=126).contains(&c) {
                return false;
            }
        }
        if escaped {
            return false;
        }
    } else if local.split('.').any(|part| {
        part.is_empty()
            || !part
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~".contains(&c))
    }) {
        return false;
    }
    if let Some(address) = host.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return address
            .strip_prefix("IPv6:")
            .unwrap_or(address)
            .parse::<std::net::IpAddr>()
            .is_ok();
    }
    !host.is_empty()
        && host.split('.').all(|part| {
            !part.is_empty()
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_validation_preserves_absence_false_integer_precision_and_decline() {
        let question = Question {
            source: "fixture".into(),
            prompts: BTreeMap::from([(
                "form".into(),
                Prompt::Form {
                    message: "values".into(),
                    fields: vec![
                        Field {
                            name: "n".into(),
                            title: None,
                            description: None,
                            required: true,
                            sensitive: false,
                            default: None,
                            kind: FieldKind::Number {
                                integer: true,
                                min: Some(serde_json::Number::from(9_007_199_254_740_993_u64)),
                                max: None,
                            },
                        },
                        Field {
                            name: "b".into(),
                            title: None,
                            description: None,
                            required: false,
                            sensitive: false,
                            default: None,
                            kind: FieldKind::Boolean,
                        },
                    ],
                },
            )]),
        };
        let answer = |n: u64, b: Option<bool>| {
            let mut values = BTreeMap::from([("n".into(), Json::encode(&n).unwrap())]);
            if let Some(b) = b {
                values.insert("b".into(), Json::encode(&b).unwrap());
            }
            QuestionAnswer {
                question: Seq::new(1).unwrap(),
                responses: BTreeMap::from([("form".into(), Response::Submit(values))]),
            }
        };
        assert!(
            question
                .validate(&answer(9_007_199_254_740_992, None))
                .is_err()
        );
        assert!(
            question
                .validate(&answer(9_007_199_254_740_993, Some(false)))
                .is_ok()
        );
        assert!(
            question
                .validate(&answer(9_007_199_254_740_993, None))
                .is_ok()
        );
        assert!(
            question
                .validate(&QuestionAnswer {
                    question: Seq::new(1).unwrap(),
                    responses: BTreeMap::from([("form".into(), Response::Decline)])
                })
                .is_ok()
        );
    }

    #[test]
    fn supported_formats_validate_without_slicing_untrusted_unicode() {
        for (format, valid, invalid) in [
            ("date", "2024-02-29", "2025-02-29"),
            (
                "date-time",
                "2026-10-29T12:34:56.123+05:30",
                "2026-10-29T☃☃☃",
            ),
            (
                "uri",
                "https://example.com/a%20b",
                "https://example.com/%zz",
            ),
            (
                "email",
                "\"Ada Lovelace\"@example.com",
                "ada..name@example.com",
            ),
        ] {
            assert!(valid_format(format, valid), "{valid}");
            assert!(!valid_format(format, invalid), "{invalid}");
        }
    }
}
