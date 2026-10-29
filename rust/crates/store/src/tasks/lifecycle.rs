use super::*;

pub(super) fn dead_letter(
    db: &Connection,
    task: &mut TaskRecord,
    now: i64,
    reason: &str,
) -> Result<()> {
    task.status = TaskStatus::Failed;
    task.retry_pending = false;
    task.finished_at = Some(now);
    task.dead_lettered_at = Some(now);
    task.dead_letter_reason = Some(reason.into());
    task.error = Some(reason.into());
    save(db, task)?;
    lifecycle_event(db, task, "task.dead_lettered", now)
}

pub(crate) fn end_attempt(
    db: &Connection,
    lease: &Lease,
    status: &str,
    now: i64,
    error: Option<String>,
) -> Result<()> {
    let mut attempt: AttemptRecord = required(db, "attempts", "id", &lease.attempt_id)?;
    if attempt.status != "running" {
        return Ok(());
    }
    attempt.status = status.into();
    attempt.ended_at = Some(now);
    attempt.error = error;
    db.execute(
        "UPDATE attempts SET record=? WHERE id=?",
        params![json(db, &attempt)?, attempt.attempt_id],
    )?;
    Ok(())
}

impl SqliteStore {
    /// Checks durable dispatch ownership immediately before an external operation.
    /// The domain must still enforce the supplied fencing token with its mutation.
    pub async fn check_dispatch(&self, proof: LeaseFence, now: i64) -> Result<()> {
        self.call(move |db| {
            fence(db, &proof, now)?;
            let task: TaskRecord = required(db, "tasks", "id", &proof.task_id)?;
            if task.status != TaskStatus::Running || task.cancel_requested {
                return Err(StoreError::LeaseLost(proof.task_id));
            }
            Ok(())
        })
        .await
    }

    pub async fn renew(&self, proof: LeaseFence, now: i64, ttl_ms: i64) -> Result<Lease> {
        self.call(move |db| {
            let transaction = db.transaction()?;
            let mut lease = fence(&transaction, &proof, now)?;
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

    pub async fn update_progress(
        &self,
        proof: LeaseFence,
        progress: Value,
        now: i64,
    ) -> Result<()> {
        self.call(move |db| {
            let transaction = db.transaction()?;
            fence(&transaction, &proof, now)?;
            let mut task: TaskRecord = required(&transaction, "tasks", "id", &proof.task_id)?;
            task.progress = Some(progress);
            save(&transaction, &task)?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    /// Event evidence, message projections, task/attempt terminal state, and lease release commit together.
    pub async fn settle(&self, input: SettleTask) -> Result<TaskRecord> {
        self.settle_inner(input, None).await
    }

    /// Session-aware settle (ADR 0016 §5): the settling turn also settles
    /// its durable inputs in the SAME transaction — leftover steers marked
    /// `cancel` are cancelled, and on a completed turn the first queued
    /// follow-up is converted into the next task, so the queue advances
    /// without a window where the follow-up is converted but the turn is not
    /// terminal (or the reverse).
    pub async fn settle_session(
        &self,
        input: SettleTask,
        extra: crate::session_ops::SessionSettle,
    ) -> Result<TaskRecord> {
        self.settle_inner(input, Some(extra)).await
    }

    async fn settle_inner(
        &self,
        input: SettleTask,
        extra: Option<crate::session_ops::SessionSettle>,
    ) -> Result<TaskRecord> {
        self.call(move |db| {
            if !input.status.is_terminal() {
                return Err(StoreError::InvalidTransition(
                    "settle requires terminal status".into(),
                ));
            }
            let transaction = db.transaction()?;
            let lease = fence(&transaction, &input.fence, input.now)?;
            let mut task: TaskRecord = required(&transaction, "tasks", "id", &input.fence.task_id)?;
            if !matches!(
                task.status,
                TaskStatus::Running | TaskStatus::WaitingApproval
            ) {
                return Err(StoreError::InvalidTransition(format!(
                    "{:?} cannot settle",
                    task.status
                )));
            }
            if input.status == TaskStatus::Completed
                && !task.cancel_requested
                && let Some(expected) = extra
                    .as_ref()
                    .and_then(|extra| extra.expected_descriptor.as_ref())
            {
                let chat: Option<ChatRecord> = get(&transaction, "chats", "id", &task.scope_id)?;
                if chat
                    .as_ref()
                    .and_then(|chat| chat.metadata.get(crate::SESSION_METADATA_KEY))
                    != Some(expected)
                {
                    return Err(StoreError::SessionDescriptorChanged(task.scope_id));
                }
            }
            task.status = if task.cancel_requested {
                TaskStatus::Cancelled
            } else {
                input.status
            };
            task.finished_at = Some(input.now);
            task.retry_pending = false;
            task.error = input.error;
            append(&transaction, &task.task_id, input.events, input.now)?;
            for message in input.messages {
                validate_projection(&task, &message)?;
                crate::conversations::project(&transaction, message, input.now)?;
            }
            save(&transaction, &task)?;
            let status = json(&transaction, &task.status)?
                .trim_matches('"')
                .to_owned();
            end_attempt(&transaction, &lease, &status, input.now, task.error.clone())?;
            lifecycle_event(&transaction, &task, "task.settled", input.now)?;
            transaction.execute("DELETE FROM leases WHERE task_id=?", [&task.task_id])?;
            if let Some(extra) = &extra {
                crate::session_ops::settle_session_inputs(
                    &transaction,
                    &task,
                    task.status == TaskStatus::Completed,
                    input.now,
                    extra,
                )?;
            }
            transaction.commit()?;
            Ok(task)
        })
        .await
    }

    /// Requests cancellation without releasing a running attempt's lease.
    pub async fn cancel_task(&self, task_id: &str, now: i64) -> Result<TaskRecord> {
        let task_id = task_id.to_owned();
        self.call(move |db| {
            let transaction = db.transaction()?;
            let mut task: TaskRecord = required(&transaction, "tasks", "id", &task_id)?;
            if !task.status.is_terminal() {
                task.cancel_requested = true;
                if matches!(
                    task.status,
                    TaskStatus::Queued | TaskStatus::Interrupted | TaskStatus::WaitingApproval
                ) || task.retry_pending
                {
                    // Nothing is running to notice: a queued, interrupted,
                    // backoff-parked, or approval-parked turn cancels now.
                    // A parked turn holds no lease, and the approver may
                    // never answer, so waiting for an answer would strand it.
                    task.status = TaskStatus::Cancelled;
                    task.finished_at = Some(now);
                    task.retry_pending = false;
                    task.interrupt_requested = false;
                    crate::session_ops::cancel_turn_requests(
                        &transaction,
                        &task.scope_id,
                        &task.task_id,
                        now,
                    )?;
                }
                save(&transaction, &task)?;
                lifecycle_event(&transaction, &task, "task.cancel_requested", now)?;
            }
            transaction.commit()?;
            Ok(task)
        })
        .await
    }

    pub async fn resume_task(&self, task_id: &str, now: i64) -> Result<TaskRecord> {
        let task_id = task_id.to_owned();
        self.call(move |db| {
            let transaction = db.transaction()?;
            let mut task: TaskRecord = required(&transaction, "tasks", "id", &task_id)?;
            if task.status != TaskStatus::Interrupted {
                return Err(StoreError::InvalidTransition(
                    "only interrupted tasks may resume".into(),
                ));
            }
            task.status = TaskStatus::Queued;
            task.available_at = now;
            save(&transaction, &task)?;
            lifecycle_event(&transaction, &task, "task.queued", now)?;
            transaction.commit()?;
            Ok(task)
        })
        .await
    }

    /// Recovery evidence and retry intent are durable before the expired lease disappears.
    pub async fn recover(
        &self,
        now: i64,
        max_attempts: u32,
        retry_delay_ms: i64,
        mode: RecoveryMode,
    ) -> Result<Vec<TaskRecord>> {
        self.call(move |db| {
            if max_attempts == 0 || retry_delay_ms < 0 {
                return Err(StoreError::InvalidInput("invalid recovery limits".into()));
            }
            let transaction = db.transaction()?;
            let leases: Vec<Lease> = all(&transaction, "leases")?;
            let mut recovered = Vec::new();
            for lease in leases.into_iter().filter(|lease| lease.expires_at <= now) {
                let mut task: TaskRecord = required(&transaction, "tasks", "id", &lease.task_id)?;
                end_attempt(
                    &transaction,
                    &lease,
                    "abandoned",
                    now,
                    Some("lease_expired".into()),
                )?;
                task.poison_count = task
                    .poison_count
                    .checked_add(1)
                    .ok_or_else(|| StoreError::InvalidInput("poison count overflow".into()))?;
                recover_task(
                    &transaction,
                    &mut task,
                    now,
                    max_attempts,
                    retry_delay_ms,
                    mode,
                )?;
                transaction.execute("DELETE FROM leases WHERE task_id=?", [&task.task_id])?;
                recovered.push(task);
            }
            transaction.commit()?;
            Ok(recovered)
        })
        .await
    }

    pub async fn attempts(&self, task_id: &str) -> Result<Vec<AttemptRecord>> {
        let task_id = task_id.to_owned();
        self.call(move |db| {
            Ok(all::<AttemptRecord>(db, "attempts")?
                .into_iter()
                .filter(|a| a.task_id == task_id)
                .collect())
        })
        .await
    }
}

fn recover_task(
    db: &Connection,
    task: &mut TaskRecord,
    now: i64,
    max_attempts: u32,
    delay: i64,
    mode: RecoveryMode,
) -> Result<()> {
    task.error = Some("lease_expired".into());
    if task.cancel_requested {
        task.status = TaskStatus::Cancelled;
        task.finished_at = Some(now);
    } else if mode == RecoveryMode::Manual {
        task.status = TaskStatus::Interrupted;
    } else if task.attempt_count >= max_attempts {
        return dead_letter(db, task, now, "attempts_exhausted");
    } else {
        task.status = TaskStatus::Queued;
        task.available_at = now
            .checked_add(delay)
            .ok_or_else(|| StoreError::InvalidInput("retry time overflow".into()))?;
    }
    save(db, task)?;
    lifecycle_event(db, task, "task.recovered", now)
}
