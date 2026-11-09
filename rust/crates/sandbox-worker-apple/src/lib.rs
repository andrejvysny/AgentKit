//! Virtualization.framework backend adapter for the AgentKit sandbox worker
//! (macOS, spec §12.2).
//!
//! Security posture (S03, S05, S20):
//! - The worker passes **identifiers and digests**; every VM path is derived
//!   here under the admin-configured `storage_root`. The Swift helper
//!   (`apple/vz-helper`) is an admin-pinned binary executed with a fixed
//!   argv; it receives a manifest file, never tenant-controlled data.
//! - Launch is fail-closed: missing helper, missing entitlements, or an
//!   unsupported host makes `probe()`/`provision()` return
//!   `sandbox_isolation_unavailable`. There is no host-shell, Seatbelt-only,
//!   or Docker fallback.
//! - Trusted kernel/rootfs files are verified against the spec-pinned
//!   digests before the helper is ever spawned (S11).
//! - Termination is proven by reaping the helper child (we are its parent:
//!   `waitpid` is independent observation, S17), never by guest claims.
//! - The guest gets two virtio-block devices (ro trusted rootfs, rw private
//!   data disk), entropy, and a vsock device relayed to a worker-owned unix
//!   socket. **No network device is ever attached** (S04).

pub mod backend;
pub mod proto;

use std::sync::Arc;

use agentkit_sandbox_contracts::api::SandboxBackend;

pub use backend::{AppleVzBackend, VzBackendConfig};
pub use proto::{HelperCmd, HelperError, HelperProbe, HelperRequest, HelperResponse, VmManifest};

/// Factory used by the worker when `--backend apple-vz` is selected.
/// `None` only on invalid admin configuration; capability failures surface
/// as `sandbox_isolation_unavailable` from `probe()`.
pub fn make_backend(cfg: VzBackendConfig) -> Option<Arc<dyn SandboxBackend>> {
    match AppleVzBackend::new(cfg) {
        Ok(b) => Some(Arc::new(b)),
        Err(_) => None,
    }
}

/// Guest vsock port the guest agent listens on (parity with Firecracker).
pub const GUEST_AGENT_PORT: u32 = 52;

/// Kernel cmdline shared with the Firecracker backend; the data-disk
/// format flag is appended per generation (fresh generations only).
pub const BASE_BOOT_ARGS: &str =
    "console=hvc0 reboot=k panic=1 nomodules root=/dev/vda ro ip=off agentkit.workspace=/workspace";
