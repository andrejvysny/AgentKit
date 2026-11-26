//! Track B server sandbox backend (ADR 0039): implements
//! `agentkit_workspaces::SandboxBackend` over the launcher service, one
//! guest channel per machine and one egress listener per machine.
//!
//! - Launcher: one unix-socket connection per request
//!   (`agentkit-sandbox-proto::launcher`). It starts and stops containers,
//!   creates the per-machine socket directory and measures and removes
//!   volumes; the adapter never names an image, mount or flag.
//! - Guest channel: the supervisor inside the container dials
//!   `<socket_dir>/<machine_dir_name(ws, gen)>/control.sock`; the adapter
//!   accepts connections until one answers `hello` with the protocol,
//!   generation, image digest and the container's channel token (from the
//!   launcher's `ensured`), keeps that one, drops the listener and then
//!   runs requests concurrently over it.
//! - Egress: an `agentkit_egress::EgressProxy` on `egress.sock` in the same
//!   directory applies the workspace's grants and stamps each refusal with
//!   the operation dispatched last.
//!
//! The supervisor shares the workload's uid, so every guest reply is
//! untrusted: the adapter bounds every stream before it stores a byte,
//! checks paths, sequences and sizes against the request, and never lets
//! a receipt it has seen change. Any violation, like a lost channel,
//! makes the machine unreachable: its calls answer `sandbox_unavailable`
//! until the service stops it.

mod backend;
mod channel;
mod config;
mod expect;
mod guest;
mod launcher;
mod machine;
mod mapping;
mod port;
mod registry;
mod start;
mod token;
mod verify;

pub use backend::ContainerBackend;
pub use config::{ContainerBackendConfig, ContainerConfigError};
