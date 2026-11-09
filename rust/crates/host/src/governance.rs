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
pub use durable_execution::*;
pub use native_command_drain::drain_native_command_completions;
pub use policy::*;
pub use proposal_support::{NATIVE_GOVERNANCE_IDENTITY_VERSION, native_write_fingerprint};
pub use proposal_tool::*;
pub use staging::*;

pub use agentkit_governance::{
    Admission, ApprovalAsk, ApprovalGrant, CommandCompletion, ContributedTool,
    DurableExecutionPort, GovernanceError, GovernanceResult, GovernedWriteTool, PreGuard,
    StandingApprovalPolicy, ToolContributionContext, ToolContributor, ToolGuard, ToolGuardContext,
    ToolGuardVerdict,
};
