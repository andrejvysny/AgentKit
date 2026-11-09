//! Governance ports: the traits and plain data types through which tool
//! contributors, guards, approval policies and session authorities plug into
//! the host's staging pipeline. Implementations live in the host and in the
//! tool packs; this crate owns only the contracts.

mod approval;
mod authority;
mod contribution;
mod durable;
mod error;
mod guard;

pub use approval::{ApprovalGrant, StandingApprovalPolicy};
pub use authority::{CodingPermissionAuthority, PermissionContext};
pub use contribution::{
    ContributedTool, GovernedWriteTool, ToolContributionContext, ToolContributor,
};
pub use durable::{Admission, CommandCompletion, DurableExecutionPort, PreGuard};
pub use error::{GovernanceError, GovernanceResult};
pub use guard::{ApprovalAsk, ToolGuard, ToolGuardContext, ToolGuardVerdict};
