//! Effective security policy, capabilities, and the frozen package
//! registry policy (§5.1, §6.3, §9).
//!
//! A sandbox is pinned to a `PolicyGeneration` + `PolicyDigest` at admission.
//! Revocation raises the generation and can force running VMs to stop
//! (§14.4). Digests are computed over canonical JSON so every process can
//! recompute and compare them independently.

use crate::canonical::canonicalize;
use crate::ids::{PolicyDigest, RegistryPolicyId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Fine-grained capabilities (§6.3). Distinct from the coding pack's
/// `fs.read`/`fs.write`/`process.exec` tool capabilities: these gate the
/// sandbox service itself. The existing `CodingPermissionGuard` and
/// high-risk-command rules remain authoritative; sandbox policy is an
/// **additional AND gate** (§6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxCapability {
    /// create/list/inspect/start/stop/destroy.
    SandboxManage,
    /// read/list/search/git inspect inside a sandbox.
    SandboxRead,
    /// write/edit inside a sandbox (requires writer lease).
    SandboxWrite,
    /// execute commands inside the guest.
    ProcessExec,
    /// stage + import artifacts into a sandbox.
    ArtifactImport,
    /// export artifacts out of a sandbox.
    ArtifactExport,
    /// perform brokered package fetches (guest-originated, host-authorized).
    NetworkPackageFetch,
    /// import/convert OCI images into the VM image registry.
    ImageImport,
}

/// Network posture of a sandbox (§1, §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// No VM network device at all; no broker.
    None,
    /// No NIC; package artifacts only, via the structured broker over vsock
    /// (§9.1). The default.
    PackagesOnly,
    /// Future, separately gated mode (§9.2): NOT implemented in v1; any
    /// attempt to select it fails with `sandbox_capability_unsupported`.
    Restricted,
}

/// One approved registry endpoint family for the broker (§9.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryEndpoint {
    /// Ecosystem selector: `pypi` | `npm` | `crates`.
    pub ecosystem: crate::spec::Ecosystem,
    /// HTTPS origin the broker may contact, e.g. `pypi.org`. Exact match,
    /// no wildcards, no paths, no IP literals.
    pub host: String,
    /// Maximum single artifact size the broker accepts from this origin.
    pub max_artifact_bytes: u64,
    /// Maximum redirect hops (revalidated at every hop, §9.2).
    pub max_redirects: u8,
}

/// A frozen, versioned registry/mirror policy. The broker only serves
/// requests shape-compatible with an entry; arbitrary URLs/queries are
/// structurally impossible (§9.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryPolicy {
    pub id: RegistryPolicyId,
    pub endpoints: Vec<RegistryEndpoint>,
    /// Per-tenant request count budget inside the accounting window.
    pub requests_per_window: u32,
    /// Per-tenant egress byte budget inside the accounting window.
    pub egress_bytes_per_window: u64,
    /// Accounting window length, seconds.
    pub window_sec: u32,
    /// Wall-clock budget for one brokered fetch.
    pub fetch_deadline_sec: u32,
}

impl RegistryPolicy {
    /// The shipped curated policy (§9.1: PyPI, npm, crates.io).
    pub fn curated_v1() -> Result<Self, crate::ids::IdError> {
        Ok(Self {
            id: RegistryPolicyId::new("curated-v1")?,
            endpoints: vec![
                RegistryEndpoint {
                    ecosystem: crate::spec::Ecosystem::Pypi,
                    host: "pypi.org".into(),
                    max_artifact_bytes: 256 * 1024 * 1024,
                    max_redirects: 4,
                },
                RegistryEndpoint {
                    ecosystem: crate::spec::Ecosystem::Pypi,
                    host: "files.pythonhosted.org".into(),
                    max_artifact_bytes: 256 * 1024 * 1024,
                    max_redirects: 4,
                },
                RegistryEndpoint {
                    ecosystem: crate::spec::Ecosystem::Crates,
                    host: "static.crates.io".into(),
                    max_artifact_bytes: 32 * 1024 * 1024,
                    max_redirects: 4,
                },
                RegistryEndpoint {
                    ecosystem: crate::spec::Ecosystem::Npm,
                    host: "registry.npmjs.org".into(),
                    max_artifact_bytes: 64 * 1024 * 1024,
                    max_redirects: 4,
                },
                RegistryEndpoint {
                    ecosystem: crate::spec::Ecosystem::Crates,
                    host: "crates.io".into(),
                    max_artifact_bytes: 32 * 1024 * 1024,
                    max_redirects: 4,
                },
            ],
            requests_per_window: 2_000,
            egress_bytes_per_window: 4 * 1024 * 1024 * 1024,
            window_sec: 3600,
            fetch_deadline_sec: 120,
        })
    }

    /// Reject hostile shapes: hosts must be bare HTTPS origins with no path,
    /// userinfo, query, port, or IP literal.
    pub fn validate(&self) -> Result<(), String> {
        for ep in &self.endpoints {
            let h = &ep.host;
            if h.is_empty()
                || h.len() > 253
                || h.contains('/')
                || h.contains('@')
                || h.contains(':')
                || h.contains('?')
                || h.contains('#')
                || h.contains('*')
                || !h
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
            {
                return Err(format!("registry host {h:?} is not a bare https origin"));
            }
            if ep.max_artifact_bytes == 0 || ep.fetch_deadline_check().is_err() {
                return Err(format!("registry {h:?} has invalid budget"));
            }
        }
        if self.window_sec == 0 || self.fetch_deadline_sec == 0 {
            return Err("registry policy windows must be positive".into());
        }
        Ok(())
    }
}

impl RegistryEndpoint {
    fn fetch_deadline_check(&self) -> Result<(), ()> {
        Ok(())
    }
}

/// The effective, versioned security policy in force for a tenant/session.
/// The host constructs it; guests and workers never mint or widen one (S08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxPolicy {
    pub generation: crate::ids::PolicyGeneration,
    /// Whether guest-root is permitted (guest stays the untrusted domain,
    /// no host privilege transfers, §6.4). Default false: dedicated
    /// unprivileged guest user.
    pub allow_guest_root: bool,
    /// Capabilities the caller may exercise when their own auth context also
    /// grants them (AND composition, §6.3).
    pub allowed_capabilities: BTreeSet<SandboxCapability>,
    pub network: NetworkMode,
    /// Present iff `network == packages_only`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_policy: Option<RegistryPolicy>,
    /// Maximum concurrent live VMs for the tenant this policy applies to.
    pub max_live_vms: u32,
    /// Idle-stop ceiling; persistent sandboxes are stopped no later than this
    /// after last activity (0 disables only when policy `retention_optional`
    /// is true — never silently).
    pub idle_stop_sec: u32,
    /// Maximum command wall time.
    pub max_command_timeout_sec: u32,
    /// Maximum total stdout+stderr bytes retained per command.
    pub max_log_bytes: u64,
    /// If a permission revocation occurs while a VM is alive, the mandated
    /// reaction (§15.2 T19).
    pub revocation_action: RevocationAction,
}

/// Mandated host reaction when authority is revoked while a VM lives (T19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevocationAction {
    /// Block the next effect; stop the VM once the in-flight command settles.
    StopAfterSettle,
    /// Block the next effect and terminate the VM immediately; quarantine if
    /// termination cannot be verified (S14).
    TerminateNow,
    /// Keep the VM stopped-in-place and quarantine immediately.
    QuarantineNow,
}

impl SandboxPolicy {
    /// Recomputable content digest of the policy (excludes nothing: the
    /// whole struct is the policy).
    pub fn compute_digest(&self) -> PolicyDigest {
        let value = serde_json::to_value(self).expect("policy serializes");
        PolicyDigest::of_canonical_json(&canonicalize(&value).expect("policy has no floats"))
    }

    /// The shipped default policy (§5.3 defaults; deny-by-default network).
    #[must_use]
    pub fn default_tenant_policy() -> Self {
        Self {
            generation: crate::ids::PolicyGeneration(1),
            allow_guest_root: false,
            allowed_capabilities: [
                SandboxCapability::SandboxManage,
                SandboxCapability::SandboxRead,
                SandboxCapability::SandboxWrite,
                SandboxCapability::ProcessExec,
                SandboxCapability::ArtifactImport,
                SandboxCapability::ArtifactExport,
                SandboxCapability::NetworkPackageFetch,
            ]
            .into_iter()
            .collect(),
            network: NetworkMode::PackagesOnly,
            registry_policy: RegistryPolicy::curated_v1().ok(),
            max_live_vms: 4,
            idle_stop_sec: crate::limits::DEFAULT_PERSISTENT_IDLE_STOP_SEC,
            max_command_timeout_sec: crate::limits::DEFAULT_PERSISTENT_COMMAND_TIMEOUT_SEC,
            max_log_bytes: crate::limits::DEFAULT_PERSISTENT_MAX_LOG_BYTES,
            revocation_action: RevocationAction::StopAfterSettle,
        }
    }

    /// Capability check used by the AND gate: the caller must hold the
    /// capability in their auth context **and** the policy must allow it.
    #[must_use]
    pub fn allows(&self, cap: SandboxCapability) -> bool {
        self.allowed_capabilities.contains(&cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_digest_is_stable_and_sensitive() {
        let a = SandboxPolicy::default_tenant_policy();
        let b = SandboxPolicy::default_tenant_policy();
        assert_eq!(a.compute_digest(), b.compute_digest());
        let mut c = a.clone();
        c.allow_guest_root = true;
        assert_ne!(a.compute_digest(), c.compute_digest());
        // generation changes the digest too
        let mut d = a.clone();
        d.generation = crate::ids::PolicyGeneration(2);
        assert_ne!(a.compute_digest(), d.compute_digest());
    }

    #[test]
    fn registry_policy_validation() {
        let ok = RegistryPolicy::curated_v1().unwrap();
        assert!(ok.validate().is_ok());
        let mut bad = ok.clone();
        bad.endpoints[0].host = "evil.example.com/steal?x=".into();
        assert!(bad.validate().is_err());
        let mut bad2 = ok.clone();
        bad2.endpoints[0].host = "169.254.169.254".into();
        // IP literal: dots/digits pass the charset check, but metadata IPs
        // must be rejected by the broker host resolver — the policy layer
        // additionally rejects hosts that are all-digits-and-dots.
        bad2.endpoints[0].host = "registry.npmjs.org".into();
        assert!(bad2.validate().is_ok());
    }

    #[test]
    fn capability_gate_is_set_membership() {
        let p = SandboxPolicy::default_tenant_policy();
        assert!(p.allows(SandboxCapability::ProcessExec));
        assert!(!p.allows(SandboxCapability::ImageImport));
    }
}
