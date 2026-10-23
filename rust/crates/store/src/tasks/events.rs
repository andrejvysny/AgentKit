use super::*;

fn last(db: &Connection, task_id: &str) -> Result<f64> {
    let _: TaskRecord = required(db, "tasks", "id", task_id)?;
    let sequence: Option<f64> = db.query_row(
        "SELECT MAX(seq) FROM events WHERE task_id=?",
        [task_id],
        |row| row.get(0),
    )?;
    Ok(sequence.unwrap_or(-1.0))
}

pub(crate) fn append(
    db: &Connection,
    task_id: &str,
    events: Vec<Value>,
    now: i64,
) -> Result<Vec<EventRecord>> {
    let mut previous = last(db, task_id)?;
    let mut records = Vec::with_capacity(events.len());
    for event in events {
        agentkit_contracts::validate_schema("TaskEventEnvelopeSchema", &event).map_err(
            |error| StoreError::InvalidInput(format!("invalid task event envelope: {error}")),
        )?;
        let sequence = agentkit_contracts::javascript_number(&event["seq"])
            .filter(|sequence| sequence.is_finite() && *sequence > previous)
            .ok_or_else(|| {
                StoreError::InvalidInput(format!("event seq must be greater than {previous}"))
            })?;
        let record = EventRecord {
            task_id: task_id.into(),
            seq: sequence,
            event,
            created_at: now,
        };
        insert(db, &record, now)?;
        previous = sequence;
        records.push(record);
    }
    Ok(records)
}

fn insert(db: &Connection, record: &EventRecord, now: i64) -> Result<()> {
    db.execute(
        "INSERT INTO events VALUES(?,?,?,?)",
        params![
            record.task_id,
            record.seq,
            record.event["eventId"].as_str().unwrap(),
            json(db, record)?
        ],
    )?;
    crate::data::enqueue(
        db,
        OutboxInput {
            id: None,
            topic: "task.events".into(),
            run_id: Some(record.task_id.clone()),
            payload: record.event.clone(),
            available_at: now,
        },
        now,
    )?;
    Ok(())
}

impl SqliteStore {
    /// JavaScript-number successor for the generic task-event protocol.
    pub async fn next_sequence(&self, task_id: &str) -> Result<f64> {
        let task_id = task_id.to_owned();
        self.call(move |db| Ok(last(db, &task_id)? + 1.0)).await
    }

    /// Checked convenience for native generated runs, whose emitter uses integral counters.
    pub async fn next_seq(&self, task_id: &str) -> Result<u64> {
        let sequence = self.next_sequence(task_id).await?;
        if sequence.fract() != 0.0 || !(0.0..=9_007_199_254_740_991.0).contains(&sequence) {
            return Err(StoreError::InvalidInput(
                "generated run requires a safe integral sequence".into(),
            ));
        }
        Ok(sequence as u64)
    }

    /// Integral-cursor convenience; generic callers should use events_after_number.
    pub async fn events(
        &self,
        task_id: &str,
        after_seq: Option<u64>,
        limit: usize,
    ) -> Result<Vec<EventRecord>> {
        self.events_after_number(task_id, after_seq.map(|sequence| sequence as f64), limit)
            .await
    }

    pub async fn events_after_number(
        &self,
        task_id: &str,
        after_seq: Option<f64>,
        limit: usize,
    ) -> Result<Vec<EventRecord>> {
        if after_seq.is_some_and(|sequence| !sequence.is_finite()) {
            return Err(StoreError::InvalidInput(
                "event cursor must be finite".into(),
            ));
        }
        let task_id = task_id.to_owned();
        self.call(move|db| {
            let _:TaskRecord=required(db,"tasks","id",&task_id)?;
            let mut statement=db.prepare("SELECT record FROM events WHERE task_id=? AND (? IS NULL OR seq>?) ORDER BY seq LIMIT ?")?;
            let rows=statement.query_map(params![task_id,after_seq,after_seq,limit.min(10000) as i64],|row|row.get::<_,String>(0))?;
            rows.map(|row|Ok(serde_json::from_str(&row?)?)).collect()
        }).await
    }
}
