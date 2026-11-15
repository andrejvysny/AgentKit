//! Provider-neutral asynchronous chat execution. Secrets and connection policy belong to hosts.
mod budget;
mod call_pipeline;
mod deferred;
mod dialect;
mod dispatch;
mod envelope;
mod examples_render;
pub mod failure_catalog;
mod failure_table;
mod failure_table_agents;
mod failure_table_files;
mod layout;
mod manifest;
mod meta_call;
mod meta_tools;
mod model_profile;
pub mod permission;
mod provider_identity;
mod provider_retry;
mod registry;
mod run;
mod tool_execution;
mod toolset;
mod tool_record;
mod types;

pub use call_pipeline::{AliasRule, AliasTable, Normalised, UNNAMED_TOOL, closest_names, normalise};
pub use budget::{BudgetCheckpoint, BudgetLimits, BudgetSnapshot, ExecutionBudget};
// The event type names the public `RunHandle`/`RunEventSink` surfaces; hosts
// (and this crate's own tests) need it without a parallel contracts import.
pub use agentkit_contracts::AiRunEvent;
pub use agentkit_kernel::{Runtime, SystemRuntime};
pub use deferred::{
    Catalog, CatalogCell, CatalogEntry, Classification, DirectSet, LoadMode, META_TOOLS, Split,
    TOOL_CALL, TOOL_DESCRIBE, TOOL_SEARCH, TOOLSET_LOAD, ToolLoading, catalog_index, is_meta_tool,
    namespace_of, toolset_index,
};
// T4.1: toolsets, cards, guides and promotion (ADR 0033).
pub use toolset::{
    Guide, META_TOOLSET, Promotion, PromotionCell, PromotionObserver, Promotions, Toolset,
    ToolsetView, guide_placeholders, meta_toolset, render_guide,
};
// T3.1: the presentation layer; canonical ids stay, dialects render and translate.
pub use dialect::{
    COMMON_ID as COMMON_DIALECT_ID, COMMON_VERSION as COMMON_DIALECT_VERSION, Dialect,
    DialectError, ToolForm, Translated, WireTool, common as common_dialect,
    lookup as lookup_dialect, present_history, session_dialect,
};
// T3.2: the Anthropic dialect and its built-in tool markers.
pub use dialect::{
    ANTHROPIC_ID as ANTHROPIC_DIALECT_ID, BASH_NAME as ANTHROPIC_BASH_NAME,
    BASH_TYPE as ANTHROPIC_BASH_TYPE, BUILTIN_KEY as BUILTIN_TOOL_KEY,
    TEXT_EDITOR_NAME as ANTHROPIC_TEXT_EDITOR_NAME, TEXT_EDITOR_TYPE as ANTHROPIC_TEXT_EDITOR_TYPE,
    anthropic as anthropic_dialect, builtin_tool_type,
};
// T3.3: the OpenAI dialect (patch and shell tools) and strict schemas.
pub use dialect::{
    OPENAI_CHAT_ID as OPENAI_CHAT_DIALECT_ID, OPENAI_ID as OPENAI_DIALECT_ID,
    OPENAI_PATCH_NAME, OPENAI_SHELL_NAME, drop_nulls as drop_null_arguments, is_strict_compliant,
    openai as openai_dialect, openai_chat as openai_chat_dialect, strict_dialect, strict_schema,
};
pub use dispatch::{DispatchFence, DispatchGuard};
pub use meta_call::unwrap_tool_call;
pub use meta_tools::{
    meta_definitions, tool_call_definition, tool_describe_definition, tool_search_definition,
};
pub use examples_render::{ExampleLane, RenderedExamples, presented_description, render_examples};
pub use layout::{
    Freeze, LayoutError, PrefixDigests, PromptLayout, SEGMENT_METADATA_KEY, Segment, SystemBlock,
    cache_key, freeze, segment_of, tools_digest,
};
    toolset_load_definition,
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
