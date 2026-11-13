//! The frozen native admission stanza: payload keys, the continuation scope,
//! the broker stanza and the one validator that admission and execution share.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{ExecutionTarget, NativeExecutionTarget, TurnInput};

/// Payload key carrying the frozen `ExecutionTarget` JSON.
pub const EXECUTION_TARGET_PAYLOAD_KEY: &str = "executionTarget";
/// Payload key carrying the frozen `TurnInput` JSON.
pub const TURN_INPUT_PAYLOAD_KEY: &str = "turnInput";
/// Payload key carrying the frozen broker stanza (OwnedMcpChat only).
pub const BROKER_PAYLOAD_KEY: &str = "broker";
/// Payload key carrying the frozen continuation scope (consumer-defined
/// conversation/branch identity; review §2.3).
pub const CONTINUATION_SCOPE_PAYLOAD_KEY: &str = "continuationScope";

/// The frozen provenance scope an exact resume must match — consumer-side
/// identities the host stores opaquely. A vendor session id alone is
/// mutable state, not a transcript snapshot: continuation authority comes
/// from the FULL scope (conversation, selected parent, acknowledged
/// history position, backend, executable, config identity, prompt
/// revision) committed by the attempt that actually completed the turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContinuationScope {
    pub conversation: String,
    /// The conversation position this turn was SUBMITTED from — the input
    /// side. The exact-resume lookup binds here, and nothing else.
    pub parent: String,
    pub history_position: i64,
    /// Where the COMPLETED conversation ends — normally the assistant
    /// message the vendor answer materialises into. The commit binds
    /// here, never at the input position (review AGENTKIT-113): a vendor
    /// session is a cursor advanced through the history, so committing
    /// at the input position would let a regeneration — or an explicit
    /// completed-assistant parent — select the session that already
    /// contains the very answer being discarded or replaced.
    pub commit_parent: String,
    pub commit_history_position: i64,
    pub backend: String,
    pub executable: String,
    pub config_identity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_revision: Option<String>,
}

impl ContinuationScope {
    /// Full-shape validation: every identity component present, positions
    /// ordered. Enforced at admission (`validate_frozen_stanza`) AND at
    /// claim, so a malformed scope is refused at send, never discovered
    /// mid-turn.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("conversation", &self.conversation),
            ("parent", &self.parent),
            ("commit_parent", &self.commit_parent),
            ("backend", &self.backend),
            ("executable", &self.executable),
            ("config_identity", &self.config_identity),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{name} must be non-empty"));
            }
        }
        if self.history_position < 0 {
            return Err("history_position must not be negative".into());
        }
        if self.commit_history_position <= self.history_position {
            return Err(
                "commit_history_position must be strictly ahead of the input position".into(),
            );
        }
        if self.commit_parent.trim() == self.parent.trim() {
            return Err(
                "commit_parent must name the completed answer, not the input parent".into(),
            );
        }
        Ok(())
    }

    /// A frozen scope must agree with the frozen target it ships beside:
    /// two sources of the same facts are allowed, drift is not.
    pub fn agrees_with(&self, target: &NativeExecutionTarget) -> Option<String> {
        if self.backend != target.backend {
            return Some(format!(
                "scope backend '{}' != target backend '{}'",
                self.backend, target.backend
            ));
        }
        if self.executable != target.executable {
            return Some(format!(
                "scope executable '{}' != target executable '{}'",
                self.executable, target.executable
            ));
        }
        if self.config_identity != target.config_identity {
            return Some(format!(
                "scope config identity '{}' != target config identity '{}'",
                self.config_identity, target.config_identity
            ));
        }
        if self.system_prompt_revision != target.system_prompt_revision {
            return Some(format!(
                "scope prompt revision {:?} != target prompt revision {:?}",
                self.system_prompt_revision, target.system_prompt_revision
            ));
        }
        None
    }
}

/// One catalog entry in the serialized payload form (`broker.tools[]`).
/// Names become `mcp__<server>__<name>` at the vendor.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrokerToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub writes: bool,
}

/// The frozen broker stanza of an OwnedMcpChat turn.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrokerSpec {
    pub server_name: String,
    pub tools: Vec<BrokerToolSpec>,
    #[serde(default)]
    pub limits: Option<BrokerLimitsSpec>,
    /// The admission-frozen authorization-policy snapshot (review
    /// round-4 F6a). OPAQUE to the host: AgentKit parses and preserves
    /// it but never interprets it — it exists so the embedding host
    /// persists ONE immutable authority record with the turn (settings
    /// live-swap can never silently widen an admitted turn's tool
    /// authority) and hands it to its own factory at attempt time.
    /// `None` = pre-F6a stanza; hosts that require a policy fail closed
    /// on absence. Unknown keys remain refused (`deny_unknown_fields`).
    #[serde(default)]
    pub authorization_policy: Option<Value>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrokerLimitsSpec {
    pub max_calls: usize,
    pub max_args_bytes: usize,
    pub max_result_bytes: usize,
    pub call_timeout_ms: i64,
}

/// Parse a payload target without executing. Admission and execution
/// must apply the same parse (AGENTKIT-112/113).
pub fn parse_target(payload: &Value) -> Result<ExecutionTarget, String> {
    let raw = payload
        .get(EXECUTION_TARGET_PAYLOAD_KEY)
        .ok_or_else(|| format!("payload is missing '{EXECUTION_TARGET_PAYLOAD_KEY}'"))?;
    serde_json::from_value::<ExecutionTarget>(raw.clone())
        .map_err(|e| format!("invalid execution target: {e}"))
}

/// The ONE cross-field semantic validator of the frozen admission
/// stanza (review round-4 F8-b): a typed continuation scope must not
/// merely parse next to the frozen execution target — the facts they
/// share (backend, executable, config identity, system-prompt
/// revision) must AGREE. This is the check the executor already ran
/// only at claim time; running it at admission too makes admission
/// durably-clean: there is no successful send whose rows a later
/// execution is guaranteed to refuse. Invoked by
/// `validate_frozen_stanza` (admission) and at claim — one field
/// list, one source of truth.
pub fn validate_frozen_semantics(stanza: &serde_json::Map<String, Value>) -> Result<(), String> {
    let Some(scope_raw) = stanza.get(CONTINUATION_SCOPE_PAYLOAD_KEY) else {
        return Ok(());
    };
    if scope_raw.is_null() {
        return Ok(());
    }
    let scope = serde_json::from_value::<ContinuationScope>(scope_raw.clone())
        .map_err(|e| format!("'{CONTINUATION_SCOPE_PAYLOAD_KEY}': {e}"))
        .and_then(|scope| {
            scope
                .validate()
                .map_err(|e| format!("'{CONTINUATION_SCOPE_PAYLOAD_KEY}': {e}"))?;
            Ok(scope)
        })?;
    let Some(target_raw) = stanza.get(EXECUTION_TARGET_PAYLOAD_KEY) else {
        return Err(format!(
            "'{CONTINUATION_SCOPE_PAYLOAD_KEY}' requires a frozen '{EXECUTION_TARGET_PAYLOAD_KEY}'"
        ));
    };
    let target = parse_target(&json!({ EXECUTION_TARGET_PAYLOAD_KEY: target_raw.clone() }))
        .map_err(|e| format!("'{EXECUTION_TARGET_PAYLOAD_KEY}': {e}"))?;
    let ExecutionTarget::Native(target) = target else {
        return Err(format!(
            "'{CONTINUATION_SCOPE_PAYLOAD_KEY}' requires a native '{EXECUTION_TARGET_PAYLOAD_KEY}'"
        ));
    };
    if let Some(drift) = scope.agrees_with(&target) {
        return Err(format!(
            "'{CONTINUATION_SCOPE_PAYLOAD_KEY}' contradicts '{EXECUTION_TARGET_PAYLOAD_KEY}': {drift}"
        ));
    }
    Ok(())
}

/// Validate a consumer-frozen admission stanza at ADMISSION time (the
/// public session API calls this before any row lands, so a malformed
/// native stanza is refused at send, never discovered after queueing
/// and claim — AGENTKIT-112). Only the four frozen payload keys are
/// allowed, every present value must parse through the exact typed
/// contract the executor enforces at claim, and the CROSS-FIELD
/// semantics must agree (review round-4 F8-b).
pub fn validate_frozen_stanza(stanza: &serde_json::Map<String, Value>) -> Result<(), String> {
    const ALLOWED: [&str; 4] = [
        EXECUTION_TARGET_PAYLOAD_KEY,
        TURN_INPUT_PAYLOAD_KEY,
        BROKER_PAYLOAD_KEY,
        CONTINUATION_SCOPE_PAYLOAD_KEY,
    ];
    let mut errors = Vec::new();
    for (key, value) in stanza {
        if !ALLOWED.contains(&key.as_str()) {
            errors.push(format!("unknown frozen key '{key}'"));
            continue;
        }
        let outcome = match key.as_str() {
            EXECUTION_TARGET_PAYLOAD_KEY => {
                parse_target(&serde_json::json!({ EXECUTION_TARGET_PAYLOAD_KEY: value.clone() }))
                    .map(|_| ())
            }
            TURN_INPUT_PAYLOAD_KEY => serde_json::from_value::<TurnInput>(value.clone())
                .map(|_| ())
                .map_err(|e| e.to_string()),
            BROKER_PAYLOAD_KEY => serde_json::from_value::<BrokerSpec>(value.clone())
                .map(|_| ())
                .map_err(|e| e.to_string()),
            CONTINUATION_SCOPE_PAYLOAD_KEY => {
                serde_json::from_value::<ContinuationScope>(value.clone())
                    .map_err(|e| e.to_string())
                    .and_then(|scope| scope.validate().map_err(|e| e.to_string()))
                    .map(|_| ())
            }
            _ => unreachable!("checked against ALLOWED above"),
        };
        if let Err(detail) = outcome {
            errors.push(format!("{key}: {detail}"));
        }
    }
    // F8-b: the per-key parse says every field is well-formed; the
    // cross-field pass says they describe ONE execution. Admission
    // refuses here, before any row lands.
    if let Err(detail) = validate_frozen_semantics(stanza) {
        errors.push(detail);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}
