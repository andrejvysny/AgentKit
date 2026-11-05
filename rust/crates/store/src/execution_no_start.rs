//! Factual command refusals from trusted native execution, not operator recovery.
//! The caller must prove that its admitted command never reached OS spawn.
//! SQLite can authenticate admission identity, not independently observe effects.

use crate::execution_receipts::read_row;
use crate::{ExecutionReceiptStatus, LeaseFence, SqliteStore, StoreError};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const AUTHORITY_FIELD: &str = "nativeAdmissionAuthoritySha256";

pub(crate) fn admission_authority(proof: &LeaseFence, attempt_id: &str) -> String {
    let material = json!([
        proof.task_id,
        attempt_id,
        proof.lease_token,
        proof.fencing_token
    ]);
    Sha256::digest(material.to_string().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_result(result: &Value) -> Result<(), StoreError> {
    if result["ok"] != false
        || result["data"]["errorCode"] != "command_not_started"
        || result["data"]["phase"] != "before_spawn"
        || result["data"]["started"] != false
    {
        return Err(StoreError::InvalidInput(
            "a failed before-spawn command result is required".into(),
        ));
    }
    Ok(())
}

impl SqliteStore {
    /// Persist a trusted native driver's proof that no process started, even
    /// after its task lease ended. This is not authority to record an effect.
    /// Identity must match the exact original admission, including its private
    /// fencing digest. Legacy receipts without that digest remain uncertain.
    /// A terminal receipt is immutable; only an identical repeat is accepted.
    pub async fn complete_execution_not_started(
        &self,
        proof: LeaseFence,
        attempt_id: String,
        execution_id: String,
        result: Value,
    ) -> Result<bool, StoreError> {
        validate_result(&result)?;
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let row: Option<(String, String, Option<String>, String)> = transaction.query_row(
                "SELECT execution_id,run_id,approval_request_id,record FROM tool_executions WHERE execution_id=? AND run_id=?",
                params![execution_id, proof.task_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).optional()?;
            let Some(row) = row else { return Ok(false); };
            let mut record = read_row(&row)?;
            let mut raw = agentkit_contracts::parse_javascript_json(&row.3)?;
            let authority = admission_authority(&proof, &attempt_id);
            if !matches!(record.tool_name.as_str(), "run_command" | "run_tests")
                || record.attempt_id != attempt_id || raw[AUTHORITY_FIELD].as_str() != Some(authority.as_str()) {
                return Ok(false);
            }
            if record.status != ExecutionReceiptStatus::Admitted {
                return Ok(record.status == ExecutionReceiptStatus::Completed && record.result.as_ref() == Some(&result));
            }
            record.status = ExecutionReceiptStatus::Completed;
            record.result = Some(result);
            raw = serde_json::to_value(&record)?;
            raw[AUTHORITY_FIELD] = Value::String(authority);
            let serialized = crate::util::json(&transaction, &raw)?;
            let updated = transaction.execute(
                "UPDATE tool_executions SET record=? WHERE execution_id=? AND run_id=? AND record=?",
                params![serialized, execution_id, proof.task_id, row.3],
            )? == 1;
            transaction.commit()?;
            Ok(updated)
        }).await
    }
}
