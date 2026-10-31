use crate::jsbridge::{JsBridge, PendingKind};
use agentkit_core::{CancellationToken, CoreError, SteeringItem, SteeringSource};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct Steering {
    pub bridge: Arc<JsBridge>,
    pub cancellation: CancellationToken,
}
#[async_trait]
impl SteeringSource for Steering {
    async fn pending(&self) -> Vec<SteeringItem> {
        Vec::new()
    }
    async fn pending_context(
        &self,
        run_id: &str,
        step: u32,
    ) -> Result<Vec<SteeringItem>, CoreError> {
        let id = uuid::Uuid::new_v4().to_string();
        let value = self
            .bridge
            .clone()
            .request(
                PendingKind::Steering,
                &id,
                &json!({"runId":run_id,"step":step}),
                &self.cancellation,
            )
            .await
            .map_err(|e| CoreError::new("hook_failed", &e))?;
        serde_json::from_value(value).map_err(|e| CoreError::new("hook_failed", &e.to_string()))
    }
}
