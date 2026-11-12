//! Governance ports and the host-owned governance implementation: tool
//! contribution, guards, approvals and session authority contracts, plus
//! staging, durable write receipts, write policy and proposal application.

mod approval;
mod authority;
mod contribution;
mod core_run_ownership;
mod durable;
mod durable_execution;
mod error;
mod execution_completion;
mod guard;
mod native_command_drain;
mod policy;
mod ports;
mod proposal_support;
mod proposal_tool;
mod proposals;
mod staging;

pub use approval::{ApprovalGrant, StandingApprovalPolicy};
pub use authority::{CodingPermissionAuthority, PermissionContext};
pub use contribution::{
    ContributedTool, GovernedWriteTool, ToolContributionContext, ToolContributor,
};
pub use core_run_ownership::OwnedCoreRun;
pub use durable::{Admission, CommandCompletion, DurableExecutionPort, PreGuard};
pub use durable_execution::{
    DurableExecution, MutatingPolicy, args_sha256, execution_context_sha256, execution_id,
    ts_args_sha256, ts_canonical_json, wrap_static_writes,
};
pub use error::{GovernanceError, GovernanceResult};
pub use guard::{ApprovalAsk, ToolGuard, ToolGuardContext, ToolGuardVerdict};
pub use native_command_drain::drain_native_command_completions;
pub use policy::{
    SessionWritePolicy, ToolWritePolicy, WriteAllowance, WritePolicyMode, WriteQuery, WriteRisk,
};
pub use ports::{Authorization, MutationIdentity, ProposalApplier};
pub use proposal_support::{NATIVE_GOVERNANCE_IDENTITY_VERSION, native_write_fingerprint};
pub use proposal_tool::{
    AUTO_APPLY_FAILED_MESSAGE, GovernedProposalTool, ProposalBuild, ProposalBuilder,
    ProposalToolOptions, WriteExecutionMode,
};
pub use proposals::{ProposalApplyGuard, ProposalRevisionSource, ProposalService};
pub use staging::{ContributorFailure, StagedTools, StaticToolContributor, stage_tools};

pub(crate) use agentkit_engine::SharedRuntime;

/// The engine's execution context bound to the SQLite store, the only
/// repository the product ships.
pub(crate) type ExecutionContext = agentkit_engine::ExecutionContext<agentkit_store::SqliteStore>;
