use crate::sha256_hex;
use serde_json::Value;

/// Match JavaScript's UTF-16 key sorting while retaining array order.
pub fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<_> = object.iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
            let body: Vec<_> = entries
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("JSON key"),
                        canonical_json(value)
                    )
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        Value::Array(array) => format!(
            "[{}]",
            array
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Number(number) => {
            let number: f64 = number.as_str().parse().expect("JSON number");
            if !number.is_finite() {
                return "null".into();
            }
            let mut buffer = ryu_js::Buffer::new();
            buffer.format(number).to_owned()
        }
        _ => value.to_string(),
    }
}

/// Lowercase hex sha-256 of [`canonical_json`].
pub fn canonical_sha256(value: &Value) -> String {
    sha256_hex(canonical_json(value))
}
