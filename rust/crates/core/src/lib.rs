//! Provider-neutral asynchronous chat execution. Secrets and connection policy belong to hosts.
mod budget;
mod call_pipeline;
mod dispatch;
mod envelope;
mod examples_render;
pub mod failure_catalog;
mod failure_table;
mod failure_table_agents;
mod failure_table_files;
mod layout;
mod manifest;
mod model_profile;
pub mod permission;
mod provider_identity;
mod provider_retry;
mod registry;
mod run;
mod tool_execution;
mod tool_record;
mod types;

pub use call_pipeline::{AliasRule, AliasTable, Normalised, UNNAMED_TOOL, closest_names, normalise};
pub use budget::{BudgetCheckpoint, BudgetLimits, BudgetSnapshot, ExecutionBudget};
// The event type names the public `RunHandle`/`RunEventSink` surfaces; hosts
// (and this crate's own tests) need it without a parallel contracts import.
pub use agentkit_contracts::AiRunEvent;
pub use agentkit_kernel::{Runtime, SystemRuntime};
pub use dispatch::{DispatchFence, DispatchGuard};
pub use examples_render::{ExampleLane, RenderedExamples, presented_description, render_examples};
pub use layout::{
    Freeze, LayoutError, PrefixDigests, PromptLayout, SEGMENT_METADATA_KEY, Segment, SystemBlock,
    cache_key, freeze, segment_of, tools_digest,
};
pub use manifest::{ToolManifest, ToolSurface, manifest_hash};
pub use model_profile::{
    ANTHROPIC_PROFILE_ID, AiModelToolProfile, BODY_EXTRA_ALLOW_LIST, GENERIC_PROFILE_ID, LimitFacts,
    ModelFacts, OPENAI_PROFILE_ID, ProfileError, ProfileQuery, QWEN_PROFILE_ID, builtin_profile,
    frozen_profile, model_profile_digest, resolve_profile, stamp_model_profile, validate_profile,
    validate_profile_json,
    // T2.2: applying the frozen profile to a request.
    ProfileNote, QWEN_EFFORT_TEMPLATE_KEY, REASONING_HISTORY_FIELD, REASONING_METADATA_KEY,
    RequestSettings, Salvage, SalvagedCall, THINKING_TEMPLATE_KEY, TOOL_RESPONSE_CLOSE,
    TOOL_RESPONSE_OPEN, ToolChoice, apply_message_rules, escape_tool_response_tags,
    nudge_after_announced_call, resolve_request_settings, salvage_tool_calls, wire_tool_choice,
};
pub use provider_identity::{AuthKind, ConnectionIdentity};
pub use provider_retry::{RetryHooks, RetryPolicy, RetryProvider};
pub use registry::{RegistryError, ToolRegistry};
pub use run::{RunEventGate, RunHandle, RunOptions, RunResult, Terminal, run_chat};
pub use tokio_util::sync::CancellationToken;
pub use tool_execution::dedupe as dedupe_tool_call_ids;
pub use types::*;

/// Model-facing envelope for a tool result. Hosts reconciling durable history
/// must render a replayed recorded result exactly as live dispatch did.
pub fn tool_model_envelope(result: &ToolResult, limits: &ToolLimits) -> String {
    envelope::stringify(&envelope::build(result, limits))
}
