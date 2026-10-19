//! Frames read into types that borrow their strings, so a streamed token
//! costs no allocation beyond the text its event owns. A string borrows
//! from the frame unless it holds escapes. Each decoder picks how lenient a
//! field is: [`Json`] keeps a value of any type as what it is, [`lax`]
//! reads one of another type as absent, and a plain field fails the frame,
//! which the decoder then reads another way.

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use serde::de::value::{self, BorrowedStrDeserializer, MapAccessDeserializer};
use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// A JSON value as a decoder reads it: absent, a string, an object read as
/// `T`, an array of `E`s, or any other value (`null`, a number, a boolean).
#[derive(Default)]
pub(crate) enum Json<'a, T = Value, E = Value> {
    #[default]
    Absent,
    Str(Cow<'a, str>),
    Object(T),
    List(Vec<E>),
    Other(Value),
}

impl<'a, T, E> Json<'a, T, E> {
    pub(crate) fn str(&self) -> Option<&str> {
        match self {
            Self::Str(text) => Some(text),
            _ => None,
        }
    }

    /// The string, unless it is empty or the value is no string.
    pub(crate) fn into_text(self) -> Option<Cow<'a, str>> {
        match self {
            Self::Str(text) if !text.is_empty() => Some(text),
            _ => None,
        }
    }

    pub(crate) fn object(&self) -> Option<&T> {
        match self {
            Self::Object(object) => Some(object),
            _ => None,
        }
    }

    pub(crate) fn items(&self) -> &[E] {
        match self {
            Self::List(items) => items,
            _ => &[],
        }
    }
}

impl<T> Json<'_, T> {
    /// The value, unless it is absent or an object.
    pub(crate) fn scalar(&self) -> Option<Value> {
        match self {
            Self::Absent | Self::Object(_) => None,
            Self::Str(text) => Some(text.as_ref().into()),
            Self::List(items) => Some(Value::Array(items.clone())),
            Self::Other(value) => Some(value.clone()),
        }
    }
}

impl<T> Json<'_, T> {
    /// The value, `T` aside: `None` for an object.
    pub(crate) fn into_value(self) -> Option<Value> {
        match self {
            Self::Absent => Some(Value::Null),
            Self::Str(text) => Some(text.into_owned().into()),
            Self::Object(_) => None,
            Self::List(items) => Some(Value::Array(items)),
            Self::Other(value) => Some(value),
        }
    }
}

impl Json<'_> {
    /// The value as text: a string as it is, anything else but `null` as
    /// JSON.
    pub(crate) fn text(&self) -> Option<Cow<'_, str>> {
        match self {
            Self::Str(text) => Some(Cow::Borrowed(text)),
            Self::Object(value) => Some(value.to_string().into()),
            json => json
                .scalar()
                .filter(|value| !value.is_null())
                .map(|value| value.to_string().into()),
        }
    }
}

struct Read<'a, T, E>(PhantomData<Json<'a, T, E>>);

impl<'de: 'a, 'a, T: Deserialize<'de>, E: Deserialize<'de>> Visitor<'de> for Read<'a, T, E> {
    type Value = Json<'a, T, E>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JSON")
    }

    fn visit_unit<X: de::Error>(self) -> Result<Self::Value, X> {
        Ok(Json::Other(Value::Null))
    }

    fn visit_bool<X: de::Error>(self, value: bool) -> Result<Self::Value, X> {
        Ok(Json::Other(value.into()))
    }

    fn visit_i64<X: de::Error>(self, value: i64) -> Result<Self::Value, X> {
        Ok(Json::Other(value.into()))
    }

    fn visit_u64<X: de::Error>(self, value: u64) -> Result<Self::Value, X> {
        Ok(Json::Other(value.into()))
    }

    fn visit_f64<X: de::Error>(self, value: f64) -> Result<Self::Value, X> {
        Ok(Json::Other(value.into()))
    }

    fn visit_borrowed_str<X: de::Error>(self, text: &'de str) -> Result<Self::Value, X> {
        Ok(Json::Str(Cow::Borrowed(text)))
    }

    fn visit_str<X: de::Error>(self, text: &str) -> Result<Self::Value, X> {
        Ok(Json::Str(Cow::Owned(text.to_owned())))
    }

    fn visit_string<X: de::Error>(self, text: String) -> Result<Self::Value, X> {
        Ok(Json::Str(Cow::Owned(text)))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Json::List(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        T::deserialize(MapAccessDeserializer::new(map)).map(Json::Object)
    }
}

impl<'de: 'a, 'a, T: Deserialize<'de>, E: Deserialize<'de>> Deserialize<'de> for Json<'a, T, E> {
    fn deserialize<D: Deserializer<'de>>(json: D) -> Result<Self, D::Error> {
        json.deserialize_any(Read(PhantomData))
    }
}

/// A string that borrows from the frame inside an `Option`, where a bare
/// `Cow` never borrows.
#[derive(Deserialize)]
pub(crate) struct Str<'a>(#[serde(borrow)] pub(crate) Cow<'a, str>);

/// An object read field by field: [`Fields::field`] takes the fields it
/// names, and the rest gather in [`Fields::rest`] or are skipped. A field
/// stated twice reads as its last value, as in a `Value`. serde's `flatten`
/// would buffer every field first, and a derive costs more code a type.
pub(crate) trait Fields<'de>: Default {
    /// Read the value of `key` when it is a field of `Self`; `false` leaves
    /// it to the rest.
    fn field<A: MapAccess<'de>>(&mut self, key: &str, map: &mut A) -> Result<bool, A::Error>;

    /// Where the other fields go; `None` skips them.
    fn rest(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        None
    }
}

/// The object `json` holds, read as `T`'s [`Fields`].
pub(crate) fn fields<'de, D: Deserializer<'de>, T: Fields<'de>>(json: D) -> Result<T, D::Error> {
    struct Read<T>(PhantomData<T>);
    impl<'de, T: Fields<'de>> Visitor<'de> for Read<T> {
        type Value = T;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<T, A::Error> {
            let mut out = T::default();
            while let Some(Str(key)) = map.next_key()? {
                if out.field(&key, &mut map)? {
                    continue;
                }
                if let Some(rest) = out.rest() {
                    let value = map.next_value()?;
                    rest.insert(key.into_owned(), value);
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok(out)
        }
    }
    json.deserialize_map(Read(PhantomData))
}

/// The first element of a list, or `None` for `null`; any other value fails.
pub(crate) struct First<T>(pub(crate) Option<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for First<T> {
    fn deserialize<D: Deserializer<'de>>(list: D) -> Result<Self, D::Error> {
        struct Read<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for Read<T> {
            type Value = First<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a list")
            }
            fn visit_unit<E>(self) -> Result<First<T>, E> {
                Ok(First(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut list: A) -> Result<First<T>, A::Error> {
                let first = list.next_element()?;
                while list.next_element::<IgnoredAny>()?.is_some() {}
                Ok(First(first))
            }
        }
        list.deserialize_any(Read(PhantomData))
    }
}

/// The word of a wire vocabulary `name` is; `None` for one it lacks.
pub(crate) fn word<'a, T: Deserialize<'a>>(name: &'a str) -> Option<T> {
    T::deserialize(BorrowedStrDeserializer::<value::Error>::new(name)).ok()
}

/// The word of a wire vocabulary `value` states, which must be a string.
pub(crate) fn tag<'a, T: Deserialize<'a>>(value: Option<&'a Value>) -> Option<T> {
    word(value?.as_str()?)
}
