use crate::jsbridge::{JsBridge, PendingKind};
use agentkit_core::{CancellationToken, CoreError, RunHooks};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct Hooks {
    pub bridge: Arc<JsBridge>,
    pub names: Vec<Vec<String>>,
    pub timeout_ms: u64,
}
#[async_trait]
impl RunHooks for Hooks {
    fn indexes(&self, name: &str) -> Vec<usize> {
        self.names
            .iter()
            .enumerate()
            .filter_map(|(i, names)| names.iter().any(|n| n == name).then_some(i))
            .collect()
    }
    async fn call(
        &self,
        name: &str,
        index: usize,
        context: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, CoreError> {
        let id = uuid::Uuid::new_v4().to_string();
        let kind = if name == "onSettled" {
            PendingKind::Settled
        } else {
            PendingKind::Hook
        };
        let value = self
            .bridge
            .clone()
            .request(
                kind,
                &id,
                &json!({"name":name,"index":index,"context":context}),
                cancellation,
            )
            .await
            .map_err(|e| CoreError::new("hook_failed", &e))?;
        Ok(value.get("value").cloned().unwrap_or(Value::Null))
    }
}
impl Hooks {
    pub async fn settled(&self, context: Value) {
        let token = CancellationToken::new();
        for index in self.indexes("onSettled") {
            let call = self.call("onSettled", index, context.clone(), &token);
            if self.timeout_ms == 0 {
                let _ = call.await;
            } else {
                let _ =
                    tokio::time::timeout(std::time::Duration::from_millis(self.timeout_ms), call)
                        .await;
            }
        }
    }
}
