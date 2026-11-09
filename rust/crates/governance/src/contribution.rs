use crate::{DurableExecutionPort, GovernanceResult};
use agentkit_core::{Tool, ToolLimits};
use async_trait::async_trait;
use serde_json::Value;
use std::{collections::BTreeSet, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ToolContributionContext {
    pub chat_id: Option<String>,
    pub run_id: String,
    pub scope_id: Option<String>,
    pub actor: Value,
    pub actor_id: Option<String>,
    pub bindings: Vec<Value>,
    pub limits: ToolLimits,
    pub cancellation: CancellationToken,
    pub has_primary_binding: bool,
    pub timeout_ms: u64,
    /// Host-owned durable execution bundle (ADR 0020). When present, guarded
    /// mutating tools admit/complete receipts around their body; when absent,
    /// `effect: write` tools are refused rather than run unguarded.
    pub durable: Option<Arc<dyn DurableExecutionPort>>,
}

/// A staged write tool whose effects are proposals governed by the host.
/// The host decides whether the tool may run for the staged actor.
pub trait GovernedWriteTool: Tool {
    /// True when the tool was built for this actor identity.
    fn matches_actor(&self, actor: &Value, actor_id: Option<&str>) -> bool;
}

#[derive(Clone)]
pub enum ContributedTool {
    Read(Arc<dyn Tool>),
    /// Trusted hosts retain direct effects; guards authorize them and the domain
    /// owns durable receipts and reconciliation of uncertain external effects.
    DirectWrite(Arc<dyn Tool>),
    Write(Arc<dyn GovernedWriteTool>),
}

#[async_trait]
pub trait ToolContributor: Send + Sync {
    fn namespace(&self) -> &str;
    fn privileged(&self) -> bool {
        false
    }
    fn unbound_tool_names(&self) -> Option<BTreeSet<String>> {
        None
    }
    async fn contribute(
        &self,
        context: &ToolContributionContext,
    ) -> GovernanceResult<Vec<ContributedTool>>;
}
