use ext::Schema;
use serde::Deserialize;

macro_rules! container {
    ($name:ident, $($attr:tt)*) => {
        #[derive(Deserialize, Schema)]
        #[serde($($attr)*)]
        struct $name { value: String }
    };
}
macro_rules! field {
    ($name:ident, $($attr:tt)*) => {
        #[derive(Deserialize, Schema)]
        struct $name { #[serde($($attr)*)] value: String }
    };
}

container!(RenameAll, rename_all = "camelCase");
container!(Transparent, transparent);
container!(Defaulted, default);
container!(FromValue, from = "String");
container!(TryFromValue, try_from = "String");
container!(Tagged, tag = "type");

impl Default for Defaulted {
    fn default() -> Self { Self { value: String::new() } }
}
impl From<String> for FromValue {
    fn from(value: String) -> Self { Self { value } }
}
impl TryFrom<String> for TryFromValue {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> { Ok(Self { value }) }
}

mod codec {
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        serde::Deserialize::deserialize(d)
    }
}
fn read<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    serde::Deserialize::deserialize(d)
}

field!(Flatten, flatten);
field!(Skip, skip);
field!(SkipRead, skip_deserializing);
field!(Alias, alias = "other");
field!(With, with = "codec");
field!(ReadWith, deserialize_with = "read");

#[derive(Deserialize, Schema)]
struct DirectionalRename {
    #[serde(rename(deserialize = "input", serialize = "output"))]
    value: String,
}

#[derive(Deserialize, Schema)]
struct Duplicate {
    value: String,
    #[serde(rename = "value")]
    other: String,
}
