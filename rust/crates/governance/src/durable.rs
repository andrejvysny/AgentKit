use crate::{ApprovalGrant, StandingApprovalPolicy, ToolContributionContext, ToolGuardContext};
use agentkit_core::{CoreError, ToolContext, ToolDefinition, ToolResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// Outcome of the replay barrier that runs before the guard chain.
pub enum PreGuard {
    Proceed,
    Replay(ToolResult),
    /// The operation is uncertain (admitted without a durable result) and
    /// no answer covers this generation. The wrapper folds it into the
    /// approval flow: the human is asked, the ask carries the exact
    /// unresolved generation, and a refusal is the fallback when this host
    /// cannot park.
    Uncertain {
        refusal: ToolResult,
        generation: String,
    },
}

/// Outcome of atomic admission after the guard chain.
pub enum Admission {
    /// Non-mutating tool: no receipt required.
    Skipped,
    Granted {
        execution_id: String,
    },
    /// Boxed: refusals carry the full model-facing envelope.
    Refused(Box<ToolResult>),
}

/// Handle for a native command whose completion must outlive the caller's
/// await. Dropping it without `finish` records the completion as failed.
pub trait CommandCompletion: Send {
    fn finish(self: Box<Self>, error: Option<String>);
}

/// The host-owned durable execution bundle (ADR 0020) as seen by staging:
/// receipts, replay, approvals and standing grants for one staged run.
#[async_trait]
pub trait DurableExecutionPort: Send + Sync {
    /// The answered approval this attempt resumes for a tool call, if any.
    fn approval_grant(&self, tool_call_id: &str) -> Option<ApprovalGrant>;
    /// The exact-call remembering policy for approvable refusals.
    fn standing_policy(&self) -> Option<Arc<dyn StandingApprovalPolicy>>;
    /// Fingerprint of everything that must stay identical for a recorded
    /// result to be a faithful replay of the same operation.
    fn execution_context_sha256(
        &self,
        contribution: &ToolContributionContext,
        tool: &ToolDefinition,
        effect_context: Option<&Value>,
    ) -> String;
    /// True when the session carries an active standing grant for `digest`.
    async fn has_active_standing_grant(&self, digest: &str) -> Result<bool, CoreError>;
    /// Replay barrier + uncertainty probe, run before the guard chain.
    async fn before_guards(
        &self,
        context: &ToolContext,
        contribution: &ToolGuardContext,
        tool: &ToolDefinition,
        effect_context: Option<&Value>,
        input: &Value,
    ) -> Result<PreGuard, CoreError>;
    /// Atomic admission after the guard chain, before the body.
    async fn admit(
        &self,
        context: &ToolContext,
        contribution: &ToolGuardContext,
        tool: &ToolDefinition,
        effect_context: Option<&Value>,
        input: &Value,
    ) -> Result<Admission, CoreError>;
    /// Best-effort ledger record of a schema refusal.
    async fn record_schema_refusal(
        &self,
        context: &ToolContext,
        contribution: &ToolGuardContext,
        tool: &ToolDefinition,
        effect_context: Option<&Value>,
        input: &Value,
        message: &str,
    );
    /// Persist the result before delivery.
    async fn complete(&self, execution_id: &str, result: &ToolResult) -> Result<(), CoreError>;
    /// Persist a factual no-start result for a native command.
    async fn complete_not_started(
        &self,
        execution_id: &str,
        result: &ToolResult,
    ) -> Result<(), CoreError>;
    /// Register a native command completion the host must wait for on drain.
    fn register_command_completion(&self) -> Box<dyn CommandCompletion>;
}
