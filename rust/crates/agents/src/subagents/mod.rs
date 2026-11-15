//! Subagent delegation (ADR 0003's host-level consumer): a running session
//! spawns kind-dispatched CHILD SESSIONS through the SAME durable engine —
//! same `AgentHost` queue, same `ChatExecutor`, same store, guards, events
//! and recovery — and coordinates them with spawn/list/status/message/await/
//! cancel/resume tools.
//!
//! Design invariants:
//! - **One engine.** A child is a normal durable session (`chat.turn` task
//!   kind) claimed by the host's own worker; nothing here executes model
//!   calls directly.
//! - **Registry is a projection.** `subagents` rows track lifecycle for
//!   queries and reaping; the authoritative state is always the child's
//!   task/session records, re-derived on every read (`refresh`).
//! - **Roles are ceilings, not grants.** A role narrows which contributed
//!   tools the child may see and execute ([`SubagentRoleGuard`]); it never
//!   grants past the host's guard chain or the parent's permission state.
//! - **Bounded fan-out.** Depth, per-parent and global concurrency limits
//!   are enforced at spawn from durable rows, so they survive restarts.
//! - **Idempotent spawn.** Child identities derive from the spawning
//!   attempt's logical tool call, so an executor retry re-admits the same
//!   child instead of forking a duplicate execution.

mod authority;
mod brief;
mod budget;
mod config;
mod envelope;
mod guard;
mod memory_refs;
mod service;
mod snapshot;
mod tools;
mod toolsets;

pub use brief::{
    Brief, BriefBudget, BriefError, BriefScope, BudgetLines, ContextRef, MESSAGE_REF_CAP_BYTES,
    OutputFormat, RefKind, RenderedRef, TITLE_MAX_CHARS, cap_message_text, confine_to_workspace,
    render_brief,
};
pub use config::{
    BUILTIN_ROLES, DELEGATION_GUIDANCE, RoleSpec, SubagentConfig, SubagentRoleLimits,
    default_delegation_guidance, default_role_specs,
};
pub use envelope::{
    ChildReport, EnvelopeArtifact, EnvelopeUsage, ReportedFinding, ResultEnvelope,
    SUMMARY_CAP_BYTES, cut_summary, envelope_status, present_envelope, usage_from_events,
};
pub use guard::{SubagentRoleGuard, WorkspaceLeaseRegistry};
pub use memory_refs::MemoryRefReader;
pub use service::{
    AwaitDecision, AwaitMode, AwaitOutcome, AwaitResult, MessageReceipt, OwnerRequest,
    SpawnRequest, SpawnedChild, SubagentService, SubagentView, child_identity,
};
pub use snapshot::{EffectiveRole, RoleSnapshot, effective_role};
pub use tools::{
    REPORT_FINDINGS_TOOL, SUBAGENT_ARG_ALIASES, SUBAGENT_TOOL_NAMES, SubagentContributor,
    report_findings_definition, subagent_tool_definitions,
};
pub use toolsets::{DELEGATION_TOOLSET, delegation_toolsets};
