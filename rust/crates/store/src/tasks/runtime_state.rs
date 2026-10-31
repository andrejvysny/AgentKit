use super::*;

impl SqliteStore {
    /// Creates lineage only while the parent still owns its execution lease.
    pub async fn submit_child(
        &self,
        proof: LeaseFence,
        mut input: SubmitTask,
        now: i64,
    ) -> Result<TaskRecord> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            fence(&transaction, &proof, now)?;
            let parent: TaskRecord = required(&transaction, "tasks", "id", &proof.task_id)?;
            if parent.status != TaskStatus::Running || parent.cancel_requested {
                return Err(StoreError::InvalidTransition(
                    "parent task is not accepting children".into(),
                ));
            }
            if input.parent_task_id.as_deref() != Some(&proof.task_id) {
                return Err(StoreError::InvalidInput(
                    "child parent does not match current lease task".into(),
                ));
            }
            input.now = now;
            let task = insert(&transaction, input)?;
            transaction.commit()?;
            Ok(task)
        })
        .await
    }

    /// Compare-and-set private execution state under the current attempt's lease.
    pub async fn checkpoint_runtime_state(
        &self,
        proof: LeaseFence,
        expected: Option<Value>,
        state: Value,
        now: i64,
    ) -> Result<TaskRecord> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            fence(&transaction, &proof, now)?;
            let mut task: TaskRecord = required(&transaction, "tasks", "id", &proof.task_id)?;
            if task.runtime_state != expected {
                return Err(StoreError::InvalidTransition(
                    "runtime state changed since checkpoint".into(),
                ));
            }
            task.runtime_state = Some(state);
            save(&transaction, &task)?;
            transaction.commit()?;
            Ok(task)
        })
        .await
    }

    /// Explicit boot-time policy. Ordinary opens never change queued work.
    pub async fn interrupt_queued(&self, before: i64) -> Result<usize> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let mut count = 0;
            for mut task in all::<TaskRecord>(&transaction, "tasks")?
                .into_iter()
                .filter(|task| {
                    (task.status == TaskStatus::Queued || task.retry_pending)
                        && task.enqueued_at < before
                })
            {
                task.status = TaskStatus::Interrupted;
                save(&transaction, &task)?;
                lifecycle_event(&transaction, &task, "task.interrupted", before)?;
                count += 1;
            }
            transaction.commit()?;
            Ok(count)
        })
        .await
    }
}
