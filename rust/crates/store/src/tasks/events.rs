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
    let events = stamp_identities(db, task_id, events)?;
    let mut records = Vec::with_capacity(events.len());
    for event in events {
        let sequence = agentkit_contracts::javascript_number(&event["seq"])
            .filter(|sequence| sequence.is_finite() && *sequence > previous)
            .ok_or_else(|| StoreError::TaskInput {
                reason: agentkit_engine::TaskInputReason::EventSequence,
                message: format!("event seq must be greater than {previous}"),
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

/// AGENTKIT-90: stamp the durable identity of an agent-bound task into
/// every event at the ONE append choke-point, so the identifiers are
/// HOST facts no emitter can omit or forge — the same reasoning that
/// makes `seq` and `eventId` store-owned. Absent (or mismatched)
/// `agentId`/`taskId`/`definitionRevision`/`attemptId` are written from
/// the task row and its live lease; an unbound task (every pre-88 row)
/// and a task with no lease keep those fields ABSENT — absence is the
/// honest projection, never a fabricated identity. `status` is stamped
/// from the canonical task row only where the emitter said nothing; the
/// row remains the truth a projection reads.
fn stamp_identities(db: &Connection, task_id: &str, events: Vec<Value>) -> Result<Vec<Value>> {
    let mut events = events;
    // Cheap gate: the (task, seq) primary key was already checked by
    // `last`, and only agent-bound tasks carry identities to stamp.
    let task: TaskRecord = required(db, "tasks", "id", task_id)?;
    let (Some(agent_id), Some(agent_revision)) = (task.agent_id.clone(), task.agent_revision)
    else {
        for event in &events {
            agentkit_contracts::validate_schema("TaskEventEnvelopeSchema", event).map_err(
                |error| StoreError::InvalidInput(format!("invalid task event envelope: {error}")),
            )?;
        }
        return Ok(events);
    };
    let attempt_id = get::<Lease>(db, "leases", "task_id", task_id)?.map(|lease| lease.attempt_id);
    let status = serde_json::to_value(task.status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned));
    for event in &mut events {
        agentkit_contracts::validate_schema("TaskEventEnvelopeSchema", event).map_err(|error| {
            StoreError::InvalidInput(format!("invalid task event envelope: {error}"))
        })?;
        let Value::Object(map) = event else {
            continue;
        };
        map.insert("agentId".into(), serde_json::json!(agent_id));
        map.insert("taskId".into(), serde_json::json!(task_id));
        map.insert(
            "definitionRevision".into(),
            serde_json::json!(agent_revision),
        );
        if map
            .get("attemptId")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
            && let Some(attempt_id) = attempt_id.as_deref()
        {
            map.insert("attemptId".into(), serde_json::json!(attempt_id));
        }
        if map.get("status").is_none()
            && let Some(status) = status.as_deref()
        {
            map.insert("status".into(), serde_json::json!(status));
        }
    }
    Ok(events)
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

    /// The TAIL of a task's event log, ascending: the last `limit` events.
    /// Consumers that settle from the terminal facts (completion/terminal
    /// events appended after an unbounded delta stream) must read from the
    /// end — a head-bounded read would miss exactly the events that carry
    /// the outcome once a turn outgrows the page.
    pub async fn events_tail(&self, task_id: &str, limit: usize) -> Result<Vec<EventRecord>> {
        let task_id = task_id.to_owned();
        // Same ceiling as `events_after_number`; no floor: `limit == 0`
        // honestly means the empty tail.
        let limit = limit.min(10_000) as i64;
        self.call(move |db| {
            let _: TaskRecord = required(db, "tasks", "id", &task_id)?;
            let mut statement =
                db.prepare("SELECT record FROM events WHERE task_id=? ORDER BY seq DESC LIMIT ?")?;
            let rows =
                statement.query_map(params![task_id, limit], |row| row.get::<_, String>(0))?;
            let mut events = rows
                .map(|row| Ok(serde_json::from_str(&row?)?))
                .collect::<Result<Vec<EventRecord>>>()?;
            events.reverse();
            Ok(events)
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// AGENTKIT-90 — the bounded task projection read model.
//
// A projection is a DERIVED VIEW, not a second event store: it is
// recomputed from the task row (canonical status) and a BOUNDED page of
// the task's durable event log on every read, so it reconstructs
// identically after a process restart, mid-cancellation, and beside a
// live emitter. There is no truth column to drift — the same discipline
// as [`crate::derive_agent_display_state`], extended with the normalized
// attempt/approval/usage/source views the consumer surface needs.
// ---------------------------------------------------------------------------

/// The bounded usage totals. **Unknown is unknown**: a counter stays
/// `None` while no durable event ever reported a NUMBER for it — a
/// consumer must render that as "the provider did not say", never as a
/// reassuring `0`. An explicit JSON `null` reports "asked and not told"
/// and does not make the counter known.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskProjectionUsage {
    pub prompt_tokens: Option<f64>,
    pub completion_tokens: Option<f64>,
    pub total_tokens: Option<f64>,
    pub cached_prompt_tokens: Option<f64>,
    pub reasoning_tokens: Option<f64>,
    /// How many events contributed usage at all — the audit trail behind
    /// "unknown": zero usage events is a different answer from events
    /// that all refused to report numbers.
    pub usage_events: u64,
}

/// The last approval state the durable log recorded, with the `seq` of
/// the event that recorded it — the ordered proof.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskProjectionApproval {
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    pub seq: f64,
}

/// The normalized, bounded projection of one task: canonical status from
/// the row, extended views from the bounded event window.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskProjection {
    pub task: TaskRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seq: Option<f64>,
    pub event_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<TaskProjectionApproval>,
    pub usage: TaskProjectionUsage,
    pub source_refs: Vec<serde_json::Value>,
}

/// Derive the projection from the task row and an ordered, bounded page
/// of its durable events. PURE: same rows in, same projection out —
/// which is what makes "agrees with canonical host state after restart
/// and cancellation" a structural fact rather than a promise.
pub fn derive_task_projection(
    task: &TaskRecord,
    lease_attempt_id: Option<&str>,
    events: &[EventRecord],
) -> TaskProjection {
    let mut attempt_id: Option<String> = None;
    let mut approval: Option<TaskProjectionApproval> = None;
    let mut source_refs: Vec<serde_json::Value> = Vec::new();
    let mut usage_events: u64 = 0;
    // (sum, seen) per counter; `seen` only turns on for reported NUMBERS.
    let mut counters: [Option<f64>; 5] = [None; 5];
    for record in events {
        let event = &record.event;
        if let Some(id) = event.get("attemptId").and_then(Value::as_str)
            && !id.is_empty()
        {
            attempt_id = Some(id.to_owned());
        }
        if let Some(object) = event.get("approval").and_then(Value::as_object)
            && let Some(state) = object.get("state").and_then(Value::as_str)
        {
            approval = Some(TaskProjectionApproval {
                state: state.to_owned(),
                approval_id: object
                    .get("approvalId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                tool_name: object
                    .get("toolName")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                seq: record.seq,
            });
        }
        let mut contributed = false;
        let usage_object = event.get("usage").and_then(Value::as_object).or_else(|| {
            match event.get("type").and_then(Value::as_str) {
                Some("run.usage") => event.get("data").and_then(Value::as_object),
                _ => None,
            }
        });
        if let Some(object) = usage_object {
            contributed = true;
            for (index, key) in [
                "promptTokens",
                "completionTokens",
                "totalTokens",
                "cachedPromptTokens",
                "reasoningTokens",
            ]
            .into_iter()
            .enumerate()
            {
                if let Some(number) = object
                    .get(key)
                    .and_then(agentkit_contracts::javascript_number)
                    && number.is_finite()
                {
                    counters[index] = Some(counters[index].unwrap_or_default() + number);
                }
            }
        }
        let sources = event
            .get("sourceRefs")
            .and_then(Value::as_array)
            .or_else(|| {
                event
                    .get("data")
                    .and_then(Value::as_object)
                    .and_then(|data| data.get("sources"))
                    .and_then(Value::as_array)
            });
        if let Some(entries) = sources {
            for entry in entries.iter().filter_map(Value::as_object) {
                let id = entry.get("id").and_then(Value::as_str);
                let kind = entry.get("kind").and_then(Value::as_str);
                let label = entry.get("label").and_then(Value::as_str);
                if let (Some(id), Some(kind), Some(label)) = (id, kind, label)
                    && !source_refs
                        .iter()
                        .any(|kept: &serde_json::Value| kept["id"] == id && kept["kind"] == kind)
                {
                    let mut normalized = serde_json::Map::new();
                    normalized.insert("id".into(), object!(id));
                    normalized.insert("kind".into(), object!(kind));
                    normalized.insert("label".into(), object!(label));
                    for key in ["refId", "path"] {
                        if let Some(text) = entry.get(key).and_then(Value::as_str) {
                            normalized.insert(key.into(), object!(text));
                        }
                    }
                    source_refs.push(serde_json::Value::Object(normalized));
                }
            }
        }
        if contributed {
            usage_events += 1;
        }
    }
    let [
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cached_prompt_tokens,
        reasoning_tokens,
    ] = counters;
    TaskProjection {
        task: task.clone(),
        attempt_id: attempt_id.or_else(|| lease_attempt_id.map(str::to_owned)),
        last_seq: events.last().map(|record| record.seq),
        event_count: events.len(),
        approval,
        usage: TaskProjectionUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cached_prompt_tokens,
            reasoning_tokens,
            usage_events,
        },
        source_refs,
    }
}

impl SqliteStore {
    /// The bounded projection of one task: the canonical row plus a
    /// BOUNDED page of its durable events (head-first, capped — a
    /// projection must cost O(window), never O(log)). `window` is clamped
    /// to 1..=10_000; `0` means the host default of 1_000.
    pub async fn task_projection(
        &self,
        task_id: &str,
        window: usize,
    ) -> Result<Option<TaskProjection>> {
        let task_id = task_id.to_owned();
        let window = if window == 0 {
            1_000
        } else {
            window.min(10_000)
        };
        self.call(move |db| {
            let task: Option<TaskRecord> = get(db, "tasks", "id", &task_id)?;
            let Some(task) = task else {
                return Ok(None);
            };
            let lease_attempt_id =
                get::<Lease>(db, "leases", "task_id", &task_id)?.map(|lease| lease.attempt_id);
            let mut statement =
                db.prepare("SELECT record FROM events WHERE task_id=? ORDER BY seq LIMIT ?")?;
            let rows = statement.query_map(params![task_id, window as i64], |row| {
                row.get::<_, String>(0)
            })?;
            let events: Vec<EventRecord> = rows
                .map(|row| Ok(serde_json::from_str(&row?)?))
                .collect::<Result<_>>()?;
            Ok(Some(derive_task_projection(
                &task,
                lease_attempt_id.as_deref(),
                &events,
            )))
        })
        .await
    }
}

/// Keys whose VALUE is credential material the wire must never carry,
/// normalized to lowercase alphanumerics for the comparison (so
/// `api-key`, `API_KEY`, and `apiKey` all land on `apikey`). `secretRefs`
/// normalizes to `secretrefs` and deliberately survives: references are
/// not values.
fn sensitive_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        normalized.as_str(),
        "apikey"
            | "authorization"
            | "password"
            | "passwd"
            | "secret"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "clientsecret"
            | "cookie"
            | "setcookie"
            | "credential"
            | "credentials"
    )
}

/// Maps that bag request headers / process environment — both are
/// credential-shaped bags whose every string value is redacted whole.
fn bag_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    normalized == "env" || normalized == "headers"
}

/// The cap on one tool payload text field a projection will echo; longer
/// values are replaced by their bounded digest summary. Tool arguments
/// and results are the biggest thing a durable event can carry, and a
/// consumer view is no place to page a megabyte through.
pub const MAX_TOOL_TEXT_BYTES: usize = 16 * 1024;

/// Redact one durable event for a CONSUMER view (AGENTKIT-90): every
/// credential-named value becomes `"***"`, every `env`/`headers` bag is
/// redacted member-wise, and oversized tool payload texts
/// (`arguments`/`result`/`output`/`data` strings on `run.tool.*` events)
/// collapse to a `{truncated, sha256, bytes}` summary. The event's
/// identity, ordering, and normalized projection fields are untouched —
/// redaction narrows what leaves the host, never what is durable.
pub fn redact_event(event: &serde_json::Value) -> serde_json::Value {
    let tool = event
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.starts_with("run.tool"));
    fn walk(value: &serde_json::Value, tool_payload: bool) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let mut out = serde_json::Map::with_capacity(map.len());
                for (key, entry) in map {
                    let entry = if sensitive_key(key) {
                        if entry.is_null() {
                            entry.clone()
                        } else {
                            object!("***")
                        }
                    } else if bag_key(key) && entry.is_object() {
                        serde_json::Value::Object(
                            entry
                                .as_object()
                                .into_iter()
                                .flat_map(|bag| bag.iter())
                                .map(|(name, inner)| {
                                    (
                                        name.clone(),
                                        if inner.is_null() {
                                            inner.clone()
                                        } else {
                                            object!("***")
                                        },
                                    )
                                })
                                .collect(),
                        )
                    } else {
                        let payload = tool_payload
                            && matches!(key.as_str(), "arguments" | "result" | "output" | "data");
                        walk(entry, payload)
                    };
                    out.insert(key.clone(), entry);
                }
                serde_json::Value::Object(out)
            }
            serde_json::Value::Array(items) => serde_json::Value::Array(
                items.iter().map(|item| walk(item, tool_payload)).collect(),
            ),
            serde_json::Value::String(text) if tool_payload && text.len() > MAX_TOOL_TEXT_BYTES => {
                let digest = agentkit_kernel::sha256_hex(text.as_bytes());
                object!({
                    "truncated": true,
                    "sha256": digest,
                    "bytes": text.len()
                })
            }
            other => other.clone(),
        }
    }
    walk(event, tool)
}
