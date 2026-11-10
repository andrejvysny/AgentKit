//! Host-owned tool staging and durable write governance.
mod core_run_ownership;
mod durable_execution;
mod execution_completion;
mod native_command_drain;
mod policy;
mod proposal_support;
mod proposal_tool;
mod staging;

pub(crate) use core_run_ownership::OwnedCoreRun;
pub(crate) use durable_execution::wrap_static_writes;
pub use durable_execution::{
    DurableExecution, MutatingPolicy, args_sha256, execution_context_sha256, execution_id,
    ts_args_sha256, ts_canonical_json,
};
pub use native_command_drain::drain_native_command_completions;
pub use policy::{
    SessionWritePolicy, ToolWritePolicy, WriteAllowance, WritePolicyMode, WriteQuery, WriteRisk,
};
pub use proposal_support::{NATIVE_GOVERNANCE_IDENTITY_VERSION, native_write_fingerprint};
pub use proposal_tool::{
    AUTO_APPLY_FAILED_MESSAGE, GovernedProposalTool, ProposalBuild, ProposalBuilder,
    ProposalToolOptions, WriteExecutionMode,
};
pub use staging::{ContributorFailure, StagedTools, StaticToolContributor, stage_tools};

pub use agentkit_governance::{ApprovalGrant, StandingApprovalPolicy};
