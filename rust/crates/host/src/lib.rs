//! Durable native execution. Subscribers observe persisted events; dropping a
//! subscriber does not cancel the task that produced them.

mod advisor;
mod agents;
mod budget;
mod chat;
mod connection_transport;
mod context_window;
mod continuation;
mod dispatch;
mod emulated_tool_call;
mod governance;
#[cfg(feature = "http")]
pub mod http;
mod mcp_configuration;
mod ports;
mod profiles;
mod project_instructions;
mod proposals;
mod provider_registry;
mod retry;
mod run_content;
mod run_profiles;
mod runner;
mod session;
mod subagents;
mod subscription;
mod teammates;

pub use advisor::*;
pub use agents::*;
pub use budget::{RoleTaskOverrides, parse_budget_limits, restore_execution_budget};
pub use chat::*;
pub use connection_transport::*;
pub use emulated_tool_call::{EMULATED_TOOL_CALL_MESSAGE, looks_like_emulated_tool_call};
pub use governance::*;
pub use mcp_configuration::*;
pub use ports::*;
pub use profiles::*;
pub use project_instructions::*;
pub use proposals::*;
pub use provider_registry::ProviderRegistry;
pub use retry::*;
pub use run_content::*;
pub use run_profiles::*;
pub use runner::*;
pub use session::*;
pub use subagents::*;
pub use subscription::*;
pub use teammates::*;

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
    /// A task admission named a kind this host registered no executor for.
    /// Admission is refused with this stable code BEFORE any row lands, so
    /// the queue never holds work this host has promised to run and never
    /// can; the store-level APIs stay unfiltered for deliberate cross-pool
    /// seeding, and the runner's claim filter keeps such rows invisible to
    /// this host either way.
    #[error("no executor registered for task kind: {0}")]
    NoExecutorForKind(String),
    #[error("execution cancelled")]
    Cancelled,
    #[error("{0}")]
    Execution(String),
    /// A named durable record does not exist; the message carries the
    /// stable code (e.g. `subagent_not_found: <id>`).
    #[error("{0}")]
    NotFound(String),
}

pub type HostResult<T> = Result<T, HostError>;

#[derive(Clone)]
pub struct ExecutionContext {
    pub task: TaskRecord,
    pub attempt_id: String,
    pub cancellation: CancellationToken,
    /// Set when the durable task records an interrupt request; the run stops
    /// at its next safe boundary and the host parks the turn.
    pub interruption: CancellationToken,
    /// Filled by the park sink when a tool asked a human; the runner then
    /// skips settling because the park already ended the attempt. The mutex
    /// also serializes lease renewal with the durable park and publication.
    pub(crate) suspension: Arc<Mutex<Option<agentkit_core::HumanRequestSpec>>>,
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

    /// AGENTKIT-88: park the RUNNING attempt on unfinished child tasks
    /// (the parent-wait mirror of the human-request park). One store
    /// transaction moves the task to `WaitingChild`, ends the attempt,
    /// and releases the lease; the runner then skips settlement because
    /// the park already ended the attempt. The task keeps owning its
    /// agent lane while waiting, and the pool slot returns immediately —
    /// waiting never spins. `false` means no child is unfinished: the
    /// caller proceeds to settle normally under the still-live lease.
    pub async fn park_on_children(&self) -> HostResult<bool> {
        // Same discipline as the approval park: hold the suspension guard
        // across the lease-releasing park so heartbeat renewal cannot see
        // a window where the lease is gone but the suspension is not yet
        // published.
        let mut suspension = self.suspension.lock().await;
        let parked = self
            .store
            .wait_for_children(self.fence.clone(), self.runtime.now_ms())
            .await?;
        if parked {
            *suspension = Some(agentkit_core::HumanRequestSpec {
                request_id: format!("childwait:{}", self.task.task_id),
                content: json!({"kind": "child_wait"}),
                content_hash: String::new(),
                expires_at: None,
            });
        }
        Ok(parked)
    }
}
