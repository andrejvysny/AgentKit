//! Version-qualified capability policy (AGENTKIT-107 §3.6, AGENTKIT-108).
//!
//! Unknown flags, event shapes, input types and model/effort semantics are
//! never advertised as supported. Support requires a qualified backend
//! version and evidence; anything else is [`CapabilityState::Unsupported`]
//! or [`CapabilityState::Unknown`] and admission refuses or degrades
//! honestly.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::target::{ExecutionTarget, NativeExecutionTarget, NativeProfile};

/// Stable, well-known capability flag names for this delivery. Drivers may
/// add names; readers must ignore names they do not know.
pub fn well_known_flags() -> &'static [&'static str] {
    &[
        "plain-chat",
        "owned-mcp-chat",
        "text-input",
        "extracted-text-input",
        "image-input",
        "file-input",
        "exact-resume",
        "vendor-fork",
        "seeded-new-session",
        "tool-observations",
        "live-interactions",
        "partial-deltas",
        "usage-tokens",
        "usage-cost-estimate",
        "interrupt-graceful",
        "permission-prompts-none",
        "safe-mode",
        "strict-mcp-config",
    ]
}

/// Support state of one capability, version-qualified.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase", deny_unknown_fields)]
pub enum CapabilityState {
    /// Qualified on a concrete backend version with recorded evidence.
    Supported {
        qualified_version: String,
        /// Short human-readable evidence reference (documentation section,
        /// qualification run id). Never a secret.
        evidence: String,
    },
    /// Explicitly not supported for this delivery (out of scope or proven
    /// unsuitable), e.g. native subagents, bare mode, image input.
    Unsupported { reason: String },
    /// Not observed on this version. Behaves like `Unsupported` at admission
    /// but is reported separately so UI can say "unknown" instead of "no".
    Unknown,
}

impl CapabilityState {
    pub fn is_supported(&self) -> bool {
        matches!(self, CapabilityState::Supported { .. })
    }
}

/// A backend's full capability matrix at one matrix revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityMatrix {
    pub backend: String,
    /// Matrix definition revision owned by the driver.
    pub matrix_version: u32,
    /// Map of qualified backend version ranges → flags. Key is the driver's
    /// version range text (e.g. `"2.1.259+"`) so qualification is explicit.
    pub versions: BTreeMap<String, BTreeMap<String, CapabilityState>>,
}

/// The frozen view of a matrix for one concrete backend version, embedded in
/// a [`NativeExecutionTarget`]. Absent flags mean [`CapabilityState::Unknown`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilitySnapshot {
    pub backend: String,
    pub matrix_version: u32,
    /// Backend version this snapshot was qualified against.
    pub qualified_version: String,
    pub flags: BTreeMap<String, CapabilityState>,
}

impl CapabilitySnapshot {
    pub fn state(&self, flag: &str) -> CapabilityState {
        self.flags
            .get(flag)
            .cloned()
            .unwrap_or(CapabilityState::Unknown)
    }

    pub fn supports(&self, flag: &str) -> bool {
        self.state(flag).is_supported()
    }
}

/// A single admission-time capability violation. `code` is stable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityViolation {
    pub code: &'static str,
    pub detail: String,
}

/// Validate an execution target against its embedded capability snapshot
/// BEFORE any process spawn (AGENTKIT-108 acceptance). Returns every
/// violation at once so callers can present actionable errors. Legacy
/// [`ModelLoopTarget`]s always pass — their behavior is unchanged.
pub fn validate_execution_target(target: &ExecutionTarget) -> Result<(), Vec<CapabilityViolation>> {
    let ExecutionTarget::Native(native) = target else {
        return Ok(());
    };
    validate_native_target(native)
}

fn validate_native_target(native: &NativeExecutionTarget) -> Result<(), Vec<CapabilityViolation>> {
    let mut violations = Vec::new();
    let snap = &native.capabilities;

    if native.schema_version != crate::target::EXECUTION_TARGET_SCHEMA_VERSION {
        violations.push(CapabilityViolation {
            code: "unsupported_schema_version",
            detail: format!(
                "execution target schema {} is not understood",
                native.schema_version
            ),
        });
    }
    if native.backend.trim().is_empty() {
        violations.push(CapabilityViolation {
            code: "backend_missing",
            detail: "native backend identity is empty".into(),
        });
    }
    if native.executable.trim().is_empty() {
        violations.push(CapabilityViolation {
            code: "executable_missing",
            detail: "no executable reference frozen on the target".into(),
        });
    }
    if native.config_identity.trim().is_empty() {
        violations.push(CapabilityViolation {
            code: "config_identity_missing",
            detail: "frozen configuration identity is empty".into(),
        });
    }
    if snap.backend != native.backend {
        violations.push(CapabilityViolation {
            code: "capability_backend_mismatch",
            detail: format!(
                "capability snapshot is for {} but target selects {}",
                snap.backend, native.backend
            ),
        });
    }

    // Profile requirements.
    let profile_flag = match native.profile {
        NativeProfile::PlainChat => "plain-chat",
        NativeProfile::OwnedMcpChat => "owned-mcp-chat",
    };
    require(native, profile_flag, &mut violations);
    require(native, "text-input", &mut violations);
    require(native, "extracted-text-input", &mut violations);

    // Content modalities must be backed by capability evidence both ways:
    // policy enabling a modality requires support, and unsupported
    // modalities must not be enabled by the policy.
    if native.content.images && !snap.supports("image-input") {
        violations.push(CapabilityViolation {
            code: "content_modality_unsupported",
            detail: "image input requested without qualified capability".into(),
        });
    }
    if native.content.files && !snap.supports("file-input") {
        violations.push(CapabilityViolation {
            code: "content_modality_unsupported",
            detail: "file input requested without qualified capability".into(),
        });
    }
    if !native.content.text {
        violations.push(CapabilityViolation {
            code: "content_policy_invalid",
            detail: "text input is mandatory for this delivery".into(),
        });
    }
    if native.content.policy_version != crate::target::CONTENT_POLICY_VERSION {
        violations.push(CapabilityViolation {
            code: "unsupported_schema_version",
            detail: format!("content policy version {}", native.content.policy_version),
        });
    }

    // Out-of-scope features must never appear as supported on this
    // surface (AGENTKIT-107 §1): native sub-agents/coding tools, Codex,
    // the native permission-prompt tool, and deferred coding tools are
    // refused even if a future version were to advertise them.
    for forbidden in [
        "native-subagents",
        "codex",
        "native-coding-tools",
        "live-permission-prompt",
        "deferred-tools",
    ] {
        if snap.supports(forbidden) {
            violations.push(CapabilityViolation {
                code: "out_of_scope_capability",
                detail: format!(
                    "capability {forbidden} is outside this delivery and cannot be advertised"
                ),
            });
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

fn require(native: &NativeExecutionTarget, flag: &str, violations: &mut Vec<CapabilityViolation>) {
    match native.capabilities.state(flag) {
        CapabilityState::Supported { .. } => {}
        CapabilityState::Unsupported { reason } => violations.push(CapabilityViolation {
            code: "capability_unsupported",
            detail: format!("{flag} unsupported: {reason}"),
        }),
        CapabilityState::Unknown => violations.push(CapabilityViolation {
            code: "capability_unknown",
            detail: format!(
                "{flag} is not qualified for backend version {:?}",
                native.backend_version
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{AuthExpectation, ContentPolicy, EXECUTION_TARGET_SCHEMA_VERSION};

    fn supported() -> CapabilityState {
        CapabilityState::Supported {
            qualified_version: "2.1.310".into(),
            evidence: "docs/qualification".into(),
        }
    }

    fn unsupported() -> CapabilityState {
        CapabilityState::Unsupported {
            reason: "out of scope".into(),
        }
    }

    fn matrix(image: CapabilityState, mcp: CapabilityState) -> CapabilitySnapshot {
        let mut flags = BTreeMap::new();
        flags.insert("plain-chat".into(), supported());
        flags.insert("owned-mcp-chat".into(), mcp);
        flags.insert("text-input".into(), supported());
        flags.insert("extracted-text-input".into(), supported());
        flags.insert("image-input".into(), image);
        CapabilitySnapshot {
            backend: "claude-code".into(),
            matrix_version: 1,
            qualified_version: "2.1.310".into(),
            flags,
        }
    }

    fn target(profile: NativeProfile, snap: CapabilitySnapshot) -> NativeExecutionTarget {
        NativeExecutionTarget {
            schema_version: EXECUTION_TARGET_SCHEMA_VERSION,
            backend: "claude-code".into(),
            backend_version: Some("2.1.310".into()),
            profile,
            executable: "/bin/claude".into(),
            requested_model: None,
            requested_effort: None,
            auth_expectation: AuthExpectation::SubscriptionLogin,
            capabilities: snap,
            config_identity: "cfg".into(),
            system_prompt_revision: None,
            content: ContentPolicy::default(),
        }
    }

    #[test]
    fn valid_plain_chat_passes() {
        let t = target(
            NativeProfile::PlainChat,
            matrix(unsupported(), unsupported()),
        );
        assert_eq!(
            validate_execution_target(&ExecutionTarget::Native(t)),
            Ok(())
        );
    }

    #[test]
    fn owned_mcp_chat_requires_bridge_capability() {
        let t = target(
            NativeProfile::OwnedMcpChat,
            matrix(unsupported(), CapabilityState::Unknown),
        );
        let err = validate_execution_target(&ExecutionTarget::Native(t)).unwrap_err();
        assert!(
            err.iter()
                .any(|v| v.code == "capability_unknown" && v.detail.contains("owned-mcp-chat"))
        );
    }

    #[test]
    fn image_policy_without_capability_is_rejected_before_spawn() {
        let snap = matrix(unsupported(), unsupported());
        let mut t = target(NativeProfile::PlainChat, snap);
        t.content.images = true;
        let err = validate_execution_target(&ExecutionTarget::Native(t)).unwrap_err();
        assert!(err.iter().any(|v| v.code == "content_modality_unsupported"));
    }

    #[test]
    fn out_of_scope_flags_cannot_be_advertised() {
        let mut snap = matrix(unsupported(), unsupported());
        snap.flags.insert("native-subagents".into(), supported());
        let t = target(NativeProfile::PlainChat, snap);
        let err = validate_execution_target(&ExecutionTarget::Native(t)).unwrap_err();
        assert!(err.iter().any(|v| v.code == "out_of_scope_capability"));
    }

    #[test]
    fn model_loop_targets_always_pass() {
        assert_eq!(
            validate_execution_target(&ExecutionTarget::model_loop("p", "m")),
            Ok(())
        );
    }
}
