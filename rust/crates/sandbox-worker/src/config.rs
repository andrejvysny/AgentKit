//! Worker CLI configuration (all paths trusted only from the admin CLI,
//! S20 — wire frames never carry host paths). Env fallback: `AGENTKIT_SANDBOX_*`.

pub use crate::backend::{AppleConfig, BackendKind, FirecrackerConfig};
use crate::registry::QuotaCeilings;
use std::path::PathBuf;

/// Fully resolved worker configuration.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub uds: PathBuf,
    pub storage_root: PathBuf,
    pub peer_uids: Vec<u32>,
    pub worker_id: String,
    pub revision: u64,
    pub install_fingerprint: String,
    pub backend: BackendKind,
    pub guest_vsock_port: u32,
    pub kernel_path: Option<PathBuf>,
    pub rootfs_path: Option<PathBuf>,
    pub fc_bin: Option<PathBuf>,
    pub jailer_bin: Option<PathBuf>,
    /// Admin-pinned Virtualization.framework helper (`--backend apple-vz`).
    pub helper_path: Option<PathBuf>,
    pub cgroup_root: Option<PathBuf>,
    pub idle_sweep_sec: u64,
    pub ceilings: QuotaCeilings,
    /// Structured package broker master switch (admin-only, S20). Off by
    /// default: with it off the worker denies every guest fetch honestly.
    pub broker_enabled: bool,
    /// TEST-ONLY exact `host:port` http origins for a local fake registry.
    /// MUST be empty in production; loudly audited when set.
    pub broker_test_origins: Vec<String>,
}

impl WorkerConfig {
    /// Environment fallback for a flag (S20: admin-only configuration).
    #[must_use]
    pub fn env(key: &str) -> Option<String> {
        std::env::var(format!(
            "AGENTKIT_SANDBOX_{}",
            key.to_ascii_uppercase().replace('-', "_")
        ))
        .ok()
        .filter(|v| !v.is_empty())
    }

    fn require(&self) -> Result<(), String> {
        if self.peer_uids.is_empty() {
            return Err("--peer-uid is required (repeatable)".into());
        }
        if !self.install_fingerprint.starts_with("sha256:") {
            return Err("--install-fingerprint must be sha256:<64 hex>".into());
        }
        agentkit_sandbox_contracts::ids::InstallFingerprint::parse(&self.install_fingerprint)
            .map_err(|e| format!("--install-fingerprint invalid: {e}"))?;
        agentkit_sandbox_contracts::ids::WorkerId::new(&self.worker_id)
            .map_err(|e| format!("--worker-id invalid: {e}"))?;
        if !self.broker_enabled && !self.broker_test_origins.is_empty() {
            return Err("--broker-test-origin requires --broker-enabled".into());
        }
        if self.broker_enabled {
            for o in &self.broker_test_origins {
                if o.contains('/') || o.contains('@') || o.contains('?') || !o.contains(':') {
                    return Err(format!("--broker-test-origin must be host:port, got {o:?}"));
                }
            }
        }
        if self.uds.as_os_str().is_empty() {
            return Err("--uds is required".into());
        }
        if self.storage_root.as_os_str().is_empty() {
            return Err("--storage-root is required".into());
        }
        if matches!(self.backend, BackendKind::Firecracker) {
            for (flag, slot) in [
                ("--kernel-path", &self.kernel_path),
                ("--rootfs-path", &self.rootfs_path),
                ("--fc-bin", &self.fc_bin),
                ("--jailer-bin", &self.jailer_bin),
            ] {
                if slot.is_none() {
                    return Err(format!("{flag} is required for the firecracker backend"));
                }
            }
        }
        if matches!(self.backend, BackendKind::AppleVz) {
            for (flag, slot) in [
                ("--kernel-path", &self.kernel_path),
                ("--rootfs-path", &self.rootfs_path),
                ("--helper-path", &self.helper_path),
            ] {
                if slot.is_none() {
                    return Err(format!("{flag} is required for the apple-vz backend"));
                }
            }
        }
        Ok(())
    }

    /// Parse argv (excluding program name). Flags take precedence over env.
    pub fn from_args(args: &[String]) -> Result<Self, String> {
        let mut cfg = Self::defaults();
        let mut i = 0usize;
        let mut backend_seen = false;
        let mut peer_uid_seen = false;
        let next = |i: &mut usize, flag: &str| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        while i < args.len() {
            let flag = args[i].as_str();
            match flag {
                "--uds" => cfg.uds = PathBuf::from(&next(&mut i, flag)?),
                "--storage-root" => cfg.storage_root = PathBuf::from(&next(&mut i, flag)?),
                "--peer-uid" => {
                    if !peer_uid_seen {
                        cfg.peer_uids.clear();
                        peer_uid_seen = true;
                    }
                    let v = next(&mut i, flag)?;
                    let uid: u32 = v
                        .parse()
                        .map_err(|_| format!("--peer-uid invalid: {v:?}"))?;
                    cfg.peer_uids.push(uid);
                }
                "--worker-id" => cfg.worker_id = next(&mut i, flag)?,
                "--revision" => {
                    cfg.revision = next(&mut i, flag)?
                        .parse()
                        .map_err(|_| "revision must be an integer")?
                }
                "--install-fingerprint" => cfg.install_fingerprint = next(&mut i, flag)?,
                "--backend" => {
                    cfg.backend = BackendKind::parse(&next(&mut i, flag)?)?;
                    backend_seen = true;
                }
                "--guest-vsock-port" => {
                    cfg.guest_vsock_port = next(&mut i, flag)?
                        .parse()
                        .map_err(|_| "port must be an integer")?
                }
                "--rootfs-path" => cfg.rootfs_path = Some(PathBuf::from(&next(&mut i, flag)?)),
                "--kernel-path" => cfg.kernel_path = Some(PathBuf::from(&next(&mut i, flag)?)),
                "--fc-bin" => cfg.fc_bin = Some(PathBuf::from(&next(&mut i, flag)?)),
                "--jailer-bin" => cfg.jailer_bin = Some(PathBuf::from(&next(&mut i, flag)?)),
                "--helper-path" => cfg.helper_path = Some(PathBuf::from(&next(&mut i, flag)?)),
                "--cgroup-root" => cfg.cgroup_root = Some(PathBuf::from(&next(&mut i, flag)?)),
                "--broker-enabled" => cfg.broker_enabled = true,
                "--broker-test-origin" => cfg.broker_test_origins.push(next(&mut i, flag)?),
                "--idle-sweep-sec" => {
                    cfg.idle_sweep_sec = next(&mut i, flag)?
                        .parse()
                        .map_err(|_| "idle-sweep-sec must be an integer")?
                }
                "--help" | "-h" => return Err(usage()),
                other => return Err(format!("unknown flag {other:?}\n{}", usage())),
            }
            i += 1;
        }
        // Env fallbacks for anything left at default.
        let env_fill = |key: &str, slot: &mut PathBuf| {
            if let Some(v) = Self::env(key) {
                *slot = PathBuf::from(v);
            }
        };
        env_fill("uds", &mut cfg.uds);
        env_fill("storage-root", &mut cfg.storage_root);
        if let Some(v) = Self::env("worker-id") {
            cfg.worker_id = v;
        }
        if let Some(v) = Self::env("install-fingerprint") {
            cfg.install_fingerprint = v;
        }
        if !backend_seen && let Some(v) = Self::env("backend") {
            cfg.backend = BackendKind::parse(&v)?;
        }
        if !peer_uid_seen && let Some(v) = Self::env("peer-uid") {
            cfg.peer_uids = v
                .split(',')
                .filter_map(|p| p.trim().parse::<u32>().ok())
                .collect();
        }
        cfg.require()?;
        Ok(cfg)
    }

    fn defaults() -> Self {
        Self {
            uds: PathBuf::new(),
            storage_root: PathBuf::new(),
            peer_uids: Vec::new(),
            worker_id: "worker-local".into(),
            revision: 1,
            install_fingerprint:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(),
            backend: BackendKind::Fake,
            guest_vsock_port: crate::guestlink::GUEST_VSOCK_PORT,
            kernel_path: None,
            rootfs_path: None,
            helper_path: None,
            fc_bin: None,
            jailer_bin: None,
            cgroup_root: None,
            idle_sweep_sec: 30,
            broker_enabled: false,
            broker_test_origins: Vec::new(),
            ceilings: QuotaCeilings::default(),
        }
    }

    /// Stable backend name for handles/replies.
    #[must_use]
    pub fn backend_flag(&self) -> String {
        match self.backend {
            BackendKind::Fake => "fake".into(),
            BackendKind::Firecracker => "firecracker".into(),
            BackendKind::AppleVz => "apple-vz".into(),
        }
    }

    /// Firecracker launch config from admin flags (S20 trusted source).
    pub fn firecracker_config(&self) -> Option<FirecrackerConfig> {
        Some(FirecrackerConfig {
            storage_root: self.storage_root.clone(),
            kernel_path: self.kernel_path.clone()?,
            rootfs_path: self.rootfs_path.clone()?,
            fc_bin: self.fc_bin.clone()?,
            jailer_bin: self.jailer_bin.clone()?,
            cgroup_root: self.cgroup_root.clone(),
        })
    }

    /// Virtualization.framework launch config from admin flags (S20).
    pub fn apple_config(&self) -> Option<AppleConfig> {
        Some(AppleConfig {
            storage_root: self.storage_root.clone(),
            kernel_path: self.kernel_path.clone()?,
            rootfs_path: self.rootfs_path.clone()?,
            helper_path: self.helper_path.clone()?,
        })
    }
}

fn usage() -> String {
    "usage: agentkit-sandbox-worker --uds <path> --storage-root <path> --peer-uid <uid>[ --peer-uid <uid>...] \
     [--worker-id id] [--revision n] [--install-fingerprint sha256:...] \
     [--backend fake|firecracker|apple-vz] [--guest-vsock-port 52] \
     [--kernel-path p] [--rootfs-path p] [--fc-bin p] [--jailer-bin p] [--helper-path p] [--cgroup-root p] [--idle-sweep-sec n]"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn parses_all_flags_with_repeatable_peer_uid() {
        let c = WorkerConfig::from_args(&args(
            "--uds /run/ak/w.sock --storage-root /var/lib/ak --peer-uid 1000 --peer-uid 1001 \
             --worker-id w1 --revision 3 --install-fingerprint sha256:0000000000000000000000000000000000000000000000000000000000000000 \
             --backend fake --guest-vsock-port 60 --idle-sweep-sec 5",
        ))
        .unwrap();
        assert_eq!(c.peer_uids, vec![1000, 1001]);
        assert_eq!(c.guest_vsock_port, 60);
        assert_eq!(c.revision, 3);
        assert_eq!(c.backend, BackendKind::Fake);
    }

    #[test]
    fn peer_uid_required_and_fingerprint_validated() {
        let e = WorkerConfig::from_args(&args("--uds /r/s --storage-root /r/d")).unwrap_err();
        assert!(e.contains("--peer-uid"), "{e}");
        let e = WorkerConfig::from_args(&args(
            "--uds /r/s --storage-root /r/d --peer-uid 1 --install-fingerprint nope",
        ))
        .unwrap_err();
        assert!(e.contains("fingerprint"), "{e}");
    }
}
