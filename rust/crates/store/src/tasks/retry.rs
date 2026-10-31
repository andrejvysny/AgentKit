use super::*;

impl SqliteStore {
    /// Persists a failed attempt and retry intent before releasing ownership.
    /// A running retry without a lease reserves its scope until the due claim.
    pub async fn retry_task(&self, input: RetryTask) -> Result<TaskRecord> {
        self.call(move |db| {
            if input.max_attempts == 0 || input.available_at < input.now {
                return Err(StoreError::InvalidInput(
                    "invalid retry limits or deadline".into(),
                ));
            }
            let transaction = crate::transaction::atomic(db)?;
            let lease = fence(&transaction, &input.fence, input.now)?;
            let mut task: TaskRecord = required(&transaction, "tasks", "id", &input.fence.task_id)?;
            if task.status != TaskStatus::Running {
                return Err(StoreError::InvalidTransition(
                    "retry requires running task".into(),
                ));
            }
            let attempt_status = if task.cancel_requested {
                task.status = TaskStatus::Cancelled;
                task.finished_at = Some(input.now);
                task.error = None;
                save(&transaction, &task)?;
                lifecycle_event(&transaction, &task, "task.settled", input.now)?;
                "cancelled"
            } else if task.attempt_count >= input.max_attempts {
                lifecycle::dead_letter(&transaction, &mut task, input.now, &input.error)?;
                "failed"
            } else {
                task.available_at = input.available_at;
                task.retry_pending = true;
                task.error = Some(input.error.clone());
                save(&transaction, &task)?;
                lifecycle_event(&transaction, &task, "task.retry_scheduled", input.now)?;
                "failed"
            };
            lifecycle::end_attempt(
                &transaction,
                &lease,
                attempt_status,
                input.now,
                Some(input.error),
            )?;
            transaction.execute("DELETE FROM leases WHERE task_id=?", [&task.task_id])?;
            transaction.commit()?;
            Ok(task)
        })
        .await
    }
}
