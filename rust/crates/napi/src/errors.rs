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
        E::InvalidInput(_) | E::TaskInput { .. } | E::ManagedInput { .. } => {
            ("invalid_input", error.to_string())
        }
        E::NotFound(_) => ("not_found", error.to_string()),
        E::Duplicate(_) | E::AnswerConflict(_) => {
            (duplicate.unwrap_or("input_conflict"), error.to_string())
        }
        E::ScopeBusy(_) => ("chat_busy", error.to_string()),
        E::LeaseLost(_) => ("lease_lost", error.to_string()),
        E::InvalidTransition(_)
        | E::AgentInputTransition(_)
        | E::AnswerStale(_)
        | E::ReceiptTransition { .. } => (
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

pub fn managed_store(error: agentkit_store::StoreError) -> Error {
    if matches!(
        &error,
        agentkit_store::StoreError::ManagedInput {
            reason: agentkit_store::ManagedInputReason::SecretReferencesUnsupported,
            ..
        }
    ) {
        return stable("mcp_secret_missing", error.to_string(), None);
    }
    store_port(error, "invalid_state", "invalid_input")
}

pub fn session(error: agentkit_sessions::SessionsError) -> Error {
    use agentkit_sessions::SessionsError as S;
    match error {
        S::InvalidInput(message) => stable("invalid_input", message, None),
        S::InvalidInputContext(source) => stable("invalid_input", source.to_string(), None),
        S::ChatBusy(message) => stable("chat_busy", message, None),
        S::ChatIdle(message) => stable("chat_idle", message, None),
        S::InvalidState(message) => stable("invalid_state", message, None),
        S::InputNotFound(message) | S::TurnNotFound(message) | S::ExecutionNotFound(message) => {
            stable("not_found", message, None)
        }
        S::SessionNotFound(message) => stable("session_not_found", message, None),
        S::AnswerStale(message) => stable("answer_stale", message, None),
        S::NoExecutorForKind(message) => stable("no_executor_for_kind", message, None),
        S::SessionContextInvalid(message) => stable("session_context_invalid", message, None),
        S::SessionStore(error) => session_store(error.0),
        S::AnswerStore(error) => match error.0 {
            agentkit_store::StoreError::AnswerStale(message) => {
                stable("answer_stale", message, None)
            }
            other => session_store(other),
        },
        S::ReceiptStore(error) => match error.0 {
            agentkit_store::StoreError::ReceiptTransition {
                reason: agentkit_store::ReceiptTransitionReason::RunActive,
                message,
            } => stable("receipt_run_active", message, None),
            agentkit_store::StoreError::ReceiptTransition {
                reason: agentkit_store::ReceiptTransitionReason::NotUncertain,
                message,
            } => stable("receipt_not_uncertain", message, None),
            other => session_store(other),
        },
        S::ReceiptInvalidInput(message) => stable("invalid_input", message, None),
        S::ReceiptNotFound {
            execution_id,
            chat_id,
        } => stable(
            "not_found",
            format!("execution {execution_id} not found in chat {chat_id}."),
            None,
        ),
        other => stable("internal", other.to_string(), None),
    }
}
fn session_store(error: agentkit_store::StoreError) -> Error {
    use agentkit_store::StoreError as E;
    match error {
        E::NotFound(message) => stable("not_found", message, None),
        E::AnswerConflict(message) => stable("answer_conflict", message, None),
        E::Duplicate(message) => stable("input_conflict", message, None),
        E::ScopeBusy(scope) => stable("chat_busy", format!("Chat {scope} is busy."), None),
        E::InvalidTransition(message)
        | E::AgentInputTransition(message)
        | E::AnswerStale(message)
        | E::ReceiptTransition { message, .. } => stable("invalid_state", message, None),
        E::LeaseLost(task) => stable("invalid_state", format!("lease lost: {task}"), None),
        E::InvalidInput(message)
        | E::TaskInput { message, .. }
        | E::ManagedInput { message, .. } => stable("invalid_input", message, None),
        other => stable("store_failure", other.to_string(), None),
    }
}
pub fn host(error: agentkit_engine::EngineError<agentkit_store::StoreError>) -> Error {
    match error {
        agentkit_engine::EngineError::Store(error) => store(error),
        other => stable("internal_error", other.to_string(), None),
    }
}
pub fn sessions(error: agentkit_sessions::SessionsError) -> Error {
    use agentkit_sessions::SessionsError as S;
    match error {
        S::Engine(error) => host(error),
        S::Store(error) => store(error),
        S::AgentNotFound(id) => stable("not_found", format!("agent {id}"), None),
        S::TaskNotFound(id) => stable("not_found", format!("task {id}"), None),
        S::ReceiptNotFound {
            execution_id,
            chat_id,
        } => stable(
            "not_found",
            format!("not_found: execution {execution_id} not found in chat {chat_id}."),
            None,
        ),
        other => stable("internal_error", other.to_string(), None),
    }
}
pub fn chat(error: agentkit_chat::ChatError) -> Error {
    match error {
        agentkit_chat::ChatError::Engine(error) => host(error),
        agentkit_chat::ChatError::Store(error) => store(error),
        agentkit_chat::ChatError::Sessions(error) => sessions(error),
        other => stable("internal_error", other.to_string(), None),
    }
}
pub fn agents(error: AgentsError) -> Error {
    match error {
        AgentsError::Engine(error) => host(error),
        AgentsError::Store(error) => store(error),
        AgentsError::Sessions(error) => sessions(error),
        AgentsError::Chat(error) => chat(error),
        AgentsError::AgentNotFound(id) => {
            stable("not_found", format!("agent_not_found: {id}"), None)
        }
        AgentsError::SubagentNotFound(id) => {
            stable("not_found", format!("subagent_not_found: {id}"), None)
        }
        AgentsError::SubagentsNotConfigured => {
            stable("subagents_not_configured", "subagents not configured", None)
        }
        AgentsError::AdvisorNotConfigured => {
            stable("advisor_not_configured", "advisor not configured", None)
        }
        other => stable("internal_error", other.to_string(), None),
    }
}
pub fn teammate(error: impl Into<AgentsError>) -> Error {
    use agentkit_sessions::SessionsError as S;
    match error.into() {
        refusal @ (AgentsError::InvalidAgent(_) | AgentsError::InvalidAdmission(_)) => {
            invalid(refusal.to_string())
        }
        AgentsError::Sessions(S::InvalidOverrides(message)) => {
            invalid(format!("invalid_overrides: {message}"))
        }
        AgentsError::Sessions(S::RunProfileRefused(message)) => {
            invalid(format!("run_profile_refused:{message}"))
        }
        AgentsError::Sessions(S::Store(error)) | AgentsError::Store(error) => teammate_store(error),
        AgentsError::Engine(agentkit_engine::EngineError::Store(error)) => teammate_store(error),
        other => agents(other),
    }
}
fn teammate_store(error: agentkit_store::StoreError) -> Error {
    match error {
        agentkit_store::StoreError::Conflict(message) => stable("revision_conflict", message, None),
        other => store_port(other, "invalid_task_transition", "duplicate_task"),
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
        RuntimeError::Engine(error) => host(error),
        RuntimeError::Agents(error) => agents(error),
        RuntimeError::Sessions(error) => sessions(error),
        RuntimeError::Chat(error) => chat(error),
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
mod tests;
