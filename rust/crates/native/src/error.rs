//! Shared error vocabulary for native backend contracts.

use serde::{Deserialize, Serialize};

/// A pre-spawn refusal: the backend refuses to run a target and no vendor
/// process was ever started. Actionable and never a silent fallback.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeRefusal {
    /// Stable machine code (e.g. `"managed_policy_conflict"`).
    pub code: String,
    pub message: String,
    /// Whether retrying the same frozen target could plausibly succeed.
    /// Never authorizes automatic retry loops.
    pub retryable: bool,
}

impl NativeRefusal {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        NativeRefusal {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }
}

impl std::fmt::Display for NativeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for NativeRefusal {}

/// Errors surfaced by injected ports during a native turn.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum NativeError {
    /// Deterministic refusal before any process spawn.
    #[error("refused: {refusal}")]
    Refusal { refusal: NativeRefusal },
    /// Transport-level fault while an owned child was live.
    #[error("transport fault: {code}: {message}")]
    Transport { code: String, message: String },
    /// The vendor protocol produced a shape the qualified adapter cannot
    /// interpret without risking correctness.
    #[error("protocol violation: {code}: {message}")]
    Protocol { code: String, message: String },
    /// The attempt's authority/lease was replaced; results may not commit.
    #[error("stale attempt: {attempt_id}")]
    StaleAttempt { attempt_id: String },
    /// Host or user cancellation.
    #[error("cancelled")]
    Cancelled,
}

pub type NativeResult<T> = Result<T, NativeError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_round_trips_and_displays() {
        let refusal =
            NativeRefusal::new("managed_policy_conflict", "policy forbids profile", false);
        let value = serde_json::to_value(&refusal).unwrap();
        assert_eq!(value["code"], "managed_policy_conflict");
        assert_eq!(
            serde_json::from_value::<NativeRefusal>(value).unwrap(),
            refusal
        );
        assert!(refusal.to_string().contains("policy forbids profile"));
    }

    #[test]
    fn error_round_trips_without_secrets() {
        let error = NativeError::Transport {
            code: "stdin_blocked".into(),
            message: "stdin write stalled past limit".into(),
        };
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(value["kind"], "transport");
        let back: NativeError = serde_json::from_value(value).unwrap();
        assert_eq!(back, error);
    }
}
