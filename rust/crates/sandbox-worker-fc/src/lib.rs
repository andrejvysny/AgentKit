//! Firecracker/KVM/jailer backend for the AgentKit sandbox worker (Linux).
//!
//! Security posture (spec §12.1, S03, S05, S20):
//! - The worker passes **identifiers and digests** to this crate, never raw
//!   host paths from the wire. All VM paths are derived here, under the
//!   admin-configured `storage_root`.
//! - The guest gets exactly two virtio-block devices (read-only trusted
//!   rootfs, private writable data disk), one vsock device, and a serial
//!   console. **No network device is ever attached.**
//! - Launch is fail-closed: without a readable/writable `/dev/kvm`, a
//!   matching pinned Firecracker, and a usable jailer, `CreateVm` returns
//!   `sandbox_isolation_unavailable`. There is no fallback path.
//! - Termination is proven by independent process observation, never by
//!   guest claims.

pub mod api;
pub mod backend;
pub mod cgroup;
pub mod disk;
pub mod probe;
pub mod vsock_client;

use std::sync::Arc;

use agentkit_sandbox_contracts::api::SandboxBackend;

pub use backend::{BackendConfig, FirecrackerBackend};

/// Factory used by the worker when `--backend firecracker` is selected.
/// Returns `None` only on invalid configuration (missing admin paths);
/// runtime capability failures surface as `sandbox_isolation_unavailable`
/// from `probe()`/`provision()` so operators see actionable errors.
pub fn make_backend(cfg: BackendConfig) -> Option<Arc<dyn SandboxBackend>> {
    match FirecrackerBackend::new(cfg) {
        Ok(b) => Some(Arc::new(b)),
        Err(_) => None,
    }
}

/// Minimum Firecracker version this backend is written and tested against.
/// Operators pin the exact binary; this is the lower bound we refuse to run
/// below, plus a compatibility major-version ceiling (S19: no guessing).
pub const FC_MIN_VERSION: (u32, u32, u32) = (1, 8, 0);
pub const FC_MAX_MAJOR: u32 = 1;

/// Pinned kernel cmdline for guests. `reboot=k panic=1` makes a guest
/// panic/reboot terminate the VM (the worker treats VM exit as observed
/// termination). The nonce is appended by the backend per generation.
pub const BASE_BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off nomodules root=/dev/vda ro ip=off agentkit.workspace=/workspace";

/// Guest vsock port the guest agent listens on.
pub const GUEST_AGENT_PORT: u32 = 52;

#[derive(Debug, thiserror::Error)]
pub enum FcError {
    #[error("io: {context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },
    #[error("firecracker api: {0}")]
    Api(String),
    #[error("probe failure (fail closed): {0}")]
    Probe(String),
    #[error("verification failure: {0}")]
    Verify(String),
    #[error("timeout: {0}")]
    Timeout(String),
}

impl FcError {
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

impl From<std::io::Error> for FcError {
    fn from(e: std::io::Error) -> Self {
        Self::io("io", e)
    }
}

impl From<FcError> for agentkit_sandbox_contracts::error::SandboxError {
    fn from(e: FcError) -> Self {
        use agentkit_sandbox_contracts::error::{SandboxError, SandboxErrorCode};
        let code = match &e {
            FcError::Probe(_) => SandboxErrorCode::IsolationUnavailable,
            FcError::Verify(_) => SandboxErrorCode::ImageInvalid,
            FcError::Timeout(_) => SandboxErrorCode::TerminationUnverified,
            FcError::Api(_) | FcError::Io { .. } => SandboxErrorCode::Unavailable,
        };
        SandboxError::new(code, e.to_string())
    }
}

pub(crate) fn path_str(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}
