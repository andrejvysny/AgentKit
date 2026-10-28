use serde::{Deserialize, Serialize};
use serde_json::Value;

agentkit_contracts::impl_json_via_serde!(TaskStatus);
agentkit_contracts::impl_json_via_serde!(TaskRecord);
agentkit_contracts::impl_json_via_serde!(Lease);
agentkit_contracts::impl_json_via_serde!(EventRecord);
agentkit_contracts::impl_json_via_serde!(AttemptRecord);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Interrupted,
    WaitingApproval,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitTask {
    pub task_id: String,
    pub kind: String,
    pub scope_id: String,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub payload: Value,
    pub priority: i32,
    pub now: i64,
    pub available_at: Option<i64>,
    pub depends_on: Vec<String>,
    pub parent_task_id: Option<String>,
    pub exclusive_scope: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub task_id: String,
    pub kind: String,
    pub scope_id: String,
    pub status: TaskStatus,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub payload: Value,
    pub priority: i32,
    pub enqueued_at: i64,
    pub available_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub depends_on: Vec<String>,
    pub parent_task_id: Option<String>,
    pub attempt_count: u32,
    pub poison_count: u32,
    pub dead_lettered_at: Option<i64>,
    pub dead_letter_reason: Option<String>,
    pub error: Option<String>,
    #[serde(
        default,
        deserialize_with = "agentkit_contracts::deserialize_preserving"
    )]
    pub progress: Option<Value>,
    pub cancel_requested: bool,
    #[serde(
        default,
        deserialize_with = "agentkit_contracts::deserialize_preserving"
    )]
    pub runtime_state: Option<Value>,
    #[serde(default)]
    /// Durable backoff reserves a running scope while no attempt owns a lease.
    pub retry_pending: bool,
    #[serde(default)]
    /// Durable request to interrupt the running turn at a safe boundary.
    pub interrupt_requested: bool,
}

#[derive(Clone, Debug)]
pub struct ClaimInput {
    pub owner_id: String,
    pub now: i64,
    pub ttl_ms: i64,
    pub kinds: Vec<String>,
    pub busy_scopes: Vec<String>,
    pub max_attempts: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    pub task_id: String,
    pub attempt_id: String,
    pub owner_id: String,
    pub lease_token: String,
    pub fencing_token: u64,
    pub expires_at: i64,
}

impl Lease {
    pub fn fence(&self) -> LeaseFence {
        LeaseFence {
            task_id: self.task_id.clone(),
            lease_token: self.lease_token.clone(),
            fencing_token: self.fencing_token,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseFence {
    pub task_id: String,
    pub lease_token: String,
    pub fencing_token: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimedTask {
    pub task: TaskRecord,
    pub lease: Lease,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRecord {
    pub task_id: String,
    pub seq: f64,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub event: Value,
    pub created_at: i64,
}

#[derive(Clone, Debug)]
pub struct SettleTask {
    pub fence: LeaseFence,
    pub now: i64,
    pub status: TaskStatus,
    pub error: Option<String>,
    pub events: Vec<Value>,
    pub messages: Vec<crate::MessageInput>,
}

#[derive(Clone, Debug)]
pub struct RetryTask {
    pub fence: LeaseFence,
    pub now: i64,
    pub available_at: i64,
    pub max_attempts: u32,
    pub error: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryMode {
    Manual,
    Automatic,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptRecord {
    pub attempt_id: String,
    pub task_id: String,
    pub attempt_number: u32,
    pub status: String,
    pub owner_id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub error: Option<String>,
}
