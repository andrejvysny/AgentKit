use std::time::{SystemTime, UNIX_EPOCH};
pub trait Runtime: Send + Sync {
    fn timestamp(&self) -> String;
    fn id(&self, prefix: &str) -> String;
    fn now_ms(&self) -> u64;
}
pub struct SystemRuntime;
impl Runtime for SystemRuntime {
    fn timestamp(&self) -> String {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }
    fn id(&self, prefix: &str) -> String {
        format!("{prefix}{}", uuid::Uuid::new_v4())
    }
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}
