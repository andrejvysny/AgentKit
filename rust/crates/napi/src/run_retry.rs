use crate::jsbridge::{JsBridge, PendingKind};
use agentkit_core::{CancellationToken, CoreError, RetryHooks};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct Hooks {
    pub bridge: Arc<JsBridge>,
}
#[async_trait]
impl RetryHooks for Hooks {
    async fn random(&self, cancellation: &CancellationToken) -> Result<f64, CoreError> {
        let id = uuid::Uuid::new_v4().to_string();
        let value = self
            .bridge
            .clone()
            .request(
                PendingKind::Retry,
                &id,
                &json!({"operation":"random"}),
                cancellation,
            )
            .await
            .map_err(|e| CoreError::new("provider_error", &e))?;
        Ok(value.as_f64().unwrap_or(0.5))
    }
    async fn sleep(&self, ms: u64, cancellation: &CancellationToken) {
        let id = uuid::Uuid::new_v4().to_string();
        let _ = self
            .bridge
            .clone()
            .request(
                PendingKind::Retry,
                &id,
                &json!({"operation":"sleep","ms":ms}),
                cancellation,
            )
            .await;
    }
}
