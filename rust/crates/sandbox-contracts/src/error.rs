//! Stable machine-readable error contract (§6.5).
//!
//! Security-critical distinctions must never collapse into a generic
//! `command_failed`: the retry class tells the caller whether an action is
//! safe after a fresh authorizing decision, replayable by `execution_id`,
//! uncertain until reconciled, or requires quarantine. Guest-claimed state is
//! never an input to these classifications — only host-observed evidence is
//! (S14, S17).

use crate::ids::SandboxId;
use serde::{Deserialize, Serialize};

/// Stable error codes. Serialized form is the exact snake-case string; these
/// strings are part of the public contract and must not be renamed casually.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SandboxErrorCode {
    #[serde(rename = "sandbox_not_found")]
    NotFound,
    #[serde(rename = "tenant_forbidden")]
    TenantForbidden,
    #[serde(rename = "sandbox_policy_denied")]
    PolicyDenied,
    #[serde(rename = "sandbox_capability_unsupported")]
    CapabilityUnsupported,
    #[serde(rename = "sandbox_isolation_unavailable")]
    IsolationUnavailable,
    #[serde(rename = "sandbox_unavailable")]
    Unavailable,
    #[serde(rename = "sandbox_stale_generation")]
    StaleGeneration,
    #[serde(rename = "sandbox_quarantined")]
    Quarantined,
    #[serde(rename = "sandbox_resource_exhausted")]
    ResourceExhausted,
    #[serde(rename = "sandbox_image_invalid")]
    ImageInvalid,
    #[serde(rename = "sandbox_image_unsupported")]
    ImageUnsupported,
    #[serde(rename = "sandbox_package_denied")]
    PackageDenied,
    #[serde(rename = "sandbox_artifact_invalid")]
    ArtifactInvalid,
    #[serde(rename = "sandbox_execution_not_started")]
    ExecutionNotStarted,
    #[serde(rename = "sandbox_execution_uncertain")]
    ExecutionUncertain,
    #[serde(rename = "sandbox_termination_unverified")]
    TerminationUnverified,
    #[serde(rename = "sandbox_protocol_incompatible")]
    ProtocolIncompatible,
}

impl SandboxErrorCode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "sandbox_not_found",
            Self::TenantForbidden => "tenant_forbidden",
            Self::PolicyDenied => "sandbox_policy_denied",
            Self::CapabilityUnsupported => "sandbox_capability_unsupported",
            Self::IsolationUnavailable => "sandbox_isolation_unavailable",
            Self::Unavailable => "sandbox_unavailable",
            Self::StaleGeneration => "sandbox_stale_generation",
            Self::Quarantined => "sandbox_quarantined",
            Self::ResourceExhausted => "sandbox_resource_exhausted",
            Self::ImageInvalid => "sandbox_image_invalid",
            Self::ImageUnsupported => "sandbox_image_unsupported",
            Self::PackageDenied => "sandbox_package_denied",
            Self::ArtifactInvalid => "sandbox_artifact_invalid",
            Self::ExecutionNotStarted => "sandbox_execution_not_started",
            Self::ExecutionUncertain => "sandbox_execution_uncertain",
            Self::TerminationUnverified => "sandbox_termination_unverified",
            Self::ProtocolIncompatible => "sandbox_protocol_incompatible",
        }
    }

    /// Default retry classification for this code (§6.5).
    #[must_use]
    pub fn retry_class(self) -> RetryClass {
        match self {
            // Denied before dispatch: safe only after a fresh authorizing decision.
            Self::NotFound
            | Self::TenantForbidden
            | Self::PolicyDenied
            | Self::CapabilityUnsupported
            | Self::IsolationUnavailable
            | Self::ResourceExhausted
            | Self::ImageInvalid
            | Self::ImageUnsupported
            | Self::PackageDenied
            | Self::ArtifactInvalid
            | Self::ProtocolIncompatible => RetryClass::SafeAfterFreshDecision,
            // Proven not-started may be retried under the same identity.
            Self::ExecutionNotStarted => RetryClass::ReplayableByIdentity,
            // Unknown external effects: reconcile, never self-retry.
            Self::Unavailable | Self::ExecutionUncertain | Self::TerminationUnverified => {
                RetryClass::UncertainReconcile
            }
            // Quarantine and stale-generation require authoritative resolution.
            Self::Quarantined | Self::StaleGeneration => RetryClass::RefusedUntilReconciled,
        }
    }
}

/// What a caller may do after an error of this class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryClass {
    /// Rejected before dispatch; retry only after a fresh authorizing decision.
    SafeAfterFreshDecision,
    /// Proven never started; a duplicate with identical identity+digest may
    /// be dispatched again.
    ReplayableByIdentity,
    /// Dispatched but completion unknown: reconcile first; never automatic
    /// replay (S07, S14).
    UncertainReconcile,
    /// Resource is quarantined/stale: refuses until reconciliation or
    /// renewed authority.
    RefusedUntilReconciled,
}

/// A structured sandbox error. `message` is log-safe: constructed only from
/// ids, digests and static text — never from untrusted guest content, file
/// contents, or environment values (S18).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxError {
    pub code: SandboxErrorCode,
    pub message: String,
    /// Sandbox the error refers to, when one was resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_id: Option<SandboxId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_generation: Option<u64>,
    /// Stable machine detail (e.g. which check failed), safe to log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub retry: RetryClass,
}

impl SandboxError {
    pub fn new(code: SandboxErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            sandbox_id: None,
            instance_generation: None,
            detail: None,
            retry: code.retry_class(),
        }
    }
    #[must_use]
    pub fn with_sandbox(mut self, id: &SandboxId, generation: Option<u64>) -> Self {
        self.sandbox_id = Some(id.clone());
        self.instance_generation = generation;
        self
    }
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// True when the failure leaves an external effect of unknown status;
    /// callers must route through reconciliation instead of retry (S14).
    #[must_use]
    pub fn is_uncertain(&self) -> bool {
        self.retry == RetryClass::UncertainReconcile
    }
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)?;
        if let Some(id) = &self.sandbox_id {
            write!(f, " (sandbox {id}")?;
            if let Some(g) = self.instance_generation {
                write!(f, " gen {g}")?;
            }
            write!(f, ")")?;
        }
        Ok(())
    }
}

impl std::error::Error for SandboxError {}

pub type SandboxResult<T> = Result<T, SandboxError>;

/// Convenience constructors used across worker/host/guest.
pub mod prelude {
    use super::*;
    pub fn not_found(msg: impl Into<String>) -> SandboxError {
        SandboxError::new(SandboxErrorCode::NotFound, msg)
    }
    pub fn tenant_forbidden() -> SandboxError {
        SandboxError::new(
            SandboxErrorCode::TenantForbidden,
            "principal is not the owner of this sandbox",
        )
    }
    pub fn stale_generation() -> SandboxError {
        SandboxError::new(
            SandboxErrorCode::StaleGeneration,
            "request references an old instance generation",
        )
    }
    pub fn quarantined(reason: impl Into<String>) -> SandboxError {
        SandboxError::new(SandboxErrorCode::Quarantined, reason)
    }
    pub fn protocol(msg: impl Into<String>) -> SandboxError {
        SandboxError::new(SandboxErrorCode::ProtocolIncompatible, msg)
    }
    pub fn uncertain(msg: impl Into<String>) -> SandboxError {
        SandboxError::new(SandboxErrorCode::ExecutionUncertain, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_roundtrip_exact_strings() {
        for (code, text) in [
            (SandboxErrorCode::NotFound, "\"sandbox_not_found\""),
            (SandboxErrorCode::TenantForbidden, "\"tenant_forbidden\""),
            (
                SandboxErrorCode::CapabilityUnsupported,
                "\"sandbox_capability_unsupported\"",
            ),
            (
                SandboxErrorCode::IsolationUnavailable,
                "\"sandbox_isolation_unavailable\"",
            ),
            (SandboxErrorCode::Unavailable, "\"sandbox_unavailable\""),
            (
                SandboxErrorCode::StaleGeneration,
                "\"sandbox_stale_generation\"",
            ),
            (SandboxErrorCode::Quarantined, "\"sandbox_quarantined\""),
            (
                SandboxErrorCode::ResourceExhausted,
                "\"sandbox_resource_exhausted\"",
            ),
            (SandboxErrorCode::ImageInvalid, "\"sandbox_image_invalid\""),
            (
                SandboxErrorCode::ImageUnsupported,
                "\"sandbox_image_unsupported\"",
            ),
            (
                SandboxErrorCode::PackageDenied,
                "\"sandbox_package_denied\"",
            ),
            (
                SandboxErrorCode::ArtifactInvalid,
                "\"sandbox_artifact_invalid\"",
            ),
            (
                SandboxErrorCode::ExecutionNotStarted,
                "\"sandbox_execution_not_started\"",
            ),
            (
                SandboxErrorCode::ExecutionUncertain,
                "\"sandbox_execution_uncertain\"",
            ),
            (
                SandboxErrorCode::TerminationUnverified,
                "\"sandbox_termination_unverified\"",
            ),
            (
                SandboxErrorCode::ProtocolIncompatible,
                "\"sandbox_protocol_incompatible\"",
            ),
            (SandboxErrorCode::PolicyDenied, "\"sandbox_policy_denied\""),
        ] {
            assert_eq!(serde_json::to_string(&code).unwrap(), text);
            assert_eq!(
                serde_json::from_str::<SandboxErrorCode>(text).unwrap(),
                code
            );
            assert_eq!(code.as_str(), text.trim_matches('"'));
        }
        assert!(serde_json::from_str::<SandboxErrorCode>("\"command_failed\"").is_err());
    }

    #[test]
    fn uncertain_classes_flagged() {
        assert!(SandboxError::new(SandboxErrorCode::ExecutionUncertain, "x").is_uncertain());
        assert!(SandboxError::new(SandboxErrorCode::TerminationUnverified, "x").is_uncertain());
        assert!(!SandboxError::new(SandboxErrorCode::PolicyDenied, "x").is_uncertain());
    }
}
