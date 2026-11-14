//! Durable execution receipts (ADR 0020).
//!
//! An external effect and its receipt cannot form one atomic transaction, so
//! the store records admission *before* a mutating tool body runs and the
//! result *after* it returns. Only a newly successful atomic admission may
//! invoke the body, and the transaction ends first. A receipt left in
//! `admitted` without a durable result is the honest representation of an
//! uncertain effect: it must never be silently re-executed.
//!
//! Statuses are `admitted`, `completed` and `resolved`; "uncertain" is derived
//! as `admitted` without `completed`. `resolved` is only ever written by an
//! explicit operator resolution (`execution_resolution.rs`). Receipt records
//! are separate from model conversation history and from process quarantine.

use crate::{
    AgentInputKind, AgentInputStatus, SqliteStore, StoreError, agent_inputs::get_input,
    tasks::fence, util::json,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// The execution-receipt records are the sessions context's (ADR 0024 §2).
pub use agentkit_sessions::{
    ArgsHashProvenance, ExecutionReceiptRecord, ExecutionReceiptStatus, ExecutionResolution,
    ResolutionOutcome,
};

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
    /// The SAME arguments hashed through the TypeScript canonicalizer
    /// (`durable_execution::ts_args_sha256`). Optional: an admission that
    /// does not carry it simply cannot cross-match an imported
    /// `ts-canonical` receipt in the pending-generation gate, so hosts that
    /// may import TS databases must supply it.
    pub ts_args_sha256: Option<String>,
    /// Which canonicalizer produced `args_sha256`. Native admissions pass
    /// `Canonical`; the napi port stamps `TsCanonical` when the JS side
    /// says the hash came from the TypeScript pipeline.
    pub args_hash_provenance: ArgsHashProvenance,
    pub context_sha256: String,
    pub attempt_id: String,
    pub approval_request_id: Option<String>,
    pub approval_generation: Option<String>,
    pub now: i64,
}

/// True when `stored` — recorded under `provenance` — names the same
/// arguments as the native hash `args_sha256` (and, for an imported
/// `ts-canonical` receipt, the TS-pipeline hash `ts_args_sha256` of the
/// same values). This is the compatibility lookup the uncertainty guard
/// and every store-side gate share.
pub fn args_match(
    provenance: ArgsHashProvenance,
    stored: &str,
    args_sha256: &str,
    ts_args_sha256: Option<&str>,
) -> bool {
    stored == args_sha256
        || (provenance == ArgsHashProvenance::TsCanonical
            && ts_args_sha256.is_some_and(|hash| hash == stored))
}

/// A durable uncertainty barrier: the honest residue of an admitted effect
/// whose completion is unknown AND whose receipt could not be placed
/// (tolerant import refused it). Silent omission of such a row would let
/// the identical mutation run twice, so the import records this placeholder
/// keyed by tool name plus whatever best-known identity survives, and the
/// host guard treats every barrier row as an unresolved uncertainty until
/// a human reauthorizes its exact id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UncertaintyBarrierRecord {
    /// Deterministic id over the known identity, so a re-import is
    /// insert-if-absent instead of a duplicate.
    pub barrier_id: String,
    /// The chat the blocked effect belonged to, when it could be proven.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    /// The run the blocked effect belonged to, when the row carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub tool_name: String,
    /// The arguments fingerprint when the source record still carried one;
    /// `None` blocks every reissue of the tool by name (best-known
    /// identity — an unknown fingerprint is never guessed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_sha256: Option<String>,
    pub args_hash_provenance: ArgsHashProvenance,
    /// Why the real receipt could not be placed; shown to the human when
    /// the barrier refuses an execution.
    pub reason: String,
}

impl UncertaintyBarrierRecord {
    /// Does this barrier block an invocation of `tool_name` whose arguments
    /// hash to `args_sha256` (native) / `ts_args_sha256` (TS pipeline)?
    pub fn blocks(&self, tool_name: &str, args_sha256: &str, ts_args_sha256: Option<&str>) -> bool {
        self.tool_name == tool_name
            && self.args_sha256.as_deref().is_none_or(|stored| {
                args_match(
                    self.args_hash_provenance,
                    stored,
                    args_sha256,
                    ts_args_sha256,
                )
            })
    }
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
    if record.status == ExecutionReceiptStatus::Resolved
        && (record.result.is_some()
            || record
                .resolution
                .as_ref()
                .is_none_or(|resolution| resolution.outcome == ResolutionOutcome::Completed))
    {
        return Err(unreadable(
            "a resolved receipt must carry a non-completed resolution and no result",
        ));
    }
    Ok(record)
}

pub(crate) fn read_row(
    row: &(String, String, Option<String>, String),
) -> Result<ExecutionReceiptRecord, StoreError> {
    let value = agentkit_contracts::parse_javascript_json(&row.3).map_err(|error| {
        StoreError::RecoveryUnreadable(format!("cannot parse receipt JSON: {error}"))
    })?;
    read_execution_receipt(value, &row.0, &row.1, Some(row.2.as_deref()))
}

pub(crate) fn select_run(
    db: &Connection,
    run_id: &str,
) -> Result<Vec<ExecutionReceiptRecord>, StoreError> {
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

/// Read one barrier row, refusing a malformed one as recovery evidence
/// unreadable — a corrupt barrier must block, never read as absence.
fn read_barrier(
    row: &(
        String,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        String,
        String,
    ),
) -> Result<UncertaintyBarrierRecord, StoreError> {
    let unreadable = |reason: &str| {
        StoreError::RecoveryUnreadable(format!(
            "An uncertainty barrier is malformed ({reason}); mutations remain blocked."
        ))
    };
    if row.3.is_empty() {
        return Err(unreadable("toolName must be non-empty"));
    }
    if row.4.as_deref().is_some_and(|hash| !is_hex64(hash)) {
        return Err(unreadable("argsSha256 must be 64 lowercase hex characters"));
    }
    let args_hash_provenance = match row.5.as_str() {
        "canonical" => ArgsHashProvenance::Canonical,
        "ts-canonical" => ArgsHashProvenance::TsCanonical,
        other => return Err(unreadable(&format!("unknown argsHashProvenance {other:?}"))),
    };
    if row.0.is_empty() || row.6.is_empty() {
        return Err(unreadable("barrierId and reason must be non-empty"));
    }
    Ok(UncertaintyBarrierRecord {
        barrier_id: row.0.clone(),
        chat_id: row.1.clone(),
        run_id: row.2.clone(),
        tool_name: row.3.clone(),
        args_sha256: row.4.clone(),
        args_hash_provenance,
        reason: row.6.clone(),
    })
}

const BARRIER_COLUMNS: &str =
    "barrier_id, chat_id, run_id, tool_name, args_sha256, args_hash_provenance, reason";

/// The barrier scan shared by the admission gate and the host guard: rows
/// scoped to this chat (directly, or through the run's binding), to this
/// very run, or carrying no attributable identity at all.
pub(crate) fn select_barriers(
    db: &Connection,
    chat_id: &str,
    run_id: &str,
) -> Result<Vec<UncertaintyBarrierRecord>, StoreError> {
    let mut statement = db.prepare(&format!(
        "SELECT {BARRIER_COLUMNS} FROM uncertainty_barriers \
         WHERE (chat_id IS NULL AND run_id IS NULL) \
            OR chat_id = ?1 OR run_id = ?2 \
            OR run_id IN (SELECT run_id FROM run_bindings WHERE chat_id = ?1) \
         ORDER BY rowid"
    ))?;
    let rows = statement.query_map(params![chat_id, run_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    rows.map(|row| read_barrier(&row?)).collect()
}

/// Insert-if-absent. Re-imports of the same source row derive the same
/// deterministic `barrier_id` and refresh provenance in place (never the
/// reason of an existing row, which may name a stricter refusal).
pub fn insert_barrier(
    db: &Connection,
    barrier: &UncertaintyBarrierRecord,
) -> Result<bool, StoreError> {
    if barrier.barrier_id.is_empty()
        || barrier.tool_name.is_empty()
        || barrier.reason.is_empty()
        || barrier
            .args_sha256
            .as_deref()
            .is_some_and(|hash| !is_hex64(hash))
    {
        return Err(StoreError::InvalidInput(
            "an uncertainty barrier needs a barrier id, tool name, reason and a well-formed argument hash".into(),
        ));
    }
    // An upsert that updates reports one change just like an insert does, so
    // existence is read first or a re-import would count the barrier twice.
    let existed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM uncertainty_barriers WHERE barrier_id=?)",
        [&barrier.barrier_id],
        |row| row.get(0),
    )?;
    db.execute(
        "INSERT INTO uncertainty_barriers(barrier_id,chat_id,run_id,tool_name,args_sha256,args_hash_provenance,reason) \
         VALUES(?,?,?,?,?,?,?) ON CONFLICT(barrier_id) DO UPDATE SET \
         chat_id=COALESCE(uncertainty_barriers.chat_id, excluded.chat_id), \
         run_id=COALESCE(uncertainty_barriers.run_id, excluded.run_id), \
         args_hash_provenance=excluded.args_hash_provenance",
        params![
            barrier.barrier_id,
            barrier.chat_id,
            barrier.run_id,
            barrier.tool_name,
            barrier.args_sha256,
            match barrier.args_hash_provenance {
                ArgsHashProvenance::Canonical => "canonical",
                ArgsHashProvenance::TsCanonical => "ts-canonical",
            },
            barrier.reason,
        ],
    )?;
    Ok(!existed)
}

/// The deterministic barrier id: sha-256 over the best-known identity of
/// the unplaceable source row. Two imports of the same row derive the same
/// id, so barriers are insert-if-absent across re-runs.
pub fn barrier_id_of(
    execution_id: Option<&str>,
    run_id: Option<&str>,
    tool_name: &str,
    args_sha256: Option<&str>,
) -> String {
    let binding = serde_json::json!({
        "executionId": execution_id,
        "runId": run_id,
        "toolName": tool_name,
        "argsSha256": args_sha256,
    });
    let canonical = serde_json::to_string(&binding).unwrap_or_else(|_| "{}".into());
    format!("barrier:{}", agentkit_kernel::sha256_hex(&canonical))
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
            if admission
                .ts_args_sha256
                .as_deref()
                .is_some_and(|hash| !is_hex64(hash))
            {
                return Err(StoreError::InvalidInput(
                    "the TypeScript-pipeline argument hash must be 64 lowercase hex characters".into(),
                ));
            }
            validate_approval(&transaction, &admission)?;
            // Uncertainty barriers: an unplaceable admitted effect blocks the
            // identical reissue even though no receipt row carries it. Only
            // an answer that names the barrier's exact id opens the gate.
            if select_barriers(&transaction, &admission.chat_id, &admission.run_id)?
                .iter()
                .any(|barrier| {
                    barrier.blocks(
                        &admission.tool_name,
                        &admission.args_sha256,
                        admission.ts_args_sha256.as_deref(),
                    ) && admission.approval_generation.as_deref()
                        != Some(barrier.barrier_id.as_str())
                })
            {
                transaction.commit()?;
                return Ok(false);
            }
            // Anti-corruption on the existing rows this decision reads.
            let existing = select_run(&transaction, &admission.run_id)?;
            // Pending generation gate: while any execution of this exact
            // operation is admitted-without-result, only that receipt's id
            // (as a fresh approval generation) may admit again. An imported
            // `ts-canonical` receipt names the same arguments under the TS
            // pipeline hash, so it holds the gate too.
            let previous = existing.iter().rfind(|record| {
                record.tool_name == admission.tool_name
                    && record.context_sha256 == admission.context_sha256
                    && args_match(
                        record.args_hash_provenance,
                        &record.args_sha256,
                        &admission.args_sha256,
                        admission.ts_args_sha256.as_deref(),
                    )
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
                args_hash_provenance: admission.args_hash_provenance,
                context_sha256: admission.context_sha256.clone(),
                attempt_id: admission.attempt_id.clone(),
                approval_request_id: admission.approval_request_id.clone(),
                approval_generation: admission.approval_generation.clone(),
                status: ExecutionReceiptStatus::Admitted,
                result: None,
                resolution: None,
            };
            let mut serialized_record = serde_json::to_value(&record)?;
            serialized_record[crate::execution_no_start::AUTHORITY_FIELD] = serde_json::Value::String(
                crate::execution_no_start::admission_authority(&proof, &record.attempt_id),
            );
            let serialized = json(&transaction, &serialized_record)?;
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
                // Only the UNIQUE code: any other constraint failure is a
                // real defect and must surface, not read as "already consumed".
                Err(rusqlite::Error::SqliteFailure(failure, _))
                    if failure.code == rusqlite::ErrorCode::ConstraintViolation
                        && failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
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
            // A resolved receipt was closed by an operator: a late body
            // result must not overwrite that decision.
            if record.status != ExecutionReceiptStatus::Admitted {
                return Err(StoreError::InvalidTransition(format!(
                    "execution {} is already {:?}",
                    record.execution_id, record.status
                )));
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

    /// The uncertainty barriers this execution must respect: rows left by a
    /// tolerant import for admitted effects whose receipt could not be
    /// placed, scoped to this chat (directly or through the run binding),
    /// to this run, or carrying no attributable identity at all. The host
    /// guard treats every returned row as an unresolved uncertainty; a
    /// malformed barrier row is a hard recovery error, never an absence.
    pub async fn list_uncertainty_barriers(
        &self,
        chat_id: String,
        run_id: String,
    ) -> Result<Vec<UncertaintyBarrierRecord>, StoreError> {
        self.call(move |db| select_barriers(db, &chat_id, &run_id))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserting_the_same_barrier_twice_counts_it_once() {
        let (db, _) = crate::schema::open(std::path::Path::new(":memory:")).unwrap();
        let barrier = UncertaintyBarrierRecord {
            barrier_id: barrier_id_of(Some("exec"), Some("run"), "write_file", None),
            chat_id: None,
            run_id: Some("run".into()),
            tool_name: "write_file".into(),
            args_sha256: None,
            args_hash_provenance: ArgsHashProvenance::Canonical,
            reason: "unplaceable".into(),
        };
        assert!(insert_barrier(&db, &barrier).unwrap());
        assert!(!insert_barrier(&db, &barrier).unwrap());
        let rows: i64 = db
            .query_row("SELECT count(*) FROM uncertainty_barriers", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 1);
    }
}
