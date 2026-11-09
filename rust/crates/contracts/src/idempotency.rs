pub use agentkit_kernel::canonical_json;
use agentkit_kernel::sha256_hex;
use serde_json::Value;

pub const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
pub const IDEMPOTENT_TASK_ID_PREFIX: &str = "task_ik_";
pub const REGENERATE_TASK_ID_PREFIX: &str = "task_rk_";

pub fn derive_idempotent_task_id(chat_id: &str, key: &str) -> String {
    format!(
        "{IDEMPOTENT_TASK_ID_PREFIX}{}",
        sha256_hex(format!("{chat_id}:{key}"))
    )
}

pub fn derive_regenerate_task_id(chat_id: &str, message_id: &str, key: &str) -> String {
    format!(
        "{REGENERATE_TASK_ID_PREFIX}{}",
        sha256_hex(format!("{chat_id}:{message_id}:{key}"))
    )
}

pub fn turn_fingerprint(parts: &Value) -> String {
    sha256_hex(canonical_json(parts))
}
