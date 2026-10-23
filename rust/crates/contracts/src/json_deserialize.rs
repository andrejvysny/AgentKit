//! Preserve arbitrary user objects while decoding typed contracts.
use crate::{Nullable, Optional, parse_javascript_json};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};

/// Decode a JSON value without reinterpreting Serde's private marker keys.
pub trait JsonDeserialize: Sized {
    fn from_json(value: Value) -> Result<Self, serde_json::Error>;
}

/// A Serde field adapter for Value, Vec, Option, maps and generated contracts.
pub fn deserialize_preserving<'de, T: JsonDeserialize, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
    let value = parse_javascript_json(raw.get()).map_err(serde::de::Error::custom)?;
    T::from_json(value).map_err(serde::de::Error::custom)
}

impl JsonDeserialize for Value {
    fn from_json(value: Value) -> Result<Self, serde_json::Error> {
        Ok(value)
    }
}
macro_rules! primitive {
    ($($name:ty),*) => { $(impl JsonDeserialize for $name {
        fn from_json(value: Value) -> std::result::Result<Self, serde_json::Error> { serde_json::from_value(value) }
    })* };
}
primitive!(String, bool, f64, (), i64, u64, u32, usize);

impl<T: JsonDeserialize> JsonDeserialize for Vec<T> {
    fn from_json(value: Value) -> std::result::Result<Self, serde_json::Error> {
        match value {
            Value::Array(items) => items.into_iter().map(T::from_json).collect(),
            _ => Err(json_error("expected array")),
        }
    }
}
impl<T: JsonDeserialize> JsonDeserialize for BTreeMap<String, T> {
    fn from_json(value: Value) -> std::result::Result<Self, serde_json::Error> {
        json_object(value)?
            .into_iter()
            .map(|(key, value)| Ok((key, T::from_json(value)?)))
            .collect()
    }
}
impl<T: JsonDeserialize, S: std::hash::BuildHasher + Default> JsonDeserialize
    for HashMap<String, T, S>
{
    fn from_json(value: Value) -> std::result::Result<Self, serde_json::Error> {
        json_object(value)?
            .into_iter()
            .map(|(key, value)| Ok((key, T::from_json(value)?)))
            .collect()
    }
}
impl<T: JsonDeserialize> JsonDeserialize for Map<String, T>
where
    Map<String, T>: FromIterator<(String, T)>,
{
    fn from_json(value: Value) -> std::result::Result<Self, serde_json::Error> {
        json_object(value)?
            .into_iter()
            .map(|(key, value)| Ok((key, T::from_json(value)?)))
            .collect()
    }
}
impl<T: JsonDeserialize> JsonDeserialize for Option<T> {
    fn from_json(value: Value) -> std::result::Result<Self, serde_json::Error> {
        if value.is_null() {
            Ok(None)
        } else {
            T::from_json(value).map(Some)
        }
    }
}
impl<T: JsonDeserialize> JsonDeserialize for Optional<T> {
    fn from_json(value: Value) -> std::result::Result<Self, serde_json::Error> {
        T::from_json(value).map(Self::Present)
    }
}
impl<T: JsonDeserialize> JsonDeserialize for Nullable<T> {
    fn from_json(value: Value) -> std::result::Result<Self, serde_json::Error> {
        if value.is_null() {
            Ok(Self::Null)
        } else {
            T::from_json(value).map(Self::Value)
        }
    }
}

#[doc(hidden)]
pub fn json_error(message: &str) -> serde_json::Error {
    <serde_json::Error as serde::de::Error>::custom(message)
}
#[doc(hidden)]
pub fn json_object(value: Value) -> std::result::Result<Map<String, Value>, serde_json::Error> {
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(json_error("expected object")),
    }
}
#[doc(hidden)]
pub fn required_field<T: JsonDeserialize>(
    object: &mut Map<String, Value>,
    name: &str,
) -> std::result::Result<T, serde_json::Error> {
    T::from_json(
        object
            .remove(name)
            .ok_or_else(|| json_error(&format!("missing field {name}")))?,
    )
}
#[doc(hidden)]
pub fn optional_field<T: JsonDeserialize>(
    object: &mut Map<String, Value>,
    name: &str,
) -> std::result::Result<Optional<T>, serde_json::Error> {
    object.remove(name).map_or(Ok(Optional::Absent), |value| {
        T::from_json(value).map(Optional::Present)
    })
}
#[doc(hidden)]
pub fn reject_extra_fields(
    object: &Map<String, Value>,
) -> std::result::Result<(), serde_json::Error> {
    if object.is_empty() {
        Ok(())
    } else {
        Err(json_error("unexpected object field"))
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! impl_json_deserialize {
    ($name:ident, $decode:expr) => {
        impl $crate::JsonDeserialize for $name {
            fn from_json(value: serde_json::Value) -> std::result::Result<Self, serde_json::Error> {
                ($decode)(value)
            }
        }
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(
                deserializer: D,
            ) -> std::result::Result<Self, D::Error> {
                $crate::deserialize_preserving(deserializer)
            }
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! impl_json_via_serde {
    ($name:ident) => {
        impl $crate::JsonDeserialize for $name {
            fn from_json(value: serde_json::Value) -> std::result::Result<Self, serde_json::Error> {
                serde_json::from_value(value)
            }
        }
    };
}
