use crate::{GovernanceResult, ToolContributionContext};
use agentkit_core::ToolDefinition;
use async_trait::async_trait;
use serde_json::Value;

#[derive(Clone)]
pub struct ToolGuardContext {
    pub contribution: ToolContributionContext,
    pub namespace: String,
    pub tool: ToolDefinition,
    /// The live call's arguments, present on `canExecute` only (TS
    /// `ToolGuardContext.arguments?`; visibility never sees them). Guards
    /// that classify command text depend on this field.
    pub arguments: Option<Value>,
}

pub enum ToolGuardVerdict {
    Allowed,
    Refused(String),
    /// The call could run IF a human approves it (ADR 0016 §2, ADR 0019).
    /// The wrapper — not the guard — assembles the durable request: it owns
    /// the argument hash, the one-use approvals, and the standing grants.
    /// A `Refused` anywhere in the chain wins over any number of these
    /// (denial is final; approval cannot reopen a role ceiling).
    AwaitingHuman(ApprovalAsk),
}

/// The approvable refusal a guard offers: the model-facing reason, the text
/// the human approves on, and the mode a plan REQUESTS (recorded for the
/// human, never obeyed — the model may not author its own permissions).
/// The durable wrapper adds `uncertainty_generation` (the execution id of
/// the unresolved admitted receipt) so an approval is bound to the exact
/// uncertain effect it authorizes; a later uncertainty needs a new answer.
#[derive(Clone, Debug, Default)]
pub struct ApprovalAsk {
    pub reason: String,
    pub preview: Option<String>,
    pub requested_mode: Option<String>,
    pub uncertainty_generation: Option<String>,
}

#[async_trait]
pub trait ToolGuard: Send + Sync {
    async fn is_visible(&self, _context: &ToolGuardContext) -> GovernanceResult<bool> {
        Ok(true)
    }
    async fn can_execute(&self, _context: &ToolGuardContext) -> GovernanceResult<ToolGuardVerdict> {
        Ok(ToolGuardVerdict::Allowed)
    }
}
