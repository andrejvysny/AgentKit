use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};

pub(crate) const PREFIX: &str = "\0agentkit-sdk-key:";
const NUMBER: &str = "$serde_json::private::Number";
const RAW_VALUE: &str = "$serde_json::private::RawValue";

// The arbitrary_precision/raw_value Value visitor treats these ordinary user keys
// as internal tokens. Keys escape only while an SDK parser owns them. Prefix
// keys escape too, making the mapping reversible without user-key collisions.
pub(crate) fn escape(value: &Value) -> Value {
    keys(value, true)
}
pub(crate) fn restore(value: &Value) -> Value {
    keys(value, false)
}
fn keys(value: &Value, encode: bool) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    let key = if encode
                        && (key == NUMBER || key == RAW_VALUE || key.starts_with(PREFIX))
                    {
                        format!("{PREFIX}{key}")
                    } else if !encode {
                        key.strip_prefix(PREFIX).unwrap_or(key).to_owned()
                    } else {
                        key.clone()
                    };
                    (key, keys(value, encode))
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|item| keys(item, encode)).collect()),
        _ => value.clone(),
    }
}
pub(crate) fn sdk_text(raw: &str) -> Result<String, serde_json::Error> {
    let value = agentkit_contracts::parse_javascript_json(raw)?;
    serde_json::to_string(&escape(&value))
}
pub(crate) fn sdk_decode<T: DeserializeOwned>(raw: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(&sdk_text(raw)?)
}
pub(crate) fn wire_text(value: &impl Serialize) -> Result<String, serde_json::Error> {
    agentkit_contracts::stringify_javascript_value(&serde_json::to_value(value)?)
}
pub(crate) fn map_object(object: &mut Map<String, Value>, map: fn(&Value) -> Value) {
    *object = map(&Value::Object(std::mem::take(object)))
        .as_object()
        .expect("object key mapping")
        .clone();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdk_key_escape_is_bijective_for_repeated_prefixes_and_reserved_objects() {
        let mut object = Map::new();
        for depth in 0..8 {
            let mut nested = Map::new();
            nested.insert(NUMBER.into(), Value::String("123".into()));
            object.insert(
                format!("{}{NUMBER}", PREFIX.repeat(depth)),
                Value::Array(vec![Value::Object(nested), Value::from(123)]),
            );
            let mut raw = Map::new();
            raw.insert(RAW_VALUE.into(), Value::String("{\"inside\":1}".into()));
            object.insert(
                format!("{}{RAW_VALUE}", PREFIX.repeat(depth)),
                Value::Object(raw),
            );
            object.insert(
                format!("{}literal", PREFIX.repeat(depth)),
                Value::String(format!("{PREFIX}{NUMBER}")),
            );
        }
        object.insert(PREFIX.into(), Value::String("empty suffix".into()));
        let original = Value::Object(object);
        let protected: Value = sdk_decode(&serde_json::to_string(&original).unwrap()).unwrap();
        assert_eq!(restore(&protected), original);
    }
}
