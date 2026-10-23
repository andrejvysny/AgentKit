use super::*;

pub(super) fn acquire(
    db: &Connection,
    task_id: &str,
    attempt_id: &str,
    owner_id: &str,
    now: i64,
    ttl_ms: i64,
) -> Result<Lease> {
    let expires_at = util::checked_expiry(now, ttl_ms)?;
    db.execute("UPDATE counters SET value=value+1 WHERE name='fence'", [])?;
    let token: i64 = db.query_row("SELECT value FROM counters WHERE name='fence'", [], |row| {
        row.get(0)
    })?;
    let lease = Lease {
        task_id: task_id.into(),
        attempt_id: attempt_id.into(),
        owner_id: owner_id.into(),
        lease_token: util::id(),
        fencing_token: token
            .try_into()
            .map_err(|_| StoreError::InvalidInput("fencing token overflow".into()))?,
        expires_at,
    };
    db.execute(
        "INSERT INTO leases VALUES(?,?) ON CONFLICT(task_id) DO UPDATE SET record=excluded.record",
        params![task_id, json(db, &lease)?],
    )?;
    Ok(lease)
}

impl SqliteStore {
    /// Mints fresh authority, replacing any existing lease even for a terminal task.
    /// This trusted low-level operation does not perform worker admission or create an attempt.
    pub async fn acquire_lease(
        &self,
        task_id: &str,
        attempt_id: &str,
        owner_id: &str,
        now: i64,
        ttl_ms: i64,
    ) -> Result<Lease> {
        let (task_id, attempt_id, owner_id) = (
            task_id.to_owned(),
            attempt_id.to_owned(),
            owner_id.to_owned(),
        );
        self.call(move |db| {
            let transaction = db.transaction()?;
            let _: TaskRecord = required(&transaction, "tasks", "id", &task_id)?;
            let lease = acquire(&transaction, &task_id, &attempt_id, &owner_id, now, ttl_ms)?;
            transaction.commit()?;
            Ok(lease)
        })
        .await
    }
}
