//! Port-shaped task operations.
//!
//! These methods mirror the `TaskStore` port surface the native binding
//! exposes: they take the caller's facts (ids, tokens, instants) and own the
//! store-side guarantees — transition legality, first-terminal-wins attempt
//! ends, poison counting on abandoned attempts, lease token checks, and the
//! busy-scope refusal before a scope delete. Each runs as one actor job and
//! one atomic transaction, so it flattens into an open transaction scope
//! exactly like every other engine write.

use super::*;

/// The shared `TASK_TRANSITIONS` table from the port contract; the store is
/// the authority, so an illegal edge is refused here rather than trusted to
/// a caller that already checked.
fn allowed(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;
    matches!(
        (from, to),
        (Queued, Running | Cancelled | Failed | Interrupted)
            | (Interrupted, Queued | Cancelled)
            | (
                Running,
                Interrupted | WaitingApproval | Completed | Failed | Cancelled
            )
            | (
                WaitingApproval,
                Running | Queued | Completed | Failed | Cancelled
            )
    )
}

/// The port hands fenced writers only the lease token; the fencing token
/// lives in the stored lease, so the check reads the current lease and
/// compares — same facts `fence` enforces, addressed by token.
fn fence_by_token(db: &Connection, task_id: &str, lease_token: &str, now: i64) -> Result<Lease> {
    let lease: Option<Lease> = get(db, "leases", "task_id", task_id)?;
    lease
        .filter(|lease| lease.lease_token == lease_token && lease.expires_at > now)
        .ok_or_else(|| StoreError::LeaseLost(task_id.to_owned()))
}

fn lease_by_token(db: &Connection, lease_token: &str) -> Result<Option<Lease>> {
    Ok(select::<Lease>(
        db,
        "SELECT record FROM leases WHERE json_extract(record,'$.leaseToken')=? LIMIT 1",
        [lease_token],
    )?
    .into_iter()
    .next())
}

impl SqliteStore {
    /// Compare-and-set a task status against the shared transition table,
    /// applying the patch in the same write. With a lease token the write is
    /// fenced: a stale token loses before anything changes.
    pub async fn transition_task(
        &self,
        task_id: &str,
        from: Vec<TaskStatus>,
        to: TaskStatus,
        patch: TaskTransitionPatch,
        lease_token: Option<String>,
        now: i64,
    ) -> Result<TaskRecord> {
        let (task_id, from, lease_token) = (task_id.to_owned(), from, lease_token);
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let mut task: TaskRecord = required(&transaction, "tasks", "id", &task_id)?;
            if let Some(token) = lease_token.as_deref() {
                fence_by_token(&transaction, &task_id, token, now)?;
            }
            if !from.contains(&task.status) || !allowed(task.status, to) {
                return Err(StoreError::InvalidTransition(format!(
                    "task {task_id} is {:?}, expected one of {from:?} to {to:?}",
                    task.status
                )));
            }
            task.status = to;
            if patch.started_at.is_some() {
                task.started_at = patch.started_at;
            }
            if patch.finished_at.is_some() {
                task.finished_at = patch.finished_at;
            }
            if patch.error.is_some() {
                task.error = patch.error;
            }
            if let Some(available_at) = patch.available_at {
                task.available_at = available_at;
            }
            if let Some(priority) = patch.priority {
                task.priority = priority;
            }
            if let Some(poison_count) = patch.poison_count {
                task.poison_count = poison_count;
            }
            if let Some(payload) = patch.payload {
                task.payload = payload;
            }
            save(&transaction, &task)?;
            // A terminal or interrupted task owns nothing: a lease left behind
            // would let a later recovery pass restate finished work as a crash.
            if to.is_terminal() || to == TaskStatus::Interrupted {
                transaction.execute("DELETE FROM leases WHERE task_id=?", [&task_id])?;
            }
            transaction.commit()?;
            Ok(task)
        })
        .await
    }

    /// Register a running attempt and bump the task's attempt count in one
    /// transaction (the claim path mints its own pair inside `claim`).
    pub async fn create_attempt(
        &self,
        task_id: &str,
        attempt_id: &str,
        owner_id: &str,
        now: i64,
    ) -> Result<AttemptRecord> {
        let (task_id, attempt_id, owner_id) = (
            task_id.to_owned(),
            attempt_id.to_owned(),
            owner_id.to_owned(),
        );
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let mut task: TaskRecord = required(&transaction, "tasks", "id", &task_id)?;
            if get::<AttemptRecord>(&transaction, "attempts", "id", &attempt_id)?.is_some() {
                return Err(StoreError::Duplicate(attempt_id));
            }
            task.attempt_count = task
                .attempt_count
                .checked_add(1)
                .ok_or_else(|| StoreError::InvalidInput("attempt count overflow".into()))?;
            let attempt = AttemptRecord {
                attempt_id,
                task_id: task.task_id.clone(),
                attempt_number: task.attempt_count,
                status: "running".into(),
                owner_id,
                started_at: now,
                ended_at: None,
                error: None,
            };
            transaction.execute(
                "INSERT INTO attempts VALUES(?,?,?)",
                params![
                    attempt.attempt_id,
                    task.task_id,
                    json(&transaction, &attempt)?
                ],
            )?;
            save(&transaction, &task)?;
            transaction.commit()?;
            Ok(attempt)
        })
        .await
    }

    /// End one attempt by id. FIRST TERMINAL WINS: an already-ended attempt
    /// is returned as it stands, so a recovery pass acting on an expired
    /// lease cannot restate a clean finish as a crash. An `abandoned` edge —
    /// and only that edge — counts as poison in the same transaction.
    pub async fn end_attempt(
        &self,
        attempt_id: &str,
        status: &str,
        error: Option<String>,
        lease_token: Option<String>,
        now: i64,
    ) -> Result<AttemptRecord> {
        let (attempt_id, status, error, lease_token) =
            (attempt_id.to_owned(), status.to_owned(), error, lease_token);
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let mut attempt: AttemptRecord = required(&transaction, "attempts", "id", &attempt_id)?;
            if let Some(token) = lease_token.as_deref() {
                fence_by_token(&transaction, &attempt.task_id, token, now)?;
            }
            if attempt.status != "running" {
                return Ok(attempt);
            }
            if !["completed", "failed", "abandoned", "cancelled"].contains(&status.as_str()) {
                return Err(StoreError::InvalidInput(format!(
                    "attempt cannot end {status:?}"
                )));
            }
            attempt.status = status.clone();
            attempt.ended_at = Some(now);
            if error.is_some() {
                attempt.error = error;
            }
            transaction.execute(
                "UPDATE attempts SET record=? WHERE id=?",
                params![json(&transaction, &attempt)?, attempt.attempt_id],
            )?;
            if status == "abandoned" {
                let mut task: TaskRecord = required(&transaction, "tasks", "id", &attempt.task_id)?;
                task.poison_count = task
                    .poison_count
                    .checked_add(1)
                    .ok_or_else(|| StoreError::InvalidInput("poison count overflow".into()))?;
                save(&transaction, &task)?;
            }
            transaction.commit()?;
            Ok(attempt)
        })
        .await
    }

    /// Force a queued or running task to `interrupted` with the caller's
    /// reason, cancelling its running attempts and dropping its lease in the
    /// same transaction. Answers whether anything moved.
    pub async fn interrupt_task(
        &self,
        task_id: &str,
        reason: &str,
        lease_token: Option<String>,
        now: i64,
    ) -> Result<bool> {
        let (task_id, reason, lease_token) = (task_id.to_owned(), reason.to_owned(), lease_token);
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let mut task: TaskRecord = required(&transaction, "tasks", "id", &task_id)?;
            if let Some(token) = lease_token.as_deref() {
                fence_by_token(&transaction, &task_id, token, now)?;
            }
            if !matches!(task.status, TaskStatus::Queued | TaskStatus::Running) {
                return Ok(false);
            }
            task.status = TaskStatus::Interrupted;
            task.error = Some(reason.clone());
            save(&transaction, &task)?;
            for mut attempt in all::<AttemptRecord>(&transaction, "attempts")?
                .into_iter()
                .filter(|attempt| attempt.task_id == task_id && attempt.status == "running")
            {
                attempt.status = "cancelled".into();
                attempt.error = Some(reason.clone());
                attempt.ended_at = Some(now);
                transaction.execute(
                    "UPDATE attempts SET record=? WHERE id=?",
                    params![json(&transaction, &attempt)?, attempt.attempt_id],
                )?;
            }
            transaction.execute("DELETE FROM leases WHERE task_id=?", [&task_id])?;
            transaction.commit()?;
            Ok(true)
        })
        .await
    }

    /// Tasks whose `parentTaskId` is `task_id` — one level, not the subtree.
    pub async fn list_children(&self, task_id: &str) -> Result<Vec<TaskRecord>> {
        let task_id = task_id.to_owned();
        self.call(move |db| {
            let mut children: Vec<TaskRecord> = select(
                db,
                "SELECT record FROM tasks WHERE json_extract(record, '$.parentTaskId')=? ORDER BY rowid",
                [&task_id],
            )?;
            children.sort_by_key(|task| (task.enqueued_at, task.task_id.clone()));
            Ok(children)
        })
        .await
    }

    /// Delete every task in a scope — with its events, leases, attempts, and
    /// lifecycle outbox rows — refusing with `ScopeBusy` while anything in
    /// the scope is running, waiting approval, or interrupted. The check and the deletes
    /// are one transaction: no caller can slip a claim between them.
    pub async fn delete_by_scope(&self, scope_id: &str) -> Result<usize> {
        let scope_id = scope_id.to_owned();
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let tasks: Vec<TaskRecord> = select(
                &transaction,
                "SELECT record FROM tasks WHERE scope=? ORDER BY rowid",
                [&scope_id],
            )?;
            if tasks.iter().any(|task| {
                matches!(
                    task.status,
                    TaskStatus::Running | TaskStatus::WaitingApproval | TaskStatus::Interrupted
                )
            }) {
                return Err(StoreError::ScopeBusy(scope_id));
            }
            let mut removed = 0;
            for task in tasks {
                transaction.execute(
                    "DELETE FROM outbox WHERE json_extract(record,'$.runId')=?",
                    [&task.task_id],
                )?;
                transaction.execute("DELETE FROM tasks WHERE id=?", [&task.task_id])?;
                removed += 1;
            }
            transaction.commit()?;
            Ok(removed)
        })
        .await
    }

    /// Mark a task dead-lettered without touching its status: the marker
    /// hides it from claims while recovery keeps the recorded outcome.
    pub async fn mark_dead_lettered(
        &self,
        task_id: &str,
        reason: &str,
        lease_token: Option<String>,
        now: i64,
    ) -> Result<TaskRecord> {
        let (task_id, reason, lease_token) = (task_id.to_owned(), reason.to_owned(), lease_token);
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let mut task: TaskRecord = required(&transaction, "tasks", "id", &task_id)?;
            if let Some(token) = lease_token.as_deref() {
                fence_by_token(&transaction, &task_id, token, now)?;
            }
            task.dead_lettered_at = Some(now);
            task.dead_letter_reason = Some(reason);
            // The lease stays: the marker is not a status change, and the live
            // attempt still lands its own terminal write under this lease
            // (port conformance "fenced terminal write"); the terminal
            // transition is what drops it.
            save(&transaction, &task)?;
            transaction.commit()?;
            Ok(task)
        })
        .await
    }

    /// Renew the current lease named by its token. A token that is not the
    /// task's current, unexpired lease is `LeaseLost`.
    pub async fn renew_lease_by_token(
        &self,
        lease_token: &str,
        now: i64,
        ttl_ms: i64,
    ) -> Result<Lease> {
        let lease_token = lease_token.to_owned();
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let mut lease: Lease = lease_by_token(&transaction, &lease_token)?
                .ok_or_else(|| StoreError::LeaseLost(lease_token.clone()))?;
            if lease.expires_at <= now {
                return Err(StoreError::LeaseLost(lease.task_id));
            }
            lease.expires_at = util::checked_expiry(now, ttl_ms)?;
            transaction.execute(
                "UPDATE leases SET record=? WHERE task_id=?",
                params![json(&transaction, &lease)?, lease.task_id],
            )?;
            transaction.commit()?;
            Ok(lease)
        })
        .await
    }

    /// Drop the lease named by its token. An unknown token proves nothing
    /// owned the task — `LeaseLost`.
    pub async fn release_lease(&self, lease_token: &str) -> Result<()> {
        let lease_token = lease_token.to_owned();
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let lease = lease_by_token(&transaction, &lease_token)?;
            let Some(lease) = lease else {
                return Err(StoreError::LeaseLost(lease_token));
            };
            transaction.execute("DELETE FROM leases WHERE task_id=?", [&lease.task_id])?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    /// Delete every lease whose expiry has passed at `now` and answer with
    /// the leases that died. Recovery owns what their tasks do next.
    pub async fn expire_stale_leases(&self, now: i64) -> Result<Vec<Lease>> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let stale: Vec<Lease> = all::<Lease>(&transaction, "leases")?
                .into_iter()
                .filter(|lease| lease.expires_at <= now)
                .collect();
            for lease in &stale {
                transaction.execute("DELETE FROM leases WHERE task_id=?", [&lease.task_id])?;
            }
            transaction.commit()?;
            Ok(stale)
        })
        .await
    }
}
