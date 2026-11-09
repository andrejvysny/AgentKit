//! AgentKit VM sandbox worker daemon (unprivileged control plane).
//!
//! The worker owns the *physical* sandbox inventory: a digest-chained
//! append-only journal, the authoritative in-memory registry rebuilt from
//! it, the authenticated UDS surface to the trusted Host, the guest link
//! pool over vsock (or in-process duplex for contract tests), and the
//! pluggable VM backend (fake here; firecracker via the separate
//! `agentkit-sandbox-worker-fc` crate).
//!
//! Normative sources: `docs/specs/sandbox-runtime.md` (§4-§8, §12.1;
//! invariants S01-S20) and ADR 0021. All semantics types come from
//! `agentkit-sandbox-contracts`; nothing here redefines the wire shapes.

pub mod backend;
pub mod broker;
pub mod config;
pub mod error;
pub mod guestlink;
pub mod journal;
pub mod oci;
pub mod registry;
pub mod service;
pub mod transport;
pub mod util;

pub use config::WorkerConfig;
pub use error::{WorkerError, WorkerResult};
pub use journal::{Journal, JournalRecord};
pub use registry::Registry;
pub use service::WorkerService;
