use serde_json::Value;
use sha2::{Digest, Sha256};

pub const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
pub const IDEMPOTENT_TASK_ID_PREFIX: &str = "task_ik_";
pub const REGENERATE_TASK_ID_PREFIX: &str = "task_rk_";

pub fn derive_idempotent_task_id(chat_id: &str, key: &str) -> String {
    format!(
        "{IDEMPOTENT_TASK_ID_PREFIX}{}",
        digest(&format!("{chat_id}:{key}"))
    )
}

pub fn derive_regenerate_task_id(chat_id: &str, message_id: &str, key: &str) -> String {
    format!(
        "{REGENERATE_TASK_ID_PREFIX}{}",
        digest(&format!("{chat_id}:{message_id}:{key}"))
    )
}

pub fn turn_fingerprint(parts: &Value) -> String {
    digest(&canonical_json(parts))
}

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
        Value::Number(_) => {
            let number = crate::javascript_number(value).expect("JSON number");
            if !number.is_finite() {
                return "null".into();
            }
            let mut buffer = ryu_js::Buffer::new();
            buffer.format(number).to_owned()
        }
        _ => value.to_string(),
    }
}

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
