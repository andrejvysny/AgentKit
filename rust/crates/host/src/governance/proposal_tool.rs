use super::proposal_support::{
    NATIVE_GOVERNANCE_IDENTITY_VERSION, action_id, missing_chat, query, receipt_result, result,
};
use super::{ToolWritePolicy, WriteQuery, WriteRisk};
use crate::{
    Authorization, HostError, HostResult, ProposalApplyGuard, ProposalService, SharedRuntime,
};
use agentkit_core::{CoreError, Tool, ToolContext, ToolDefinition, ToolResult};
use agentkit_store::{ProposalPatch, ProposalRecord, ProposalStatus, ReceiptRecord, SqliteStore};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const AUTO_APPLY_FAILED_MESSAGE: &str = "Auto-apply outcome is unconfirmed. Inspect proposal and domain state before retrying; use a new action_id only for remaining work.";

#[derive(Clone, Copy)]
pub enum WriteExecutionMode {
    ProposeOnly,
    DirectWhenAuthorized,
}

/// Builders describe intent only. Domain mutation belongs exclusively to ProposalApplier.
pub struct ProposalBuild {
    pub scope_key: String,
    pub kind: String,
    pub risk: WriteRisk,
    pub operations: Vec<Value>,
    pub warnings: Vec<String>,
    pub truncated: bool,
    pub envelope: Value,
    pub revision_at_create: Option<String>,
}

#[async_trait]
pub trait ProposalBuilder: Send + Sync {
    async fn build(&self, context: &ToolContext, input: &Value) -> HostResult<ProposalBuild>;
    async fn current_revision(&self, _scope_key: &str) -> HostResult<Option<String>> {
        Ok(None)
    }
}

pub struct ProposalToolOptions {
    pub store: SqliteStore,
    pub service: Arc<ProposalService>,
    pub builder: Arc<dyn ProposalBuilder>,
    pub authorization: Arc<dyn Authorization>,
    pub policy: Arc<dyn ToolWritePolicy>,
    pub runtime: SharedRuntime,
    pub actor: Value,
    pub actor_id: Option<String>,
    pub mode: WriteExecutionMode,
    pub policy_id: String,
}

pub struct GovernedProposalTool {
    definition: ToolDefinition,
    options: ProposalToolOptions,
}

impl GovernedProposalTool {
    pub fn new(mut definition: ToolDefinition, options: ProposalToolOptions) -> HostResult<Self> {
        if definition.effect != "write" || options.actor.is_null() || options.policy_id.is_empty() {
            return Err(HostError::Execution(
                "write_tool_requires_write_effect_and_trusted_actor".into(),
            ));
        }
        let notice = match options.mode {
            WriteExecutionMode::ProposeOnly => {
                "Stages a write proposal; confirmation is required before applying."
            }
            WriteExecutionMode::DirectWhenAuthorized => {
                "Stages a write proposal; applies only with current host authorization and write-policy consent."
            }
        };
        definition.description = format!("{notice} {}", definition.description);
        Ok(Self {
            definition,
            options,
        })
    }

    pub(super) fn matches_actor(&self, actor: &Value, actor_id: Option<&str>) -> bool {
        self.options.actor == *actor && self.options.actor_id.as_deref() == actor_id
    }

    async fn stage(
        &self,
        context: &ToolContext,
        input: &Value,
    ) -> HostResult<(ProposalRecord, WriteQuery, bool)> {
        let chat = context
            .chat_id
            .as_ref()
            .ok_or_else(|| HostError::Execution("write_tool_requires_chat".into()))?;
        self.options.authorization.authorize(&self.options.actor, "tool.execute", &json!({
            "toolName":self.definition.name,"effect":"write","chatId":chat,"scopeKey":context.scope_id
        })).await?;
        let built = self.options.builder.build(context, input).await?;
        if built.scope_key.is_empty() || built.kind.is_empty() || !built.envelope.is_object() {
            return Err(HostError::Execution("invalid_proposal_build".into()));
        }
        let query = query(
            &self.definition.name,
            chat,
            input,
            &built,
            self.options.actor_id.clone(),
        );
        self.options
            .authorization
            .authorize(
                &self.options.actor,
                "proposal.stage",
                &serde_json::to_value(&query)
                    .map_err(|_| HostError::Execution("invalid_write_query".into()))?,
            )
            .await?;
        let record = self.record(context, input, built, &query);
        if let Some(prior) = self.prior(&record).await? {
            return Ok((prior, query, false));
        }
        match self.options.store.create_proposal(record.clone()).await {
            Ok(created) => Ok((created, query, true)),
            Err(error) => match self.prior(&record).await? {
                Some(prior) => Ok((prior, query, false)),
                None => Err(error.into()),
            },
        }
    }

    fn record(
        &self,
        context: &ToolContext,
        input: &Value,
        built: ProposalBuild,
        query: &WriteQuery,
    ) -> ProposalRecord {
        let mut envelope = built.envelope;
        envelope["__agentkitInvocation"] = json!({"identityVersion":NATIVE_GOVERNANCE_IDENTITY_VERSION,"actor":self.options.actor,"actorId":query.actor_id,
            "payloadFingerprint":query.payload_fingerprint,"revision":query.revision});
        let (action, warning) = action_id(input, query);
        let mut warnings = built.warnings;
        warnings.extend(warning);
        ProposalRecord {
            id: self.options.runtime.id(),
            chat_id: query.chat_id.clone(),
            run_id: Some(context.run_id.clone()),
            scope_key: query.scope_key.clone(),
            action_id: action,
            tool_name: self.definition.name.clone(),
            kind: built.kind,
            risk: built.risk.as_str().into(),
            status: ProposalStatus::Pending,
            envelope,
            operations: built.operations,
            warnings,
            truncated: built.truncated,
            revision_at_create: built.revision_at_create,
            operation_id: None,
            decision: None,
            reason: None,
            created_at: self.options.runtime.now_ms(),
            decided_at: None,
            claimed_at: None,
            applied_at: None,
        }
    }

    async fn prior(&self, record: &ProposalRecord) -> HostResult<Option<ProposalRecord>> {
        let Some(action) = &record.action_id else {
            return Ok(None);
        };
        let prior = self
            .options
            .store
            .proposal_by_action(&record.scope_key, action)
            .await?;
        Ok(prior.filter(|prior| {
            !matches!(
                prior.status,
                ProposalStatus::Rejected | ProposalStatus::Invalidated
            )
        }))
    }

    async fn execute_staged(&self, context: &ToolContext, input: &Value) -> HostResult<ToolResult> {
        let (proposal, query, created) = self.stage(context, input).await?;
        if !created {
            return self.duplicate(context, &proposal).await;
        }
        let can_apply = matches!(self.options.mode, WriteExecutionMode::DirectWhenAuthorized)
            && !context.cancellation.is_cancelled()
            && !proposal.operations.is_empty()
            && !proposal.truncated
            && (proposal.risk != "destructive" || proposal.warnings.is_empty())
            && (query.actor_id.is_none() || proposal.action_id.is_some())
            && self.options.policy.is_auto_apply_allowed(&query);
        if !can_apply {
            return Ok(result(
                context,
                &proposal,
                true,
                if proposal.warnings.is_empty() {
                    "pending"
                } else {
                    "partial"
                },
                0,
                None,
            ));
        }
        match self.auto_apply(context, &proposal, query).await {
            Ok(receipt) => Ok(receipt_result(context, &proposal, &receipt, false)),
            Err(_) => {
                let recorded = self
                    .options
                    .store
                    .get_proposal(&proposal.id)
                    .await?
                    .unwrap_or(proposal);
                Ok(result(
                    context,
                    &recorded,
                    false,
                    "partial",
                    0,
                    Some(AUTO_APPLY_FAILED_MESSAGE),
                ))
            }
        }
    }

    async fn duplicate(
        &self,
        context: &ToolContext,
        proposal: &ProposalRecord,
    ) -> HostResult<ToolResult> {
        self.options
            .authorization
            .authorize(
                &self.options.actor,
                "proposal.read",
                &json!({
                    "id":proposal.id,"chatId":proposal.chat_id,"scopeKey":proposal.scope_key
                }),
            )
            .await?;
        if proposal.status == ProposalStatus::Applied
            && let Some(operation) = &proposal.operation_id
            && let Some(receipt) = self.options.store.receipt(operation).await?
        {
            return Ok(receipt_result(context, proposal, &receipt, true));
        }
        Ok(result(
            context,
            proposal,
            false,
            "partial",
            0,
            Some(
                "Duplicate action blocked. Inspect the existing proposal; use a new action_id only for a new intent.",
            ),
        ))
    }

    async fn auto_apply(
        &self,
        context: &ToolContext,
        proposal: &ProposalRecord,
        query: WriteQuery,
    ) -> HostResult<ReceiptRecord> {
        let guard = Arc::new(PolicyGuard {
            policy: self.options.policy.clone(),
            builder: self.options.builder.clone(),
            query,
            cancellation: context.cancellation.clone(),
            dispatch_guard: context.dispatch_guard.clone(),
        });
        guard.authorize(proposal).await?;
        self.options.authorization.authorize(&self.options.actor, "approve", &json!({
            "id":proposal.id,"chatId":proposal.chat_id,"scopeKey":proposal.scope_key,"risk":proposal.risk
        })).await?;
        self.options.store.transition_proposal(&proposal.id, vec![ProposalStatus::Pending], ProposalStatus::Approved,
            ProposalPatch { decision: Some(json!({"actor":"policy","policyId":self.options.policy_id,"subject":self.options.actor})),
                ..ProposalPatch::default() }, self.options.runtime.now_ms()).await?;
        self.options
            .service
            .apply_guarded(
                &proposal.id,
                &self.options.runtime.id(),
                &self.options.actor,
                guard,
            )
            .await
    }
}

#[async_trait]
impl Tool for GovernedProposalTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        if context.cancellation.is_cancelled() {
            return Err(CoreError::new("cancelled", "aborted"));
        }
        if context.chat_id.is_none() {
            return Ok(missing_chat(&context));
        }
        self.execute_staged(&context, &input).await.map_err(|_| {
            CoreError::new(
                "write_tool_failed",
                "Write staging failed; inspect proposal state before retrying.",
            )
        })
    }
}

struct PolicyGuard {
    policy: Arc<dyn ToolWritePolicy>,
    builder: Arc<dyn ProposalBuilder>,
    query: WriteQuery,
    cancellation: CancellationToken,
    dispatch_guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
}

#[async_trait]
impl ProposalApplyGuard for PolicyGuard {
    async fn authorize_dispatch(&self) -> HostResult<()> {
        if let Some(guard) = &self.dispatch_guard {
            guard
                .check()
                .await
                .map_err(|_| HostError::Execution("dispatch_fence_lost".into()))?;
        }
        Ok(())
    }

    fn check_current(&self) -> HostResult<()> {
        if self.cancellation.is_cancelled() || !self.policy.is_auto_apply_allowed(&self.query) {
            return Err(HostError::Execution("write_policy_revoked".into()));
        }
        Ok(())
    }

    fn fence(&self) -> Option<agentkit_core::DispatchFence> {
        self.dispatch_guard.as_ref().map(|guard| guard.fence())
    }

    async fn authorize(&self, proposal: &ProposalRecord) -> HostResult<()> {
        self.check_current()?;
        if self.builder.current_revision(&proposal.scope_key).await? != self.query.revision {
            return Err(HostError::Execution("revision_conflict".into()));
        }
        self.check_current()
    }
}
