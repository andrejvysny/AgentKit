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

mod config;
mod guard;
mod service;
mod tools;

pub use config::{
    BUILTIN_ROLES, RoleSpec, SubagentConfig, SubagentRoleLimits, default_delegation_guidance,
    default_role_specs,
};
pub use guard::{SubagentRoleGuard, WorkspaceLeaseRegistry};
pub use service::{
    AwaitOutcome, AwaitResult, MessageReceipt, SpawnRequest, SpawnedChild, SubagentService,
    SubagentView, child_identity, task_overrides,
};
pub use tools::{SUBAGENT_TOOL_NAMES, SubagentContributor};
