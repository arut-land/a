//! Immutable ranges share their item payloads across clients in one process.

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Weak};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::Item;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Items(Arc<Vec<Item>>);

#[derive(Clone, Debug)]
pub struct WeakItems(Weak<Vec<Item>>);

impl Items {
    pub fn downgrade(&self) -> WeakItems {
        WeakItems(Arc::downgrade(&self.0))
    }
}

impl WeakItems {
    pub fn upgrade(&self) -> Option<Items> {
        self.0.upgrade().map(Items)
    }
}

impl Deref for Items {
    type Target = Vec<Item>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Items {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::make_mut(&mut self.0)
    }
}

impl From<Vec<Item>> for Items {
    fn from(items: Vec<Item>) -> Self {
        Self(Arc::new(items))
    }
}

impl FromIterator<Item> for Items {
    fn from_iter<T: IntoIterator<Item = Item>>(items: T) -> Self {
        Vec::from_iter(items).into()
    }
}

impl IntoIterator for Items {
    type Item = Item;
    type IntoIter = std::vec::IntoIter<Item>;
    fn into_iter(self) -> Self::IntoIter {
        Arc::unwrap_or_clone(self.0).into_iter()
    }
}

impl<'a> IntoIterator for &'a Items {
    type Item = &'a Item;
    type IntoIter = std::slice::Iter<'a, Item>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a> IntoIterator for &'a mut Items {
    type Item = &'a mut Item;
    type IntoIter = std::slice::IterMut<'a, Item>;
    fn into_iter(self) -> Self::IntoIter {
        Arc::make_mut(&mut self.0).iter_mut()
    }
}

impl PartialEq<Vec<Item>> for Items {
    fn eq(&self, other: &Vec<Item>) -> bool {
        **self == *other
    }
}

impl PartialEq<Items> for Vec<Item> {
    fn eq(&self, other: &Items) -> bool {
        *self == **other
    }
}

impl Serialize for Items {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.as_slice().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Items {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<Item>::deserialize(deserializer).map(Self::from)
    }
}
