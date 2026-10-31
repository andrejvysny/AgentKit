use crate::{
    util::{all, get, json, required},
    *,
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

agentkit_contracts::impl_json_via_serde!(ProposalStatus);
agentkit_contracts::impl_json_via_serde!(ProposalRecord);
agentkit_contracts::impl_json_via_serde!(ReceiptRecord);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Approved,
    Applying,
    Applied,
    Failed,
    Rejected,
    Invalidated,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalRecord {
    pub id: String,
    pub chat_id: String,
    pub run_id: Option<String>,
    pub scope_key: String,
    pub action_id: Option<String>,
    pub tool_name: String,
    pub kind: String,
    pub risk: String,
    pub status: ProposalStatus,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub envelope: Value,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub operations: Vec<Value>,
    pub warnings: Vec<String>,
    pub truncated: bool,
    pub revision_at_create: Option<String>,
    pub operation_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "agentkit_contracts::deserialize_preserving"
    )]
    pub decision: Option<Value>,
    pub reason: Option<String>,
    pub created_at: i64,
    pub decided_at: Option<i64>,
    pub claimed_at: Option<i64>,
    pub applied_at: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct ProposalPatch {
    pub operation_id: Option<String>,
    pub decision: Option<Value>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyOutcome {
    pub status: String,
    pub applied_ops: u32,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub failed_ops: Vec<Value>,
    pub result_json: Option<String>,
    pub revision: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptRecord {
    pub proposal_id: String,
    pub operation_id: String,
    pub outcome: ApplyOutcome,
    pub created_at: i64,
}

pub fn validate_apply_outcome(proposal: &ProposalRecord, outcome: &ApplyOutcome) -> Result<()> {
    let mut failed = std::collections::BTreeSet::new();
    let invalid_failures = outcome.failed_ops.iter().any(|failure| {
        crate::util::safe_integer(&failure["opIndex"])
            .is_none_or(|index| index >= proposal.operations.len() as u64 || !failed.insert(index))
            || !failure["error"].is_string()
    });
    let invalid_count = (outcome.applied_ops as usize)
        .checked_add(outcome.failed_ops.len())
        .is_none_or(|count| count > proposal.operations.len());
    if !["applied", "partial", "failed"].contains(&outcome.status.as_str())
        || invalid_failures
        || invalid_count
        || outcome.status == "failed" && outcome.applied_ops != 0
        || outcome.status == "applied" && !outcome.failed_ops.is_empty()
    {
        return Err(StoreError::InvalidInput(
            "invalid domain apply outcome; reconciliation required".into(),
        ));
    }
    Ok(())
}

fn save(db: &Connection, proposal: &ProposalRecord) -> Result<()> {
    let status = json(db, &proposal.status)?.trim_matches('"').to_owned();
    db.execute(
        "UPDATE proposals SET status=?,operation_id=?,record=? WHERE id=?",
        params![
            status,
            proposal.operation_id,
            json(db, proposal)?,
            proposal.id
        ],
    )?;
    Ok(())
}

fn allowed(from: ProposalStatus, to: ProposalStatus) -> bool {
    use ProposalStatus::*;
    matches!(
        (from, to),
        (Pending, Approved | Rejected | Invalidated)
            | (Approved, Applying)
            | (Applying, Applied | Failed)
    )
}

impl SqliteStore {
    pub async fn create_proposal(&self, proposal: ProposalRecord) -> Result<ProposalRecord> {
        self.call(move |db| {
            if proposal.status != ProposalStatus::Pending || proposal.operation_id.is_some() {
                return Err(StoreError::InvalidInput(
                    "new proposal must be pending without operation id".into(),
                ));
            }
            if !["low", "medium", "high", "destructive"].contains(&proposal.risk.as_str()) {
                return Err(StoreError::InvalidInput("invalid risk level".into()));
            }
            let _: ChatRecord = required(db, "chats", "id", &proposal.chat_id)?;
            db.execute(
                "INSERT INTO proposals VALUES(?,?,?,?,?,?)",
                params![
                    proposal.id,
                    proposal.scope_key,
                    proposal.action_id,
                    "pending",
                    proposal.operation_id,
                    json(db, &proposal)?
                ],
            )?;
            Ok(proposal)
        })
        .await
    }

    pub async fn get_proposal(&self, proposal_id: &str) -> Result<Option<ProposalRecord>> {
        let proposal_id = proposal_id.to_owned();
        self.call(move |db| get(db, "proposals", "id", &proposal_id))
            .await
    }

    pub async fn proposals(
        &self,
        chat_id: Option<String>,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<ProposalRecord>> {
        self.call(move |db| {
            Ok(all::<ProposalRecord>(db, "proposals")?
                .into_iter()
                .filter(|p| {
                    chat_id.as_ref().is_none_or(|id| *id == p.chat_id)
                        && status.is_none_or(|s| s == p.status)
                })
                .collect())
        })
        .await
    }

    pub async fn proposal_by_action(
        &self,
        scope: &str,
        action: &str,
    ) -> Result<Option<ProposalRecord>> {
        let (scope, action) = (scope.to_owned(), action.to_owned());
        self.call(move |db| {
            Ok(all::<ProposalRecord>(db, "proposals")?
                .into_iter()
                .filter(|p| p.scope_key == scope && p.action_id.as_deref() == Some(&action))
                .max_by_key(|p| p.created_at))
        })
        .await
    }

    pub async fn transition_proposal(
        &self,
        proposal_id: &str,
        from: Vec<ProposalStatus>,
        to: ProposalStatus,
        patch: ProposalPatch,
        now: i64,
    ) -> Result<ProposalRecord> {
        let proposal_id = proposal_id.to_owned();
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let mut proposal: ProposalRecord =
                required(&transaction, "proposals", "id", &proposal_id)?;
            if !from.contains(&proposal.status) || !allowed(proposal.status, to) {
                return Err(StoreError::InvalidTransition(format!(
                    "{:?} to {to:?}",
                    proposal.status
                )));
            }
            if matches!(to, ProposalStatus::Applied | ProposalStatus::Failed) {
                return Err(StoreError::InvalidInput(
                    "terminal apply transition requires complete_proposal and bound receipt".into(),
                ));
            }
            if to == ProposalStatus::Applying
                && patch.operation_id.as_deref().is_none_or(str::is_empty)
            {
                return Err(StoreError::InvalidInput(
                    "apply claim requires operation id".into(),
                ));
            }
            if to != ProposalStatus::Applying && patch.operation_id.is_some() {
                return Err(StoreError::InvalidInput(
                    "operation id can only be set by apply claim".into(),
                ));
            }
            if patch.decision.as_ref().is_some_and(|d| {
                d.get("actor").and_then(Value::as_str) == Some("policy")
                    && d.get("policyId")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
            }) {
                return Err(StoreError::InvalidInput(
                    "policy decision requires policyId".into(),
                ));
            }
            proposal.status = to;
            if patch.operation_id.is_some() {
                proposal.operation_id = patch.operation_id;
            }
            if patch.decision.is_some() {
                proposal.decision = patch.decision;
                proposal.decided_at = Some(now);
            }
            if patch.reason.is_some() {
                proposal.reason = patch.reason;
            }
            if to == ProposalStatus::Applying {
                proposal.claimed_at = Some(now);
            }
            if to == ProposalStatus::Applied {
                proposal.applied_at = Some(now);
            }
            save(&transaction, &proposal)?;
            transaction.commit()?;
            Ok(proposal)
        })
        .await
    }

    /// Receipt identity is bound to both proposal and claimed operation. First committed outcome wins.
    pub async fn complete_proposal(
        &self,
        proposal_id: &str,
        operation_id: &str,
        outcome: ApplyOutcome,
        now: i64,
    ) -> Result<ReceiptRecord> {
        let (proposal_id, operation_id) = (proposal_id.to_owned(), operation_id.to_owned());
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let mut proposal: ProposalRecord =
                required(&transaction, "proposals", "id", &proposal_id)?;
            if proposal.operation_id.as_deref() != Some(&operation_id) {
                return Err(StoreError::InvalidInput(
                    "receipt does not match proposal operation".into(),
                ));
            }
            if let Some(receipt) =
                get::<ReceiptRecord>(&transaction, "receipts", "operation_id", &operation_id)?
            {
                if receipt.proposal_id != proposal_id {
                    return Err(StoreError::InvalidInput(
                        "receipt belongs to another proposal".into(),
                    ));
                }
                return Ok(receipt);
            }
            if proposal.status != ProposalStatus::Applying {
                return Err(StoreError::InvalidTransition(
                    "receipt requires applying proposal".into(),
                ));
            }
            validate_apply_outcome(&proposal, &outcome)?;
            proposal.status = if outcome.status == "failed" {
                ProposalStatus::Failed
            } else {
                ProposalStatus::Applied
            };
            proposal.applied_at = Some(now);
            let receipt = ReceiptRecord {
                proposal_id,
                operation_id,
                outcome,
                created_at: now,
            };
            transaction.execute(
                "INSERT INTO receipts VALUES(?,?,?)",
                params![
                    receipt.operation_id,
                    receipt.proposal_id,
                    json(&transaction, &receipt)?
                ],
            )?;
            save(&transaction, &proposal)?;
            transaction.commit()?;
            Ok(receipt)
        })
        .await
    }

    pub async fn receipt(&self, operation_id: &str) -> Result<Option<ReceiptRecord>> {
        let operation_id = operation_id.to_owned();
        self.call(move |db| get(db, "receipts", "operation_id", &operation_id))
            .await
    }

    pub async fn invalidate_pending(&self, scope: &str, revision: &str) -> Result<usize> {
        let (scope, revision) = (scope.to_owned(), revision.to_owned());
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let mut count = 0;
            for mut proposal in all::<ProposalRecord>(&transaction, "proposals")?
                .into_iter()
                .filter(|p| {
                    p.scope_key == scope
                        && p.status == ProposalStatus::Pending
                        && p.revision_at_create.as_deref() != Some(&revision)
                })
            {
                proposal.status = ProposalStatus::Invalidated;
                proposal.reason = Some("revision_conflict".into());
                save(&transaction, &proposal)?;
                count += 1;
            }
            transaction.commit()?;
            Ok(count)
        })
        .await
    }
}
