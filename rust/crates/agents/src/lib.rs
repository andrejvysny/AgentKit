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
    WatchdogSignals, advisor_run_chat_id, explicit_review, is_advisor_chat_id,
};
pub use agents::{AgentDefinitionService, validate_agent_definition};
pub use errors::{AgentsError, AgentsResult, LeaseHeld, RoleDenial, SnapshotError};
pub use subagents::{
    AwaitOutcome, AwaitResult, BUILTIN_ROLES, EffectiveRole, MessageReceipt, RoleSnapshot,
    RoleSpec, SUBAGENT_TOOL_NAMES, SpawnRequest, SpawnedChild, SubagentConfig, SubagentContributor,
    SubagentRoleGuard, SubagentRoleLimits, SubagentService, SubagentView, WorkspaceLeaseRegistry,
    child_identity, default_delegation_guidance, default_role_specs, effective_role,
};
pub use teammates::{TeammateAdmission, TeammateService};
