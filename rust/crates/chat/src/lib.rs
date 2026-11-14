//! Chat execution, tool governance, and proposal application.

mod budget;
mod chat;
mod continuation;
mod emulated_tool_call;
mod error;
mod ports;
mod project_instructions;
mod provider_registry;

pub use budget::{
    RoleTaskOverrides, parse_budget_limits, restore_execution_budget, task_overrides,
};
pub use chat::{
    AttachmentBudgets, ChatExecutor, ChatExecutorOptions, ChatHookTimeouts, ChatHostExt,
    CorrectionConfig, SubmitChat,
};
pub use emulated_tool_call::{EMULATED_TOOL_CALL_MESSAGE, looks_like_emulated_tool_call};
pub use error::{ChatError, ChatResult};
pub use ports::{
    AttachmentResolveContext, AttachmentResolver, Authorization, ContextProvider,
    DelegationGuidance, MutationIdentity, ProposalApplier, ResolvedAttachment, UsageAuthorizer,
    is_mutating_subagent_tool,
};
pub use project_instructions::{
    FileProjectInstructions, MAX_INSTRUCTION_BYTES, PROJECT_INSTRUCTION_FILES,
    ProjectInstructionsBlock, ProjectInstructionsSource, ProjectInstructionsText,
    append_to_system_prompt, load_project_instructions,
};
pub use provider_registry::ProviderRegistry;

use agentkit_governance::{ToolContributionContext, ToolContributor, ToolGuard};
use agentkit_store::StoreError;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use agentkit_engine::{ExecutionFailure, TaskExecutor, VerificationHook, VerificationReport};

use agentkit_sessions::{
    CHAT_TURN_TASK_KIND, ChatProviderFactory, ContentAuthority, read_session_descriptor,
};

/// The engine's generic types at the one repository the product ships: the
/// SQLite adapter (ADR 0024 §2). The adapter arrives from the composition
/// root; these names exist so the context's signatures say the repository
/// once, in this file, rather than at every call site.
pub type AgentHost = agentkit_engine::AgentHost<agentkit_store::SqliteStore>;
pub type ExecutionContext = agentkit_engine::ExecutionContext<agentkit_store::SqliteStore>;
pub type HostOptions = agentkit_engine::HostOptions<agentkit_store::SqliteStore>;
pub type RunHandle = agentkit_engine::RunHandle<agentkit_store::SqliteStore>;
pub type VerificationInput = agentkit_engine::VerificationInput<agentkit_store::SqliteStore>;
pub type WorkerHandle = agentkit_engine::WorkerHandle<agentkit_store::SqliteStore>;

#[cfg(test)]
use agentkit_engine::{Runtime, SystemRuntime};
