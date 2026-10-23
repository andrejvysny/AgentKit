//! JavaScript JSON number semantics without losing nonfinite tool inputs.
use serde_json::{Number, Value};
use std::str::FromStr;

struct RawObject(Vec<(String, Box<serde_json::value::RawValue>)>);

impl<'de> serde::Deserialize<'de> for RawObject {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor;
        impl<'de> serde::de::Visitor<'de> for ObjectVisitor {
            type Value = RawObject;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(RawObject(entries))
            }
        }
        deserializer.deserialize_map(ObjectVisitor)
    }
}

/// Interpret a JSON number as JavaScript's binary64 Number, including overflow.
pub fn javascript_number(value: &Value) -> Option<f64> {
    value
        .as_number()
        .and_then(|number| number.as_str().parse().ok())
}

/// Parse JSON as JavaScript does for numbers. Unpaired UTF-16 surrogates remain
/// the separately approved native UTF-8 compatibility exception.
pub fn parse_javascript_json(input: &str) -> Result<Value, serde_json::Error> {
    let raw: Box<serde_json::value::RawValue> = serde_json::from_str(input)?;
    from_raw(&raw, 0)
}

fn from_raw(raw: &serde_json::value::RawValue, depth: usize) -> Result<Value, serde_json::Error> {
    if depth > 128 {
        return Err(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "recursion limit exceeded",
        )));
    }
    let text = raw.get();
    match text.as_bytes()[0] {
        b'{' => {
            // Deserialize raw members directly so Serde's private number marker
            // cannot turn an ordinary user object into a numeric node.
            let object: RawObject = serde_json::from_str(text)?;
            object
                .0
                .into_iter()
                .map(|(key, raw)| Ok((key, from_raw(&raw, depth + 1)?)))
                .collect::<Result<serde_json::Map<String, Value>, _>>()
                .map(Value::Object)
        }
        b'[' => {
            let items: Vec<Box<serde_json::value::RawValue>> = serde_json::from_str(text)?;
            items
                .iter()
                .map(|raw| from_raw(raw, depth + 1))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
        b'-' | b'0'..=b'9' => Ok(Value::Number(parsed_number(text))),
        _ => serde_json::from_str(text),
    }
}

fn parsed_number(text: &str) -> Number {
    let parsed = text.parse::<f64>().expect("validated JSON number");
    if parsed == 0.0 && parsed.is_sign_negative() {
        Number::from_str("-0.0").expect("negative zero")
    } else if parsed.is_finite() {
        number_from_javascript(parsed)
    } else {
        Number::from_str(if parsed.is_sign_negative() {
            "-1e400"
        } else {
            "1e400"
        })
        .expect("valid overflowing JSON number")
    }
}

/// Apply JSON.stringify's nonfinite-number coercion recursively for persistence
/// or a wire response. Tool callbacks must receive the original value first.
pub fn javascript_json_value(value: &Value) -> Value {
    match value {
        Value::Number(_) => javascript_number(value)
            .filter(|number| number.is_finite())
            .map(number_from_javascript)
            .map_or(Value::Null, Value::Number),
        Value::Array(items) => Value::Array(items.iter().map(javascript_json_value).collect()),
        Value::Object(object) => {
            let mut entries: Vec<_> = object.iter().collect();
            // ECMAScript enumerates array-index keys before other insertion-order keys.
            entries.sort_by_key(|(key, _)| {
                let index = array_index(key);
                (index.is_none(), index)
            });
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), javascript_json_value(value)))
                    .collect(),
            )
        }
        _ => value.clone(),
    }
}

fn number_from_javascript(number: f64) -> Number {
    let mut buffer = ryu_js::Buffer::new();
    Number::from_str(buffer.format(number)).expect("finite JavaScript number")
}

fn array_index(key: &str) -> Option<u32> {
    let index: u32 = key.parse().ok()?;
    (index != u32::MAX && index.to_string() == key).then_some(index)
}

pub fn stringify_javascript_value(value: &Value) -> Result<String, serde_json::Error> {
    serde_json::to_string(&javascript_json_value(value))
}
