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

impl<T, E> Json<'_, T, E> {
    pub(crate) fn str(&self) -> Option<&str> {
        match self {
            Self::Str(text) => Some(text),
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

/// Whether the field is present, whatever it holds (`null` included).
pub(crate) fn present<'de, D: Deserializer<'de>>(field: D) -> Result<bool, D::Error> {
    IgnoredAny::deserialize(field).map(|_| true)
}

/// A field of another type is absent.
pub(crate) fn lax<'de, D, T>(field: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Lax<T> {
        Is(T),
        Not(IgnoredAny),
    }
    Ok(match Option::deserialize(field)? {
        Some(Lax::Is(value)) => Some(value),
        Some(Lax::Not(_)) | None => None,
    })
}

/// The first element of a list, or `None` for `null`; any other value fails.
pub(crate) fn first<'de, D, T>(list: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct First<T>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for First<T> {
        type Value = Option<T>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a list")
        }
        fn visit_unit<E>(self) -> Result<Option<T>, E> {
            Ok(None)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut list: A) -> Result<Option<T>, A::Error> {
            let first = list.next_element()?;
            while list.next_element::<IgnoredAny>()?.is_some() {}
            Ok(first)
        }
    }
    list.deserialize_any(First(PhantomData))
}

/// The word of a wire vocabulary `name` is; `None` for one it lacks.
pub(crate) fn word<'a, T: Deserialize<'a>>(name: &'a str) -> Option<T> {
    T::deserialize(BorrowedStrDeserializer::<value::Error>::new(name)).ok()
}

/// The word of a wire vocabulary `value` states, which must be a string.
pub(crate) fn tag<'a, T: Deserialize<'a>>(value: Option<&'a Value>) -> Option<T> {
    word(value?.as_str()?)
}
