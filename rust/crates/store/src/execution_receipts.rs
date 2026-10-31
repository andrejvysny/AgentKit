//! Durable execution receipts (ADR 0020).
//!
//! An external effect and its receipt cannot form one atomic transaction, so
//! the store records admission *before* a mutating tool body runs and the
//! result *after* it returns. Only a newly successful atomic admission may
//! invoke the body, and the transaction ends first. A receipt left in
//! `admitted` without a durable result is the honest representation of an
//! uncertain effect: it must never be silently re-executed.
//!
//! Statuses are exactly `admitted` and `completed`; "uncertain" is derived as
//! `admitted` without `completed`. Receipt records are separate from model
//! conversation history and from process quarantine.

use crate::{
    AgentInputKind, AgentInputStatus, SqliteStore, StoreError, agent_inputs::get_input,
    tasks::fence, util::json,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

agentkit_contracts::impl_json_via_serde!(ExecutionReceiptRecord);

/// One durable execution receipt. Field names mirror the TypeScript
/// reference implementation (`ToolExecutionRecord`) so cross-language
/// fixtures compare one-to-one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionReceiptRecord {
    /// Deterministic id: sha-256 hex over the canonical
    /// `{runId, toolCallId, approvalRequestId ?? "initial"}` binding.
    pub execution_id: String,
    pub run_id: String,
    /// The chat whose runs own this effect. Persisted in the record so
    /// chat-scoped queries (uncertainty across runs, recovery) need no
    /// join onto the continuation binding table — session turns have
    /// receipts but no provider continuation.
    pub chat_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    /// sha-256 hex of the canonical-JSON arguments.
    pub args_sha256: String,
    /// sha-256 hex of the host execution-context binding.
    pub context_sha256: String,
    pub attempt_id: String,
    /// The answered approval input consumed by this execution (single-use
    /// via the `approval_request_id` UNIQUE index); absent until the
    /// approval flow spends one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_request_id: Option<String>,
    /// Authorization generation being spent; absent until approvals exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_generation: Option<String>,
    pub status: ExecutionReceiptStatus,
    /// Serialized model-facing `ToolResult`, present iff `completed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub result: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionReceiptStatus {
    Admitted,
    Completed,
}

/// The insert payload for admission (`ToolExecutionAdmission` in TS):
/// everything but status and result.
#[derive(Clone, Debug)]
pub struct ExecutionReceiptAdmission {
    pub execution_id: String,
    pub run_id: String,
    pub chat_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args_sha256: String,
    pub context_sha256: String,
    pub attempt_id: String,
    pub approval_request_id: Option<String>,
    pub approval_generation: Option<String>,
    pub now: i64,
}

fn is_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Anti-corruption validator: a malformed receipt is a hard recovery error,
/// never an absence of uncertain execution.
pub fn read_execution_receipt(
    value: serde_json::Value,
    expected_execution_id: &str,
    expected_run_id: &str,
    expected_approval_request_id: Option<Option<&str>>,
) -> Result<ExecutionReceiptRecord, StoreError> {
    let unreadable = |reason: &str| {
        StoreError::RecoveryUnreadable(format!(
            "A durable execution receipt is malformed ({reason}); mutations remain blocked."
        ))
    };
    let record: ExecutionReceiptRecord =
        serde_json::from_value(value).map_err(|error| unreadable(&error.to_string()))?;
    for (field, value) in [
        ("executionId", &record.execution_id),
        ("runId", &record.run_id),
        ("chatId", &record.chat_id),
        ("toolCallId", &record.tool_call_id),
        ("toolName", &record.tool_name),
        ("attemptId", &record.attempt_id),
    ] {
        if value.is_empty() {
            return Err(unreadable(&format!("{field} must be non-empty")));
        }
    }
    for (field, value) in [
        ("argsSha256", &record.args_sha256),
        ("contextSha256", &record.context_sha256),
    ] {
        if !is_hex64(value) {
            return Err(unreadable(&format!(
                "{field} must be 64 lowercase hex characters"
            )));
        }
    }
    for (field, value) in [
        ("approvalRequestId", &record.approval_request_id),
        ("approvalGeneration", &record.approval_generation),
    ] {
        if value.as_ref().is_some_and(|value| value.is_empty()) {
            return Err(unreadable(&format!(
                "{field} must be non-empty when present"
            )));
        }
    }
    if record.execution_id != expected_execution_id {
        return Err(unreadable("column and payload disagree on executionId"));
    }
    if record.run_id != expected_run_id {
        return Err(unreadable("column and payload disagree on runId"));
    }
    if let Some(expected) = expected_approval_request_id
        && record.approval_request_id.as_deref() != expected
    {
        return Err(unreadable(
            "column and payload disagree on approvalRequestId",
        ));
    }
    if record.status == ExecutionReceiptStatus::Completed
        && !record
            .result
            .as_ref()
            .is_some_and(|result| result.is_object())
    {
        return Err(unreadable(
            "a completed receipt must carry a tool-result object",
        ));
    }
    Ok(record)
}

fn read_row(
    row: &(String, String, Option<String>, String),
) -> Result<ExecutionReceiptRecord, StoreError> {
    let value = agentkit_contracts::parse_javascript_json(&row.3).map_err(|error| {
        StoreError::RecoveryUnreadable(format!("cannot parse receipt JSON: {error}"))
    })?;
    read_execution_receipt(value, &row.0, &row.1, Some(row.2.as_deref()))
}

fn select_run(db: &Connection, run_id: &str) -> Result<Vec<ExecutionReceiptRecord>, StoreError> {
    let mut statement = db.prepare(
        "SELECT execution_id, run_id, approval_request_id, record FROM tool_executions WHERE run_id = ? ORDER BY rowid",
    )?;
    let rows = statement.query_map([run_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    rows.map(|row| read_row(&row?)).collect()
}

fn validate_approval(
    db: &Connection,
    admission: &ExecutionReceiptAdmission,
) -> Result<(), StoreError> {
    let mismatch = || {
        StoreError::InvalidInput(
            "approval_mismatch: approval does not authorize this execution".into(),
        )
    };
    let Some(id) = admission.approval_request_id.as_deref() else {
        return if admission.approval_generation.is_none() {
            Ok(())
        } else {
            Err(mismatch())
        };
    };
    let request = get_input(db, id)?.ok_or_else(mismatch)?;
    let content = &request.content;
    let generation = content.get("approvalGeneration");
    let legacy = content.get("uncertaintyGeneration");
    let hash = content.get("contextSha256");
    if generation.is_some_and(|value| !value.is_string())
        || legacy.is_some_and(|value| !value.is_string())
        || generation.is_some() && legacy.is_some() && generation != legacy
        || hash.is_some_and(|value| !value.is_string())
        || request.input_id != id
        || request.kind != AgentInputKind::HumanRequest
        || request.status != AgentInputStatus::Answered
        || !request.applied_message_id.as_deref().is_some_and(|marker| {
            marker.starts_with("approval:") && marker.len() > "approval:".len()
        })
        || request.answer_id.as_deref().is_none_or(str::is_empty)
        || request.session_id != admission.chat_id
        || request.target_task_id.as_deref() != Some(admission.run_id.as_str())
        || request.tool_call_id.as_deref() != Some(admission.tool_call_id.as_str())
        || content["kind"].as_str() != Some("approval")
        || content["toolName"].as_str() != Some(admission.tool_name.as_str())
        || content["argsSha256"].as_str() != Some(admission.args_sha256.as_str())
        || generation.or(legacy).and_then(serde_json::Value::as_str)
            != admission.approval_generation.as_deref()
        || hash.is_some_and(|value| value.as_str() != Some(admission.context_sha256.as_str()))
    {
        return Err(mismatch());
    }
    Ok(())
}

impl SqliteStore {
    /// Atomic, fenced insert. Only a newly successful admission returns
    /// `true`; an existing execution, a consumed approval, or the pending
    /// generation gate returns `false`. The transaction ends before any
    /// external effect: the caller must invoke the body only on `true`.
    pub async fn admit_execution(
        &self,
        proof: crate::LeaseFence,
        admission: ExecutionReceiptAdmission,
    ) -> Result<bool, StoreError> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            fence(&transaction, &proof, admission.now)?;
            if admission.run_id != proof.task_id {
                return Err(StoreError::InvalidInput(
                    "Execution does not belong to the fenced run.".into(),
                ));
            }
            for field in [
                &admission.execution_id,
                &admission.chat_id,
                &admission.tool_call_id,
                &admission.tool_name,
                &admission.attempt_id,
            ] {
                if field.is_empty() {
                    return Err(StoreError::InvalidInput(
                        "receipt identity fields must be non-empty".into(),
                    ));
                }
            }
            if !is_hex64(&admission.args_sha256) || !is_hex64(&admission.context_sha256) {
                return Err(StoreError::InvalidInput(
                    "receipt hashes must be 64 lowercase hex characters".into(),
                ));
            }
            validate_approval(&transaction, &admission)?;
            // Anti-corruption on the existing rows this decision reads.
            let existing = select_run(&transaction, &admission.run_id)?;
            // Pending generation gate: while any execution of this exact
            // operation is admitted-without-result, only that receipt's id
            // (as a fresh approval generation) may admit again.
            let previous = existing
                .iter()
                .rfind(|record| {
                    record.tool_name == admission.tool_name
                        && record.args_sha256 == admission.args_sha256
                        && record.context_sha256 == admission.context_sha256
                });
            if let Some(previous) = previous
                && previous.status == ExecutionReceiptStatus::Admitted
                && admission.approval_generation.as_deref() != Some(previous.execution_id.as_str())
            {
                transaction.commit()?;
                return Ok(false);
            }
            let record = ExecutionReceiptRecord {
                execution_id: admission.execution_id.clone(),
                run_id: admission.run_id.clone(),
                chat_id: admission.chat_id.clone(),
                tool_call_id: admission.tool_call_id.clone(),
                tool_name: admission.tool_name.clone(),
                args_sha256: admission.args_sha256.clone(),
                context_sha256: admission.context_sha256.clone(),
                attempt_id: admission.attempt_id.clone(),
                approval_request_id: admission.approval_request_id.clone(),
                approval_generation: admission.approval_generation.clone(),
                status: ExecutionReceiptStatus::Admitted,
                result: None,
            };
            let serialized = json(&transaction, &record)?;
            let inserted = transaction.execute(
                "INSERT INTO tool_executions(execution_id,run_id,approval_request_id,record) VALUES(?,?,?,?) ON CONFLICT(execution_id) DO NOTHING",
                params![
                    record.execution_id,
                    record.run_id,
                    record.approval_request_id,
                    serialized
                ],
            );
            let admitted = match inserted {
                Ok(changes) => changes == 1,
                // The UNIQUE approval index fired: this authorization is
                // already consumed by another execution.
                Err(rusqlite::Error::SqliteFailure(failure, _))
                    if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    false
                }
                Err(error) => return Err(error.into()),
            };
            transaction.commit()?;
            Ok(admitted)
        })
        .await
    }

    /// Persist the result before delivery to the core loop or UI. Idempotent
    /// for an already-completed receipt; a missing receipt is a bug in the
    /// admission flow and is reported, never ignored.
    pub async fn complete_execution(
        &self,
        proof: crate::LeaseFence,
        execution_id: String,
        result: serde_json::Value,
        now: i64,
    ) -> Result<(), StoreError> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            fence(&transaction, &proof, now)?;
            let row: Option<(String, String, Option<String>, String)> = transaction
                .query_row(
                    "SELECT execution_id, run_id, approval_request_id, record FROM tool_executions WHERE execution_id = ? AND run_id = ?",
                    params![execution_id, proof.task_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                        ))
                    },
                )
                .optional()?;
            let Some(row) = row else {
                return Err(StoreError::NotFound(format!(
                    "execution receipt {execution_id}"
                )));
            };
            let mut record = read_row(&row)?;
            if record.status == ExecutionReceiptStatus::Completed {
                transaction.commit()?;
                return Ok(());
            }
            if !result.is_object() {
                return Err(StoreError::InvalidInput(
                    "a tool result must be a JSON object".into(),
                ));
            }
            record.status = ExecutionReceiptStatus::Completed;
            record.result = Some(result);
            let serialized = json(&transaction, &record)?;
            // Re-validate what was just written before it becomes durable.
            let check = agentkit_contracts::parse_javascript_json(&serialized)?;
            read_execution_receipt(check, &record.execution_id, &record.run_id, Some(record.approval_request_id.as_deref()))?;
            transaction.execute(
                "UPDATE tool_executions SET record = ? WHERE execution_id = ?",
                params![serialized, record.execution_id],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    /// The only read path for receipts. Any malformed row blocks mutations
    /// rather than reading as absence.
    pub async fn list_executions(
        &self,
        run_id: String,
    ) -> Result<Vec<ExecutionReceiptRecord>, StoreError> {
        self.call(move |db| select_run(db, &run_id)).await
    }

    /// Receipts for every run of a chat, oldest run first. Cross-run scan
    /// used by the uncertainty guard: an uncertain effect under one run id
    /// blocks an identical reissue under a fresh call id in the same chat.
    pub async fn list_executions_by_chat(
        &self,
        chat_id: String,
    ) -> Result<Vec<ExecutionReceiptRecord>, StoreError> {
        self.call(move |db| {
            // The chat lives IN the record: receipts belong to runs that
            // may have no continuation binding at all (session turns).
            let mut statement = db.prepare(
                "SELECT execution_id, run_id, approval_request_id, record \
                 FROM tool_executions WHERE json_extract(record,'$.chatId') = ?1 ORDER BY rowid",
            )?;
            let rows = statement.query_map([chat_id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            rows.map(|row| read_row(&row?)).collect()
        })
        .await
    }
}
