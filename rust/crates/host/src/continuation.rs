use crate::ExecutionContext;
use agentkit_core::{ContinuationStore, CoreError};
use agentkit_store::ProviderContinuationRecord;
use async_trait::async_trait;
use serde_json::Value;

pub(crate) struct DurableContinuation {
    pub context: ExecutionContext,
    pub chat_id: String,
    pub anchor: String,
}

#[async_trait]
impl ContinuationStore for DurableContinuation {
    async fn save(&self, state: Value) -> Result<(), CoreError> {
        let scope = state
            .get("scope")
            .cloned()
            .ok_or_else(|| CoreError::responses("invalid_continuation"))?;
        let now = self.context.runtime.now_ms();
        self.context
            .store
            .bind_run(
                &self.context.task.task_id,
                &self.chat_id,
                scope,
                self.context.fence.clone(),
                now,
            )
            .await
            .map_err(|_| CoreError::responses("continuation_persistence_failed"))?;
        self.context
            .store
            .put_continuation(
                ProviderContinuationRecord {
                    run_id: self.context.task.task_id.clone(),
                    anchor_message_id: self.anchor.clone(),
                    state,
                },
                self.context.fence.clone(),
                now,
            )
            .await
            .map_err(|_| CoreError::responses("continuation_persistence_failed"))?;
        Ok(())
    }
}
