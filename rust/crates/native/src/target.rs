//! The frozen execution target: which execution owner runs an admitted turn.
//!
//! OneMind keeps its public task kind (`onemind.chat`) and dispatches on the
//! frozen target; the library never hardcodes another public task label
//! (AGENTKIT-107 §3.3). Existing HTTP/local-provider behavior is represented
//! by [`ModelLoopTarget`] and retains its current interpretation; native
//! selection is purely additive (AGENTKIT-108).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canonical_digest;
use crate::capability::CapabilitySnapshot;

/// Schema version stamped on every serialized [`ExecutionTarget`]. Bump on a
/// breaking wire change; readers must refuse unknown versions explicitly
/// rather than guess semantics.
pub const EXECUTION_TARGET_SCHEMA_VERSION: u32 = 1;
/// Schema version for content admission policy.
pub const CONTENT_POLICY_VERSION: u32 = 1;
/// Accepted JSON-Schema identity tag for chat input content (text-first).
pub const CONTENT_SCHEMA_VERSION: &str = "agentkit.chat.content.v1";

/// The execution owner selected for one admitted turn. Serialized form is
/// externally tagged (`{"native": …}` / `{"modelLoop": …}`) and the payload
/// always carries `schemaVersion`; unknown fields are refused so a future
/// field can never be silently dropped by an older reader.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)] // native targets carry the frozen capability snapshot
pub enum ExecutionTarget {
    /// The existing durable model loop (HTTP/local providers via
    /// `ChatProvider`/`ChatProviderFactory`). Semantics unchanged.
    ModelLoop(ModelLoopTarget),
    /// An optional native runtime driving the turn inside its own process.
    Native(NativeExecutionTarget),
}

impl ExecutionTarget {
    /// Convenience constructor preserving the legacy provider/model meaning.
    pub fn model_loop(provider_id: impl Into<String>, model: impl Into<String>) -> Self {
        ExecutionTarget::ModelLoop(ModelLoopTarget {
            schema_version: EXECUTION_TARGET_SCHEMA_VERSION,
            provider_id: provider_id.into(),
            model: model.into(),
            reasoning_effort: None,
        })
    }

    /// Stable identity of the frozen selection across admission, promotion,
    /// recovery and idempotency checks (AGENTKIT-112).
    pub fn digest(&self) -> String {
        canonical_digest(&serde_json::to_value(self).unwrap_or(Value::Null))
    }

    pub fn is_native(&self) -> bool {
        matches!(self, ExecutionTarget::Native(_))
    }
}

/// Frozen provider-loop selection. Mirrors the current turn-configuration
/// keys (model / reasoningEffort / providerId) — nothing more.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelLoopTarget {
    /// Must equal [`EXECUTION_TARGET_SCHEMA_VERSION`].
    pub schema_version: u32,
    pub provider_id: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// The two delivery profiles of this scope (AGENTKIT-107 §1). Bare mode is
/// deliberately unrepresentable: it cannot serve the subscription-auth path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NativeProfile {
    /// Plain conversation: no application tools, built-in tools disabled.
    PlainChat,
    /// Conversation plus an explicit catalog of AgentKit-owned MCP tools.
    OwnedMcpChat,
}

/// Frozen authentication expectation. Credentials stay CLI-owned; this is an
/// expectation checked at preflight, never credential material.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthExpectation {
    /// Official installed-CLI subscription sign-in.
    SubscriptionLogin,
    /// An explicit API-auth route observed from the official status output
    /// (surfaced honestly; never silently selected).
    ApiAuth,
    /// Whatever the official status reports; unknown stays unknown.
    AsConfigured,
}

/// Immutable native admission snapshot: backend/profile/executable/model/
/// effort/auth expectation, content references and configuration identity,
/// frozen per admitted turn (AGENTKIT-107 §3.4). Settings changes affect
/// future turns only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeExecutionTarget {
    /// Must equal [`EXECUTION_TARGET_SCHEMA_VERSION`]; refused otherwise.
    pub schema_version: u32,
    /// Stable backend identity, e.g. `"claude-code"`.
    pub backend: String,
    /// Backend version this target was admitted against (version-qualified
    /// capability evidence), or `None` when unobserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_version: Option<String>,
    pub profile: NativeProfile,
    /// Executable identity as resolved by the host-supplied locator: an
    /// explicit path or the default discovery name, plus the observed
    /// version when probed. Probed and executed through the same official
    /// configuration namespace.
    pub executable: String,
    /// Model requested by the user; the observed effective model may differ
    /// and is reported separately in normalized events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>,
    /// Effort requested by the user; observed effective effort may differ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_effort: Option<String>,
    pub auth_expectation: AuthExpectation,
    /// Version-qualified capability matrix captured at admission.
    pub capabilities: CapabilitySnapshot,
    /// Identity of the frozen launch configuration (argv template, env keys,
    /// private-run-dir layout, MCP endpoint layout) — never its secrets.
    pub config_identity: String,
    /// Revision of the stable application system prompt in force.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_revision: Option<String>,
    /// Content admission policy for this target.
    pub content: ContentPolicy,
}

impl NativeExecutionTarget {
    pub fn digest(&self) -> String {
        canonical_digest(&serde_json::to_value(self).unwrap_or(Value::Null))
    }
}

/// Content modalities accepted by the selected execution target. Text and
/// application-validated extracted text are mandatory for this delivery;
/// additional modalities exist only behind qualified capability flags and
/// are refused before admission — never silently dropped (AGENTKIT-107 §1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContentPolicy {
    /// Must equal [`CONTENT_POLICY_VERSION`].
    pub policy_version: u32,
    /// Identity tag of the accepted content schema.
    pub schema_version: String,
    pub text: bool,
    pub extracted_text: bool,
    /// Enabled only with qualified CLI-encoding evidence; otherwise false
    /// and image inputs are refused before admission with the draft kept.
    pub images: bool,
    /// Structured attachments beyond extracted text (files/audio/…) are out
    /// of scope for this delivery; the flag exists so the absence is
    /// explicit rather than implied.
    pub files: bool,
}

impl Default for ContentPolicy {
    fn default() -> Self {
        ContentPolicy {
            policy_version: CONTENT_POLICY_VERSION,
            schema_version: CONTENT_SCHEMA_VERSION.to_owned(),
            text: true,
            extracted_text: true,
            images: false,
            files: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{CapabilitySnapshot, CapabilityState, validate_execution_target};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn qualified_snapshot() -> CapabilitySnapshot {
        let supported = || CapabilityState::Supported {
            qualified_version: "2.1.310".into(),
            evidence: "test".into(),
        };
        let mut flags = BTreeMap::new();
        flags.insert("plain-chat".to_owned(), supported());
        flags.insert("owned-mcp-chat".to_owned(), supported());
        flags.insert("text-input".to_owned(), supported());
        flags.insert("extracted-text-input".to_owned(), supported());
        CapabilitySnapshot {
            backend: "claude-code".into(),
            matrix_version: 1,
            qualified_version: "2.1.310".into(),
            flags,
        }
    }

    fn native_target(profile: NativeProfile) -> NativeExecutionTarget {
        NativeExecutionTarget {
            schema_version: EXECUTION_TARGET_SCHEMA_VERSION,
            backend: "claude-code".into(),
            backend_version: Some("2.1.310".into()),
            profile,
            executable: "/usr/local/bin/claude".into(),
            requested_model: Some("sonnet".into()),
            requested_effort: Some("medium".into()),
            auth_expectation: AuthExpectation::SubscriptionLogin,
            capabilities: qualified_snapshot(),
            config_identity: "cfg-test".into(),
            system_prompt_revision: Some("onemind-general-v1".into()),
            content: ContentPolicy::default(),
        }
    }

    #[test]
    fn target_round_trips_with_schema_version_and_camel_tag() {
        let target = ExecutionTarget::Native(native_target(NativeProfile::PlainChat));
        let value = serde_json::to_value(&target).unwrap();
        assert!(value.get("native").is_some(), "{value}");
        assert_eq!(
            value["native"]["schemaVersion"],
            EXECUTION_TARGET_SCHEMA_VERSION
        );
        let back: ExecutionTarget = serde_json::from_value(value).unwrap();
        assert_eq!(back, target);
    }

    #[test]
    fn unknown_fields_are_refused_not_dropped() {
        let mut value = serde_json::to_value(ExecutionTarget::Native(native_target(
            NativeProfile::PlainChat,
        )))
        .unwrap();
        value["native"]
            .as_object_mut()
            .unwrap()
            .insert("futureField".into(), json!(1));
        let err = serde_json::from_value::<ExecutionTarget>(value).unwrap_err();
        assert!(err.to_string().contains("futureField"), "{err}");
    }

    #[test]
    fn unknown_schema_version_is_refused_by_validation() {
        let mut target = native_target(NativeProfile::PlainChat);
        target.schema_version = 99;
        let violations =
            validate_execution_target(&ExecutionTarget::Native(target.clone())).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.code == "unsupported_schema_version"),
            "{violations:?}"
        );
    }

    #[test]
    fn no_credential_bearing_field_names_exist_in_serialized_targets() {
        let value = serde_json::to_value(ExecutionTarget::Native(native_target(
            NativeProfile::PlainChat,
        )))
        .unwrap();
        let text = value.to_string().to_lowercase();
        for needle in [
            "token", "apikey", "api_key", "secret", "keychain", "oauth", "cookie",
        ] {
            assert!(
                !text.contains(needle),
                "serialized target leaked {needle}: {text}"
            );
        }
        assert!(native_target(NativeProfile::PlainChat).digest().len() == 64);
    }

    #[test]
    fn model_loop_target_keeps_legacy_provider_model_semantics() {
        let target = ExecutionTarget::model_loop("openrouter", "claude-sonnet-4.5");
        let value = serde_json::to_value(&target).unwrap();
        assert!(value.get("modelLoop").is_some(), "{value}");
        assert_eq!(value["modelLoop"]["providerId"], "openrouter");
        assert_eq!(value["modelLoop"]["model"], "claude-sonnet-4.5");
        assert_eq!(
            value["modelLoop"]["schemaVersion"],
            EXECUTION_TARGET_SCHEMA_VERSION
        );
        let back: ExecutionTarget = serde_json::from_value(value).unwrap();
        assert_eq!(back, target);
    }
}
