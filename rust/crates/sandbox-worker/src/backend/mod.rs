//! Backend seam. The contracts `SandboxBackend` trait is the only platform
//! interface; the firecracker implementation arrives from the separate
//! `agentkit-sandbox-worker-fc` crate (integrator-owned), wired behind the
//! optional `firecracker` feature.

pub mod fake;

pub use fake::{FakeBackend, FaultInjector};

use agentkit_sandbox_contracts::api::SandboxBackend;
use std::path::PathBuf;
use std::sync::Arc;

/// Worker selection of a backend, from `--backend fake|firecracker|apple-vz`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendKind {
    Fake,
    Firecracker,
    AppleVz,
}

impl BackendKind {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "fake" => Ok(Self::Fake),
            "firecracker" => Ok(Self::Firecracker),
            "apple-vz" => Ok(Self::AppleVz),
            other => Err(format!(
                "unknown backend {other:?} (expected fake|firecracker|apple-vz)"
            )),
        }
    }
}

/// Launch configuration the firecracker backend needs (admin CLI only).
#[derive(Debug, Clone)]
pub struct FirecrackerConfig {
    pub storage_root: PathBuf,
    pub kernel_path: PathBuf,
    pub rootfs_path: PathBuf,
    pub fc_bin: PathBuf,
    pub jailer_bin: PathBuf,
    /// Admin-delegated cgroup v2 subtree; `None` => limits are reported as
    /// not enforced (S19 honesty), never faked.
    pub cgroup_root: Option<PathBuf>,
}

/// Launch configuration the Virtualization.framework backend needs
/// (admin CLI only, macOS hosts, spec §12.2).
#[derive(Debug, Clone)]
pub struct AppleConfig {
    pub storage_root: PathBuf,
    pub kernel_path: PathBuf,
    pub rootfs_path: PathBuf,
    /// Admin-pinned Swift helper binary.
    pub helper_path: PathBuf,
}

/// Reconcile verdict for a stale generation discovered at startup (§12.1).
/// The firecracker crate decides from flock state + pidfile identity + api
/// socket liveness; the worker only applies the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileDecision {
    /// Prove termination via the terminate path.
    Kill,
    /// Unverifiable: keep quarantined, refuse launch on this generation.
    Quarantine,
    /// Live and identical: adopt into the live map.
    Adopt,
}

/// Build the firecracker backend when the feature is linked. Returns `None`
/// when the backend is not linked into this build, so the CLI can fail
/// closed with `CapabilityUnsupported` instead of silently falling back to
/// fake. The concrete implementation is supplied by the integrator's
/// `agentkit-sandbox-worker-fc` crate once it lands; until then this entry
/// point honestly reports "not linked".
#[cfg(feature = "firecracker")]
pub fn make_firecracker_backend(cfg: FirecrackerConfig) -> Option<Arc<dyn SandboxBackend>> {
    agentkit_sandbox_worker_fc::make_backend(agentkit_sandbox_worker_fc::BackendConfig {
        storage_root: cfg.storage_root,
        kernel_path: cfg.kernel_path,
        rootfs_path: cfg.rootfs_path,
        fc_bin: cfg.fc_bin,
        jailer_bin: cfg.jailer_bin,
        cgroup_root: cfg.cgroup_root,
        pinned_fc_version: None,
        jailer_uid: unsafe { libc::getuid() },
        jailer_gid: unsafe { libc::getgid() },
    })
}

#[cfg(not(feature = "firecracker"))]
pub fn make_firecracker_backend(_cfg: FirecrackerConfig) -> Option<Arc<dyn SandboxBackend>> {
    None
}

/// Build the apple-vz backend when the feature is linked (`--features
/// apple`). `None` => not linked; the CLI fails closed, never falls back.
#[cfg(feature = "apple")]
pub fn make_apple_vz_backend(cfg: AppleConfig) -> Option<Arc<dyn SandboxBackend>> {
    agentkit_sandbox_worker_apple::make_backend(agentkit_sandbox_worker_apple::VzBackendConfig {
        storage_root: cfg.storage_root,
        kernel_path: cfg.kernel_path,
        rootfs_path: cfg.rootfs_path,
        helper_path: cfg.helper_path,
        min_helper_version: (1, 0, 0),
        guest_vsock_port: agentkit_sandbox_worker_apple::GUEST_AGENT_PORT,
    })
}

#[cfg(not(feature = "apple"))]
pub fn make_apple_vz_backend(_cfg: AppleConfig) -> Option<Arc<dyn SandboxBackend>> {
    None
}
