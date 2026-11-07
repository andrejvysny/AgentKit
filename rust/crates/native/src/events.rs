//! Normalized native-stream events, terminal outcomes and usage provenance.
//!
//! Separate event classes (AGENTKIT-107 §3.5): text and vendor tool
//! observations are data; terminal outcome, process exit and pipe EOF are
//! different signals. Only the governed MCP bridge executes application
//! tools — a [`NativeEvent::ToolObservation`] is never dispatchable.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Phase of a vendor tool observation (data only).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VendorToolPhase {
    /// The vendor reported it intends to call a tool (`tool_use` block).
    Requested,
    /// The vendor reported a tool result (`tool_result` block).
    Result,
}

/// One normalized event decoded from the vendor stream. Drivers translate
/// vendor shapes into this vocabulary; unknown vendor event shapes that
/// affect correctness fail the frame adapter, they never map to `Warning`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum NativeEvent {
    /// Session initialization evidence: identity the vendor reports for the
    /// run (observed model/effort/session), distinct from what was requested.
    Init {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_version: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },
    /// Incremental assistant text.
    TextDelta { text: String },
    /// A complete assistant message; the accumulator reconciles deltas with
    /// it so exactly one user-visible text sequence exists per turn.
    MessageCompleted { text: String },
    /// Incremental reasoning text, when the backend qualifies it.
    ReasoningDelta { text: String },
    /// A vendor-side tool call/result card. DATA ONLY: never dispatch.
    ToolObservation {
        vendor_tool_use_id: String,
        name: String,
        phase: VendorToolPhase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<Value>,
        /// Identity of the vendor conversation the observation belongs to;
        /// child/secondary streams keep a distinct identity.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
    },
    /// Usage provenance update (tokens/cost estimates); per-turn vs
    /// cumulative is kept explicit, unknown is not zero.
    Usage(NativeUsage),
    /// Non-fatal vendor warning.
    Warning { code: String, message: String },
    /// Vendor retry notice (e.g. rate limit backoff) for progress display.
    Retry {
        attempt: u32,
        max_retries: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_class: Option<FailureClass>,
    },
    /// The vendor denied a tool call at its permission layer.
    PermissionDenied { tool_name: String },
}

/// Token counts as reported by the vendor; absent means unknown, never zero.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenSet {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

/// How a usage figure was obtained.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UsageProvenance {
    /// Reported by the vendor runtime itself.
    VendorReported,
    /// Client-side estimate; must never be presented as an actual charge.
    Estimated,
    /// Unknown is the default: unknown is not zero.
    #[default]
    Unknown,
}

/// Billing inclusion evidence for the selected model. Subscription sign-in
/// alone is NOT proof of included usage (AGENTKIT-107 §4).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "inclusion", rename_all = "camelCase", deny_unknown_fields)]
pub enum BillingInclusion {
    Included {
        evidence: String,
    },
    NotIncluded {
        evidence: String,
    },
    #[default]
    Unknown,
}

/// Usage provenance for a turn or a session.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_turn: Option<TokenSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative: Option<TokenSet>,
    /// API-equivalent cost estimate; an estimate, never an actual charge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_equivalent_estimate_usd: Option<f64>,
    #[serde(default)]
    pub estimate_provenance: UsageProvenance,
    #[serde(default)]
    pub billing_inclusion: BillingInclusion,
}

/// Classified failure families. The set is closed so callers can map every
/// failure to actionable UX; classification never implies a fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    NotSignedIn,
    AuthExpired,
    QuotaExhausted,
    BillingBlocked,
    ModelNotEntitled,
    UnsupportedVersion,
    ManagedPolicyConflict,
    ExecutableMissing,
    ExecutableReplaced,
    SpawnFailed,
    ProtocolViolation,
    StartupTimeout,
    IdleTimeout,
    TurnDeadline,
    /// Process ended without ever producing a valid root result.
    ExitWithoutResult,
    /// Process disappeared (signal/crash) with no conclusive outcome.
    ProcessLost,
    /// Input was accepted but no conclusive outcome was reconciled.
    UncertainOutcome,
    CleanupUncertain,
    McpBridgeUnavailable,
    UnsupportedContent,
    UnsupportedCapability,
    DeferredUnsupported,
    Internal,
}

impl FailureClass {
    /// Whether retrying the same frozen target may succeed. Quota/auth
    /// failures stop predictably without provider fallback
    /// (AGENTKIT-107 §7); `retryable` never authorizes automatic retries.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            FailureClass::QuotaExhausted
                | FailureClass::StartupTimeout
                | FailureClass::IdleTimeout
                | FailureClass::ProcessLost
                | FailureClass::SpawnFailed
        )
    }
}

/// Evidence the driver collected when the vendor process ended. Exit code,
/// signal and result-visibility are separate facts (AGENTKIT-107 §3.5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessTerminalEvidence {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// A valid terminal vendor result was observed before exit.
    pub saw_root_result: bool,
    /// A graceful interrupt had been requested before termination.
    pub interrupt_requested: bool,
}

/// The single authoritative terminal outcome for one native turn. Produced
/// exactly once per turn by the driver's adapter from the vendor's root
/// result; process exit alone can never produce `Completed`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "camelCase", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)] // completed results carry full usage/denial payloads
pub enum NativeTerminalOutcome {
    /// Valid root result with final assistant text observed.
    Completed {
        text: String,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<NativeUsage>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        permission_denials: Vec<String>,
        /// Observed model/effort evidence accompanying the result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_effort: Option<String>,
    },
    /// Definitively failed before/at inference with an actionable class.
    Failed {
        class: FailureClass,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partial_text: Option<String>,
    },
    /// The vendor ended the turn via a deferred/child result path. Native
    /// deferral is excluded from this profile: it is typed unsupported —
    /// pending evidence retained, cleanup performed, no indefinite wait.
    DeferredBlocked {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partial_text: Option<String>,
    },
    /// Graceful interrupt acknowledged by the vendor (SIGINT path), which
    /// leaves the turn unfinished; distinct from user "stop and discard".
    Interrupted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partial_text: Option<String>,
    },
    /// Accepted input without a conclusive outcome; requires an explicit
    /// safe-recovery decision (AGENTKIT-107 §7).
    Uncertain {
        class: FailureClass,
        detail: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partial_text: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_observation_round_trips_and_is_data_only() {
        let event = NativeEvent::ToolObservation {
            vendor_tool_use_id: "toolu_1".into(),
            name: "Read".into(),
            phase: VendorToolPhase::Requested,
            input: Some(json!({"path": "x"})),
            parent_tool_use_id: None,
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "toolObservation");
        let back: NativeEvent = serde_json::from_value(value).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn unknown_event_shape_is_refused() {
        let err = serde_json::from_value::<NativeEvent>(json!({"type": "mystery"})).unwrap_err();
        assert!(err.to_string().contains("mystery"), "{err}");
    }

    #[test]
    fn usage_unknown_is_not_zero() {
        let usage = NativeUsage::default();
        assert_eq!(usage.per_turn, None);
        assert_eq!(usage.estimate_provenance, UsageProvenance::Unknown);
        assert_eq!(usage.billing_inclusion, BillingInclusion::Unknown);
    }

    #[test]
    fn failure_classes_are_closed_and_classify_retryability() {
        assert!(!FailureClass::AuthExpired.retryable());
        assert!(!FailureClass::NotSignedIn.retryable());
        assert!(!FailureClass::ExitWithoutResult.retryable());
        assert!(FailureClass::QuotaExhausted.retryable());
        assert_eq!(
            serde_json::to_value(FailureClass::QuotaExhausted).unwrap(),
            json!("quota_exhausted")
        );
    }

    #[test]
    fn exit_without_result_cannot_be_completed() {
        // A terminal outcome can only be Completed via an explicit root
        // result payload; ProcessTerminalEvidence alone cannot construct it.
        let evidence = ProcessTerminalEvidence {
            exit_code: Some(0),
            signal: None,
            saw_root_result: false,
            interrupt_requested: false,
        };
        assert!(!evidence.saw_root_result);
    }
}
