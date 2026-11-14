//! Durable teammate admission and run ownership (AGENTKIT-88): the
//! service boundary over the store's admission ledger, mailbox, and
//! attempt fencing — on the EXISTING [`agentkit_store`] task engine and
//! the host [`agentkit_chat::AgentHost`] runner. No second queue, no second task
//! store.
//!
//! What this layer guarantees, on top of the store primitives:
//! - **Admission is checked, not guessed.** [`TeammateService::admit`]
//!   reconciles the agent's unfinished runs FIRST (a crashed attempt is
//!   classified as `Uncertain` before new work is admitted, never
//!   silently re-run), then runs the AGENTKIT-87 admission gate
//!   (archived/deleted/unknown agents refuse BEFORE dispatch), then
//!   admits through the store's single idempotent transaction. A
//!   duplicate idempotency key returns the same logical task; a
//!   reconnecting client and a history load create nothing.
//! - **One active state-mutating task per agent.** Enforced by the
//!   store's claim path on the agent lane; the service exposes the lane
//!   facts (`tasks`, `display_state`) rather than a mutable copy that
//!   could drift.
//! - **Waiting releases capacity.** `wait_for_children` /
//!   `continue_after_children` are the parent-wait contract; a parked
//!   parent holds no pool slot and never spins, and the bounded shared
//!   pool (`HostOptions::concurrency`, the runner's existing knob) stays
//!   free for other agents.
//! - **Late results are fenced.** All fenced writes go through the
//!   store's lease check by attempt/lease generation; the service adds
//!   no bypass.

use crate::{AgentsError, AgentsResult};
use agentkit_engine::SharedRuntime;
use agentkit_store::{
    AdmitTeammateTask, AdmittedTask, AgentDisplayState, AgentInputRecord, AgentInputStatus,
    AgentTaskPage, CreateAgentMailbox, PromoteMailbox, SqliteStore, TaskRecord, TaskStatus,
    derive_agent_display_state,
};

/// A caller's admission request. `admission_key` is the idempotency key:
/// the same key from the same agent admits THE SAME logical task, even
/// across reconnects and process restarts.
#[derive(Clone, Debug)]
pub struct TeammateAdmission {
    /// Optional caller-pinned logical task id (retry of a known
    /// admission); `None` mints one.
    pub task_id: Option<String>,
    pub agent_id: String,
    pub admission_key: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub priority: i32,
    pub depends_on: Vec<String>,
    pub parent_task_id: Option<String>,
    /// AGENTKIT-89: the sealed effective run profile from
    /// [`agentkit_sessions::RunProfileService::resolve`]. It is pinned INSIDE the
    /// admission transaction — the admitted task's durable binding, not a
    /// second ledger. `None` admits without a profile (pre-89 shape).
    pub run_profile: Option<agentkit_store::PinnedRunProfile>,
}

/// The host service boundary for persistent-agent admission. Cheap to
/// clone: everything shared is the store handle and the injected
/// runtime, exactly like [`agentkit_chat::AgentDefinitionService`].
#[derive(Clone)]
pub struct TeammateService {
    store: SqliteStore,
    runtime: SharedRuntime,
}

impl TeammateService {
    /// Build the service over the host's existing store and runtime.
    pub fn with_runtime(store: SqliteStore, runtime: SharedRuntime) -> Self {
        Self { store, runtime }
    }

    /// The underlying durable store (for hosts that compose admission
    /// into their own paths).
    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    /// Admit work under an agent — the only admission path.
    ///
    /// Order is the contract: reconcile unfinished runs, check the
    /// agent lifecycle gate, then admit idempotently. Reconciliation
    /// runs before the gate so an orphaned run of an agent that was
    /// archived mid-flight is still classified; it is a store
    /// transaction that changes only provably-orphaned tasks.
    pub async fn admit(&self, input: TeammateAdmission) -> AgentsResult<AdmittedTask> {
        if input.admission_key.trim().is_empty() {
            return Err(AgentsError::InvalidAdmission(
                "admissionKey must be non-empty".into(),
            ));
        }
        if input.kind.trim().is_empty() {
            return Err(AgentsError::InvalidAdmission(
                "kind must be non-empty".into(),
            ));
        }
        // "Reconcile unfinished runs before restarting admission":
        // admission never runs beside an unclassified orphan.
        self.store
            .reconcile_agent_runs(input.agent_id.clone(), self.runtime.now_ms())
            .await?;
        // Lifecycle gate BEFORE dispatch: archived refuses, deleted and
        // unknown are absence. (The store re-checks inside the
        // admission transaction; this read is the early refusal.)
        self.store
            .check_agent_admission(input.agent_id.clone())
            .await?;
        let now = self.runtime.now_ms();
        Ok(self
            .store
            .admit_teammate_task(AdmitTeammateTask {
                task_id: input.task_id,
                agent_id: input.agent_id,
                admission_key: input.admission_key,
                kind: input.kind,
                payload: input.payload,
                priority: input.priority,
                now,
                available_at: None,
                depends_on: input.depends_on,
                parent_task_id: input.parent_task_id,
                run_profile: input.run_profile,
            })
            .await?)
    }

    /// Explicit startup reconciliation for one agent: classify orphaned
    /// runs (expired or missing lease → `Uncertain`) and land queued
    /// cancellations. Hosts call this when resuming a persistent agent
    /// before letting workers claim again; [`Self::admit`] does it per
    /// admission as well.
    pub async fn reconcile(&self, agent_id: &str) -> AgentsResult<Vec<TaskRecord>> {
        Ok(self
            .store
            .reconcile_agent_runs(agent_id.to_owned(), self.runtime.now_ms())
            .await?)
    }

    /// The agent's logical tasks, insertion-ordered. UNBOUNDED pure read;
    /// prefer [`Self::tasks_page`] for history.
    pub async fn tasks(&self, agent_id: &str) -> AgentsResult<Vec<TaskRecord>> {
        Ok(self.store.list_agent_tasks(agent_id.to_owned()).await?)
    }

    /// One newest-first page of the agent's tasks (`before` is the
    /// previous page's `next_before`; `limit` clamps to 1..=500). Pure
    /// read: a reconnecting client loads history through here.
    pub async fn tasks_page(
        &self,
        agent_id: &str,
        before: Option<i64>,
        limit: usize,
    ) -> AgentsResult<AgentTaskPage> {
        Ok(self
            .store
            .list_agent_tasks_page(agent_id.to_owned(), before, limit)
            .await?)
    }

    /// The agent's unfinished tasks (not completed/failed/cancelled).
    pub async fn unfinished_tasks(&self, agent_id: &str) -> AgentsResult<Vec<TaskRecord>> {
        Ok(self
            .store
            .list_agent_unfinished_tasks(agent_id.to_owned())
            .await?)
    }

    /// Read one logical task by id (reconnect/history load path).
    pub async fn task(&self, task_id: &str) -> AgentsResult<Option<TaskRecord>> {
        Ok(self.store.get_task(task_id).await?)
    }

    /// The agent's display state, DERIVED from its task rows on every
    /// read — there is no truth column to drift.
    pub async fn display_state(&self, agent_id: &str) -> AgentsResult<AgentDisplayState> {
        Ok(derive_agent_display_state(
            &self.unfinished_tasks(agent_id).await?,
        ))
    }

    /// Request cancellation of a logical task. A queued, parked, or
    /// backoff-parked task settles now; a running attempt observes the
    /// durable flag through the lease heartbeat (existing runner path).
    pub async fn cancel(&self, task_id: &str) -> AgentsResult<TaskRecord> {
        Ok(self
            .store
            .cancel_task(task_id, self.runtime.now_ms())
            .await?)
    }

    /// Park the RUNNING attempt named by `fence` on its unfinished
    /// children: the attempt ends, the lease (and therefore the pool
    /// slot) is released, and the task keeps owning its agent lane.
    /// `false` means there was nothing to wait for — settle normally.
    pub async fn wait_for_children(&self, fence: agentkit_store::LeaseFence) -> AgentsResult<bool> {
        Ok(self
            .store
            .wait_for_children(fence, self.runtime.now_ms())
            .await?)
    }

    /// Resolve a child wait: back to the queue for a fresh attempt on
    /// the SAME logical task.
    pub async fn continue_after_children(&self, task_id: &str) -> AgentsResult<TaskRecord> {
        Ok(self
            .store
            .continue_after_children(task_id.to_owned(), self.runtime.now_ms())
            .await?)
    }

    /// The execution path could not decide the outcome: park the fenced
    /// attempt as `Uncertain` so nothing — not a retry, not a stale
    /// duplicate worker — restates it until evidence resolves it.
    pub async fn mark_uncertain(
        &self,
        fence: agentkit_store::LeaseFence,
        reason: &str,
    ) -> AgentsResult<TaskRecord> {
        Ok(self
            .store
            .mark_uncertain(fence, self.runtime.now_ms(), reason)
            .await?)
    }

    /// Settle an `Uncertain` outcome on evidence: terminal, or back to
    /// the queue for a NEW attempt on the same logical task.
    pub async fn resolve_uncertain(
        &self,
        task_id: &str,
        to: TaskStatus,
        error: Option<String>,
    ) -> AgentsResult<TaskRecord> {
        Ok(self
            .store
            .resolve_uncertain(task_id.to_owned(), to, error, self.runtime.now_ms())
            .await?)
    }

    /// Enqueue a durable ordered input for the agent's mailbox
    /// (idempotent on `input_id`).
    pub async fn enqueue_input(
        &self,
        agent_id: &str,
        input: CreateAgentMailbox,
    ) -> AgentsResult<(AgentInputRecord, bool)> {
        Ok(self
            .store
            .enqueue_agent_mailbox(agent_id.to_owned(), input)
            .await?)
    }

    /// The mailbox in durable FIFO order.
    pub async fn pending_inputs(&self, agent_id: &str) -> AgentsResult<Vec<AgentInputRecord>> {
        Ok(self
            .store
            .list_agent_mailbox(agent_id.to_owned(), Some(vec![AgentInputStatus::Pending]))
            .await?)
    }

    /// Promote the oldest pending mailbox input into a durable task,
    /// atomically (strict FIFO, CAS — two promoters never take the same
    /// input twice). `kind`/`priority` shape the admitted task; a refused
    /// admission leaves the input pending.
    pub async fn promote_next_input(
        &self,
        agent_id: &str,
        kind: &str,
        priority: i32,
    ) -> AgentsResult<Option<(AgentInputRecord, TaskRecord)>> {
        Ok(self
            .store
            .promote_next_agent_mailbox(
                agent_id.to_owned(),
                PromoteMailbox {
                    kind: kind.to_owned(),
                    priority,
                    now: self.runtime.now_ms(),
                },
            )
            .await?)
    }

    /// Cancel a still-pending mailbox input.
    pub async fn cancel_input(&self, input_id: &str) -> AgentsResult<AgentInputRecord> {
        Ok(self
            .store
            .cancel_agent_mailbox(input_id.to_owned(), self.runtime.now_ms())
            .await?)
    }
}
