use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Idempotency {
    pub key: String,
    pub task_id: String,
    pub fingerprint: String,
}

/// Byte-identical to the TypeScript transport's persisted task identifiers.
pub fn derive_task_id(chat: &str, message: Option<&str>, key: &str) -> String {
    match message {
        Some(message) => agentkit_contracts::derive_regenerate_task_id(chat, message, key),
        None => agentkit_contracts::derive_idempotent_task_id(chat, key),
    }
}

pub(crate) fn fingerprint(operation: &str, body: &Value) -> String {
    let mut parts = serde_json::Map::new();
    for field in ["model", "providerId"] {
        parts.insert(
            field.into(),
            body.get(field).cloned().unwrap_or(Value::Null),
        );
    }
    if operation == "submitMessage" {
        parts.insert("content".into(), body["content"].clone());
        parts.insert(
            "metadata".into(),
            body.get("metadata")
                .cloned()
                .unwrap_or(serde_json::json!({})),
        );
        parts.insert(
            "parentMessageId".into(),
            body.get("parentMessageId").cloned().unwrap_or(Value::Null),
        );
    }
    agentkit_contracts::turn_fingerprint(&Value::Object(parts))
}
