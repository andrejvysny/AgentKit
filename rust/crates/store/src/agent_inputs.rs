//! Durable agent-session inputs (ADR 0016 §2).
//!
//! Steering, follow-ups, and human requests are durable records that do not
//! become a turn immediately. Creation is idempotent on `inputId`; status
//! moves are compare-and-set against the per-kind machine, whose non-pending
//! statuses are terminal. The store-assigned `order` is the FIFO key.

use crate::{Result, SqliteStore, StoreError, util::json};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

agentkit_contracts::impl_json_via_serde!(AgentInputRecord);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentInputKind {
    Steer,
    FollowUp,
    HumanRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentInputStatus {
    Pending,
    Applied,
    Converted,
    Answered,
    Cancelled,
}

/// What happens to a steer whose target turn ends before it is placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentInputSettlePolicy {
    FollowUp,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInputRecord {
    /// Primary key and the caller's idempotency key.
    pub input_id: String,
    /// The chat id.
    pub session_id: String,
    pub kind: AgentInputKind,
    pub status: AgentInputStatus,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub content: Value,
    /// Caller-supplied digest of `content`; how a replay is told from a conflict.
    pub content_hash: String,
    /// Store-assigned, strictly increasing insertion order (the FIFO key).
    pub order: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_task_id: Option<String>,
    /// Steer only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_settled: Option<AgentInputSettlePolicy>,
    /// Human request: the generation and call an answer must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Receipt: where the input landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug)]
pub struct CreateAgentInput {
    pub input_id: String,
    pub session_id: String,
    pub kind: AgentInputKind,
    pub content: Value,
    pub content_hash: String,
    pub target_task_id: Option<String>,
    pub on_settled: Option<AgentInputSettlePolicy>,
    pub attempt_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct AgentInputQuery {
    pub session_id: String,
    pub kinds: Option<Vec<AgentInputKind>>,
    pub statuses: Option<Vec<AgentInputStatus>>,
    pub target_task_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct AgentInputPatch {
    pub applied_task_id: Option<String>,
    pub applied_message_id: Option<String>,
    pub answer_id: Option<String>,
}

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
        return Err(StoreError::InvalidTransition(format!(
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
        return Err(StoreError::InvalidTransition(format!(
            "Input {} ({:?}) cannot move {:?} → {to:?}.",
            record.input_id, record.kind, record.status
        )));
    }
    Ok(())
}

/// Parse the ISO-8601 subset JavaScript `Date.parse` accepts for the
/// timestamps this stack writes (`YYYY-MM-DDTHH:MM:SS.mmmZ`, optional
/// fraction, extended `+YYYYYY` years, optional `Z`/offset). `None` mirrors
/// `Date.parse` yielding `NaN`: an unreadable expiry never expires.
/// A time without an offset is read as UTC.
pub(crate) fn parse_iso_millis(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    let digits = |pos: usize, len: usize| -> Option<(i64, usize)> {
        let slice = bytes.get(pos..pos + len)?;
        if slice.is_empty() || !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        Some((std::str::from_utf8(slice).ok()?.parse().ok()?, pos + len))
    };
    let take = |pos: usize, ch: u8| -> Option<usize> {
        bytes.get(pos).filter(|byte| **byte == ch).map(|_| pos + 1)
    };
    let (year, p) = if bytes.first().is_some_and(|byte| *byte == b'+') {
        digits(1, 6)?
    } else {
        digits(0, 4)?
    };
    let p = take(p, b'-')?;
    let (month, p) = digits(p, 2)?;
    let p = take(p, b'-')?;
    let (day, p) = digits(p, 2)?;
    let (mut hour, mut minute, mut second, mut fraction) = (0, 0, 0, 0);
    if let Some(p2) = take(p, b'T') {
        let (h, p2) = digits(p2, 2)?;
        let p2 = take(p2, b':')?;
        let (m, p2) = digits(p2, 2)?;
        let p2 = take(p2, b':')?;
        let (s, mut p2) = digits(p2, 2)?;
        hour = h;
        minute = m;
        second = s;
        if let Some(p3) = take(p2, b'.') {
            let start = p3;
            let mut end = p3;
            while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            if end == start {
                return None;
            }
            let (value, _) = digits(start, (end - start).min(3))?;
            fraction = match (end - start).min(3) {
                1 => value * 100,
                2 => value * 10,
                _ => value,
            };
            p2 = end;
        }
        match bytes.get(p2) {
            None => {}
            Some(b'Z') | Some(b'z') if p2 + 1 == bytes.len() => {}
            Some(sign @ (b'+' | b'-')) => {
                let offset_sign = if *sign == b'+' { 1 } else { -1 };
                let (oh, q) = digits(p2 + 1, 2)?;
                let q = take(q, b':')?;
                let (om, q) = digits(q, 2)?;
                if q != bytes.len() {
                    return None;
                }
                return Some(
                    days_from_civil(year, month, day) * 86_400_000
                        + hour * 3_600_000
                        + minute * 60_000
                        + second * 1_000
                        + fraction
                        - offset_sign * (oh * 3_600_000 + om * 60_000),
                );
            }
            _ => return None,
        }
    } else if p != bytes.len() {
        return None;
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 24 || minute > 60 || second > 60 {
        return None;
    }
    Some(
        days_from_civil(year, month, day) * 86_400_000
            + hour * 3_600_000
            + minute * 60_000
            + second * 1_000
            + fraction,
    )
}

/// Days since the Unix epoch (Howard Hinnant's civil algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
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

fn next_order(db: &Connection) -> Result<i64> {
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
        "INSERT INTO agent_inputs VALUES(?,?,?,?,?,?)",
        params![
            record.input_id,
            record.session_id,
            kind_text(record.kind),
            status_text(record.status),
            record.target_task_id,
            json(db, &record)?,
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
         AND (?2 IS NULL OR target_task_id=?2)",
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
    Ok(db.execute("DELETE FROM agent_inputs WHERE session_id=?", [session_id])? as u64)
}

pub(crate) fn kind_text(kind: AgentInputKind) -> &'static str {
    match kind {
        AgentInputKind::Steer => "steer",
        AgentInputKind::FollowUp => "follow_up",
        AgentInputKind::HumanRequest => "human_request",
    }
}

fn status_text(status: AgentInputStatus) -> &'static str {
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
            let transaction = db.transaction()?;
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
            let transaction = db.transaction()?;
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
            let transaction = db.transaction()?;
            let removed = delete_session_inputs(&transaction, &session_id)?;
            transaction.commit()?;
            Ok(removed)
        })
        .await
    }
}
