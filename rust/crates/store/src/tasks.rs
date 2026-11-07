use crate::{
    util::{self, all, get, json, required, select},
    *,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json as object};

mod events;

const DEFAULT_AGING_INTERVAL_MS: i64 = 30_000;

/// Opt-in starvation valve for `claim`: `priority + min(max_bonus,
/// floor(wait / interval) * bonus)`. Identical to the TS `task-aging.ts`
/// formula; `bonus == 0` (the default) leaves plain priority order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskAging {
    pub interval_ms: i64,
    pub bonus: i64,
    pub max_bonus: i64,
}

impl Default for TaskAging {
    fn default() -> Self {
        Self {
            interval_ms: DEFAULT_AGING_INTERVAL_MS,
            bonus: 0,
            max_bonus: i64::MAX,
        }
    }
}

impl TaskAging {
    /// Refuses configurations the formula cannot honour instead of silently
    /// substituting defaults at claim time.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.bonus < 0 || self.max_bonus < 0 || self.interval_ms <= 0 {
            return Err(StoreError::InvalidInput(
                "task aging requires bonus >= 0, max_bonus >= 0, interval_ms > 0".into(),
            ));
        }
        Ok(())
    }

    pub fn effective_priority(&self, priority: i32, enqueued_at: i64, now: i64) -> i64 {
        let priority = i64::from(priority);
        if self.bonus == 0 {
            return priority;
        }
        let interval = if self.interval_ms > 0 {
            self.interval_ms
        } else {
            DEFAULT_AGING_INTERVAL_MS
        };
        let wait = now.saturating_sub(enqueued_at).max(0);
        let earned = (wait / interval).saturating_mul(self.bonus);
        priority.saturating_add(earned.min(self.max_bonus))
    }
}

mod lease;
mod lifecycle;
mod ports;
mod retry;
mod runtime_state;
pub(crate) use events::append;
pub use events::{
    MAX_TOOL_TEXT_BYTES, TaskProjection, TaskProjectionApproval, TaskProjectionUsage,
    derive_task_projection, redact_event,
};
pub(crate) use lifecycle::end_attempt;

pub(crate) fn save(db: &Connection, task: &TaskRecord) -> Result<()> {
    let status = json(db, &task.status)?.trim_matches('"').to_owned();
    db.execute(
        "UPDATE tasks SET status=?,record=? WHERE id=?",
        params![status, json(db, task)?, task.task_id],
    )?;
    Ok(())
}

pub(crate) fn fence(db: &Connection, proof: &LeaseFence, now: i64) -> Result<Lease> {
    let lease: Option<Lease> = get(db, "leases", "task_id", &proof.task_id)?;
    lease
        .filter(|lease| {
            lease.lease_token == proof.lease_token
                && lease.fencing_token == proof.fencing_token
                && lease.expires_at > now
        })
        .ok_or_else(|| StoreError::LeaseLost(proof.task_id.clone()))
}

pub(crate) fn lifecycle_event(
    db: &Connection,
    task: &TaskRecord,
    kind: &str,
    now: i64,
) -> Result<()> {
    let event =
        object!({"type":kind,"taskId":task.task_id,"status":task.status,"error":task.error});
    crate::data::enqueue(
        db,
        OutboxInput {
            id: None,
            topic: "tasks.lifecycle".into(),
            run_id: Some(task.task_id.clone()),
            payload: event,
            available_at: now,
        },
        now,
    )?;
    Ok(())
}

pub(crate) fn insert(db: &Connection, input: SubmitTask) -> Result<TaskRecord> {
    insert_bound(db, input, None)
}

/// The single task-insert path. `binding` is the AGENTKIT-88 agent
/// binding (identity, immutable revision, idempotency key); it is written
/// exactly here, exactly once, by the admission transaction. Unbound
/// submits pass `None` and land records byte-identical to pre-88 rows.
pub(crate) fn insert_bound(
    db: &Connection,
    input: SubmitTask,
    binding: Option<TaskBinding>,
) -> Result<TaskRecord> {
    if get::<TaskRecord>(db, "tasks", "id", &input.task_id)?.is_some() {
        return Err(StoreError::Duplicate(input.task_id));
    }
    if input.task_id.is_empty() || input.scope_id.is_empty() || input.kind.is_empty() {
        return Err(StoreError::InvalidInput(
            "task id, scope, and kind must be nonempty".into(),
        ));
    }
    if input.exclusive_scope {
        let busy: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE scope=? AND status NOT IN ('completed','failed','cancelled'))",
            [&input.scope_id],
            |row| row.get(0),
        )?;
        if busy {
            return Err(StoreError::ScopeBusy(input.scope_id));
        }
    }
    for dependency in input.depends_on.iter().chain(input.parent_task_id.iter()) {
        if get::<TaskRecord>(db, "tasks", "id", dependency)?.is_none() {
            return Err(StoreError::InvalidInput(format!(
                "unknown dependency: {dependency}"
            )));
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
        agent_id: binding.as_ref().map(|binding| binding.agent_id.clone()),
        agent_revision: binding.as_ref().map(|binding| binding.agent_revision),
        admission_key: binding
            .as_ref()
            .map(|binding| binding.admission_key.clone()),
        // AGENTKIT-89: the pinned effective profile rides the SAME binding
        // — one transaction, one durable row, never a second ledger.
        run_profile: binding
            .as_ref()
            .and_then(|binding| binding.run_profile.as_ref())
            .map(|profile| Box::new(profile.clone())),
    };
    db.execute(
        "INSERT INTO tasks VALUES(?,?,?,?)",
        params![task.task_id, task.scope_id, "queued", json(db, &task)?],
    )?;
    lifecycle_event(db, &task, "task.queued", input.now)?;
    Ok(task)
}

impl SqliteStore {
    /// Verify that the supplied lease still owns the task at `now`.
    pub async fn verify_fence(&self, proof: &LeaseFence, now: i64) -> Result<()> {
        let proof = proof.clone();
        self.call(move |db| fence(db, &proof, now).map(|_| ()))
            .await
    }
    pub async fn submit(&self, input: SubmitTask) -> Result<TaskRecord> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let task = insert(&transaction, input)?;
            transaction.commit()?;
            Ok(task)
        })
        .await
    }

    pub async fn submit_turn(
        &self,
        input: SubmitTask,
        messages: Vec<MessageInput>,
    ) -> Result<TaskRecord> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            if let Some(task) = get::<TaskRecord>(&transaction, "tasks", "id", &input.task_id)? {
                if task.kind != input.kind
                    || task.scope_id != input.scope_id
                    || task.payload != input.payload
                    || task.depends_on != input.depends_on
                    || task.parent_task_id != input.parent_task_id
                {
                    return Err(StoreError::InvalidInput(
                        "task replay identity mismatch".into(),
                    ));
                }
                return Ok(task);
            }
            let now = input.now;
            let task = insert(&transaction, input)?;
            for message in messages {
                if message.role == "user" && message.run_id.is_none() {
                    if message.chat_id != task.scope_id {
                        return Err(StoreError::InvalidInput(
                            "message scope does not match task".into(),
                        ));
                    }
                } else {
                    validate_projection(&task, &message)?;
                }
                crate::conversations::append(&transaction, message, now)?;
            }
            transaction.commit()?;
            Ok(task)
        })
        .await
    }

    pub async fn append_projected(
        &self,
        proof: LeaseFence,
        events: Vec<Value>,
        messages: Vec<MessageInput>,
        now: i64,
    ) -> Result<Vec<EventRecord>> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            fence(&transaction, &proof, now)?;
            let task: TaskRecord = required(&transaction, "tasks", "id", &proof.task_id)?;
            let records = append(&transaction, &proof.task_id, events, now)?;
            for message in messages {
                validate_projection(&task, &message)?;
                crate::conversations::project(&transaction, message, now)?;
            }
            transaction.commit()?;
            Ok(records)
        })
        .await
    }

    pub async fn get_task(&self, task_id: &str) -> Result<Option<TaskRecord>> {
        let task_id = task_id.to_owned();
        self.call(move |db| get(db, "tasks", "id", &task_id)).await
    }

    pub async fn list_tasks(&self, scope_id: Option<String>) -> Result<Vec<TaskRecord>> {
        self.call(move |db| match scope_id {
            Some(scope) => select(
                db,
                "SELECT record FROM tasks WHERE scope=? ORDER BY rowid",
                [&scope],
            ),
            None => all::<TaskRecord>(db, "tasks"),
        })
        .await
    }

    pub async fn append_events(
        &self,
        proof: LeaseFence,
        events: Vec<Value>,
        now: i64,
    ) -> Result<Vec<EventRecord>> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            fence(&transaction, &proof, now)?;
            let records = append(&transaction, &proof.task_id, events, now)?;
            transaction.commit()?;
            Ok(records)
        })
        .await
    }

    pub async fn claim(&self, input: ClaimInput) -> Result<Option<ClaimedTask>> {
        self.claim_with_concurrency(input, usize::MAX).await
    }

    /// Pending retries retain a concurrency slot even between attempt leases.
    /// A `WaitingApproval` task holds no slot (the approver may never answer)
    /// but still blocks its own scope.
    pub async fn claim_with_concurrency(
        &self,
        input: ClaimInput,
        concurrency: usize,
    ) -> Result<Option<ClaimedTask>> {
        let aging = self.aging;
        self.call(move |db| {
            aging.validate()?;
            util::checked_expiry(input.now, input.ttl_ms)?;
            if input.max_attempts == 0 || concurrency == 0 {
                return Err(StoreError::InvalidInput(
                    "max_attempts and concurrency must be positive".into(),
                ));
            }
            let transaction = crate::transaction::atomic(db)?;
            // Only these statuses can occupy a slot, block a lane, or be
            // claimed. `waiting_child` and `uncertain` join the scan purely
            // as lane blockers (AGENTKIT-88): they hold their agent's lane
            // so no second state-mutating attempt starts beside them, yet
            // the claimable filter below never selects them, so a parked
            // or unresolved run reserves no pool slot and never spins.
            let mut tasks: Vec<TaskRecord> = select(
                &transaction,
                "SELECT record FROM tasks WHERE status IN ('queued','running','waiting_approval','waiting_child','uncertain') ORDER BY rowid",
                [],
            )?;
            let occupied = tasks
                .iter()
                .filter(|task| {
                    task.status == TaskStatus::Running
                        || task.retry_pending && task.status == TaskStatus::Queued
                })
                .count();
            tasks.sort_by_key(|task| {
                (
                    std::cmp::Reverse(aging.effective_priority(
                        task.priority,
                        task.enqueued_at,
                        input.now,
                    )),
                    task.enqueued_at,
                )
            });
            let mut claimed = None;
            for mut task in tasks
                .iter()
                .filter(|t| {
                    (t.status == TaskStatus::Queued
                        || t.status == TaskStatus::Running && t.retry_pending)
                        && t.available_at <= input.now
                        && t.dead_lettered_at.is_none()
                })
                .cloned()
            {
                if !input.kinds.is_empty() && !input.kinds.contains(&task.kind) {
                    continue;
                }
                if task.status == TaskStatus::Queued
                    && !task.retry_pending
                    && occupied >= concurrency
                {
                    continue;
                }
                if input.busy_scopes.contains(&task.scope_id)
                    || tasks.iter().any(|t| {
                        t.task_id != task.task_id
                            && (t.scope_id == task.scope_id
                                // AGENTKIT-88: an admitted task also runs
                                // in its agent's lane — one active
                                // state-mutating attempt per persistent
                                // agent, across scopes.
                                || (task.agent_id.is_some() && t.agent_id == task.agent_id))
                            && (matches!(
                                t.status,
                                TaskStatus::Running
                                    | TaskStatus::WaitingApproval
                                    | TaskStatus::WaitingChild
                                    | TaskStatus::Uncertain
                            ) || t.retry_pending && t.status == TaskStatus::Queued)
                    })
                {
                    continue;
                }
                if !dependencies(&transaction, &mut task, input.now)? {
                    continue;
                }
                // AGENTKIT-89: revocation fences NEW dispatch even for an
                // already-pinned profile — the check runs at USE, on this
                // transaction, against the durable facts the pin points
                // at (provider registration, capability evidence, skill
                // revisions, source grants, approvals). A fenced task stays
                // in the queue untouched: nothing dispatches it and nothing
                // rewrites its pin; the reason is readable through
                // `run_profile_dispatch_refusal`.
                if crate::run_profiles::dispatch_fenced(&transaction, &task) {
                    continue;
                }
                if task.attempt_count >= input.max_attempts {
                    lifecycle::dead_letter(
                        &transaction,
                        &mut task,
                        input.now,
                        "attempts_exhausted",
                    )?;
                    continue;
                }
                claimed = Some(start(&transaction, task, &input)?);
                break;
            }
            transaction.commit()?;
            Ok(claimed)
        })
        .await
    }
}

fn dependencies(db: &Connection, task: &mut TaskRecord, now: i64) -> Result<bool> {
    let mut blocked = false;
    for id in &task.depends_on {
        let dependency: Option<TaskRecord> = get(db, "tasks", "id", id)?;
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
            save(db, task)?;
            lifecycle_event(db, task, "task.settled", now)?;
            return Ok(false);
        }
        blocked |= dependency.is_some_and(|t| t.status != TaskStatus::Completed);
    }
    Ok(!blocked)
}

fn start(db: &Connection, mut task: TaskRecord, input: &ClaimInput) -> Result<ClaimedTask> {
    task.status = TaskStatus::Running;
    task.retry_pending = false;
    // A fresh attempt starts clean: a durable interrupt request belongs to
    // the attempt that observed it, not to whatever runs next.
    task.interrupt_requested = false;
    task.attempt_count += 1;
    task.started_at = Some(input.now);
    let attempt = AttemptRecord {
        attempt_id: util::id(),
        task_id: task.task_id.clone(),
        attempt_number: task.attempt_count,
        status: "running".into(),
        owner_id: input.owner_id.clone(),
        started_at: input.now,
        ended_at: None,
        error: None,
    };
    let lease = lease::acquire(
        db,
        &task.task_id,
        &attempt.attempt_id,
        &input.owner_id,
        input.now,
        input.ttl_ms,
    )?;
    db.execute(
        "INSERT INTO attempts VALUES(?,?,?)",
        params![attempt.attempt_id, task.task_id, json(db, &attempt)?],
    )?;
    save(db, &task)?;
    lifecycle_event(db, &task, "task.started", input.now)?;
    Ok(ClaimedTask { task, lease })
}

pub(crate) fn validate_projection(task: &TaskRecord, message: &MessageInput) -> Result<()> {
    if message.chat_id != task.scope_id || message.run_id.as_deref() != Some(&task.task_id) {
        return Err(StoreError::InvalidInput(
            "message scope/run does not match task".into(),
        ));
    }
    Ok(())
}
