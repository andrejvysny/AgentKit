//! Sandbox specification (§5.2, §5.3).
//!
//! `SandboxSpec` is the **untrusted wire shape** — it intentionally carries
//! no `tenant_id`: the host injects it from authenticated context (S02).
//! Validation clamps requested limits into server ceilings; `host_mounts`
//! must be empty and a non-empty array is **rejected, never ignored** (S03).

use crate::ids::{AgentId, ImageDigest, PrincipalId};
use crate::limits::{
    DEFAULT_EPHEMERAL_COMMAND_TIMEOUT_SEC, DEFAULT_EPHEMERAL_DISK_MIB,
    DEFAULT_EPHEMERAL_MAX_LOG_BYTES, DEFAULT_EPHEMERAL_MAX_PROCESSES, DEFAULT_EPHEMERAL_MEMORY_MIB,
    DEFAULT_EPHEMERAL_VCPUS, DEFAULT_PERSISTENT_COMMAND_TIMEOUT_SEC, DEFAULT_PERSISTENT_DISK_MIB,
    DEFAULT_PERSISTENT_IDLE_STOP_SEC, DEFAULT_PERSISTENT_MAX_LOG_BYTES,
    DEFAULT_PERSISTENT_MAX_PROCESSES, DEFAULT_PERSISTENT_MEMORY_MIB, DEFAULT_PERSISTENT_VCPUS,
    ResourceCeilings, clamp_u32, clamp_u64,
};
use crate::path::GuestPath;
use crate::policy::NetworkMode;
use serde::{Deserialize, Serialize};

/// Package ecosystems known to the broker (§9.1). Anything else is
/// `sandbox_package_denied`/`sandbox_capability_unsupported`, never a
/// free-form fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ecosystem {
    Pypi,
    Npm,
    Crates,
}

/// Ownership of a sandbox (§13.1). `agent_id` may be absent for general
/// Chat; then the binding is `(tenant, principal, session)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxOwner {
    pub principal_id: PrincipalId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
}

/// Ephemeral sandboxes are destroyed on completion; persistent sandboxes
/// retain their private disk across VM generations (§5.3, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Ephemeral,
    Persistent,
}

/// Image reference by content digest only (§10). References by mutable tag
/// are not accepted at this layer: resolution to a digest happens in the
/// authorized image importer, never at launch time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRef {
    pub digest: ImageDigest,
    /// Must be `linux` for the isolated tier.
    pub os: String,
    /// `x86_64` or `aarch64`, arch-matched to the host (no cross-arch
    /// emulation promise, §10.3).
    pub arch: String,
}

impl ImageRef {
    pub fn validate(&self) -> Result<(), String> {
        if self.os != "linux" {
            return Err(format!("unsupported guest os {:?}", self.os));
        }
        if self.arch != "x86_64" && self.arch != "aarch64" {
            return Err(format!("unsupported guest arch {:?}", self.arch));
        }
        Ok(())
    }
}

/// Requested resource profile. Zero/unset fields take the lifecycle default;
/// all values are clamped server-side into the ceilings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceProfile {
    #[serde(default)]
    pub vcpus: u32,
    #[serde(default)]
    pub memory_mib: u32,
    #[serde(default)]
    pub writable_disk_mib: u32,
    #[serde(default)]
    pub max_processes: u32,
    #[serde(default)]
    pub command_timeout_sec: u32,
    #[serde(default)]
    pub max_log_bytes: u64,
}

/// Effective (clamped) resources recorded at admission (S01, S13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveResources {
    pub vcpus: u32,
    pub memory_mib: u32,
    pub writable_disk_mib: u32,
    pub max_processes: u32,
    pub command_timeout_sec: u32,
    pub max_log_bytes: u64,
}

/// Network block on the wire (§5.2). `mode` other than `none`/`packages_only`
/// is rejected as unsupported (v1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSpec {
    pub mode: NetworkMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_policy_id: Option<crate::ids::RegistryPolicyId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionSpec {
    /// Idle seconds before a persistent sandbox's VM is stopped (private
    /// disk retained). 0 means "take default".
    #[serde(default)]
    pub idle_stop_sec: u32,
}

/// Workspace block (§5.2). `host_mounts` exists **only** so a client that
/// sends it can be rejected with an actionable error (S03): the VM tier
/// accepts an empty array and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSpec {
    pub root_in_guest: GuestPath,
    #[serde(default)]
    pub host_mounts: Vec<String>,
}

/// The untrusted creation request (§5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxSpec {
    pub version: u32,
    pub owner: SandboxOwner,
    pub lifecycle: Lifecycle,
    pub image: ImageRef,
    pub resources: ResourceProfile,
    pub network: NetworkSpec,
    pub retention: RetentionSpec,
    pub workspace: WorkspaceSpec,
}

/// Rejection reasons surfaced as `sandbox_policy_denied`/`sandbox_image_*`
/// details.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SpecError {
    #[error("unsupported spec version {0}; this host speaks version {expected}", expected = crate::SPEC_VERSION)]
    UnsupportedVersion(u32),
    #[error(
        "host_mounts must be empty for the VM security tier (received {0} entries); host mounts are refused, never ignored"
    )]
    HostMountsForbidden(usize),
    #[error("workspace root must be {expected}", expected = GuestRootExpected)]
    UnexpectedWorkspaceRoot,
    #[error("image invalid: {0}")]
    ImageInvalid(String),
    #[error("network mode {0:?} is not supported in v1")]
    NetworkUnsupported(NetworkMode),
    #[error("registry policy {0} is not the policy in force ({1})")]
    RegistryPolicyMismatch(String, String),
    #[error("requested {field}={requested} exceeds ceiling {ceiling}")]
    OverCeiling {
        field: &'static str,
        requested: u64,
        ceiling: u64,
    },
    #[error("image digest is not in the approved image registry")]
    ImageNotApproved,
}

struct GuestRootExpected;
impl std::fmt::Display for GuestRootExpected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "/workspace")
    }
}

/// The admission-verified, clamped specification the worker receives.
/// Carries the server-derived tenant binding; **the worker must refuse any
/// command whose envelope tenant differs** (S02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedSandboxSpec {
    pub tenant_id: crate::ids::TenantId,
    pub owner: SandboxOwner,
    pub lifecycle: Lifecycle,
    pub image: ImageRef,
    pub resources: EffectiveResources,
    pub network: NetworkMode,
    pub idle_stop_sec: u32,
    pub workspace_root: GuestPath,
    /// Digest of the policy in force at admission (S01).
    pub policy_digest: crate::ids::PolicyDigest,
    pub policy_generation: crate::ids::PolicyGeneration,
}

/// Validation environment supplied by the trusted host (§5.3): ceilings, the
/// current policy, and the set of approved image digests.
pub struct AdmissionContext<'a> {
    pub ceilings: &'a ResourceCeilings,
    pub policy: &'a crate::policy::SandboxPolicy,
    /// Approved image digests (image registry). Empty means "none approved".
    pub approved_images: &'a std::collections::BTreeSet<ImageDigest>,
    /// Host architecture; the image arch must match exactly (§10.3).
    pub host_arch: &'static str,
}

impl SandboxSpec {
    /// Validate and clamp into a `VerifiedSandboxSpec`, injecting the
    /// authenticated tenant (S02). Every rejection is explicit; nothing is
    /// silently defaulted except unset (zero) resource fields.
    pub fn verify(
        &self,
        tenant_id: &crate::ids::TenantId,
        ctx: &AdmissionContext<'_>,
    ) -> Result<VerifiedSandboxSpec, SpecError> {
        if self.version != crate::SPEC_VERSION {
            return Err(SpecError::UnsupportedVersion(self.version));
        }
        if !self.workspace.host_mounts.is_empty() {
            return Err(SpecError::HostMountsForbidden(
                self.workspace.host_mounts.len(),
            ));
        }
        if self.workspace.root_in_guest.as_str() != "/workspace" {
            return Err(SpecError::UnexpectedWorkspaceRoot);
        }
        self.image.validate().map_err(SpecError::ImageInvalid)?;
        if self.image.arch != ctx.host_arch {
            return Err(SpecError::ImageInvalid(format!(
                "image arch {} does not match host arch {}",
                self.image.arch, ctx.host_arch
            )));
        }
        if !ctx.approved_images.contains(&self.image.digest) {
            return Err(SpecError::ImageNotApproved);
        }
        match self.network.mode {
            NetworkMode::None | NetworkMode::PackagesOnly => {}
            other => return Err(SpecError::NetworkUnsupported(other)),
        }
        if self.network.mode == NetworkMode::PackagesOnly {
            let in_force = ctx
                .policy
                .registry_policy
                .as_ref()
                .map(|p| p.id.as_str().to_string());
            let want = self
                .network
                .registry_policy_id
                .as_ref()
                .map(|p| p.as_str().to_string());
            match (&in_force, &want) {
                (_, None) => {} // accept the in-force policy
                (Some(a), Some(b)) if a == b => {}
                _ => {
                    return Err(SpecError::RegistryPolicyMismatch(
                        want.unwrap_or_default(),
                        in_force.unwrap_or_default(),
                    ));
                }
            }
        } else if self.network.registry_policy_id.is_some() {
            return Err(SpecError::RegistryPolicyMismatch(
                "with mode none a registry policy must not be named".into(),
                String::new(),
            ));
        }

        let c = ctx.ceilings;
        let resources = match self.lifecycle {
            Lifecycle::Ephemeral => EffectiveResources {
                vcpus: clamp_u32(self.resources.vcpus, DEFAULT_EPHEMERAL_VCPUS, c.max_vcpus),
                memory_mib: clamp_u32(
                    self.resources.memory_mib,
                    DEFAULT_EPHEMERAL_MEMORY_MIB,
                    c.max_memory_mib,
                ),
                writable_disk_mib: clamp_u32(
                    self.resources.writable_disk_mib,
                    DEFAULT_EPHEMERAL_DISK_MIB,
                    c.max_writable_disk_mib,
                ),
                max_processes: clamp_u32(
                    self.resources.max_processes,
                    DEFAULT_EPHEMERAL_MAX_PROCESSES,
                    c.max_processes,
                ),
                command_timeout_sec: clamp_u32(
                    self.resources.command_timeout_sec,
                    DEFAULT_EPHEMERAL_COMMAND_TIMEOUT_SEC,
                    c.max_command_timeout_sec
                        .min(ctx.policy.max_command_timeout_sec),
                ),
                max_log_bytes: clamp_u64(
                    self.resources.max_log_bytes,
                    DEFAULT_EPHEMERAL_MAX_LOG_BYTES,
                    c.max_log_bytes.min(ctx.policy.max_log_bytes),
                ),
            },
            Lifecycle::Persistent => EffectiveResources {
                vcpus: clamp_u32(self.resources.vcpus, DEFAULT_PERSISTENT_VCPUS, c.max_vcpus),
                memory_mib: clamp_u32(
                    self.resources.memory_mib,
                    DEFAULT_PERSISTENT_MEMORY_MIB,
                    c.max_memory_mib,
                ),
                writable_disk_mib: clamp_u32(
                    self.resources.writable_disk_mib,
                    DEFAULT_PERSISTENT_DISK_MIB,
                    c.max_writable_disk_mib,
                ),
                max_processes: clamp_u32(
                    self.resources.max_processes,
                    DEFAULT_PERSISTENT_MAX_PROCESSES,
                    c.max_processes,
                ),
                command_timeout_sec: clamp_u32(
                    self.resources.command_timeout_sec,
                    DEFAULT_PERSISTENT_COMMAND_TIMEOUT_SEC,
                    c.max_command_timeout_sec
                        .min(ctx.policy.max_command_timeout_sec),
                ),
                max_log_bytes: clamp_u64(
                    self.resources.max_log_bytes,
                    DEFAULT_PERSISTENT_MAX_LOG_BYTES,
                    c.max_log_bytes.min(ctx.policy.max_log_bytes),
                ),
            },
        };
        let idle_stop_sec = clamp_u32(
            self.retention.idle_stop_sec,
            DEFAULT_PERSISTENT_IDLE_STOP_SEC,
            ctx.policy.idle_stop_sec,
        );
        Ok(VerifiedSandboxSpec {
            tenant_id: tenant_id.clone(),
            owner: self.owner.clone(),
            lifecycle: self.lifecycle,
            image: self.image.clone(),
            resources,
            network: self.network.mode,
            idle_stop_sec,
            workspace_root: self.workspace.root_in_guest.clone(),
            policy_digest: ctx.policy.compute_digest(),
            policy_generation: ctx.policy.generation,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ImageDigest, PrincipalId, TenantId};
    use crate::path::GuestPath;

    fn spec() -> SandboxSpec {
        SandboxSpec {
            version: 1,
            owner: SandboxOwner {
                principal_id: PrincipalId::new("user-1").unwrap(),
                agent_id: None,
            },
            lifecycle: Lifecycle::Ephemeral,
            image: ImageRef {
                digest: ImageDigest::of_bytes(b"img"),
                os: "linux".into(),
                arch: crate::host_arch().to_string(),
            },
            resources: ResourceProfile::default(),
            network: NetworkSpec {
                mode: NetworkMode::PackagesOnly,
                registry_policy_id: None,
            },
            retention: RetentionSpec { idle_stop_sec: 0 },
            workspace: WorkspaceSpec {
                root_in_guest: GuestPath::parse("/workspace").unwrap(),
                host_mounts: vec![],
            },
        }
    }

    #[test]
    fn host_mounts_are_rejected_not_ignored() {
        let mut s = spec();
        s.workspace.host_mounts = vec!["/home/victim".into()];
        let ctx = admission();
        let err = s
            .verify(&tenant(), &ctx)
            .expect_err("must reject host_mounts");
        assert!(matches!(err, SpecError::HostMountsForbidden(1)));
    }

    #[test]
    fn tenant_is_injected_wire_has_no_tenant_field() {
        let raw = serde_json::to_value(spec()).unwrap();
        assert!(raw.get("tenant_id").is_none(), "wire spec must omit tenant");
        let bad = r#"{"version":1,"tenant_id":"evil","owner":{"principal_id":"u"},"lifecycle":"ephemeral","image":{"digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","os":"linux","arch":"x86_64"},"resources":{},"network":{"mode":"packages_only"},"retention":{},"workspace":{"root_in_guest":"/workspace","host_mounts":[]}}"#;
        assert!(
            serde_json::from_str::<SandboxSpec>(bad).is_err(),
            "deny_unknown_fields must reject injected tenant_id"
        );
    }

    #[test]
    fn limits_are_clamped_never_trusted() {
        let mut s = spec();
        s.resources.vcpus = u32::MAX;
        s.resources.memory_mib = u32::MAX;
        s.resources.command_timeout_sec = u32::MAX;
        let ctx = admission();
        let v = s.verify(&tenant(), &ctx).unwrap();
        assert_eq!(v.resources.vcpus, ctx.ceilings.max_vcpus);
        assert_eq!(v.resources.memory_mib, ctx.ceilings.max_memory_mib);
        assert!(v.resources.command_timeout_sec <= ctx.ceilings.max_command_timeout_sec);
        // zero means default, not unbounded
        let v0 = spec().verify(&tenant(), &ctx).unwrap();
        assert_eq!(v0.resources.vcpus, DEFAULT_EPHEMERAL_VCPUS);
        assert_eq!(
            v0.resources.command_timeout_sec,
            DEFAULT_EPHEMERAL_COMMAND_TIMEOUT_SEC
        );
    }

    #[test]
    fn unsupported_shapes_fail_closed() {
        let ctx = admission();
        let mut s = spec();
        s.version = 99;
        assert!(matches!(
            s.verify(&tenant(), &ctx),
            Err(SpecError::UnsupportedVersion(99))
        ));
        let mut s = spec();
        s.network.mode = NetworkMode::Restricted;
        assert!(matches!(
            s.verify(&tenant(), &ctx),
            Err(SpecError::NetworkUnsupported(_))
        ));
        let mut s = spec();
        s.image.os = "windows".into();
        assert!(matches!(
            s.verify(&tenant(), &ctx),
            Err(SpecError::ImageInvalid(_))
        ));
        let mut s = spec();
        s.image.digest = ImageDigest::of_bytes(b"not-approved");
        assert!(matches!(
            s.verify(&tenant(), &ctx),
            Err(SpecError::ImageNotApproved)
        ));
    }

    fn tenant() -> TenantId {
        TenantId::new("tenant-a").unwrap()
    }
    static CEILINGS: std::sync::LazyLock<ResourceCeilings> =
        std::sync::LazyLock::new(ResourceCeilings::default);
    static POLICY: std::sync::LazyLock<crate::policy::SandboxPolicy> =
        std::sync::LazyLock::new(crate::policy::SandboxPolicy::default_tenant_policy);
    static APPROVED: std::sync::LazyLock<std::collections::BTreeSet<ImageDigest>> =
        std::sync::LazyLock::new(|| [ImageDigest::of_bytes(b"img")].into_iter().collect());

    fn admission() -> AdmissionContext<'static> {
        AdmissionContext {
            ceilings: &CEILINGS,
            policy: &POLICY,
            approved_images: &APPROVED,
            host_arch: crate::host_arch(),
        }
    }
}
