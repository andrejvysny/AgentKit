use crate::*;
use agentkit_store::{
    ApplyOutcome, ProposalPatch, ProposalRecord, ProposalStatus, ReceiptRecord,
    validate_apply_outcome,
};
use async_trait::async_trait;

#[async_trait]
pub trait ProposalApplyGuard: Send + Sync {
    /// Check current authority and revision without mutating domain state.
    async fn authorize(&self, proposal: &ProposalRecord) -> HostResult<()>;
    async fn authorize_dispatch(&self) -> HostResult<()> {
        Ok(())
    }
    /// Recheck revocable local state after the final asynchronous preflight.
    fn check_current(&self) -> HostResult<()> {
        Ok(())
    }
    fn fence(&self) -> Option<agentkit_core::DispatchFence> {
        None
    }
}

#[async_trait]
pub trait ProposalRevisionSource: Send + Sync {
    async fn current_revision(&self, scope_key: &str) -> HostResult<Option<String>>;
}

pub struct ProposalService {
    store: SqliteStore,
    applier: Arc<dyn ProposalApplier>,
    authorization: Arc<dyn Authorization>,
    runtime: SharedRuntime,
    revisions: Option<Arc<dyn ProposalRevisionSource>>,
}

impl ProposalService {
    pub fn new(
        store: SqliteStore,
        applier: Arc<dyn ProposalApplier>,
        authorization: Arc<dyn Authorization>,
        runtime: SharedRuntime,
    ) -> Self {
        Self {
            store,
            applier,
            authorization,
            runtime,
            revisions: None,
        }
    }

    /// Preflight is advisory to the domain transaction: the applier must still
    /// enforce the expected document revision atomically with its mutation.
    pub fn with_revisions(mut self, revisions: Arc<dyn ProposalRevisionSource>) -> Self {
        self.revisions = Some(revisions);
        self
    }

    async fn authorized(
        &self,
        id: &str,
        actor: &Value,
        action: &str,
    ) -> HostResult<ProposalRecord> {
        let proposal = self
            .store
            .get_proposal(id)
            .await?
            .ok_or_else(|| StoreError::NotFound(id.into()))?;
        self.authorization
            .authorize(
                actor,
                action,
                &json!({"type":"proposal",
            "id":id,"chatId":proposal.chat_id,"scopeKey":proposal.scope_key,
            "risk":proposal.risk,"revision":proposal.revision_at_create}),
            )
            .await?;
        Ok(proposal)
    }

    pub async fn approve(&self, id: &str, actor: &Value) -> HostResult<ProposalRecord> {
        self.approve_with_reason(id, actor, None).await
    }

    pub async fn approve_with_reason(
        &self,
        id: &str,
        actor: &Value,
        reason: Option<String>,
    ) -> HostResult<ProposalRecord> {
        self.authorized(id, actor, "approve").await?;
        Ok(self
            .store
            .transition_proposal(
                id,
                vec![ProposalStatus::Pending],
                ProposalStatus::Approved,
                ProposalPatch {
                    decision: Some(json!({"actor":"user","subject":actor})),
                    reason,
                    ..ProposalPatch::default()
                },
                self.runtime.now_ms(),
            )
            .await?)
    }

    pub async fn reject(
        &self,
        id: &str,
        actor: &Value,
        reason: Option<String>,
    ) -> HostResult<ProposalRecord> {
        self.authorized(id, actor, "reject").await?;
        Ok(self
            .store
            .transition_proposal(
                id,
                vec![ProposalStatus::Pending],
                ProposalStatus::Rejected,
                ProposalPatch {
                    decision: Some(json!({"actor":"user","subject":actor})),
                    reason,
                    ..ProposalPatch::default()
                },
                self.runtime.now_ms(),
            )
            .await?)
    }

    pub async fn apply(
        &self,
        id: &str,
        operation_id: &str,
        actor: &Value,
    ) -> HostResult<ReceiptRecord> {
        self.apply_inner(id, operation_id, actor, None).await
    }

    pub async fn apply_guarded(
        &self,
        id: &str,
        operation_id: &str,
        actor: &Value,
        guard: Arc<dyn ProposalApplyGuard>,
    ) -> HostResult<ReceiptRecord> {
        self.apply_inner(id, operation_id, actor, Some(guard)).await
    }

    async fn apply_inner(
        &self,
        id: &str,
        operation_id: &str,
        actor: &Value,
        guard: Option<Arc<dyn ProposalApplyGuard>>,
    ) -> HostResult<ReceiptRecord> {
        let proposal = self.authorized(id, actor, "apply").await?;
        if proposal
            .operation_id
            .as_deref()
            .is_some_and(|existing| existing != operation_id)
        {
            return Err(HostError::Execution("proposal_operation_mismatch".into()));
        }
        if let Some(receipt) = self.store.receipt(operation_id).await? {
            if receipt.proposal_id != id {
                return Err(HostError::Execution("proposal_operation_mismatch".into()));
            }
            return Ok(receipt);
        }
        let mut identity = mutation_identity(&proposal, operation_id);
        if proposal.status == ProposalStatus::Applying {
            return self.reconcile_one(&proposal, identity).await;
        }
        self.claim(id, operation_id).await?;
        if let Some(receipt) = self
            .authorize_effect(&proposal, operation_id, actor, guard.as_deref())
            .await?
        {
            return Ok(receipt);
        }
        identity.execution_fence = guard.as_ref().and_then(|guard| guard.fence());
        // An applier error does not undo a domain commit. Retain the applying
        // claim so recovery consults the domain receipt instead of retrying.
        let outcome = self
            .applier
            .apply(identity, Value::Array(proposal.operations))
            .await?;
        self.complete(id, operation_id, outcome).await
    }

    async fn authorize_effect(
        &self,
        proposal: &ProposalRecord,
        operation_id: &str,
        actor: &Value,
        guard: Option<&dyn ProposalApplyGuard>,
    ) -> HostResult<Option<ReceiptRecord>> {
        if let Err(error) = self.authorize_claim(proposal, actor).await {
            self.record_guard_refusal(proposal, operation_id).await?;
            return Err(error);
        }
        if let Some(guard) = guard
            && guard.authorize(proposal).await.is_err()
        {
            return self
                .record_guard_refusal(proposal, operation_id)
                .await
                .map(Some);
        }
        self.authorize_dispatch(proposal, operation_id, actor)
            .await?;
        if let Some(guard) = guard
            && (guard.authorize_dispatch().await.is_err() || guard.check_current().is_err())
        {
            return self
                .record_guard_refusal(proposal, operation_id)
                .await
                .map(Some);
        }
        Ok(None)
    }

    async fn claim(&self, id: &str, operation_id: &str) -> HostResult<()> {
        self.store
            .transition_proposal(
                id,
                vec![ProposalStatus::Approved],
                ProposalStatus::Applying,
                ProposalPatch {
                    operation_id: Some(operation_id.into()),
                    ..ProposalPatch::default()
                },
                self.runtime.now_ms(),
            )
            .await?;
        Ok(())
    }

    async fn authorize_claim(&self, proposal: &ProposalRecord, actor: &Value) -> HostResult<()> {
        if let (Some(source), Some(expected)) = (&self.revisions, &proposal.revision_at_create)
            && source.current_revision(&proposal.scope_key).await?.as_ref() != Some(expected)
        {
            return Err(HostError::Execution("revision_conflict".into()));
        }
        self.authorized(&proposal.id, actor, "apply").await?;
        Ok(())
    }

    async fn authorize_dispatch(
        &self,
        proposal: &ProposalRecord,
        operation_id: &str,
        actor: &Value,
    ) -> HostResult<()> {
        // Awaited policy or revision guards may revoke host authority after the
        // claim check. The last authorization must precede domain dispatch.
        if let Err(error) = self.authorized(&proposal.id, actor, "apply").await {
            self.record_guard_refusal(proposal, operation_id).await?;
            return Err(error);
        }
        Ok(())
    }

    async fn record_guard_refusal(
        &self,
        proposal: &ProposalRecord,
        operation: &str,
    ) -> HostResult<ReceiptRecord> {
        let failed_ops = proposal.operations.iter().enumerate().map(|(index, _)| json!({
            "opIndex":index,"error":"Apply authorization or revision check refused before domain dispatch."
        })).collect();
        let outcome = ApplyOutcome {
            status: "failed".into(),
            applied_ops: 0,
            failed_ops,
            result_json: None,
            revision: None,
        };
        Ok(self
            .store
            .complete_proposal(&proposal.id, operation, outcome, self.runtime.now_ms())
            .await?)
    }

    pub async fn reconcile(&self) -> HostResult<Vec<ReceiptRecord>> {
        let proposals = self
            .store
            .proposals(None, Some(ProposalStatus::Applying))
            .await?;
        let mut receipts = vec![];
        for proposal in proposals {
            let operation = proposal.operation_id.as_deref().ok_or_else(|| {
                HostError::Execution("applying proposal has no operation identity".into())
            })?;
            let identity = mutation_identity(&proposal, operation);
            if let Some(outcome) = self.applier.reconcile(identity).await? {
                receipts.push(self.complete(&proposal.id, operation, outcome).await?);
            }
        }
        Ok(receipts)
    }

    async fn reconcile_one(
        &self,
        proposal: &ProposalRecord,
        identity: MutationIdentity,
    ) -> HostResult<ReceiptRecord> {
        let outcome = self
            .applier
            .reconcile(identity.clone())
            .await?
            .ok_or_else(|| HostError::Execution("proposal_outcome_unresolved".into()))?;
        self.complete(&proposal.id, &identity.operation_id, outcome)
            .await
    }

    async fn complete(&self, id: &str, operation: &str, value: Value) -> HostResult<ReceiptRecord> {
        let outcome: ApplyOutcome = serde_json::from_value(value).map_err(|_| {
            HostError::Execution("invalid domain apply outcome; reconciliation required".into())
        })?;
        let proposal = self
            .store
            .get_proposal(id)
            .await?
            .ok_or_else(|| StoreError::NotFound(id.into()))?;
        validate_apply_outcome(&proposal, &outcome)?;
        Ok(self
            .store
            .complete_proposal(id, operation, outcome, self.runtime.now_ms())
            .await?)
    }
}

fn mutation_identity(proposal: &ProposalRecord, operation_id: &str) -> MutationIdentity {
    MutationIdentity {
        proposal_id: proposal.id.clone(),
        operation_id: operation_id.into(),
        execution_fence: None,
        fingerprint: agentkit_contracts::turn_fingerprint(&json!({
            "proposalId":proposal.id,"scopeKey":proposal.scope_key,
            "operations":proposal.operations,"envelope":proposal.envelope,
            "revision":proposal.revision_at_create
        })),
    }
}
