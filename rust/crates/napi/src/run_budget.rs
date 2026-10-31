use crate::jsbridge::{JsBridge, PendingKind};
use agentkit_core::{BudgetCheckpoint, BudgetSnapshot, CancellationToken, CoreError};
use async_trait::async_trait;
use serde_json::json;
use std::sync::{Arc, Mutex};

pub struct Checkpoint {
    pub bridge: Arc<JsBridge>,
    pub cancellation: CancellationToken,
    pub previous: Mutex<BudgetSnapshot>,
}
#[async_trait]
impl BudgetCheckpoint for Checkpoint {
    async fn checkpoint(&self, state: BudgetSnapshot) -> Result<(), CoreError> {
        let counter = {
            let mut previous = self.previous.lock().unwrap_or_else(|e| e.into_inner());
            let name = if state.provider_requests != previous.provider_requests {
                "providerRequests"
            } else if state.tool_calls != previous.tool_calls {
                "toolCalls"
            } else if state.correction_passes != previous.correction_passes {
                "correctionPasses"
            } else if state.retries != previous.retries {
                "retries"
            } else {
                return Err(CoreError::new(
                    "invalid_budget",
                    "Unknown budget reservation",
                ));
            };
            *previous = state;
            name
        };
        let id = uuid::Uuid::new_v4().to_string();
        let value = self
            .bridge
            .clone()
            .request(
                PendingKind::Budget,
                &id,
                &json!({"counter":counter}),
                &self.cancellation,
            )
            .await
            .map_err(|e| CoreError::new("provider_error", &e))?;
        if let Some(error) = value.get("error") {
            return Err(CoreError::new(
                error["code"].as_str().unwrap_or("provider_error"),
                error["message"]
                    .as_str()
                    .unwrap_or("Budget checkpoint failed"),
            ));
        }
        Ok(())
    }
}
