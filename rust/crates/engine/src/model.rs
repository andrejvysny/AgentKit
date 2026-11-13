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
    /// AGENTKIT-88: a durable park on unfinished child tasks. Like
    /// `WaitingApproval` it is NON-terminal and holds no execution slot
    /// (waiting must not spin or reserve the pool), but it keeps owning
    /// its agent lane until the wait resolves.
    WaitingChild,
    /// AGENTKIT-88: an attempt ended without a decidable outcome (its
    /// lease expired while running and reconciliation had not classified
    /// the result). Non-terminal, lane-owning, unclaimable: only an
    /// explicit `SqliteStore::resolve_uncertain` settles it, so
    /// a late or duplicated worker can never restate finished work.
    Uncertain,
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
    /// AGENTKIT-88: the persistent agent identity this logical task runs
    /// under, written once at admission and never mutated. `None` is an
    /// unbound task (every pre-88 row); absence is the honest projection,
    /// not a default that could drift from the admission ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// AGENTKIT-88: the IMMUTABLE definition revision pinned at admission
    /// (the `agent_config_revisions` row this run executes under). A
    /// later definition edit never moves it — the ledger resolves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_revision: Option<i64>,
    /// AGENTKIT-88: the caller's idempotency key, present exactly for
    /// admitted tasks. The durable admission ledger is authoritative;
    /// this copy is the readable receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_key: Option<String>,
    /// AGENTKIT-89: the ONE immutable effective run profile, pinned inside
    /// the same admission transaction as the binding above. `None` is an
    /// unbound or pre-89 admission. A later edit never rewrites it; the
    /// dispatch-time check re-validates what it REFERENCES, never the pin
    /// itself. Boxed: the pin is the biggest payload a task row can carry
    /// and most rows never have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_profile: Option<Box<PinnedRunProfile>>,
}

/// AGENTKIT-89: the ONE immutable effective run profile pinned to an
/// admitted task — the resolved union of host defaults, the agent
/// definition revision, the project's context selection, and the task's
/// overrides, frozen at admission with the digest that seals it. Later
/// definition or setting edits never rewrite it; revocation of anything
/// it references fences NEW dispatch (checked at use), not the pin.
/// Contains no secret material by construction: values never enter, only
/// opaque references and digests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PinnedRunProfile {
    /// sha256 over the canonical (key-sorted) serialization of
    /// `profile`; every reader recomputes it and fails closed.
    pub digest: String,
    /// The resolved profile document (grammar: `run_profile_digest`
    /// module docs). Stored verbatim so the pinned meaning survives any
    /// later edit to the services that resolved it.
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub profile: Value,
    /// Clock reading at the admission that pinned it (provenance).
    pub pinned_at: i64,
}

/// The agent binding an admitted task carries: identity plus the
/// immutable definition revision plus the idempotency key that admitted
/// it — and, for AGENTKIT-89 admissions, the pinned effective run
/// profile. Written once, inside the admission transaction, never again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskBinding {
    pub agent_id: String,
    pub agent_revision: i64,
    pub admission_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_profile: Option<PinnedRunProfile>,
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
    pub messages: Vec<MessageInput>,
}

/// Fields a `transition_task` may write alongside the status change,
/// atomically. Mirrors the TypeScript port's `TaskPatch`: absent fields keep
/// whatever the store has counted.
#[derive(Clone, Debug, Default)]
pub struct TaskTransitionPatch {
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
    pub available_at: Option<i64>,
    pub priority: Option<i32>,
    pub poison_count: Option<u32>,
    pub payload: Option<Value>,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageInput {
    pub id: Option<String>,
    pub chat_id: String,
    pub run_id: Option<String>,
    pub role: String,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub content: Value,
    pub parent_message_id: Option<String>,
    pub tool_call_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "agentkit_contracts::deserialize_preserving"
    )]
    pub tool_calls: Option<Value>,
    pub model_result_json: Option<String>,
    #[serde(
        default = "empty_metadata",
        deserialize_with = "agentkit_contracts::deserialize_preserving"
    )]
    pub metadata: Value,
    #[serde(default = "activate_default")]
    pub activate: bool,
    /// The TypeScript port distinguishes an absent `parentMessageId` (append to
    /// the active leaf) from an explicit `null` (create another root). JSON
    /// erases that difference, so the binding sets this flag for the explicit
    /// root case and `append` then ignores the active leaf.
    #[serde(default)]
    pub explicit_root: bool,
}

fn empty_metadata() -> Value {
    Value::Object(Default::default())
}

fn activate_default() -> bool {
    true
}

impl Default for MessageInput {
    fn default() -> Self {
        Self {
            id: None,
            chat_id: String::new(),
            run_id: None,
            role: "user".into(),
            content: Value::String(String::new()),
            parent_message_id: None,
            tool_call_id: None,
            tool_calls: None,
            model_result_json: None,
            metadata: serde_json::json!({}),
            activate: true,
            explicit_root: false,
        }
    }
}
