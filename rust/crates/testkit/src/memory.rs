//! In-memory `TaskRepository` mirroring the SQLite store's observable
//! behaviour (the reference); every mutating call is atomic.

use agentkit_engine::{
    AttemptRecord, ClaimInput, ClaimedTask, EventRecord, Lease, LeaseFence, MessageInput,
    RecoveryMode, RepositoryFault, RetryTask, SessionSettlement, SettleTask, SubmitTask,
    TaskRecord, TaskRepository, TaskStatus,
};
use async_trait::async_trait;
use serde_json::Value;
use std::{cmp::Reverse, sync::Arc};
use tokio::sync::Mutex;

/// A lease-less `running` task untouched this long is swept by `recover`
/// (the SQLite store's orphan grace, 10 minutes).
const ORPHAN_GRACE_MS: i64 = 10 * 60 * 1000;
const MAX_EVENT_PAGE: usize = 10_000;

/// Faults of [`InMemoryTaskRepository`]; variants mirror the store's.
#[derive(Debug, thiserror::Error)]
pub enum InMemoryRepositoryError {
    #[error("duplicate record: {0}")]
    Duplicate(String),
    #[error("record not found: {0}")]
    NotFound(String),
    #[error("lease lost for task {0}")]
    LeaseLost(String),
    #[error("invalid transition: {0}")]
    InvalidTransition(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("invalid input: {message}")]
    TaskInput {
        reason: agentkit_engine::TaskInputReason,
        message: String,
    },
    #[error("scope busy: {0}")]
    ScopeBusy(String),
    #[error("session descriptor changed while staging settlement: {0}")]
    SessionConflict(String),
}

type Result<T> = std::result::Result<T, InMemoryRepositoryError>;
use InMemoryRepositoryError as Fault;

impl RepositoryFault for InMemoryRepositoryError {
    fn is_transient(&self) -> bool {
        false
    }

    fn is_duplicate(&self) -> bool {
        matches!(self, Self::Duplicate(_))
    }

    fn is_lease_lost(&self) -> bool {
        matches!(self, Self::LeaseLost(_))
    }

    fn is_invalid_transition(&self) -> bool {
        matches!(self, Self::InvalidTransition(_))
    }

    fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound(_))
    }

    fn invalid_input(&self) -> Option<&str> {
        match self {
            Self::InvalidInput(message) | Self::TaskInput { message, .. } => Some(message),
            _ => None,
        }
    }

    fn task_input_reason(&self) -> Option<agentkit_engine::TaskInputReason> {
        match self {
            Self::TaskInput { reason, .. } => Some(*reason),
            _ => None,
        }
    }

    fn session_conflict(&self) -> Option<&str> {
        match self {
            Self::SessionConflict(scope) => Some(scope),
            _ => None,
        }
    }
}

/// The session settlement of the fake: only the kinds it would stage.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InMemorySessionSettle {
    pub staged_kinds: Vec<String>,
}

impl SessionSettlement for InMemorySessionSettle {
    fn staged_kinds(&self) -> Vec<String> {
        self.staged_kinds.clone()
    }

    fn without_promotion(self) -> Self {
        Self {
            staged_kinds: Vec::new(),
        }
    }
}

#[derive(Clone, Default)]
struct State {
    tasks: Vec<TaskRecord>,
    attempts: Vec<AttemptRecord>,
    leases: Vec<Lease>,
    events: Vec<EventRecord>,
    fencing_counter: u64,
    id_counter: u64,
    session_settlements: Vec<InMemorySessionSettle>,
}

/// A `TaskRepository` held in memory. Clones share one state.
#[derive(Clone, Default)]
pub struct InMemoryTaskRepository {
    state: Arc<Mutex<State>>,
}

impl InMemoryTaskRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every session settlement `settle_session` committed, in order.
    pub async fn session_settlements(&self) -> Vec<InMemorySessionSettle> {
        self.state.lock().await.session_settlements.clone()
    }

    /// The attempts recorded for `task_id`, oldest first.
    pub async fn attempts(&self, task_id: &str) -> Vec<AttemptRecord> {
        let state = self.state.lock().await;
        state
            .attempts
            .iter()
            .filter(|attempt| attempt.task_id == task_id)
            .cloned()
            .collect()
    }

    /// All-or-nothing: a failed operation leaves no partial change.
    async fn atomic<T>(&self, operation: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let mut guard = self.state.lock().await;
        let mut next = guard.clone();
        let out = operation(&mut next)?;
        *guard = next;
        Ok(out)
    }
}

fn checked_expiry(now: i64, ttl: i64) -> Result<i64> {
    if ttl < 0 {
        return Err(Fault::InvalidInput("lease TTL must not be negative".into()));
    }
    now.checked_add(ttl)
        .ok_or_else(|| Fault::InvalidInput("lease expiry overflow".into()))
}

fn status_name(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "running",
        TaskStatus::Interrupted => "interrupted",
        TaskStatus::WaitingApproval => "waiting_approval",
        TaskStatus::WaitingChild => "waiting_child",
        TaskStatus::Uncertain => "uncertain",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn validate_projection(task: &TaskRecord, message: &MessageInput) -> Result<()> {
    if message.chat_id != task.scope_id || message.run_id.as_deref() != Some(&task.task_id) {
        return Err(Fault::InvalidInput(
            "message scope/run does not match task".into(),
        ));
    }
    Ok(())
}

impl State {
    fn find(&self, task_id: &str) -> Option<&TaskRecord> {
        self.tasks.iter().find(|task| task.task_id == task_id)
    }

    fn required(&self, task_id: &str) -> Result<TaskRecord> {
        self.find(task_id)
            .cloned()
            .ok_or_else(|| Fault::NotFound(format!("task {task_id}")))
    }

    fn save(&mut self, task: &TaskRecord) {
        if let Some(slot) = self.tasks.iter_mut().find(|t| t.task_id == task.task_id) {
            *slot = task.clone();
        }
    }

    fn lease_of(&self, task_id: &str) -> Option<&Lease> {
        self.leases.iter().find(|lease| lease.task_id == task_id)
    }

    fn drop_lease(&mut self, task_id: &str) {
        self.leases.retain(|lease| lease.task_id != task_id);
    }

    fn fence(&self, proof: &LeaseFence, now: i64) -> Result<Lease> {
        self.lease_of(&proof.task_id)
            .filter(|lease| {
                lease.lease_token == proof.lease_token
                    && lease.fencing_token == proof.fencing_token
                    && lease.expires_at > now
            })
            .cloned()
            .ok_or_else(|| Fault::LeaseLost(proof.task_id.clone()))
    }

    fn fence_by_token(&self, task_id: &str, token: &str, now: i64) -> Result<Lease> {
        self.lease_of(task_id)
            .filter(|lease| lease.lease_token == token && lease.expires_at > now)
            .cloned()
            .ok_or_else(|| Fault::LeaseLost(task_id.to_owned()))
    }

    fn next_id(&mut self, prefix: &str) -> String {
        self.id_counter += 1;
        format!("{prefix}-{}", self.id_counter)
    }

    fn acquire(
        &mut self,
        task_id: &str,
        attempt_id: &str,
        owner_id: &str,
        now: i64,
        ttl_ms: i64,
    ) -> Result<Lease> {
        let expires_at = checked_expiry(now, ttl_ms)?;
        self.fencing_counter += 1;
        let lease = Lease {
            task_id: task_id.into(),
            attempt_id: attempt_id.into(),
            owner_id: owner_id.into(),
            lease_token: self.next_id("lease"),
            fencing_token: self.fencing_counter,
            expires_at,
        };
        match self.leases.iter_mut().find(|l| l.task_id == task_id) {
            Some(slot) => *slot = lease.clone(),
            None => self.leases.push(lease.clone()),
        }
        Ok(lease)
    }

    fn end_attempt(&mut self, attempt_id: &str, status: &str, now: i64, error: Option<String>) {
        if let Some(attempt) = self
            .attempts
            .iter_mut()
            .find(|a| a.attempt_id == attempt_id && a.status == "running")
        {
            attempt.status = status.into();
            attempt.ended_at = Some(now);
            attempt.error = error;
        }
    }

    fn end_running_attempts(&mut self, task_id: &str, now: i64) {
        for attempt in self
            .attempts
            .iter_mut()
            .filter(|a| a.task_id == task_id && a.status == "running")
        {
            attempt.status = "abandoned".into();
            attempt.ended_at = Some(now);
            attempt.error = Some("lease_expired".into());
        }
    }

    fn dead_letter(&mut self, task: &mut TaskRecord, now: i64, reason: &str) {
        task.status = TaskStatus::Failed;
        task.retry_pending = false;
        task.finished_at = Some(now);
        task.dead_lettered_at = Some(now);
        task.dead_letter_reason = Some(reason.into());
        task.error = Some(reason.into());
        self.save(task);
    }

    fn unfinished_children(&self, task_id: &str) -> bool {
        self.tasks.iter().any(|task| {
            task.parent_task_id.as_deref() == Some(task_id) && !task.status.is_terminal()
        })
    }

    fn insert(&mut self, input: SubmitTask) -> Result<TaskRecord> {
        if self.find(&input.task_id).is_some() {
            return Err(Fault::Duplicate(input.task_id));
        }
        if input.task_id.is_empty() || input.scope_id.is_empty() || input.kind.is_empty() {
            return Err(Fault::InvalidInput(
                "task id, scope, and kind must be nonempty".into(),
            ));
        }
        if input.exclusive_scope
            && self
                .tasks
                .iter()
                .any(|task| task.scope_id == input.scope_id && !task.status.is_terminal())
        {
            return Err(Fault::ScopeBusy(input.scope_id));
        }
        for dependency in input.depends_on.iter().chain(input.parent_task_id.iter()) {
            if self.find(dependency).is_none() {
                return Err(Fault::TaskInput {
                    reason: agentkit_engine::TaskInputReason::UnknownDependency,
                    message: format!("unknown dependency: {dependency}"),
                });
            }
        }
        let task = TaskRecord {
            task_id: input.task_id,
            kind: input.kind,
            scope_id: input.scope_id,
            status: TaskStatus::Queued,
            payload: input.payload,
            priority: input.priority,
            enqueued_at: input.now,
            available_at: input.available_at.unwrap_or(input.now),
            started_at: None,
            finished_at: None,
            depends_on: input.depends_on,
            parent_task_id: input.parent_task_id,
            attempt_count: 0,
            poison_count: 0,
            dead_lettered_at: None,
            dead_letter_reason: None,
            error: None,
            progress: None,
            cancel_requested: false,
            runtime_state: None,
            retry_pending: false,
            interrupt_requested: false,
            agent_id: None,
            agent_revision: None,
            admission_key: None,
            run_profile: None,
        };
        self.tasks.push(task.clone());
        Ok(task)
    }

    fn last_seq(&self, task_id: &str) -> Result<f64> {
        self.required(task_id)?;
        Ok(self
            .events
            .iter()
            .filter(|record| record.task_id == task_id)
            .map(|record| record.seq)
            .fold(-1.0, f64::max))
    }

    fn append(&mut self, task_id: &str, events: Vec<Value>, now: i64) -> Result<Vec<EventRecord>> {
        let mut previous = self.last_seq(task_id)?;
        let mut records = Vec::with_capacity(events.len());
        for event in events {
            let seq = event
                .get("seq")
                .and_then(Value::as_f64)
                .filter(|seq| seq.is_finite() && *seq > previous)
                .ok_or_else(|| Fault::TaskInput {
                    reason: agentkit_engine::TaskInputReason::EventSequence,
                    message: format!("event seq must be greater than {previous}"),
                })?;
            let record = EventRecord {
                task_id: task_id.into(),
                seq,
                event,
                created_at: now,
            };
            previous = seq;
            self.events.push(record.clone());
            records.push(record);
        }
        Ok(records)
    }

    /// Resolves dependencies for a claim candidate; a failed or cancelled
    /// dependency settles the candidate on the spot.
    fn dependencies(&mut self, task: &mut TaskRecord, now: i64) -> bool {
        let mut blocked = false;
        for id in task.depends_on.clone() {
            let dependency = self.find(&id).cloned();
            let failed = dependency
                .as_ref()
                .is_none_or(|t| t.status == TaskStatus::Failed || t.dead_lettered_at.is_some());
            let cancelled = dependency
                .as_ref()
                .is_some_and(|t| t.status == TaskStatus::Cancelled);
            if failed || cancelled {
                task.status = if cancelled && !failed {
                    TaskStatus::Cancelled
                } else {
                    TaskStatus::Failed
                };
                task.error = failed.then(|| format!("dependency_failed: {id}"));
                task.finished_at = Some(now);
                task.retry_pending = false;
                self.save(task);
                return false;
            }
            blocked |= dependency.is_some_and(|t| t.status != TaskStatus::Completed);
        }
        !blocked
    }

    fn start(&mut self, mut task: TaskRecord, input: &ClaimInput) -> Result<ClaimedTask> {
        task.status = TaskStatus::Running;
        task.retry_pending = false;
        task.interrupt_requested = false;
        task.attempt_count += 1;
        task.started_at = Some(input.now);
        let attempt = AttemptRecord {
            attempt_id: self.next_id("attempt"),
            task_id: task.task_id.clone(),
            attempt_number: task.attempt_count,
            status: "running".into(),
            owner_id: input.owner_id.clone(),
            started_at: input.now,
            ended_at: None,
            error: None,
        };
        let lease = self.acquire(
            &task.task_id,
            &attempt.attempt_id,
            &input.owner_id,
            input.now,
            input.ttl_ms,
        )?;
        self.attempts.push(attempt);
        self.save(&task);
        Ok(ClaimedTask { task, lease })
    }

    fn claim(&mut self, input: ClaimInput, concurrency: usize) -> Result<Option<ClaimedTask>> {
        checked_expiry(input.now, input.ttl_ms)?;
        if input.max_attempts == 0 || concurrency == 0 {
            return Err(Fault::InvalidInput(
                "max_attempts and concurrency must be positive".into(),
            ));
        }
        let mut tasks: Vec<TaskRecord> = self
            .tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.status,
                    TaskStatus::Queued
                        | TaskStatus::Running
                        | TaskStatus::WaitingApproval
                        | TaskStatus::WaitingChild
                        | TaskStatus::Uncertain
                )
            })
            .cloned()
            .collect();
        let occupied = tasks
            .iter()
            .filter(|task| {
                task.status == TaskStatus::Running
                    || task.retry_pending && task.status == TaskStatus::Queued
            })
            .count();
        tasks.sort_by_key(|task| (Reverse(task.priority), task.enqueued_at));
        let candidates = tasks.iter().filter(|t| {
            (t.status == TaskStatus::Queued || t.status == TaskStatus::Running && t.retry_pending)
                && t.available_at <= input.now
                && t.dead_lettered_at.is_none()
        });
        for mut task in candidates.cloned() {
            if !input.kinds.is_empty() && !input.kinds.contains(&task.kind) {
                continue;
            }
            if task.status == TaskStatus::Queued && !task.retry_pending && occupied >= concurrency {
                continue;
            }
            let lane_held = tasks.iter().any(|t| {
                t.task_id != task.task_id
                    && t.scope_id == task.scope_id
                    && (matches!(
                        t.status,
                        TaskStatus::Running
                            | TaskStatus::WaitingApproval
                            | TaskStatus::WaitingChild
                            | TaskStatus::Uncertain
                    ) || t.retry_pending && t.status == TaskStatus::Queued)
            });
            if input.busy_scopes.contains(&task.scope_id) || lane_held {
                continue;
            }
            if !self.dependencies(&mut task, input.now) {
                continue;
            }
            if task.attempt_count >= input.max_attempts {
                self.dead_letter(&mut task, input.now, "attempts_exhausted");
                continue;
            }
            return self.start(task, &input).map(Some);
        }
        Ok(None)
    }

    fn settle(&mut self, input: SettleTask) -> Result<TaskRecord> {
        if !input.status.is_terminal() {
            return Err(Fault::InvalidTransition(
                "settle requires terminal status".into(),
            ));
        }
        let lease = self.fence(&input.fence, input.now)?;
        let mut task = self.required(&input.fence.task_id)?;
        if !matches!(
            task.status,
            TaskStatus::Running | TaskStatus::WaitingApproval
        ) {
            return Err(Fault::InvalidTransition(format!(
                "{:?} cannot settle",
                task.status
            )));
        }
        task.status = if task.cancel_requested {
            TaskStatus::Cancelled
        } else {
            input.status
        };
        task.finished_at = Some(input.now);
        task.retry_pending = false;
        task.error = input.error;
        self.append(&task.task_id, input.events, input.now)?;
        // The fake keeps no conversation store; it only enforces the
        // projection identity the store checks.
        for message in &input.messages {
            validate_projection(&task, message)?;
        }
        self.save(&task);
        self.end_attempt(
            &lease.attempt_id,
            status_name(task.status),
            input.now,
            task.error.clone(),
        );
        self.drop_lease(&task.task_id);
        Ok(task)
    }

    fn cancel(&mut self, task_id: &str, now: i64) -> Result<TaskRecord> {
        let mut task = self.required(task_id)?;
        if !task.status.is_terminal() {
            task.cancel_requested = true;
            if matches!(
                task.status,
                TaskStatus::Queued
                    | TaskStatus::Interrupted
                    | TaskStatus::WaitingApproval
                    | TaskStatus::WaitingChild
            ) || task.retry_pending
            {
                task.status = TaskStatus::Cancelled;
                task.finished_at = Some(now);
                task.retry_pending = false;
                task.interrupt_requested = false;
            }
            self.save(&task);
        }
        Ok(task)
    }

    fn interrupt(
        &mut self,
        task_id: &str,
        reason: &str,
        lease_token: Option<String>,
        now: i64,
    ) -> Result<bool> {
        let mut task = self.required(task_id)?;
        if let Some(token) = lease_token.as_deref() {
            self.fence_by_token(task_id, token, now)?;
        }
        if !matches!(task.status, TaskStatus::Queued | TaskStatus::Running) {
            return Ok(false);
        }
        task.status = TaskStatus::Interrupted;
        task.error = Some(reason.into());
        self.save(&task);
        for attempt in self
            .attempts
            .iter_mut()
            .filter(|a| a.task_id == task_id && a.status == "running")
        {
            attempt.status = "cancelled".into();
            attempt.error = Some(reason.into());
            attempt.ended_at = Some(now);
        }
        self.drop_lease(task_id);
        Ok(true)
    }

    fn recover(
        &mut self,
        now: i64,
        max_attempts: u32,
        retry_delay_ms: i64,
        mode: RecoveryMode,
    ) -> Result<Vec<TaskRecord>> {
        if max_attempts == 0 || retry_delay_ms < 0 {
            return Err(Fault::InvalidInput("invalid recovery limits".into()));
        }
        let mut recovered = Vec::new();
        let expired: Vec<Lease> = self
            .leases
            .iter()
            .filter(|lease| lease.expires_at <= now)
            .cloned()
            .collect();
        for lease in expired {
            let mut task = self.required(&lease.task_id)?;
            if !matches!(
                task.status,
                TaskStatus::Running | TaskStatus::WaitingApproval
            ) {
                self.end_running_attempts(&task.task_id, now);
                self.drop_lease(&task.task_id);
                continue;
            }
            self.end_attempt(
                &lease.attempt_id,
                "abandoned",
                now,
                Some("lease_expired".into()),
            );
            self.abandon(&mut task, now, max_attempts, retry_delay_ms, mode)?;
            self.drop_lease(&task.task_id);
            recovered.push(task);
        }
        let orphans: Vec<TaskRecord> = self
            .tasks
            .iter()
            .filter(|task| {
                task.status == TaskStatus::Running
                    && !task.retry_pending
                    && self.lease_of(&task.task_id).is_none()
            })
            .cloned()
            .collect();
        for mut task in orphans {
            let last_activity = self
                .attempts
                .iter()
                .filter(|attempt| attempt.task_id == task.task_id)
                .map(|attempt| attempt.started_at)
                .chain(task.started_at)
                .max()
                .unwrap_or(task.enqueued_at);
            if now.saturating_sub(last_activity) <= ORPHAN_GRACE_MS {
                continue;
            }
            self.end_running_attempts(&task.task_id, now);
            self.abandon(&mut task, now, max_attempts, retry_delay_ms, mode)?;
            recovered.push(task);
        }
        Ok(recovered)
    }

    fn abandon(
        &mut self,
        task: &mut TaskRecord,
        now: i64,
        max_attempts: u32,
        delay: i64,
        mode: RecoveryMode,
    ) -> Result<()> {
        task.poison_count = task
            .poison_count
            .checked_add(1)
            .ok_or_else(|| Fault::InvalidInput("poison count overflow".into()))?;
        task.error = Some("lease_expired".into());
        if task.cancel_requested {
            task.status = TaskStatus::Cancelled;
            task.finished_at = Some(now);
        } else if mode == RecoveryMode::Manual {
            task.status = TaskStatus::Interrupted;
        } else if task.attempt_count >= max_attempts {
            self.dead_letter(task, now, "attempts_exhausted");
            return Ok(());
        } else {
            task.status = TaskStatus::Queued;
            task.available_at = now
                .checked_add(delay)
                .ok_or_else(|| Fault::InvalidInput("retry time overflow".into()))?;
        }
        self.save(task);
        Ok(())
    }

    fn retry(&mut self, input: RetryTask) -> Result<TaskRecord> {
        if input.max_attempts == 0 || input.available_at < input.now {
            return Err(Fault::InvalidInput(
                "invalid retry limits or deadline".into(),
            ));
        }
        let lease = self.fence(&input.fence, input.now)?;
        let mut task = self.required(&input.fence.task_id)?;
        if task.status != TaskStatus::Running {
            return Err(Fault::InvalidTransition(
                "retry requires running task".into(),
            ));
        }
        let attempt_status = if task.cancel_requested {
            task.status = TaskStatus::Cancelled;
            task.finished_at = Some(input.now);
            task.error = None;
            self.save(&task);
            "cancelled"
        } else if task.attempt_count >= input.max_attempts {
            self.dead_letter(&mut task, input.now, &input.error);
            "failed"
        } else {
            task.available_at = input.available_at;
            task.retry_pending = true;
            task.error = Some(input.error.clone());
            self.save(&task);
            "failed"
        };
        self.end_attempt(
            &lease.attempt_id,
            attempt_status,
            input.now,
            Some(input.error),
        );
        self.drop_lease(&task.task_id);
        Ok(task)
    }

    fn park_interrupted(&mut self, fence: &LeaseFence, now: i64) -> Result<()> {
        let lease = self.fence(fence, now)?;
        let mut task = self.required(&fence.task_id)?;
        if task.status != TaskStatus::Running {
            return Err(Fault::InvalidTransition(format!(
                "Task {} cannot move running -> interrupted: its status is {:?}.",
                task.task_id, task.status
            )));
        }
        task.status = TaskStatus::Interrupted;
        task.interrupt_requested = false;
        self.save(&task);
        self.end_attempt(&lease.attempt_id, "interrupted", now, None);
        self.drop_lease(&task.task_id);
        Ok(())
    }

    fn wait_for_children(&mut self, proof: &LeaseFence, now: i64) -> Result<bool> {
        let lease = self.fence(proof, now)?;
        let mut task = self.required(&proof.task_id)?;
        if task.status != TaskStatus::Running || task.cancel_requested {
            return Err(Fault::LeaseLost(proof.task_id.clone()));
        }
        if !self.unfinished_children(&task.task_id) {
            return Ok(false);
        }
        task.status = TaskStatus::WaitingChild;
        self.save(&task);
        self.end_attempt(&lease.attempt_id, "completed", now, None);
        self.drop_lease(&task.task_id);
        Ok(true)
    }
}

#[async_trait]
impl TaskRepository for InMemoryTaskRepository {
    type Error = InMemoryRepositoryError;
    type SessionSettle = InMemorySessionSettle;

    async fn submit(&self, input: SubmitTask) -> Result<TaskRecord> {
        self.atomic(|state| state.insert(input)).await
    }

    async fn get_task(&self, task_id: &str) -> Result<Option<TaskRecord>> {
        Ok(self.state.lock().await.find(task_id).cloned())
    }

    async fn list_children(&self, task_id: &str) -> Result<Vec<TaskRecord>> {
        let state = self.state.lock().await;
        let mut children: Vec<TaskRecord> = state
            .tasks
            .iter()
            .filter(|task| task.parent_task_id.as_deref() == Some(task_id))
            .cloned()
            .collect();
        children.sort_by_key(|task| (task.enqueued_at, task.task_id.clone()));
        Ok(children)
    }

    async fn interrupt_queued(&self, before: i64) -> Result<usize> {
        self.atomic(|state| {
            let candidates: Vec<TaskRecord> = state
                .tasks
                .iter()
                .filter(|task| {
                    (task.status == TaskStatus::Queued || task.retry_pending)
                        && task.enqueued_at < before
                })
                .cloned()
                .collect();
            for mut task in candidates.iter().cloned() {
                task.status = TaskStatus::Interrupted;
                state.save(&task);
            }
            Ok(candidates.len())
        })
        .await
    }

    async fn interrupt_task(
        &self,
        task_id: &str,
        reason: &str,
        lease_token: Option<String>,
        now: i64,
    ) -> Result<bool> {
        self.atomic(|state| state.interrupt(task_id, reason, lease_token, now))
            .await
    }

    async fn cancel_task(&self, task_id: &str, now: i64) -> Result<TaskRecord> {
        self.atomic(|state| state.cancel(task_id, now)).await
    }

    async fn resume_task(&self, task_id: &str, now: i64) -> Result<TaskRecord> {
        self.atomic(|state| {
            let mut task = state.required(task_id)?;
            if task.status != TaskStatus::Interrupted {
                return Err(Fault::InvalidTransition(
                    "only interrupted tasks may resume".into(),
                ));
            }
            task.status = TaskStatus::Queued;
            task.available_at = now;
            state.save(&task);
            Ok(task)
        })
        .await
    }

    async fn recover(
        &self,
        now: i64,
        max_attempts: u32,
        retry_delay_ms: i64,
        mode: RecoveryMode,
    ) -> Result<Vec<TaskRecord>> {
        self.atomic(|state| state.recover(now, max_attempts, retry_delay_ms, mode))
            .await
    }

    async fn claim_with_concurrency(
        &self,
        input: ClaimInput,
        concurrency: usize,
    ) -> Result<Option<ClaimedTask>> {
        self.atomic(|state| state.claim(input, concurrency)).await
    }

    async fn renew(&self, proof: LeaseFence, now: i64, ttl_ms: i64) -> Result<Lease> {
        self.atomic(|state| {
            let mut lease = state.fence(&proof, now)?;
            lease.expires_at = checked_expiry(now, ttl_ms)?;
            if let Some(slot) = state.leases.iter_mut().find(|l| l.task_id == lease.task_id) {
                *slot = lease.clone();
            }
            Ok(lease)
        })
        .await
    }

    async fn park_interrupted(&self, fence: LeaseFence, now: i64) -> Result<()> {
        self.atomic(|state| state.park_interrupted(&fence, now))
            .await
    }

    async fn settle(&self, input: SettleTask) -> Result<TaskRecord> {
        self.atomic(|state| state.settle(input)).await
    }

    async fn settle_session(
        &self,
        input: SettleTask,
        extra: InMemorySessionSettle,
    ) -> Result<TaskRecord> {
        self.atomic(|state| {
            let task = state.settle(input)?;
            state.session_settlements.push(extra);
            Ok(task)
        })
        .await
    }

    async fn retry_task(&self, input: RetryTask) -> Result<TaskRecord> {
        self.atomic(|state| state.retry(input)).await
    }

    async fn submit_child(
        &self,
        proof: LeaseFence,
        mut input: SubmitTask,
        now: i64,
    ) -> Result<TaskRecord> {
        self.atomic(|state| {
            state.fence(&proof, now)?;
            let parent = state.required(&proof.task_id)?;
            if parent.status != TaskStatus::Running || parent.cancel_requested {
                return Err(Fault::InvalidTransition(
                    "parent task is not accepting children".into(),
                ));
            }
            if input.parent_task_id.as_deref() != Some(&proof.task_id) {
                return Err(Fault::InvalidInput(
                    "child parent does not match current lease task".into(),
                ));
            }
            input.now = now;
            state.insert(input)
        })
        .await
    }

    async fn update_progress(&self, proof: LeaseFence, progress: Value, now: i64) -> Result<()> {
        self.atomic(|state| {
            state.fence(&proof, now)?;
            let mut task = state.required(&proof.task_id)?;
            task.progress = Some(progress);
            state.save(&task);
            Ok(())
        })
        .await
    }

    async fn next_seq(&self, task_id: &str) -> Result<u64> {
        let sequence = self.state.lock().await.last_seq(task_id)? + 1.0;
        if sequence.fract() != 0.0 || !(0.0..=9_007_199_254_740_991.0).contains(&sequence) {
            return Err(Fault::InvalidInput(
                "generated run requires a safe integral sequence".into(),
            ));
        }
        Ok(sequence as u64)
    }

    async fn append_events(
        &self,
        proof: LeaseFence,
        events: Vec<Value>,
        now: i64,
    ) -> Result<Vec<EventRecord>> {
        self.atomic(|state| {
            state.fence(&proof, now)?;
            state.append(&proof.task_id, events, now)
        })
        .await
    }

    async fn events_after_number(
        &self,
        task_id: &str,
        after_seq: Option<f64>,
        limit: usize,
    ) -> Result<Vec<EventRecord>> {
        if after_seq.is_some_and(|sequence| !sequence.is_finite()) {
            return Err(Fault::InvalidInput("event cursor must be finite".into()));
        }
        let state = self.state.lock().await;
        state.required(task_id)?;
        let mut page: Vec<EventRecord> = state
            .events
            .iter()
            .filter(|record| {
                record.task_id == task_id && after_seq.is_none_or(|after| record.seq > after)
            })
            .cloned()
            .collect();
        page.sort_by(|a, b| a.seq.total_cmp(&b.seq));
        page.truncate(limit.min(MAX_EVENT_PAGE));
        Ok(page)
    }

    async fn wait_for_children(&self, proof: LeaseFence, now: i64) -> Result<bool> {
        self.atomic(|state| state.wait_for_children(&proof, now))
            .await
    }

    async fn check_dispatch(&self, proof: LeaseFence, now: i64) -> Result<()> {
        let state = self.state.lock().await;
        state.fence(&proof, now)?;
        let task = state.required(&proof.task_id)?;
        if task.status != TaskStatus::Running || task.cancel_requested {
            return Err(Fault::LeaseLost(proof.task_id));
        }
        Ok(())
    }
}
