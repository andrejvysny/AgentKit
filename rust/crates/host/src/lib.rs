//! Durable native execution. Subscribers observe persisted events; dropping a
//! subscriber does not cancel the task that produced them.

mod budget;
mod chat;
mod connection_transport;
mod continuation;
mod dispatch;
mod emulated_tool_call;
mod governance;
#[cfg(feature = "http")]
pub mod http;
mod mcp_configuration;
mod ports;
mod proposals;
mod retry;
mod runner;
mod subscription;

pub use budget::{parse_budget_limits, restore_execution_budget};
pub use chat::*;
pub use connection_transport::*;
pub use emulated_tool_call::{EMULATED_TOOL_CALL_MESSAGE, looks_like_emulated_tool_call};
pub use governance::*;
pub use mcp_configuration::*;
pub use ports::*;
pub use proposals::*;
pub use retry::*;
pub use runner::*;
pub use subscription::*;

use agentkit_store::{LeaseFence, SqliteStore, StoreError, TaskRecord};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error(transparent)]
    Failure(#[from] ExecutionFailure),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("task executor not registered: {0}")]
    UnknownKind(String),
    #[error("execution cancelled")]
    Cancelled,
    #[error("{0}")]
    Execution(String),
}

pub type HostResult<T> = Result<T, HostError>;

#[derive(Clone)]
pub struct ExecutionContext {
    pub task: TaskRecord,
    pub attempt_id: String,
    pub cancellation: CancellationToken,
    pub(crate) store: SqliteStore,
    pub(crate) fence: LeaseFence,
    pub(crate) runtime: SharedRuntime,
    pub(crate) event_gate: Arc<Mutex<()>>,
}

impl ExecutionContext {
    pub async fn spawn_child(
        &self,
        mut input: agentkit_store::SubmitTask,
    ) -> HostResult<TaskRecord> {
        self.check_cancelled()?;
        input.parent_task_id = Some(self.task.task_id.clone());
        Ok(self
            .store
            .submit_child(self.fence.clone(), input, self.runtime.now_ms())
            .await?)
    }

    pub async fn report_progress(&self, progress: Value) -> HostResult<()> {
        self.check_cancelled()?;
        self.store
            .update_progress(self.fence.clone(), progress, self.runtime.now_ms())
            .await?;
        Ok(())
    }

    /// Completed external effects must be recorded even after cancellation.
    /// The store fence still rejects evidence from a replaced worker.
    pub async fn emit(&self, event_type: &str, data: Value) -> HostResult<()> {
        let _guard = self.event_gate.lock().await;
        let seq = self.store.next_seq(&self.task.task_id).await?;
        let now = self.runtime.now_ms();
        let timestamp = agentkit_store::ConversationTimestamp::from_epoch_millis(now)
            .and_then(|value| value.wire_text().map(|text| text.into_owned()))
            .map_err(|_| HostError::Execution("invalid runtime timestamp".into()))?;
        let event = json!({
            "type": event_type, "data": data, "runId": self.task.task_id,
            "seq": seq, "eventId": self.runtime.id(), "timestamp": timestamp,
            "contractVersion": "0.7.0", "attemptId": self.attempt_id
        });
        self.store
            .append_events(self.fence.clone(), vec![event], now)
            .await?;
        Ok(())
    }

    pub fn check_cancelled(&self) -> HostResult<()> {
        if self.cancellation.is_cancelled() {
            return Err(HostError::Cancelled);
        }
        Ok(())
    }
}
