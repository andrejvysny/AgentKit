//! Trusted host-side bridge to the AgentKit VM sandbox worker daemon.
//!
//! Parts:
//! - [`client`]: `WorkerClient` — implements the frozen `SandboxManager` +
//!   `SandboxWorkspaceIo` contracts over the worker UDS framed protocol,
//!   fills request digests via the single authority, enforces tenancy from
//!   the server-derived binding only (S02), and classifies lost-after-write
//!   mutations as uncertain (S07/S14) instead of retrying them.
//! - [`spawn`]: launch the `agentkit-sandbox-worker` binary and hand out a
//!   connected `WorkerClient`.
//! - [`binding`]: versioned `SandboxBinding` records, a `SandboxBindingStore`
//!   (in-memory + atomic file), and create-or-adopt / stop / destroy /
//!   restart lifecycle helpers.
//! - [`reconcile`]: post-restart reconciliation of worker inventory against
//!   the binding store, producing adopted/quarantined/orphaned/uncertain.
//!
//! Normative source: `docs/specs/sandbox-runtime.md` (§4, §6, §8, §9, §11.4,
//! §12; invariants S02/S03/S06/S07/S08/S09/S12/S14/S16/S18). All semantics
//! types come from `agentkit-sandbox-contracts`; nothing here redefines the
//! wire shapes or the digest convention.

pub mod binding;
pub mod client;
pub mod conn;
pub mod reconcile;
pub mod spawn;
pub mod sqlite_store;
pub mod subagent;

pub use agentkit_sandbox_contracts as contracts;

pub use binding::{
    BindingMode, BindingState, FileStore, InMemoryStore, RestartDecision, SandboxBinding,
    SandboxBindingStore, ensure_sandbox,
};
pub use client::{
    ClientBinding, HelloInfo, HostAdmission, HostError, Mutation, WorkerClient, WorkerClientConfig,
};
pub use reconcile::{ReconcileReport, reconcile};
pub use spawn::{SpawnConfig, SpawnedWorker, spawn_worker};
pub use subagent::{
    ROOT_PREFIXES, SubagentError, authorize_child_grant, authorize_root_grant, cancel_children,
    child_authority, derive_child, effective_capabilities, effective_prefixes,
    find_parent_by_child_task, reviewer_snapshot, root_capabilities,
};
