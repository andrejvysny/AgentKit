//! Durable agent-session inputs (ADR 0016 §2).
//!
//! Steering, follow-ups, and human requests are durable records that do not
//! become a turn immediately. Creation is idempotent on `inputId`; status
//! moves are compare-and-set against the per-kind machine, whose non-pending
//! statuses are terminal. The store-assigned `order` is the FIFO key.

use crate::{Result, SqliteStore, StoreError, util::json};
pub use agentkit_sessions::parse_iso_millis;
use rusqlite::{Connection, OptionalExtension, params};

/// The queued-input records are the sessions context's (ADR 0024 §2); this
/// adapter reads and writes them, so they are named here for the rest of the
/// crate and re-exported from the crate root.
pub use agentkit_sessions::{
    AgentInputKind, AgentInputPatch, AgentInputQuery, AgentInputRecord, AgentInputSettlePolicy,
    AgentInputStatus, CreateAgentInput,
};

/// Allowed targets from `pending`, per kind. Every non-pending status is
/// terminal, so this is the whole machine.
pub const AGENT_INPUT_TRANSITIONS: [(AgentInputKind, &[AgentInputStatus]); 3] = [
    (
        AgentInputKind::Steer,
        &[
            AgentInputStatus::Applied,
            AgentInputStatus::Converted,
            AgentInputStatus::Cancelled,
        ],
    ),
    (
        AgentInputKind::FollowUp,
        &[AgentInputStatus::Converted, AgentInputStatus::Cancelled],
    ),
    (
        AgentInputKind::HumanRequest,
        &[AgentInputStatus::Answered, AgentInputStatus::Cancelled],
    ),
];

/// Shared CAS rule; mirrors `assertAgentInputTransition`.
pub fn assert_agent_input_transition(
    record: &AgentInputRecord,
    from: &[AgentInputStatus],
    to: AgentInputStatus,
) -> Result<()> {
    if !from.contains(&record.status) {
        return Err(StoreError::AgentInputTransition(format!(
            "Input {} is {:?}, expected one of {from:?}.",
            record.input_id, record.status
        )));
    }
    let legal = record.status == AgentInputStatus::Pending
        && AGENT_INPUT_TRANSITIONS
            .iter()
            .find(|(kind, _)| *kind == record.kind)
            .is_some_and(|(_, targets)| targets.contains(&to));
    if !legal {
        return Err(StoreError::AgentInputTransition(format!(
            "Input {} ({:?}) cannot move {:?} → {to:?}.",
            record.input_id, record.kind, record.status
        )));
    }
    Ok(())
}

/// Shared create-time validation; mirrors `validateCreateAgentInput`.
pub fn validate_create_agent_input(input: &CreateAgentInput) -> Result<()> {
    let bad = |message: String| Err(StoreError::InvalidInput(message));
    for (field, value) in [
        ("inputId", Some(input.input_id.as_str())),
        ("sessionId", Some(input.session_id.as_str())),
        ("contentHash", Some(input.content_hash.as_str())),
    ] {
        if value.is_none_or(|value| value.is_empty()) {
            return bad(format!("{field} must be a non-empty string."));
        }
    }
    if let Some(policy) = input.on_settled {
        if input.kind != AgentInputKind::Steer {
            return bad("onSettled is only valid for steer.".into());
        }
        let _ = policy;
    }
    if input.kind == AgentInputKind::HumanRequest {
        for (field, value) in [
            ("attemptId", input.attempt_id.as_deref()),
            ("toolCallId", input.tool_call_id.as_deref()),
        ] {
            if value.is_none_or(|value| value.is_empty()) {
                return bad(format!("{field} is required for human_request."));
            }
        }
    } else if input.attempt_id.is_some()
        || input.tool_call_id.is_some()
        || input.expires_at.is_some()
    {
        return bad("attemptId, toolCallId and expiresAt are only valid for human_request.".into());
    }
    if input.target_task_id.as_deref().is_some_and(str::is_empty) {
        return bad("targetTaskId must be non-empty when present.".into());
    }
    if input
        .expires_at
        .as_deref()
        .is_some_and(|value| parse_iso_millis(value).is_none())
    {
        return bad("expiresAt must be a valid ISO timestamp.".into());
    }
    Ok(())
}

pub(crate) fn next_order(db: &Connection) -> Result<i64> {
    let current: i64 = db.query_row(
        "SELECT value FROM counters WHERE name='input_order'",
        [],
        |row| row.get(0),
    )?;
    let next = current
        .checked_add(1)
        .ok_or_else(|| StoreError::InvalidInput("input order overflow".into()))?;
    db.execute(
        "UPDATE counters SET value=? WHERE name='input_order'",
        params![next],
    )?;
    Ok(next)
}

pub(crate) fn insert_input(
    db: &Connection,
    input: CreateAgentInput,
    now_text: &str,
) -> Result<(AgentInputRecord, bool)> {
    validate_create_agent_input(&input)?;
    if let Some(existing) = get_input(db, &input.input_id)? {
        if existing.session_id != input.session_id
            || existing.kind != input.kind
            || existing.content_hash != input.content_hash
            // AGENTKIT-88: a session input and an agent-mailbox input are
            // different objects even under equal facts; a session-path
            // replay must never be satisfied by a mailbox row.
            || existing.agent_id.is_some()
        {
            return Err(StoreError::Duplicate(format!(
                "input_conflict: input {} already exists with different kind, session, or content.",
                input.input_id
            )));
        }
        return Ok((existing, false));
    }
    let record = AgentInputRecord {
        input_id: input.input_id,
        session_id: input.session_id,
        agent_id: None,
        kind: input.kind,
        status: AgentInputStatus::Pending,
        content: input.content,
        content_hash: input.content_hash,
        order: next_order(db)?,
        target_task_id: input.target_task_id,
        on_settled: input.on_settled,
        attempt_id: input.attempt_id,
        tool_call_id: input.tool_call_id,
        expires_at: input.expires_at,
        applied_task_id: None,
        applied_message_id: None,
        answer_id: None,
        created_at: now_text.into(),
        updated_at: now_text.into(),
    };
    db.execute(
        "INSERT INTO agent_inputs VALUES(?,?,?,?,?,?,?)",
        params![
            record.input_id,
            record.session_id,
            kind_text(record.kind),
            status_text(record.status),
            record.target_task_id,
            json(db, &record)?,
            record.agent_id,
        ],
    )?;
    Ok((record, true))
}

pub(crate) fn get_input(db: &Connection, input_id: &str) -> Result<Option<AgentInputRecord>> {
    db.query_row(
        "SELECT record FROM agent_inputs WHERE input_id=?",
        [input_id],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|text| {
        let value = agentkit_contracts::parse_javascript_json(&text)
            .map_err(|error| StoreError::RecoveryUnreadable(error.to_string()))?;
        serde_json::from_value(value).map_err(Into::into)
    })
    .transpose()
}

pub(crate) fn list_inputs(
    db: &Connection,
    query: &AgentInputQuery,
) -> Result<Vec<AgentInputRecord>> {
    let mut statement = db.prepare(
        "SELECT record FROM agent_inputs WHERE session_id=?1 \
         AND (?2 IS NULL OR target_task_id=?2) \
         AND agent_id IS NULL",
    )?;
    let rows = statement.query_map(params![query.session_id, query.target_task_id], |row| {
        row.get::<_, String>(0)
    })?;
    let mut records = vec![];
    for row in rows {
        let text = row?;
        let value = agentkit_contracts::parse_javascript_json(&text)
            .map_err(|error| StoreError::RecoveryUnreadable(error.to_string()))?;
        let record = serde_json::from_value::<AgentInputRecord>(value)?;
        if query
            .kinds
            .as_ref()
            .is_some_and(|kinds| !kinds.contains(&record.kind))
        {
            continue;
        }
        if query
            .statuses
            .as_ref()
            .is_some_and(|statuses| !statuses.contains(&record.status))
        {
            continue;
        }
        records.push(record);
    }
    records.sort_by_key(|record| record.order);
    Ok(records)
}

pub(crate) fn transition_input(
    db: &Connection,
    input_id: &str,
    from: &[AgentInputStatus],
    to: AgentInputStatus,
    patch: &AgentInputPatch,
    now_text: &str,
) -> Result<AgentInputRecord> {
    let mut record = get_input(db, input_id)?
        .ok_or_else(|| StoreError::NotFound(format!("agent input {input_id} does not exist")))?;
    assert_agent_input_transition(&record, from, to)?;
    record.status = to;
    if patch.applied_task_id.is_some() {
        record.applied_task_id = patch.applied_task_id.clone();
    }
    if patch.applied_message_id.is_some() {
        record.applied_message_id = patch.applied_message_id.clone();
    }
    if patch.answer_id.is_some() {
        record.answer_id = patch.answer_id.clone();
    }
    record.updated_at = now_text.into();
    db.execute(
        "UPDATE agent_inputs SET status=?,record=? WHERE input_id=?",
        params![status_text(to), json(db, &record)?, input_id],
    )?;
    Ok(record)
}

pub(crate) fn delete_session_inputs(db: &Connection, session_id: &str) -> Result<u64> {
    // AGENTKIT-88: teammate-mailbox rows carry `agent_id`; deleting a
    // session's inputs must never reach them, even if a caller pinned an
    // agent id equal to a chat id.
    Ok(db.execute(
        "DELETE FROM agent_inputs WHERE session_id=? AND agent_id IS NULL",
        [session_id],
    )? as u64)
}

pub(crate) fn kind_text(kind: AgentInputKind) -> &'static str {
    match kind {
        AgentInputKind::Steer => "steer",
        AgentInputKind::FollowUp => "follow_up",
        AgentInputKind::HumanRequest => "human_request",
    }
}

pub(crate) fn status_text(status: AgentInputStatus) -> &'static str {
    match status {
        AgentInputStatus::Pending => "pending",
        AgentInputStatus::Applied => "applied",
        AgentInputStatus::Converted => "converted",
        AgentInputStatus::Answered => "answered",
        AgentInputStatus::Cancelled => "cancelled",
    }
}

impl SqliteStore {
    /// Idempotent on `inputId`: the same kind, session and `contentHash`
    /// replays the existing record; anything differing is a conflict.
    pub async fn create_input(
        &self,
        input: CreateAgentInput,
        now: i64,
    ) -> Result<(AgentInputRecord, bool)> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let now_text = crate::ConversationTimestamp::from_epoch_millis(now)?
                .wire_text()?
                .into_owned();
            let result = insert_input(&transaction, input, &now_text)?;
            transaction.commit()?;
            Ok(result)
        })
        .await
    }

    pub async fn get_input(&self, input_id: String) -> Result<Option<AgentInputRecord>> {
        self.call(move |db| get_input(db, &input_id)).await
    }

    /// Ordered by `order` ascending.
    pub async fn list_inputs(&self, query: AgentInputQuery) -> Result<Vec<AgentInputRecord>> {
        self.call(move |db| list_inputs(db, &query)).await
    }

    /// Compare-and-set against the per-kind machine; illegal edges error.
    pub async fn transition_input(
        &self,
        input_id: String,
        from: Vec<AgentInputStatus>,
        to: AgentInputStatus,
        patch: AgentInputPatch,
        now: i64,
    ) -> Result<AgentInputRecord> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let now_text = crate::ConversationTimestamp::from_epoch_millis(now)?
                .wire_text()?
                .into_owned();
            let record = transition_input(&transaction, &input_id, &from, to, &patch, &now_text)?;
            transaction.commit()?;
            Ok(record)
        })
        .await
    }

    /// Remove every input of a session, whatever its status.
    pub async fn delete_session_inputs(&self, session_id: String) -> Result<u64> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let removed = delete_session_inputs(&transaction, &session_id)?;
            transaction.commit()?;
            Ok(removed)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::parse_iso_millis;

    #[test]
    fn out_of_range_fields_are_rejected_on_every_return_path() {
        assert_eq!(parse_iso_millis("2026-13-45T99:99:99+00:00"), None);
        assert_eq!(parse_iso_millis("2026-13-45T99:99:99Z"), None);
        assert_eq!(parse_iso_millis("2026-10-22T24:00:01+00:00"), None);
        assert_eq!(parse_iso_millis("2026-10-22T24:00:00.001Z"), None);
        assert_eq!(parse_iso_millis("2026-10-22T24:30:00Z"), None);
        assert_eq!(
            parse_iso_millis("2026-10-22T24:00:00Z"),
            parse_iso_millis("2026-10-23T00:00:00Z"),
            "24:00:00.000 is midnight, as in Date.parse"
        );
        assert_eq!(parse_iso_millis("2026-10-22T10:00:00+24:00"), None);
        assert_eq!(parse_iso_millis("2026-10-22T10:00:00+01:60"), None);
        assert_eq!(parse_iso_millis("2026-10-22T23:60:00Z"), None);
        assert_eq!(parse_iso_millis("2026-10-22T23:59:60+01:00"), None);
        assert_eq!(parse_iso_millis("2026-00-22T00:00:00Z"), None);
        assert!(parse_iso_millis("2026-10-22T23:59:59.999+02:00").is_some());
        assert_eq!(
            parse_iso_millis("1970-01-01T00:00:00Z"),
            Some(0),
            "a valid instant still parses"
        );
    }
}
