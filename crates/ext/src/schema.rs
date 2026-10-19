//! Tool input schemas, derived from the input's type: `#[derive(Schema)]`
//! writes the JSON Schema of the object serde reads it from, when a
//! request first offers the tool. Nothing keeps a schema of its own.

use std::fmt::Write as _;
use std::path::PathBuf;

pub use ext_derive::Schema;

/// A type's JSON Schema, as the text a request carries.
pub trait Schema {
    /// Whether a property of this type may be left out.
    const OPTIONAL: bool = false;

    /// Writes the schema to `out`: one JSON object.
    fn schema(out: &mut String);
}

/// `T`'s schema, as text.
pub fn of<T: Schema + ?Sized>() -> String {
    let mut out = String::new();
    T::schema(&mut out);
    out
}

/// Writes the schema of a property of type `T`, described by `doc` and
/// bounded by `bounds` (`minimum`, `maximum`, `minItems`).
#[doc(hidden)]
pub fn property<T: Schema + ?Sized>(out: &mut String, doc: &str, bounds: &[(&str, u64)]) {
    T::schema(out);
    if doc.is_empty() && bounds.is_empty() {
        return;
    }
    // The schema is an object: what follows goes before its brace.
    out.pop();
    let mut first = out.ends_with('{');
    let mut comma = |out: &mut String| {
        if !std::mem::take(&mut first) {
            out.push(',');
        }
    };
    for (key, value) in bounds {
        comma(out);
        let _ = write!(out, "\"{key}\":{value}");
    }
    if !doc.is_empty() {
        comma(out);
        out.push_str("\"description\":");
        string(out, doc);
    }
    out.push('}');
}

/// `text` as a JSON string.
fn string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

macro_rules! simple {
    ($schema:literal: $($t:ty),*) => {$(
        impl Schema for $t {
            fn schema(out: &mut String) {
                out.push_str($schema);
            }
        }
    )*};
}

simple!(r#"{"type":"string"}"#: String, str, PathBuf);
simple!(r#"{"type":"boolean"}"#: bool);
simple!(r#"{"type":"integer"}"#: u8, u16, u32, u64, usize, i32, i64);
simple!(r#"{"type":"number"}"#: f64);
simple!("{}": serde_json::Value);

impl<T: Schema> Schema for Option<T> {
    const OPTIONAL: bool = true;

    fn schema(out: &mut String) {
        T::schema(out);
    }
}

impl<T: Schema> Schema for Vec<T> {
    fn schema(out: &mut String) {
        out.push_str(r#"{"type":"array","items":"#);
        T::schema(out);
        out.push('}');
    }
}
