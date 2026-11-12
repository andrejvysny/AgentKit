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
    AgentHost, CHAT_TURN_TASK_KIND, ChatProviderFactory, ContentAuthority, ExecutionContext,
    RunHandle, VerificationInput, read_session_descriptor,
};

#[cfg(test)]
use agentkit_engine::{Runtime, SystemRuntime};
