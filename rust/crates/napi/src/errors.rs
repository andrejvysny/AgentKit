//! Stable error surface for the binding.
//!
//! Every error that crosses into JavaScript is `{ code, message, details? }`
//! with a code from `error-codes.json` — the single contract shared with the
//! TS tests. Rust debug formatting never becomes observable behavior: internal
//! failures collapse to `internal_error` with a redacted message.

use agentkit_agents::AgentsError;
use agentkit_runtime::RuntimeError;
use napi::{Status, bindgen_prelude::Error};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::LazyLock;

/// The contract file itself, checked by the Rust tests so the two languages
/// can never drift.
pub const ERROR_CODES_JSON: &str = include_str!("../error-codes.json");

/// The one message every redacted error crosses the boundary with. Raw
/// runtime text never rides in `message` for an `internal_error` payload.
pub const GENERIC_MESSAGE: &str = "an internal error occurred; inspect the native logs";

/// The parsed contract: file order for reporting, a set for membership
/// checks. [`CONTRACT`] parses `ERROR_CODES_JSON` exactly once per process;
/// the file is `include_str!`-embedded and validated by the
/// `contract_is_valid_and_deduplicated` test.
struct Contract {
    ordered: Vec<String>,
    set: HashSet<String>,
}

static CONTRACT: LazyLock<Contract> = LazyLock::new(|| {
    let ordered: Vec<String> = serde_json::from_str::<Value>(ERROR_CODES_JSON)
        .expect("the error-code contract must be valid JSON")
        .get("codes")
        .and_then(Value::as_array)
        .expect("the contract must carry a `codes` array")
        .iter()
        .map(|code| code.as_str().expect("codes are strings").to_owned())
        .collect();
    Contract {
        set: ordered.iter().cloned().collect(),
        ordered,
    }
});

pub fn contract_codes() -> Vec<String> {
    CONTRACT.ordered.clone()
}

pub fn is_contract_code(code: &str) -> bool {
    CONTRACT.set.contains(code)
}

/// Build the stable error object. Codes outside the contract — and the
/// contract's own `internal_error` — are redacted: the wire code is
/// `internal_error` and the message is the fixed [`GENERIC_MESSAGE`], never
/// the caller's text; for an unknown code the real code travels only in
/// `details.origin`. Every other contract code crosses verbatim.
pub fn stable(code: &str, message: impl Into<String>, details: Option<Value>) -> Error {
    if is_contract_code(code) {
        let wire_message = if code == "internal_error" {
            // A contract code, but its message is caller-supplied runtime
            // text (host failures, store fallbacks): it never crosses.
            GENERIC_MESSAGE.to_owned()
        } else {
            message.into()
        };
        return payload(code.to_owned(), wire_message, details);
    }
    let mut merged = match details {
        Some(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    merged.insert("origin".to_owned(), Value::String(code.to_owned()));
    payload(
        "internal_error".to_owned(),
        GENERIC_MESSAGE.to_owned(),
        Some(Value::Object(merged)),
    )
}

/// Assemble the `{code, message, details?}` payload. Private: every error
/// reaches JavaScript through [`stable`] (redacting) or [`internal`]
/// (compile-time context only), never through a raw hand-built map.
fn payload(code: String, message: String, details: Option<Value>) -> Error {
    let mut payload = serde_json::Map::new();
    payload.insert("code".to_owned(), Value::String(code));
    payload.insert("message".to_owned(), Value::String(message));
    if let Some(details) = details {
        payload.insert("details".to_owned(), details);
    }
    // The JS wrapper parses this JSON from `Error.message`; a raw throw
    // (wrapper bypassed) still carries the full structured payload.
    Error::new(Status::GenericFailure, Value::Object(payload).to_string())
}

/// A pure internal failure. `context` is a compile-time call-site literal —
/// audited: no caller interpolates runtime data — so this message may keep
/// its context and bypass the blanket redaction inside [`stable`].
pub fn internal(context: &str) -> Error {
    payload(
        "internal_error".to_owned(),
        format!("{context} failed; inspect the native logs"),
        None,
    )
}

/// Map a store error: every variant gets an explicit, contract-checked code.
/// New variants land in `internal_error` (with the origin in details) rather
/// than silently minting a public code.
pub fn store(error: agentkit_store::StoreError) -> Error {
    store_mapped(error, None, None)
}

/// Port-surface mapping: the `AssistantStore` ports have their own code for
/// transition legality and for a duplicate key, and those differ per
/// namespace (`invalid_task_transition` for tasks, `input_transition_invalid`
/// for agent inputs, `duplicate_task` for task ids, …). Each port arm passes
/// the two codes its namespace promises; every other variant maps exactly as
/// [`store`] does.
pub fn store_port(
    error: agentkit_store::StoreError,
    invalid_transition: &str,
    duplicate: &str,
) -> Error {
    store_mapped(error, Some(invalid_transition), Some(duplicate))
}

fn store_mapped(
    error: agentkit_store::StoreError,
    invalid_transition: Option<&str>,
    duplicate: Option<&str>,
) -> Error {
    use agentkit_store::StoreError as E;
    if let E::InvalidImport { reason } = &error {
        // The port contract asks the refusal to say WHICH import rule broke;
        // the reason rides in `details` so an importer can act on it.
        return stable(
            "invalid_import",
            error.to_string(),
            Some(json!({ "reason": reason })),
        );
    }
    let (code, message) = match &error {
        E::AlreadyOwned => ("already_owned", error.to_string()),
        E::ForeignDatabase => ("foreign_database", error.to_string()),
        E::RecoveryUnreadable(_) => ("recovery_unreadable", error.to_string()),
        E::Closed => ("store_closed", error.to_string()),
        E::UnsupportedVersion(_) => ("unsupported_version", error.to_string()),
        E::InvalidInput(_) => ("invalid_input", error.to_string()),
        E::NotFound(_) => ("not_found", error.to_string()),
        E::Duplicate(_) => (duplicate.unwrap_or("input_conflict"), error.to_string()),
        E::ScopeBusy(_) => ("chat_busy", error.to_string()),
        E::LeaseLost(_) => ("lease_lost", error.to_string()),
        E::InvalidTransition(_) => (
            invalid_transition.unwrap_or("unknown-store-error"),
            error.to_string(),
        ),
        E::TransactionGateTimeout => ("transaction_gate_timeout", error.to_string()),
        E::TransactionScopeExpired => ("transaction_scope_expired", error.to_string()),
        // A caught panic or aborted ambient scope: the detail stays in the
        // native log, never in the JS-visible message.
        E::Internal(detail) => {
            eprintln!("agentkit-native: internal store error: {detail}");
            (
                "internal_error",
                "internal store error; inspect the native logs".to_owned(),
            )
        }
        _ => ("unknown-store-error", error.to_string()),
    };
    stable(code, message, None)
}

pub fn session(error: agentkit_host::SessionError) -> Error {
    stable(&error.code, error.message, None)
}

/// Map a host error. Store failures keep their store codes (`not_found`,
/// `already_owned`, `lease_lost`, ...); everything else collapses to a redacted `internal_error`
/// and leaves a native-log breadcrumb carrying only the variant name —
/// never the Display text, which may embed arguments or secrets.
pub fn host(error: agentkit_host::HostError) -> Error {
    use agentkit_host::HostError as H;
    let text = error.to_string();
    let kind = match error {
        H::Store(inner) => return store(inner),
        H::NotFound(message) => return stable("not_found", message, None),
        H::Failure(_) => "failure",
        H::UnknownKind(_) => "unknown_kind",
        H::NoExecutorForKind(_) => "no_executor_for_kind",
        H::Cancelled => "cancelled",
        H::Execution(_) => "execution",
    };
    eprintln!("agentkit-napi: host error mapped to internal_error (kind: {kind})");
    stable("internal_error", text, None)
}

/// Map an agents-context error: the unconfigured-lane variants get stable
/// codes, host failures map exactly as `host`, and every other variant keeps
/// the redacted `internal_error` it had as a host execution failure.
pub fn agents(error: AgentsError) -> Error {
    match error {
        AgentsError::Host(inner) => host(inner),
        AgentsError::SubagentsNotConfigured => {
            stable("subagents_not_configured", error.to_string(), None)
        }
        AgentsError::AdvisorNotConfigured => {
            stable("advisor_not_configured", error.to_string(), None)
        }
        other => host(other.into()),
    }
}

pub fn invalid(message: impl Into<String>) -> Error {
    stable("invalid_input", message, None)
}

/// Map a runtime error: refusals keep the exact code and text the binding
/// raised before the runtime was extracted.
pub fn runtime(error: RuntimeError) -> Error {
    match error {
        RuntimeError::InvalidInput(message) => invalid(message),
        RuntimeError::Coded { code, message } => stable(&code, message, None),
        RuntimeError::Host(error) => host(error),
        RuntimeError::Agents(error) => agents(error),
    }
}

/// A provider-configuration refusal: the connection a service was told to
/// use does not exist or is unusable. Construction fails with this code,
/// never a silently degraded JS-lane fallback.
pub fn connection(message: impl Into<String>) -> Error {
    stable("invalid_connection", message, None)
}

/// Map a core provider error: codes inside the contract cross verbatim
/// (e.g. `invalid_connection` from a preset constructor); everything else
/// is redacted to `internal_error` with the origin preserved in details.
pub fn core(error: agentkit_core::CoreError) -> Error {
    stable(&error.code, error.message, None)
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
    fn coded_runtime_errors_keep_the_old_pack_mapping() {
        let known = runtime(RuntimeError::Coded {
            code: "invalid_input".to_owned(),
            message: "nope".to_owned(),
        });
        let payload: Value = serde_json::from_str(&known.reason).expect("payload is JSON");
        assert_eq!(payload["code"], "invalid_input");
        assert_eq!(payload["message"], "nope");
        // A contract code crosses verbatim (workspace_busy is one).
        let verbatim = runtime(RuntimeError::Coded {
            code: "workspace_busy".to_owned(),
            message: "leased".to_owned(),
        });
        let payload: Value = serde_json::from_str(&verbatim.reason).expect("payload is JSON");
        assert_eq!(payload["code"], "workspace_busy");
        // An unknown origin is redacted to internal_error but stays
        // inspectable in details.origin.
        let foreign = runtime(RuntimeError::Coded {
            code: "not_in_the_contract".to_owned(),
            message: "x".to_owned(),
        });
        let payload: Value = serde_json::from_str(&foreign.reason).expect("payload is JSON");
        assert_eq!(payload["code"], "internal_error");
        assert_eq!(payload["message"], GENERIC_MESSAGE);
        assert_eq!(payload["details"]["origin"], "not_in_the_contract");
    }

    #[test]
    fn answer_stale_crosses_the_boundary_verbatim() {
        // The stale-answer code is part of the contract: a session-service
        // stale refusal must reach JavaScript as `answer_stale` with the
        // TypeScript message text, never redacted to `internal_error`.
        let message = "Request req-1 can no longer be answered: it was cancelled.";
        let error = session(agentkit_host::SessionError::new("answer_stale", message));
        let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
        assert_eq!(payload["code"], "answer_stale");
        assert_eq!(payload["message"], message);
        assert!(payload.get("details").is_none());
        assert!(is_contract_code("answer_stale"));
    }

    #[test]
    fn unknown_codes_are_redacted_not_leaked() {
        let error = stable("weird-debug-code", "raw internal detail", None);
        let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
        assert_eq!(payload["code"], "internal_error");
        // The raw message NEVER crosses for a non-contract code: only the
        // fixed generic string does, and the unknown code survives solely
        // as an origin marker, never as the wire code.
        let message = payload["message"].as_str().expect("message is a string");
        assert!(!message.contains("raw internal detail"));
        assert_eq!(message, GENERIC_MESSAGE);
        assert_eq!(payload["details"]["origin"], "weird-debug-code");
    }

    #[test]
    fn explicit_internal_error_messages_are_redacted() {
        // Even a caller that names the contract code cannot smuggle raw
        // runtime text across: the message is the fixed generic string.
        let error = stable("internal_error", "raw secret", None);
        let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
        assert_eq!(payload["code"], "internal_error");
        let message = payload["message"].as_str().expect("message is a string");
        assert!(!message.contains("raw secret"));
        assert_eq!(message, GENERIC_MESSAGE);
    }

    #[test]
    fn internal_keeps_its_compile_time_context() {
        // `internal` bypasses the blanket redaction because every call site
        // passes a compile-time literal (audited), so the context message
        // stays observable.
        let error = internal("run event serialization");
        let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
        assert_eq!(payload["code"], "internal_error");
        assert_eq!(
            payload["message"],
            "run event serialization failed; inspect the native logs"
        );
        assert!(payload.get("details").is_none());
    }

    #[test]
    fn host_store_errors_keep_their_store_codes_and_unconfigured_lanes_are_stable() {
        let payload = |error: Error| -> Value { serde_json::from_str(&error.reason).unwrap() };
        let mapped = payload(host(agentkit_host::HostError::Store(
            agentkit_store::StoreError::NotFound("x".into()),
        )));
        assert_eq!(mapped["code"], "not_found");
        let mapped = payload(host(agentkit_host::HostError::Store(
            agentkit_store::StoreError::LeaseLost("x".into()),
        )));
        assert_eq!(mapped["code"], "lease_lost");
        let mapped = payload(agents(AgentsError::SubagentsNotConfigured));
        assert_eq!(mapped["code"], "subagents_not_configured");
        assert_eq!(mapped["message"], "subagents not configured");
        let mapped = payload(agents(AgentsError::AdvisorNotConfigured));
        assert_eq!(mapped["code"], "advisor_not_configured");
        assert_eq!(mapped["message"], "advisor not configured");
        let mapped = payload(host(agentkit_host::HostError::NotFound(
            "subagent_not_found: c1".into(),
        )));
        assert_eq!(mapped["code"], "not_found");
        assert_eq!(mapped["message"], "subagent_not_found: c1");
    }

    #[test]
    fn host_error_text_is_redacted_not_leaked() {
        // `host` hands the Display text of a HostError to `stable` with the
        // internal_error code; the redaction now keeps it native-only.
        let error = host(agentkit_host::HostError::Execution(
            "tcp connect to 10.0.0.7:8080 refused: token abc123".into(),
        ));
        let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
        assert_eq!(payload["code"], "internal_error");
        let message = payload["message"].as_str().expect("message is a string");
        assert_eq!(message, GENERIC_MESSAGE);
        assert!(!error.reason.contains("10.0.0.7"));
        assert!(!error.reason.contains("abc123"));
    }

    #[test]
    fn store_unknown_fallback_keeps_origin_and_redacts_message() {
        // Variants with no explicit arm land in `unknown-store-error`: not a
        // contract code, so the wire code is internal_error with the origin
        // preserved and the Display text withheld.
        let error = store(agentkit_store::StoreError::SessionDescriptorChanged(
            "raw descriptor diff leaked".into(),
        ));
        let payload: Value = serde_json::from_str(&error.reason).expect("payload is JSON");
        assert_eq!(payload["code"], "internal_error");
        assert_eq!(payload["details"]["origin"], "unknown-store-error");
        assert_eq!(payload["message"], GENERIC_MESSAGE);
        assert!(!error.reason.contains("raw descriptor diff leaked"));
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
