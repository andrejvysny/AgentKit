//! Agents context: agent definitions, teammate admission, subagent
//! delegation and the advisor/watchdog, layered over the host engine,
//! sessions and chat.

mod advisor;
mod agents;
mod errors;
mod subagents;
mod teammates;

pub use advisor::{
    ADVISOR_TOOL_NAMES, AdvisorConfig, AdvisorContributor, AdvisorEngine, AdvisorEventSink,
    AdvisorReport, AdvisorToolSource, AdvisorTools, Finding, FindingCategory, FindingSeverity,
    ReviewOrigin, ReviewRequest, ReviewReservation, StaticToolSource, Watchdog, WatchdogRunHooks,
    WatchdogSignals, advisor_run_chat_id, advisor_tool_definition, explicit_review,
    is_advisor_chat_id,
};
pub use agents::{AgentDefinitionService, validate_agent_definition};
pub use errors::{AgentsError, AgentsResult, LeaseHeld, RoleDenial, SnapshotError};
pub use subagents::{
    AwaitDecision, AwaitMode, AwaitOutcome, AwaitResult, BUILTIN_ROLES, Brief, BriefBudget,
    BriefError, BriefScope, BudgetExhausted, BudgetLines, ChildReport, ContextRef,
    DELEGATION_GUIDANCE, DELEGATION_TOOLSET, EffectiveRole, EnvelopeArtifact, EnvelopeUsage,
    MESSAGE_REF_CAP_BYTES, MemoryRefReader, MessageReceipt, OutputFormat, OwnerRequest,
    REPORT_FINDINGS_TOOL, RefKind, RenderedRef, ReportedFinding, ResultEnvelope, RoleSnapshot,
    RoleSpec, SUBAGENT_ARG_ALIASES, SUBAGENT_TOKENS_FLOOR, SUBAGENT_TOOL_NAMES, SUMMARY_CAP_BYTES,
    SpawnRequest, SpawnedChild, SubagentConfig, SubagentContributor, SubagentRoleGuard,
    SubagentRoleLimits, SubagentService, SubagentTokenGate, SubagentView, TITLE_MAX_CHARS,
    WorkspaceLeaseRegistry, cap_message_text, child_identity, confine_to_workspace, cut_summary,
    default_delegation_guidance, default_role_specs, delegation_toolsets, effective_role,
    envelope_status, present_envelope, render_brief, report_findings_definition,
    subagent_tool_definitions, usage_from_events,
};
pub use teammates::{TeammateAdmission, TeammateService};
