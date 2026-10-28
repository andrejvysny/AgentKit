use crate::{
    util::{self, all, get, json, required},
    *,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json as object};

mod events;
mod lease;
mod lifecycle;
mod retry;
mod runtime_state;
pub(crate) use events::append;
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
    if get::<TaskRecord>(db, "tasks", "id", &input.task_id)?.is_some() {
        return Err(StoreError::Duplicate(input.task_id));
    }
    if input.task_id.is_empty() || input.scope_id.is_empty() || input.kind.is_empty() {
        return Err(StoreError::InvalidInput(
            "task id, scope, and kind must be nonempty".into(),
        ));
    }
    let tasks: Vec<TaskRecord> = all(db, "tasks")?;
    if input.exclusive_scope
        && tasks
            .iter()
            .any(|t| t.scope_id == input.scope_id && !t.status.is_terminal())
    {
        return Err(StoreError::ScopeBusy(input.scope_id));
    }
    for dependency in input.depends_on.iter().chain(input.parent_task_id.iter()) {
        if !tasks.iter().any(|task| task.task_id == *dependency) {
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
    };
    db.execute(
        "INSERT INTO tasks VALUES(?,?,?,?)",
        params![task.task_id, task.scope_id, "queued", json(db, &task)?],
    )?;
    lifecycle_event(db, &task, "task.queued", input.now)?;
    Ok(task)
}

impl SqliteStore {
    pub async fn submit(&self, input: SubmitTask) -> Result<TaskRecord> {
        self.call(move |db| {
            let transaction = db.transaction()?;
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
            let transaction = db.transaction()?;
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
            let transaction = db.transaction()?;
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
        self.call(move |db| {
            Ok(all::<TaskRecord>(db, "tasks")?
                .into_iter()
                .filter(|task| {
                    scope_id
                        .as_ref()
                        .is_none_or(|scope| *scope == task.scope_id)
                })
                .collect())
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
            let transaction = db.transaction()?;
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
    pub async fn claim_with_concurrency(
        &self,
        input: ClaimInput,
        concurrency: usize,
    ) -> Result<Option<ClaimedTask>> {
        self.call(move |db| {
            util::checked_expiry(input.now, input.ttl_ms)?;
            if input.max_attempts == 0 || concurrency == 0 {
                return Err(StoreError::InvalidInput(
                    "max_attempts and concurrency must be positive".into(),
                ));
            }
            let transaction = db.transaction()?;
            let mut tasks: Vec<TaskRecord> = all(&transaction, "tasks")?;
            let occupied = tasks
                .iter()
                .filter(|task| {
                    matches!(
                        task.status,
                        TaskStatus::Running | TaskStatus::WaitingApproval
                    ) || task.retry_pending && task.status == TaskStatus::Queued
                })
                .count();
            tasks.sort_by_key(|task| (std::cmp::Reverse(task.priority), task.enqueued_at));
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
                            && t.scope_id == task.scope_id
                            && (matches!(
                                t.status,
                                TaskStatus::Running | TaskStatus::WaitingApproval
                            ) || t.retry_pending && t.status == TaskStatus::Queued)
                    })
                {
                    continue;
                }
                if !dependencies(&transaction, &mut task, input.now)? {
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
