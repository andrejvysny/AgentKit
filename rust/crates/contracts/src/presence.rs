use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A missing optional field differs from a present value, including JSON null.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Optional<T> {
    #[default]
    Absent,
    Present(T),
}

impl<T> Optional<T> {
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    pub fn as_ref(&self) -> Option<&T> {
        match self {
            Self::Absent => None,
            Self::Present(value) => Some(value),
        }
    }
}

impl<T> From<T> for Optional<T> {
    fn from(value: T) -> Self {
        Self::Present(value)
    }
}

impl<T: Serialize> Serialize for Optional<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Absent => Err(serde::ser::Error::custom(
                "absent field cannot be serialized",
            )),
            Self::Present(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Optional<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::Present)
    }
}

/// A required nullable field cannot be silently omitted during deserialization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Nullable<T> {
    Null,
    Value(T),
}

impl<'de, T: crate::JsonDeserialize> Deserialize<'de> for Nullable<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        crate::deserialize_preserving(deserializer)
    }
}

/// JavaScript serializes integral safe numbers as integers, including negative zero.
#[doc(hidden)]
pub fn serialize_number<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if !value.is_finite() {
        return Err(serde::ser::Error::custom("contract numbers must be finite"));
    }
    if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0 {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}

#[doc(hidden)]
pub fn serialize_optional_number<S: Serializer>(
    value: &Optional<f64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Optional::Present(value) => serialize_number(value, serializer),
        Optional::Absent => Err(serde::ser::Error::custom(
            "absent field cannot be serialized",
        )),
    }
}
