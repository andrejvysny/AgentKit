//! Untrusted VM guest agent (§8): serves the worker over vsock with the
//! frozen `WorkerToGuest`/`GuestToWorker` catalogs, enforcing openat2-bounded
//! roots, a fixed-argv git surface, and a process-group command supervisor.
//!
//! Linux-only machinery is gated; on other targets the crate still type-checks
//! (the binary refuses to run).

pub mod cmdline;
pub mod error;
pub mod glob;

#[cfg(target_os = "linux")]
pub mod fsops;

#[cfg(target_os = "linux")]
pub mod git;
#[cfg(target_os = "linux")]
pub mod handshake;
#[cfg(target_os = "linux")]
pub mod search;
#[cfg(target_os = "linux")]
pub mod server;
#[cfg(target_os = "linux")]
pub mod staging;
#[cfg(target_os = "linux")]
pub mod supervisor;
#[cfg(target_os = "linux")]
pub mod vsock;

#[cfg(target_os = "linux")]
pub use fsops::{GuestFs, Roots};
#[cfg(target_os = "linux")]
pub use server::Server;
#[cfg(target_os = "linux")]
pub use supervisor::Supervisor;
