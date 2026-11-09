//! Host-created effective capabilities and authenticated caller context.
//!
//! `AuthContext` is constructed only inside the trusted Host from
//! authenticated server state — it deliberately does **not** implement
//! `Serialize`/`Deserialize`, so it can never arrive over any wire (S02,
//! S08). `WorkspaceGrant` is the host-minted effective capability the worker
//! checks on **every** action against authoritative expectations; a guest can
//! neither mint nor widen one (S08, §6.2).

use crate::ids::{
    AgentId, AttemptId, ExecutionId, InstanceGeneration, PolicyDigest, PolicyGeneration,
    PrincipalId, RequestDigest, SandboxId, SessionId, TaskId, TenantId, ToolCallId,
};
use crate::limits::{MAX_ENV_TOTAL_BYTES, MAX_ENV_VALUE_BYTES, MAX_ENV_VARS};
use crate::path::GuestPath;
use crate::policy::SandboxCapability;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Authenticated caller context injected by the trusted Host (never from the
/// model/client wire). All sandbox service methods take it as the first
/// argument and derive tenancy from it (S02).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthContext {
    pub tenant_id: TenantId,
    pub principal_id: PrincipalId,
    pub session_id: SessionId,
    pub agent_id: Option<AgentId>,
    pub task_id: Option<TaskId>,
    pub attempt_id: Option<AttemptId>,
    pub capabilities: BTreeSet<SandboxCapability>,
    /// Human-readable reason for this authorization, audit-safe (ids/text
    /// chosen by the host, never guest content).
    pub authorization_reason: String,
}

impl AuthContext {
    #[must_use]
    pub fn has(&self, cap: SandboxCapability) -> bool {
        self.capabilities.contains(&cap)
    }
}

/// The exclusive workspace writer token (S09). Held by at most one admitted
/// writer at a time per sandbox generation; read-only queries may run
/// concurrently only where consistency is defined by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriterLeaseToken {
    pub sandbox_id: SandboxId,
    pub instance_generation: InstanceGeneration,
    /// Monotonic per-sandbox writer epoch; stale epochs are refused (S09,
    /// S15).
    pub writer_epoch: u64,
    pub holder_task_id: TaskId,
    pub expires_at_ms: u64,
}

/// Host-minted effective capability for one intended request (§6.2).
/// Everything a worker needs to authorize server-side is in here; nothing is
/// a reusable bearer token, and it is never written into the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGrant {
    pub tenant_id: TenantId,
    pub principal_id: PrincipalId,
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<AttemptId>,
    pub sandbox_id: SandboxId,
    pub instance_generation: InstanceGeneration,
    pub policy_generation: PolicyGeneration,
    pub policy_digest: PolicyDigest,
    /// The tool capability class this grant covers.
    pub capability: SandboxCapability,
    /// Optional writer lease for mutating operations (S09).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writer_lease: Option<WriterLeaseToken>,
    /// Unix ms expiry. Expired grants fail with `sandbox_policy_denied` and
    /// require a fresh host decision.
    pub expires_at_ms: u64,
    /// Digest over the canonical JSON of the intended request body; binds the
    /// grant to exactly one request shape (S07).
    pub intended_request_digest: RequestDigest,
    /// Existing AgentKit tool-call identity for receipt correlation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    /// Paths this grant permits (exact file paths, or directory prefixes for
    /// list/search/exec-cwd). Cross-path use is refused (S12).
    pub permitted_paths: Vec<GuestPath>,
    /// Permitted guest working directory for exec, when capability is exec.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec_cwd: Option<GuestPath>,
}

/// Authoritative expectations a worker checks a grant against before acting.
#[derive(Debug, Clone)]
pub struct GrantExpectation {
    pub tenant: TenantId,
    pub sandbox: SandboxId,
    pub generation: InstanceGeneration,
    pub policy_generation: PolicyGeneration,
    pub policy_digest: PolicyDigest,
    pub capability: SandboxCapability,
    pub request_digest: RequestDigest,
    pub now_ms: u64,
}

impl WorkspaceGrant {
    /// Server-side verification against authoritative expectations. The
    /// worker calls this before acting on any request (S08: every worker
    /// action checks the grant server-side).
    pub fn verify(&self, exp: &GrantExpectation) -> Result<(), crate::error::SandboxError> {
        let expected_tenant = &exp.tenant;
        let expected_sandbox = &exp.sandbox;
        let current_generation = exp.generation;
        let expected_policy = (&exp.policy_generation, &exp.policy_digest);
        let capability = exp.capability;
        let request_digest = &exp.request_digest;
        let now_ms = exp.now_ms;
        let e = || {
            crate::error::SandboxError::new(
                crate::error::SandboxErrorCode::PolicyDenied,
                "grant does not match authoritative state",
            )
        };
        if &self.tenant_id != expected_tenant {
            // Do not disclose existence across tenants.
            return Err(crate::error::SandboxError::new(
                crate::error::SandboxErrorCode::TenantForbidden,
                "no such sandbox for this tenant",
            )
            .with_sandbox(expected_sandbox, Some(current_generation.0)));
        }
        if &self.sandbox_id != expected_sandbox {
            return Err(e().with_sandbox(expected_sandbox, Some(current_generation.0)));
        }
        if self.instance_generation != current_generation {
            return Err(crate::error::SandboxError::new(
                crate::error::SandboxErrorCode::StaleGeneration,
                "grant references an old instance generation",
            )
            .with_sandbox(expected_sandbox, Some(current_generation.0)));
        }
        if &self.policy_generation != expected_policy.0 || &self.policy_digest != expected_policy.1
        {
            return Err(crate::error::SandboxError::new(
                crate::error::SandboxErrorCode::PolicyDenied,
                "grant is pinned to a revoked policy generation",
            )
            .with_sandbox(expected_sandbox, Some(current_generation.0)));
        }
        if self.capability != capability {
            return Err(e().with_detail(format!(
                "grant covers {:?}, request needs {:?}",
                self.capability, capability
            )));
        }
        if &self.intended_request_digest != request_digest {
            return Err(e().with_detail("intended_request_digest mismatch"));
        }
        if self.expires_at_ms <= now_ms {
            return Err(e().with_detail("grant expired"));
        }
        if let Some(lease) = &self.writer_lease
            && (lease.sandbox_id != self.sandbox_id
                || lease.instance_generation != self.instance_generation
                || lease.expires_at_ms <= now_ms)
        {
            return Err(e().with_detail("writer lease invalid or expired"));
        }
        Ok(())
    }

    /// Whether `path` is within this grant's permitted paths.
    #[must_use]
    pub fn permits_path(&self, path: &GuestPath) -> bool {
        self.permitted_paths
            .iter()
            .any(|p| p == path || p.is_ancestor_of(path))
    }
}

/// Guest command environment (§6.4, §8.3, S10): constructed from an explicit
/// allowlist. The **guest** provides `PATH`/`HOME`/`TERM`/`TMPDIR`; host-side
/// callers may add task-scoped values only through this builder, which
/// rejects credential-shaped names outright. Inheriting any backend
/// environment is impossible by construction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestEnvironment {
    /// Sorted for canonical digests.
    entries: Vec<(String, String)>,
}

/// Names that can never be passed through, regardless of value.
const DENIED_EXACT: &[&str] = &[
    "HOME",
    "PATH",
    "TERM",
    "TMPDIR",
    "SSH_AUTH_SOCK",
    "GPG_AGENT_INFO",
    "SSH_AGENT_PID",
    "CURL_CA_BUNDLE",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "NPM_CONFIG_REGISTRY",
    "PIP_INDEX_URL",
    "PIP_EXTRA_INDEX_URL",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "GOPATH",
    "GOPROXY",
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "KUBECONFIG",
    "DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "ftp_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "FTP_PROXY",
    "NO_PROXY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_DEFAULT_REGION",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT",
    "AZURE_CLIENT_SECRET",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GITHUB_TOKEN",
    "GH_TOKEN",
];

/// Substrings that make a name credential-shaped (mirrors the coding pack's
/// `sanitized_env` pattern semantics).
const DENIED_SUBSTRINGS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "KEY",
    "CREDENTIAL",
    "COOKIE",
    "SESSION",
    "PRIVATE",
    "AUTH",
];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EnvironmentError {
    #[error("invalid environment variable name: {0:?}")]
    InvalidName(String),
    #[error("variable {0:?} is never passable into a guest (host-only credential surface)")]
    DeniedName(String),
    #[error("environment variable value exceeds {max} bytes", max = MAX_ENV_VALUE_BYTES)]
    ValueTooLong { max: usize },
    #[error("too many environment variables (max {max})", max = MAX_ENV_VARS)]
    TooManyVars { max: usize },
    #[error("total environment size exceeds {max} bytes", max = MAX_ENV_TOTAL_BYTES)]
    TotalTooLarge { max: usize },
}

impl GuestEnvironment {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a task-scoped variable after name/value validation (S10).
    pub fn insert(&mut self, name: &str, value: &str) -> Result<(), EnvironmentError> {
        Self::validate_name(name)?;
        if DENIED_EXACT.contains(&name) {
            return Err(EnvironmentError::DeniedName(name.to_string()));
        }
        let upper = name.to_ascii_uppercase();
        // `AUTHOR`-style names are fine (exception matches the coding pack's
        // AUTH(?!OR) pattern); `OAUTH_*` etc. remain denied.
        if DENIED_SUBSTRINGS
            .iter()
            .any(|s| upper.contains(s) && !(s == &"AUTH" && upper.contains("AUTHOR")))
        {
            return Err(EnvironmentError::DeniedName(name.to_string()));
        }
        if value.len() > MAX_ENV_VALUE_BYTES {
            return Err(EnvironmentError::ValueTooLong {
                max: MAX_ENV_VALUE_BYTES,
            });
        }
        if self.entries.len() >= MAX_ENV_VARS {
            return Err(EnvironmentError::TooManyVars { max: MAX_ENV_VARS });
        }
        if name.len() + value.len() + 2 > MAX_ENV_TOTAL_BYTES {
            return Err(EnvironmentError::TotalTooLarge {
                max: MAX_ENV_TOTAL_BYTES,
            });
        }
        self.entries.retain(|(n, _)| n != name);
        self.entries.push((name.to_string(), value.to_string()));
        self.entries.sort();
        Ok(())
    }

    fn validate_name(name: &str) -> Result<(), EnvironmentError> {
        let valid = !name.is_empty()
            && name.len() <= 64
            && name.as_bytes()[0].is_ascii_alphabetic()
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if valid {
            Ok(())
        } else {
            Err(EnvironmentError::InvalidName(name.to_string()))
        }
    }

    #[must_use]
    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }

    /// Total serialized size.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|(n, v)| n.len() + v.len() + 2)
            .sum()
    }
}

/// Where a command came from, for receipts/audit (S18).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionIdentity {
    pub execution_id: ExecutionId,
    pub tool_call_id: ToolCallId,
    pub request_digest: RequestDigest,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{InstanceGeneration, SandboxId, TenantId};

    fn grant() -> WorkspaceGrant {
        WorkspaceGrant {
            tenant_id: TenantId::new("t1").unwrap(),
            principal_id: PrincipalId::new("p1").unwrap(),
            session_id: SessionId::new("s1").unwrap(),
            task_id: None,
            attempt_id: None,
            sandbox_id: SandboxId::new("sb1").unwrap(),
            instance_generation: InstanceGeneration(3),
            policy_generation: PolicyGeneration(1),
            policy_digest: PolicyDigest::of_bytes(b"policy"),
            capability: SandboxCapability::SandboxRead,
            writer_lease: None,
            expires_at_ms: 10_000,
            intended_request_digest: RequestDigest::of_bytes(b"req"),
            tool_call_id: None,
            permitted_paths: vec![GuestPath::parse("/workspace/src").unwrap()],
            exec_cwd: None,
        }
    }

    #[test]
    fn grant_verification_matrix() {
        let g = grant();
        let t1 = TenantId::new("t1").unwrap();
        let t2 = TenantId::new("t2").unwrap();
        let sb = SandboxId::new("sb1").unwrap();
        let exp =
            |t: TenantId, genv: u64, pg: u64, pd: &[u8], cap, rd: &[u8], now| GrantExpectation {
                tenant: t,
                sandbox: sb.clone(),
                generation: InstanceGeneration(genv),
                policy_generation: PolicyGeneration(pg),
                policy_digest: PolicyDigest::of_bytes(pd),
                capability: cap,
                request_digest: RequestDigest::of_bytes(rd),
                now_ms: now,
            };
        let ok = g.verify(&exp(
            t1.clone(),
            3,
            1,
            b"policy",
            SandboxCapability::SandboxRead,
            b"req",
            9_999,
        ));
        assert!(ok.is_ok(), "{ok:?}");
        // wrong tenant -> tenant_forbidden (no existence leak)
        let err = g
            .verify(&exp(
                t2.clone(),
                3,
                1,
                b"policy",
                SandboxCapability::SandboxRead,
                b"req",
                9_999,
            ))
            .unwrap_err();
        assert_eq!(err.code, crate::error::SandboxErrorCode::TenantForbidden);
        // stale generation
        let err = g
            .verify(&exp(
                t1.clone(),
                4,
                1,
                b"policy",
                SandboxCapability::SandboxRead,
                b"req",
                9_999,
            ))
            .unwrap_err();
        assert_eq!(err.code, crate::error::SandboxErrorCode::StaleGeneration);
        // revoked policy generation
        assert_eq!(
            g.verify(&exp(
                t1.clone(),
                3,
                2,
                b"policy",
                SandboxCapability::SandboxRead,
                b"req",
                9_999,
            ))
            .unwrap_err()
            .code,
            crate::error::SandboxErrorCode::PolicyDenied
        );
        // revoked policy digest
        assert!(
            g.verify(&exp(
                t1.clone(),
                3,
                1,
                b"other-policy",
                SandboxCapability::SandboxRead,
                b"req",
                9_999,
            ))
            .is_err()
        );
        // expired
        assert!(
            g.verify(&exp(
                t1.clone(),
                3,
                1,
                b"policy",
                SandboxCapability::SandboxRead,
                b"req",
                10_000,
            ))
            .is_err()
        );
        // wrong request digest
        assert!(
            g.verify(&exp(
                t1.clone(),
                3,
                1,
                b"policy",
                SandboxCapability::SandboxRead,
                b"other",
                9_999,
            ))
            .is_err()
        );
        // escalated capability
        assert!(
            g.verify(&exp(
                t1.clone(),
                3,
                1,
                b"policy",
                SandboxCapability::SandboxWrite,
                b"req",
                9_999,
            ))
            .is_err()
        );
    }

    #[test]
    fn path_permisions_are_prefix_scoped() {
        let g = grant();
        assert!(g.permits_path(&GuestPath::parse("/workspace/src").unwrap()));
        assert!(g.permits_path(&GuestPath::parse("/workspace/src/a.rs").unwrap()));
        assert!(!g.permits_path(&GuestPath::parse("/workspace/other").unwrap()));
        assert!(!g.permits_path(&GuestPath::parse("/tmp/leak").unwrap()));
    }

    #[test]
    fn environment_blocks_host_credentials() {
        let mut e = GuestEnvironment::new();
        for denied in [
            "HOME",
            "PATH",
            "SSH_AUTH_SOCK",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "HTTP_PROXY",
            "MY_SESSION_COOKIE",
            "GITLAB_PRIVATE_TOKEN",
            "NPM_CONFIG_REGISTRY",
            "PIP_INDEX_URL",
            "DOCKER_HOST",
        ] {
            assert!(e.insert(denied, "x").is_err(), "{denied} must be denied");
        }
        assert!(e.insert("MY_BUILD_MODE", "release").is_ok());
        assert!(e.insert("AUTHOR", "ok-not-a-secret").is_ok());
        assert!(e.insert("1BAD", "x").is_err());
        assert!(e.insert("", "x").is_err());
        assert!(e.insert("WITH-DASH", "x").is_err());
        let long = "a".repeat(MAX_ENV_VALUE_BYTES + 1);
        assert!(e.insert("X_LONG", &long).is_err());
    }

    #[test]
    fn env_names_reject_invalid_shapes() {
        let mut e = GuestEnvironment::new();
        assert!(e.insert("_UNDERSCORE_FIRST", "x").is_err());
        assert!(e.insert("NAME-WITH-DASH", "x").is_err());
        assert!(e.insert("ÜL", "x").is_err());
        assert!(e.insert(&"A".repeat(65), "x").is_err());
    }
}
