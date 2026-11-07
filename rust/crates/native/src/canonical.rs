//! Stable canonical digests for frozen admission snapshots.
//!
//! JSON objects are re-serialized with keys sorted so two structurally
//! equal values always hash identically, independent of map ordering. This
//! is the identity used for idempotency fingerprints and configuration
//! identity across admission/promotion/recovery paths.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// SHA-256 (hex) of the canonical serialization of `value`.
pub fn canonical_digest(value: &Value) -> String {
    let mut buf = String::new();
    write_canonical(value, &mut buf);
    let mut hasher = Sha256::new();
    hasher.update(buf.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json_string(key, out);
                out.push(':');
                write_canonical(&map[*key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => push_scalar(other, out),
    }
}

fn push_scalar(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => write_json_string(text, out),
        // Containers are handled structurally by `write_canonical`.
        Value::Object(_) | Value::Array(_) => {
            unreachable!("containers are serialized structurally")
        }
    }
}

fn write_json_string(text: &str, out: &mut String) {
    let encoded = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned());
    out.push_str(&encoded);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_key_order_does_not_change_the_digest() {
        let a = json!({"x": 1, "y": {"b": 2, "a": 3}});
        let b = json!({"y": {"a": 3, "b": 2}, "x": 1});
        assert_eq!(canonical_digest(&a), canonical_digest(&b));
    }

    #[test]
    fn distinct_values_have_distinct_digests() {
        assert_ne!(
            canonical_digest(&json!({"a": 1})),
            canonical_digest(&json!({"a": 2}))
        );
        assert_ne!(
            canonical_digest(&json!([1, 2])),
            canonical_digest(&json!([2, 1]))
        );
    }

    #[test]
    fn strings_are_escaped_consistently() {
        assert_ne!(
            canonical_digest(&json!({"a|b": "c"})),
            canonical_digest(&json!({"a": "b", "c": ""}))
        );
        // Values with quotes/colons cannot collide with object boundaries.
        assert_ne!(
            canonical_digest(&Value::String("a\":{\"b\":1".to_owned())),
            canonical_digest(&json!({"a": {"b": 1}}))
        );
    }
}
