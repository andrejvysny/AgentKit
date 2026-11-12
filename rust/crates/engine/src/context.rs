use crate::{
    EngineError, EngineResult, LeaseFence, SharedRuntime, SubmitTask, TaskRecord, TaskRepository,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ExecutionContext<R: TaskRepository> {
    pub task: TaskRecord,
    pub attempt_id: String,
    pub cancellation: CancellationToken,
    /// Set when the durable task records an interrupt request; the run stops
    /// at its next safe boundary and the host parks the turn.
    pub interruption: CancellationToken,
    /// Filled by the park sink when a tool asked a human; the runner then
    /// skips settling because the park already ended the attempt. The mutex
    /// also serializes lease renewal with the durable park and publication.
    pub suspension: Arc<Mutex<Option<agentkit_core::HumanRequestSpec>>>,
    pub store: R,
    pub fence: LeaseFence,
    pub runtime: SharedRuntime,
    pub event_gate: Arc<Mutex<()>>,
}

impl<R: TaskRepository> ExecutionContext<R> {
    pub async fn spawn_child(&self, mut input: SubmitTask) -> EngineResult<TaskRecord, R::Error> {
        self.check_cancelled()?;
        input.parent_task_id = Some(self.task.task_id.clone());
        self.store
            .submit_child(self.fence.clone(), input, self.runtime.now_ms())
            .await
            .map_err(EngineError::Store)
    }

    pub async fn report_progress(&self, progress: Value) -> EngineResult<(), R::Error> {
        self.check_cancelled()?;
        self.store
            .update_progress(self.fence.clone(), progress, self.runtime.now_ms())
            .await
            .map_err(EngineError::Store)?;
        Ok(())
    }

    /// Completed external effects must be recorded even after cancellation.
    /// The store fence still rejects evidence from a replaced worker.
    pub async fn emit(&self, event_type: &str, data: Value) -> EngineResult<(), R::Error> {
        let _guard = self.event_gate.lock().await;
        let seq = self
            .store
            .next_seq(&self.task.task_id)
            .await
            .map_err(EngineError::Store)?;
        let now = self.runtime.now_ms();
        let timestamp = agentkit_kernel::ConversationTimestamp::from_epoch_millis(now)
            .and_then(|value| value.wire_text().map(|text| text.into_owned()))
            .map_err(|_| EngineError::InvalidTimestamp)?;
        let event = json!({
            "type": event_type, "data": data, "runId": self.task.task_id,
            "seq": seq, "eventId": self.runtime.id(), "timestamp": timestamp,
            "contractVersion": agentkit_contracts::CONTRACT_VERSION, "attemptId": self.attempt_id
        });
        self.store
            .append_events(self.fence.clone(), vec![event], now)
            .await
            .map_err(EngineError::Store)?;
        Ok(())
    }

    pub fn check_cancelled(&self) -> EngineResult<(), R::Error> {
        if self.cancellation.is_cancelled() {
            return Err(EngineError::Cancelled);
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
    pub async fn park_on_children(&self) -> EngineResult<bool, R::Error> {
        // Same discipline as the approval park: hold the suspension guard
        // across the lease-releasing park so heartbeat renewal cannot see
        // a window where the lease is gone but the suspension is not yet
        // published.
        let mut suspension = self.suspension.lock().await;
        let parked = self
            .store
            .wait_for_children(self.fence.clone(), self.runtime.now_ms())
            .await
            .map_err(EngineError::Store)?;
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

    /// The attempt's live lease proof. Lane-side executors use it with the
    /// store's fenced task APIs to settle durable rows under the SAME fence
    /// the runner's claim published; a replaced attempt is refused like
    /// everywhere else.
    #[must_use]
    pub fn lease_fence(&self) -> LeaseFence {
        self.fence.clone()
    }
}
