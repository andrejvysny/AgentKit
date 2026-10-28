//! Stable error surface for the binding.
//!
//! Every error that crosses into JavaScript is `{ code, message, details? }`
//! with a code from `error-codes.json` — the single contract shared with the
//! TS tests. Rust debug formatting never becomes observable behavior: internal
//! failures collapse to `internal_error` with a redacted message.

use napi::{Status, bindgen_prelude::Error};
use serde_json::Value;

/// The contract file itself, checked by the Rust tests so the two languages
/// can never drift.
pub const ERROR_CODES_JSON: &str = include_str!("../error-codes.json");

pub fn contract_codes() -> Vec<String> {
    serde_json::from_str::<Value>(ERROR_CODES_JSON)
        .expect("the error-code contract must be valid JSON")
        .get("codes")
        .and_then(Value::as_array)
        .expect("the contract must carry a `codes` array")
        .iter()
        .map(|code| code.as_str().expect("codes are strings").to_owned())
        .collect()
}

pub fn is_contract_code(code: &str) -> bool {
    contract_codes().iter().any(|known| known == code)
}

/// Build the stable error object. Codes outside the contract are redacted to
/// `internal_error`; the real code travels only in `details.origin`.
pub fn stable(code: &str, message: impl Into<String>, details: Option<Value>) -> Error {
    let message = message.into();
    let (wire_code, wire_details) = if is_contract_code(code) {
        (code.to_owned(), details)
    } else {
        let mut redacted = serde_json::Map::new();
        redacted.insert("origin".to_owned(), Value::String(code.to_owned()));
        let mut merged = match details {
            Some(Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        merged.extend(redacted);
        ("internal_error".to_owned(), Some(Value::Object(merged)))
    };
    let mut payload = serde_json::Map::new();
    payload.insert("code".to_owned(), Value::String(wire_code));
    payload.insert("message".to_owned(), Value::String(message));
    if let Some(details) = wire_details {
        payload.insert("details".to_owned(), details);
    }
    // The JS wrapper parses this JSON from `Error.message`; a raw throw
    // (wrapper bypassed) still carries the full structured payload.
    Error::new(Status::GenericFailure, Value::Object(payload).to_string())
}

pub fn internal(context: &str) -> Error {
    stable(
        "internal_error",
        format!("{context} failed; inspect the native logs"),
        None,
    )
}

/// Map a store error: every variant gets an explicit, contract-checked code.
/// New variants land in `internal_error` (with the origin in details) rather
/// than silently minting a public code.
pub fn store(error: agentkit_store::StoreError) -> Error {
    use agentkit_store::StoreError as E;
    let (code, message) = match &error {
        E::AlreadyOwned => ("already_owned", error.to_string()),
        E::ForeignDatabase => ("foreign_database", error.to_string()),
        E::RecoveryUnreadable(_) => ("recovery_unreadable", error.to_string()),
        E::Closed => ("store_closed", error.to_string()),
        E::UnsupportedVersion(_) => ("unsupported_version", error.to_string()),
        E::InvalidInput(_) => ("invalid_input", error.to_string()),
        E::NotFound(_) => ("not_found", error.to_string()),
        E::Duplicate(_) => ("input_conflict", error.to_string()),
        E::ScopeBusy(_) => ("chat_busy", error.to_string()),
        _ => ("unknown-store-error", error.to_string()),
    };
    stable(code, message, None)
}

pub fn session(error: agentkit_host::SessionError) -> Error {
    stable(&error.code, error.message, None)
}

pub fn host(error: agentkit_host::HostError) -> Error {
    stable("internal_error", error.to_string(), None)
}

pub fn invalid(message: impl Into<String>) -> Error {
    stable("invalid_input", message, None)
}

pub fn closed(context: &str) -> Error {
    stable("closed", context, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_is_valid_and_deduplicated() {
        let codes = contract_codes();
        assert!(!codes.is_empty());
        let unique: std::collections::HashSet<&String> = codes.iter().collect();
        assert_eq!(unique.len(), codes.len(), "codes must be deduplicated");
        assert!(codes.iter().any(|code| code == "internal_error"));
    }

    #[test]
    fn contract_codes_cross_the_boundary_verbatim() {
        let error = stable("answer_conflict", "duplicate answer", None);
        let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
        assert_eq!(payload["code"], "answer_conflict");
        assert_eq!(payload["message"], "duplicate answer");
        assert!(payload.get("details").is_none());
    }

    #[test]
    fn unknown_codes_are_redacted_not_leaked() {
        let error = stable("weird-debug-code", "raw internal detail", None);
        let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
        assert_eq!(payload["code"], "internal_error");
        // The raw message stays visible to the caller but the unknown code
        // only appears as an origin marker, never as the wire code.
        assert_eq!(payload["message"], "raw internal detail");
        assert_eq!(payload["details"]["origin"], "weird-debug-code");
    }

    #[test]
    fn store_variants_map_to_contract_codes() {
        let cases = [
            (agentkit_store::StoreError::AlreadyOwned, "already_owned"),
            (agentkit_store::StoreError::Closed, "store_closed"),
            (
                agentkit_store::StoreError::NotFound("x".into()),
                "not_found",
            ),
            (
                agentkit_store::StoreError::RecoveryUnreadable("x".into()),
                "recovery_unreadable",
            ),
            (
                agentkit_store::StoreError::UnsupportedVersion(9),
                "unsupported_version",
            ),
        ];
        for (error, expected) in cases {
            let payload: Value = serde_json::from_str(&store(error).reason).expect("JSON");
            assert_eq!(payload["code"], expected, "for {expected}");
        }
    }
}
