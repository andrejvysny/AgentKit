//! Explicit operator resolution of an uncertain execution receipt.
//!
//! An `admitted` receipt without a result blocks every identical reissue
//! forever (ADR 0020). Only a human can know whether the effect happened, so
//! resolution is a separate, audited, never-implicit transition: the outcome,
//! note, time and actor are written into the receipt JSON itself. No schema
//! change: status and audit live in the existing `record` column.

use crate::execution_receipts::{barrier_id_of, read_execution_receipt, read_row};
use crate::{
    ExecutionReceiptRecord, ExecutionReceiptStatus, ExecutionResolution, ResolutionOutcome,
    SqliteStore, StoreError, TaskRecord,
    util::{get, json},
};
use rusqlite::{OptionalExtension, params};

const MAX_NOTE_CHARS: usize = 2000;
const MAX_ACTOR_CHARS: usize = 200;
/// Largest serialized operator-supplied result the receipt will carry.
pub const MAX_RESOLUTION_RESULT_BYTES: usize = 262_144;

/// Stable message prefix for "this receipt is not (or no longer) uncertain".
pub const RECEIPT_NOT_UNCERTAIN: &str = "receipt_not_uncertain";
/// Stable message prefix: the receipt's run may still execute its tool body.
pub const RECEIPT_RUN_ACTIVE: &str = "receipt_run_active";

#[derive(Clone, Debug)]
pub struct ResolveExecution {
    pub outcome: ResolutionOutcome,
    /// Required (a tool-result object) for `Completed`; forbidden otherwise.
    pub result: Option<serde_json::Value>,
    pub note: Option<String>,
    pub resolved_by: Option<String>,
    pub now: i64,
}

fn validate(request: &ResolveExecution) -> Result<(), StoreError> {
    let invalid = |message: &str| Err(StoreError::InvalidInput(message.into()));
    match (request.outcome, &request.result) {
        (ResolutionOutcome::Completed, Some(result)) if result.is_object() => {
            if result.to_string().len() > MAX_RESOLUTION_RESULT_BYTES {
                return invalid("result exceeds 262144 bytes");
            }
        }
        (ResolutionOutcome::Completed, _) => {
            return invalid("a completed resolution requires a tool-result object");
        }
        (_, Some(_)) => return invalid("only a completed resolution may carry a result"),
        (_, None) => {}
    }
    if request
        .note
        .as_ref()
        .is_some_and(|note| note.chars().count() > MAX_NOTE_CHARS)
    {
        return invalid("note exceeds 2000 characters");
    }
    if request
        .resolved_by
        .as_ref()
        .is_some_and(|actor| actor.is_empty() || actor.chars().count() > MAX_ACTOR_CHARS)
    {
        return invalid("resolvedBy must be 1-200 characters when present");
    }
    Ok(())
}

impl SqliteStore {
    /// Close one uncertain receipt. Admitted -> Completed (outcome
    /// `completed`) or Resolved (`not_started` / `acknowledged`). Repeating
    /// the same outcome returns the stored record unchanged; any other
    /// finished receipt is refused with `receipt_not_uncertain`, so a normal
    /// completion or an earlier decision is never overwritten.
    pub async fn resolve_execution(
        &self,
        execution_id: String,
        request: ResolveExecution,
    ) -> Result<ExecutionReceiptRecord, StoreError> {
        validate(&request)?;
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let row: Option<(String, String, Option<String>, String)> = transaction
                .query_row(
                    "SELECT execution_id, run_id, approval_request_id, record FROM tool_executions WHERE execution_id = ?",
                    [&execution_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            let Some(row) = row else {
                return Err(StoreError::NotFound(format!(
                    "execution receipt {execution_id}"
                )));
            };
            let mut record = read_row(&row)?;
            if record.status != ExecutionReceiptStatus::Admitted {
                // A `completed` repeat is only the same decision when it
                // names the same result.
                return match &record.resolution {
                    Some(existing)
                        if existing.outcome == request.outcome
                            && (request.outcome != ResolutionOutcome::Completed
                                || record.result == request.result) =>
                    {
                        Ok(record)
                    }
                    _ => Err(StoreError::ReceiptTransition { reason: crate::ReceiptTransitionReason::NotUncertain, message: format!("{RECEIPT_NOT_UNCERTAIN}: execution {execution_id} is already finished") }),
                };
            }
            // The authoritative check: a non-terminal run may still execute
            // or resume the body, so resolving would erase real uncertainty.
            // A missing task row is not active.
            let active = get::<TaskRecord>(&transaction, "tasks", "id", &record.run_id)?
                .is_some_and(|task| !task.status.is_terminal());
            if active {
                return Err(StoreError::ReceiptTransition { reason: crate::ReceiptTransitionReason::RunActive, message: format!("{RECEIPT_RUN_ACTIVE}: run {} is still active", record.run_id) });
            }
            record.status = match request.outcome {
                ResolutionOutcome::Completed => ExecutionReceiptStatus::Completed,
                _ => ExecutionReceiptStatus::Resolved,
            };
            record.result = request.result;
            record.resolution = Some(ExecutionResolution {
                outcome: request.outcome,
                note: request.note,
                resolved_at: request.now,
                resolved_by: request.resolved_by,
            });
            let serialized = json(&transaction, &record)?;
            // Re-validate what is about to become durable.
            let check = agentkit_contracts::parse_javascript_json(&serialized)?;
            read_execution_receipt(
                check,
                &record.execution_id,
                &record.run_id,
                Some(record.approval_request_id.as_deref()),
            )?;
            transaction.execute(
                "UPDATE tool_executions SET record = ? WHERE execution_id = ?",
                params![serialized, record.execution_id],
            )?;
            clear_barriers(&transaction, &record)?;
            transaction.commit()?;
            Ok(record)
        })
        .await
    }
}

/// A barrier is keyed by the best-known identity of the effect it stands
/// for (`barrier_id_of`); drop every id this receipt could have produced so a
/// resolved effect is not blocked a second time by its own placeholder.
fn clear_barriers(
    db: &rusqlite::Connection,
    record: &ExecutionReceiptRecord,
) -> Result<(), StoreError> {
    for run in [Some(record.run_id.as_str()), None] {
        for args in [Some(record.args_sha256.as_str()), None] {
            let id = barrier_id_of(Some(&record.execution_id), run, &record.tool_name, args);
            db.execute(
                "DELETE FROM uncertainty_barriers WHERE barrier_id = ?",
                [id],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ArgsHashProvenance;
    use crate::execution_receipts::{UncertaintyBarrierRecord, insert_barrier};

    #[test]
    fn resolving_clears_only_the_barrier_that_names_this_receipt() {
        let (db, _) = crate::schema::open(std::path::Path::new(":memory:")).unwrap();
        let args = "a".repeat(64);
        let mine = barrier_id_of(Some("exec"), Some("run"), "write", Some(&args));
        let other = barrier_id_of(Some("other"), Some("run"), "write", Some(&args));
        for id in [&mine, &other] {
            let barrier = UncertaintyBarrierRecord {
                barrier_id: id.clone(),
                chat_id: Some("chat".into()),
                run_id: Some("run".into()),
                tool_name: "write".into(),
                args_sha256: Some(args.clone()),
                args_hash_provenance: ArgsHashProvenance::Canonical,
                reason: "unplaceable".into(),
            };
            assert!(insert_barrier(&db, &barrier).unwrap());
        }
        let record = ExecutionReceiptRecord {
            execution_id: "exec".into(),
            run_id: "run".into(),
            chat_id: "chat".into(),
            tool_call_id: "call".into(),
            tool_name: "write".into(),
            args_sha256: args,
            args_hash_provenance: ArgsHashProvenance::Canonical,
            context_sha256: "b".repeat(64),
            attempt_id: "attempt".into(),
            approval_request_id: None,
            approval_generation: None,
            status: ExecutionReceiptStatus::Resolved,
            result: None,
            resolution: None,
        };
        clear_barriers(&db, &record).unwrap();
        let left: Vec<String> = db
            .prepare("SELECT barrier_id FROM uncertainty_barriers")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(left, vec![other]);
    }
}
