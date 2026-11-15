pub use agentkit_governance::{Authorization, MutationIdentity, ProposalApplier};
use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::ChatResult;

/// Subagent coordination tools that change durable child state. They keep
/// `effect: "read"` so the permission modes still admit them, but must carry
/// execution receipts: on an attempt replay the recorded result is served
/// instead of re-running the spawn/message/cancel/resume.
pub fn is_mutating_subagent_tool(name: &str) -> bool {
    matches!(
        name,
        "subagent_spawn" | "subagent_message" | "subagent_cancel" | "subagent_resume"
    )
}

/// Per-turn delegation guidance (subagent instructions) consulted by the
/// executor for session turns. Implementations read durable/host config,
/// never process-memory caches a restart could not rebuild.
#[async_trait]
pub trait DelegationGuidance: Send + Sync {
    async fn guidance(&self, chat_id: &str) -> Option<String>;
}

/// What a [`TurnContextHook`] sees when the turn's request is built.
pub struct TurnContextInput<'a> {
    pub chat_id: &'a str,
    /// The user's text of this turn.
    pub request: &'a str,
    /// The tools the request carries (the session manifest once frozen).
    pub tools: &'a [agentkit_core::ToolDefinition],
    /// Every registered toolset and the toolset of each offered tool.
    pub view: &'a agentkit_core::ToolsetView,
}

/// Per-turn context port (T4.4). The line it returns is appended to the
/// user-side message of the turn only; it never enters the cached prefix
/// (S1-S4) and is not persisted. `observe_tool_record` sees every
/// `run.tool.record` event of the turn's run so usage can feed the advice.
#[async_trait]
pub trait TurnContextHook: Send + Sync {
    async fn line(&self, input: &TurnContextInput<'_>) -> Option<String>;
    fn observe_tool_record(
        &self,
        _chat_id: &str,
        _record: &agentkit_contracts::AiRunToolRecordEventData,
    ) {
    }
}

#[async_trait]
pub trait ContextProvider: Send + Sync {
    async fn resolve(&self, input: Value, cancel: CancellationToken) -> ChatResult<Value>;
}

#[async_trait]
pub trait AttachmentResolver: Send + Sync {
    async fn resolve(
        &self,
        reference: &str,
        context: &AttachmentResolveContext,
        cancel: CancellationToken,
    ) -> ChatResult<Option<ResolvedAttachment>>;
}

pub struct AttachmentResolveContext {
    pub chat_id: String,
}

pub struct ResolvedAttachment {
    pub media_type: String,
    pub base64: String,
}

#[async_trait]
pub trait UsageAuthorizer: Send + Sync {
    async fn authorize(&self, task_id: &str, usage: &Value) -> ChatResult<()>;
    async fn record(&self, _usage: &Value) -> ChatResult<()> {
        Ok(())
    }
}
