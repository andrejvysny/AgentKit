//! Identity newtypes (§5.1). All identifiers crossing a trust boundary are
//! explicitly typed, validated on construction, and validated again on
//! deserialization. Malformed, empty, or oversized ids are rejected at the
//! boundary; cross-tenant references are rejected by the services holding
//! authoritative state, which always compare the embedded `TenantId`.
//!
//! Ids are opaque ASCII tokens, never user-supplied file paths (S01, S02,
//! S12: "Never use user-provided file paths as sandbox ownership keys").

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::fmt;

/// Maximum character length of an opaque identifier token.
pub const MAX_ID_LEN: usize = 128;

#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone)]
pub enum IdError {
    #[error("id {field} is empty")]
    Empty { field: &'static str },
    #[error("id {field} exceeds {max} characters")]
    TooLong { field: &'static str, max: usize },
    #[error(
        "id {field} contains invalid characters (allowed: A-Z a-z 0-9 . _ -, must start alphanumeric): {value:?}"
    )]
    InvalidChars { field: &'static str, value: String },
    #[error("digest {field} is not sha256:<64 lowercase hex>: {value:?}")]
    InvalidDigest { field: &'static str, value: String },
}

fn validate_token(field: &'static str, value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty { field });
    }
    if value.len() > MAX_ID_LEN {
        return Err(IdError::TooLong {
            field,
            max: MAX_ID_LEN,
        });
    }
    let first_ok = value
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_alphanumeric());
    let rest_ok = value
        .as_bytes()
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !first_ok || !rest_ok {
        return Err(IdError::InvalidChars {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

/// Generates a validated opaque string-id newtype with a distinct field name.
macro_rules! opaque_id {
    ($(#[$meta:meta])* $name:ident, $field:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Validate-and-wrap. Rejects empty, oversized, or illegal ids.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                validate_token($field, &value)?;
                Ok(Self(value))
            }
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdError;
            fn try_from(v: &str) -> Result<Self, Self::Error> {
                Self::new(v)
            }
        }
        impl TryFrom<String> for $name {
            type Error = IdError;
            fn try_from(v: String) -> Result<Self, Self::Error> {
                Self::new(v)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                Self::new(raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

opaque_id!(
    /// Server-derived tenant identity. Mandatory on every query, record,
    /// artifact, worker journal entry, disk root, VM identity and quota
    /// (S01, S02). Never accepted from model or client input.
    TenantId, "tenant_id"
);
opaque_id!(
    /// Authenticated user/service principal within a tenant.
    PrincipalId, "principal_id"
);
opaque_id!(
    /// Existing AgentKit agent identity (not a new task record).
    AgentId, "agent_id"
);
opaque_id!(
    /// Existing AgentKit session identity.
    SessionId, "session_id"
);
opaque_id!(
    /// Existing AgentKit task identity.
    TaskId, "task_id"
);
opaque_id!(
    /// Existing AgentKit attempt identity.
    AttemptId, "attempt_id"
);
opaque_id!(
    /// Opaque stable identity of a named/ephemeral sandbox workspace.
    /// Random, server-generated; never derived from user file paths.
    SandboxId, "sandbox_id"
);
opaque_id!(
    /// Stable worker identity (paired with revision + install fingerprint in
    /// `WorkerIdentity`).
    WorkerId, "worker_id"
);
opaque_id!(
    /// Stable execution identity: idempotency key for one admitted command.
    ExecutionId, "execution_id"
);
opaque_id!(
    /// Existing AgentKit tool-call identity, bound into receipts.
    ToolCallId, "tool_call_id"
);
opaque_id!(
    /// Stable identity of one staged import/export staging object.
    StageId, "stage_id"
);
opaque_id!(
    /// Stable identity of one export manifest.
    ExportId, "export_id"
);
opaque_id!(
    /// Identifier of a frozen registry/allowlist policy for the package
    /// broker (e.g. `curated-v1`).
    RegistryPolicyId, "registry_policy_id"
);

/// Monotonic generation counter for VM create/restart/restore. Every boot of
/// a sandbox increments it; authorization, leases, protocol frames and disk
/// mounts bound to a stale generation are refused (S01, S09, S15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstanceGeneration(pub u64);

impl InstanceGeneration {
    pub const MIN: Self = Self(crate::MIN_GENERATION);
    pub fn new(value: u64) -> Result<Self, IdError> {
        if value == 0 {
            return Err(IdError::Empty {
                field: "instance_generation",
            });
        }
        Ok(Self(value))
    }
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl Serialize for InstanceGeneration {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.0)
    }
}
impl<'de> Deserialize<'de> for InstanceGeneration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = u64::deserialize(d)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

/// Monotonic security-policy revision. Running VMs are pinned to a generation
/// + digest; revocation can force a stop (S01, §14.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PolicyGeneration(pub u64);

/// A `sha256:<64 lowercase hex>` content digest.
macro_rules! digest_type {
    ($(#[$meta:meta])* $name:ident, $field:literal) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: &str) -> Result<Self, IdError> {
                let ok = value.len() == 71
                    && value.get(0..7) == Some("sha256:")
                    && value[7..].bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
                if !ok {
                    return Err(IdError::InvalidDigest {
                        field: $field,
                        value: value.to_string(),
                    });
                }
                Ok(Self(value.to_ascii_lowercase()))
            }
            /// Digest of arbitrary bytes (SHA-256, `sha256:<hex>`).
            pub fn of_bytes(bytes: &[u8]) -> Self {
                let mut h = Sha256::new();
                h.update(bytes);
                Self(format!("sha256:{}", hex_lower(&h.finalize())))
            }
            /// Wrap a bare lowercase-hex SHA-256 (manifest/sidecar files).
            pub fn from_hex(hex: &str) -> Result<Self, IdError> {
                Self::parse(&format!("sha256:{}", hex.to_ascii_lowercase()))
            }
            /// Digest of canonical JSON bytes (see `canonical::canonicalize`).
            pub fn of_canonical_json(bytes: &[u8]) -> Self {
                Self::of_bytes(bytes)
            }
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
            #[must_use]
            pub fn bytes_hex(&self) -> &str {
                &self.0[7..]
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), &self.0)
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                Self::parse(&raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

digest_type!(
    /// Content-addressed identity of an (already-validated) VM image rootfs.
    ImageDigest, "image_digest"
);
digest_type!(
    /// Content-addressed identity of a staged/exported artifact blob.
    ArtifactDigest, "artifact_digest"
);
digest_type!(
    /// Digest of the effective security policy in force.
    PolicyDigest, "policy_digest"
);
digest_type!(
    /// Digest over the canonical JSON of a request body, for idempotency and
    /// duplicate-suppression (S07, S16).
    RequestDigest, "request_digest"
);
digest_type!(
    /// Digest of a worker installation (binary + assets fingerprint).
    InstallFingerprint, "install_fingerprint"
);

/// Worker identity as presented during host<->worker handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerIdentity {
    pub worker_id: WorkerId,
    /// Monotonic build/upgrade revision of the worker installation.
    pub revision: u64,
    /// Content digest of the worker installation and its pinned assets.
    pub install_fingerprint: InstallFingerprint,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_malformed_ids() {
        assert_eq!(
            SandboxId::new("").unwrap_err(),
            IdError::Empty {
                field: "sandbox_id"
            }
        );
        assert!(SandboxId::new("/etc/passwd").is_err());
        assert!(SandboxId::new("../evil").is_err());
        assert!(SandboxId::new("with space").is_err());
        assert!(SandboxId::new("ünïcode").is_err());
        assert!(SandboxId::new("a".repeat(MAX_ID_LEN + 1)).is_err());
        assert!(SandboxId::new("ok.token-1_2").is_ok());
        assert!(SandboxId::new("-leading-dash").is_err());
    }

    #[test]
    fn digest_format_strict() {
        let good = ImageDigest::of_bytes(b"hello");
        assert!(ImageDigest::parse(good.as_str()).is_ok());
        assert!(ImageDigest::parse("sha512:abc").is_err());
        assert!(ImageDigest::parse("sha256:ABCDEF").is_err());
        assert!(ImageDigest::parse(&format!("sha256:{}", "a".repeat(63))).is_err());
        // Round-trip through serde re-validates.
        let json = serde_json::to_string(&good).unwrap();
        assert!(serde_json::from_str::<ImageDigest>("\"sha256:nope\"").is_err());
        assert_eq!(serde_json::from_str::<ImageDigest>(&json).unwrap(), good);
    }

    #[test]
    fn generation_never_zero() {
        assert!(InstanceGeneration::new(0).is_err());
        assert_eq!(InstanceGeneration::MIN, InstanceGeneration(1));
        assert_eq!(InstanceGeneration(7).next(), InstanceGeneration(8));
        assert!(serde_json::from_str::<InstanceGeneration>("0").is_err());
    }
}
