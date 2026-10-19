//! Internally tagged enums decoded from a [`Value`]. Serde decodes one by
//! copying the object into a buffer of its own to find the tag, then each
//! variant from that buffer, in code of its own for every variant and every
//! input; nested in another tagged enum, through yet another layer. Every
//! wire record is decoded from a `Value` here instead: the tag is taken out
//! of the object, and the rest decoded as the externally tagged variant it
//! names by a `#[serde(remote)]` twin of the enum, which [`tagged!`] makes
//! from the same definition. All of it then shares the `Value` decoders.

use serde::de::value::StringDeserializer;
use serde::de::{DeserializeSeed, Deserializer, EnumAccess, Error, VariantAccess, Visitor};
use serde::forward_to_deserialize_any;
use serde_json::{Map, Value};

/// Defines an internally tagged enum, serialized with the tag `$tag` in its
/// object and decoded through [`decode`].
macro_rules! tagged {
    (
        $(#[$attr:meta])*
        $vis:vis enum $name:ident in $tag:literal { $($variants:tt)* }
    ) => {
        $(#[$attr])*
        #[derive(Serialize)]
        #[serde(tag = $tag, rename_all = "snake_case")]
        $vis enum $name { $($variants)* }

        const _: () = {
            type Tagged = $name;

            #[derive(Deserialize)]
            #[serde(remote = "Tagged", rename_all = "snake_case")]
            #[allow(dead_code)]
            enum Twin { $($variants)* }

            impl<'de> Deserialize<'de> for $name {
                fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                    let value = serde_json::Value::deserialize(d)?;
                    Self::from_value(value).map_err(serde::de::Error::custom)
                }
            }

            impl $name {
                /// Decodes it from an object parsed already, without the
                /// copy that deserializing a `Value` makes.
                #[allow(dead_code)]
                pub(crate) fn from_value(value: serde_json::Value) -> serde_json::Result<Self> {
                    $crate::tagged::decode(value, $tag, Twin::deserialize)
                }
            }
        };
    };
}

pub(crate) use tagged;

/// `value`, an object, as the variant its field `tag` names.
pub fn decode<T>(
    value: Value,
    tag: &'static str,
    variant: fn(Tagged) -> serde_json::Result<T>,
) -> serde_json::Result<T> {
    let Value::Object(mut fields) = value else {
        return Err(serde_json::Error::custom("expected an object"));
    };
    let Some(Value::String(name)) = fields.remove(tag) else {
        return Err(serde_json::Error::missing_field(tag));
    };
    variant(Tagged { name, fields })
}

/// An object's fields as the externally tagged variant `name`.
pub struct Tagged {
    name: String,
    fields: Map<String, Value>,
}

impl<'de> Deserializer<'de> for Tagged {
    type Error = serde_json::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> serde_json::Result<V::Value> {
        visitor.visit_enum(self)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

impl<'de> EnumAccess<'de> for Tagged {
    type Error = serde_json::Error;
    type Variant = Rest;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> serde_json::Result<(V::Value, Rest)> {
        let name = StringDeserializer::<serde_json::Error>::new(self.name);
        Ok((seed.deserialize(name)?, Rest(Value::Object(self.fields))))
    }
}

/// The fields besides the tag, which a unit variant ignores and a newtype
/// variant's struct takes as its own.
pub struct Rest(Value);

impl<'de> VariantAccess<'de> for Rest {
    type Error = serde_json::Error;

    fn unit_variant(self) -> serde_json::Result<()> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> serde_json::Result<T::Value> {
        seed.deserialize(self.0)
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> serde_json::Result<V::Value> {
        self.0.deserialize_seq(visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> serde_json::Result<V::Value> {
        self.0.deserialize_struct("", fields, visitor)
    }
}
