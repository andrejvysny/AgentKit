//! The session metadata rules: the chat-metadata keys the session descriptor,
//! the execution state and the standing approvals live under, and the pure
//! read/write rules over them (ADR 0016 §1/§2, ADR 0019).
//!
//! The adapter writes these keys inside one transaction; the *meaning* is
//! here, so the session rule is one authority that the SQLite store and any
//! other adapter both read. `agentkit-store` re-exports every name.

use crate::{ChatRecord, parse_iso_millis};
use agentkit_kernel::ConversationTimestamp;
use serde_json::{Value, json};

/// Chat metadata key holding the session descriptor (ADR 0016 §1).
pub const SESSION_METADATA_KEY: &str = "agentkit.session";
/// Chat metadata key holding the compact-request marker (ADR 0017 part 2).
pub const COMPACT_REQUESTED_METADATA_KEY: &str = "agentkit.session.compactRequestedAt";
/// Chat metadata key holding the durable execution state (ADR 0019 part 2).
pub const EXECUTION_METADATA_KEY: &str = "agentkit.session.execution";
/// Chat metadata key holding the standing approvals.
pub const STANDING_APPROVALS_METADATA_KEY: &str = "agentkit.session.approvals";

/// Marks an approve answer in `appliedMessageId`: no tool record backs it.
pub const APPROVE_MARKER: &str = "approval:";

// ---------------------------------------------------------------------------
/// `executionStateForMode`).
pub fn execution_state_for_mode(
    mode: &str,
    updated_at: &str,
    prior: Option<&Value>,
) -> Option<Value> {
    if mode.is_empty() {
        return None;
    }
    let history = prior
        .and_then(|state| state.get("history"))
        .cloned()
        .unwrap_or_else(|| Value::Array(vec![]));
    Some(json!({
        "version": 1,
        "mode": mode,
        "updatedAt": updated_at,
        "history": history,
    }))
}

/// The session's execution state, or `None` when absent or malformed.
pub fn read_execution_state(metadata: &Value) -> Option<Value> {
    inspect_execution_state(metadata).ok()
}

/// Three-state read of the execution state. `Malformed` must be handled as
/// blocking evidence — corrupt permission state is never "no decision yet",
/// because a permissive configured fallback would then silently replace a
/// human's recorded authority.
#[derive(Debug)]
pub enum ExecutionStateRead {
    Absent,
    Malformed,
    Valid(Value),
}

impl ExecutionStateRead {
    fn ok(self) -> Option<Value> {
        match self {
            Self::Valid(state) => Some(state),
            Self::Absent | Self::Malformed => None,
        }
    }
}

pub fn inspect_execution_state(metadata: &Value) -> ExecutionStateRead {
    if metadata
        .get(EXECUTION_METADATA_KEY)
        .is_none_or(Value::is_null)
    {
        return ExecutionStateRead::Absent;
    }
    match read_execution_state_inner(metadata) {
        Some(state) => ExecutionStateRead::Valid(state),
        None => ExecutionStateRead::Malformed,
    }
}

fn read_execution_state_inner(metadata: &Value) -> Option<Value> {
    let raw = metadata.get(EXECUTION_METADATA_KEY)?.as_object()?;
    if raw.get("version")?.as_i64()? != 1 {
        return None;
    }
    let mode = raw
        .get("mode")?
        .as_str()
        .filter(|value| !value.is_empty())?;
    let updated_at = raw.get("updatedAt")?.as_str()?;
    let history_raw = match raw.get("history") {
        None => None,
        Some(Value::Array(entries)) => Some(entries),
        Some(_) => return None,
    };
    let mut history = vec![];
    // A malformed entry invalidates the whole state: a truncated audit trail
    // is not something to hand a permission decision.
    for entry in history_raw.unwrap_or(&vec![]) {
        history.push(read_grant(entry)?);
    }
    let mut state =
        json!({"version": 1, "mode": mode, "updatedAt": updated_at, "history": history});
    if let Some(grant) = raw.get("grantedPlan") {
        state["grantedPlan"] = read_grant(grant)?;
    }
    Some(state)
}

fn read_grant(raw: &Value) -> Option<Value> {
    let grant = raw.as_object()?;
    for key in ["mode", "planDigest"] {
        if !grant
            .get(key)?
            .as_str()
            .is_some_and(|value| !value.is_empty())
        {
            return None;
        }
    }
    for key in ["requestId", "turnId", "approvedAt"] {
        if !grant.get(key)?.is_string() {
            return None;
        }
    }
    for key in ["requestedMode", "plan"] {
        // Compatibility note: early M2 could persist an absent optional as
        // an explicit `null`. Neither field is ever authority (the mode a
        // plan REQUESTED is recorded for the human, never obeyed; the plan
        // text is a display copy), so reading a legacy null as "absent"
        // repairs those records without widening any permission. A null
        // on a REQUIRED field stays malformed.
        if grant.get(key).is_some_and(|value| !value.is_null())
            && grant.get(key).and_then(Value::as_str).is_none()
        {
            return None;
        }
    }
    let mut grant = Value::Object(grant.clone());
    if let Value::Object(object) = &mut grant {
        for key in ["requestedMode", "plan"] {
            if object.get(key).is_some_and(Value::is_null) {
                object.remove(key);
            }
        }
    }
    Some(grant)
}

/// The metadata with its execution state replaced (`None` clears it).
pub fn with_execution(metadata: &Value, state: Option<Value>) -> Value {
    let mut next = metadata.as_object().cloned().unwrap_or_default();
    match state {
        None => {
            next.remove(EXECUTION_METADATA_KEY);
        }
        Some(state) => {
            next.insert(EXECUTION_METADATA_KEY.into(), state);
        }
    }
    Value::Object(next)
}

const MAX_HISTORY: usize = 8;
/// The cap one session's standing-approval list may hold, active and revoked
/// alike. The adapter names it in its refusal when a write would cross it.
pub const MAX_STANDING_APPROVALS: usize = 256;

/// The state produced by approving a plan; `None` for an invalid grant
/// (mirrors the `invalid_input` throws of `executionStateForGrant`).
pub fn execution_state_for_grant(
    grant: &Value,
    now_text: &str,
    prior: Option<Value>,
) -> Option<Value> {
    let mode = grant.get("mode")?.as_str()?;
    if mode == "plan" || mode.is_empty() {
        return None;
    }
    let history = prior
        .as_ref()
        .and_then(|prior| prior.get("history"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut next = vec![grant.clone()];
    next.extend(history);
    next.truncate(MAX_HISTORY);
    Some(json!({
        "version": 1,
        "mode": mode,
        "grantedPlan": grant,
        "updatedAt": now_text,
        "history": next,
    }))
}

pub fn is_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn read_standing_grant(raw: &Value) -> Option<Value> {
    let grant = raw.as_object()?;
    if !grant
        .get("grantId")?
        .as_str()
        .is_some_and(|value| !value.is_empty())
    {
        return None;
    }
    for key in [
        "label",
        "toolName",
        "sourceRequestId",
        "answerId",
        "grantedAt",
    ] {
        if !grant.get(key)?.is_string() {
            return None;
        }
    }
    if !grant.get("digest")?.as_str().is_some_and(is_hex64) {
        return None;
    }
    if let Some(revoked) = grant.get("revokedAt")
        && revoked.as_str().is_none()
    {
        return None;
    }
    Some(Value::Object(grant.clone()))
}

enum StandingParse {
    Ok(Vec<Value>),
    Malformed,
}

fn parse_standing(metadata: &Value) -> StandingParse {
    let Some(raw) = metadata.get(STANDING_APPROVALS_METADATA_KEY) else {
        return StandingParse::Ok(vec![]);
    };
    let Some(raw) = raw.as_object() else {
        return StandingParse::Malformed;
    };
    if raw.get("version").and_then(Value::as_i64) != Some(1) {
        return StandingParse::Malformed;
    }
    let Some(list) = raw.get("grants").and_then(Value::as_array) else {
        return StandingParse::Malformed;
    };
    if list.len() > MAX_STANDING_APPROVALS {
        return StandingParse::Malformed;
    }
    let mut grants = vec![];
    // One bad entry invalidates the whole list: a partly readable permission
    // record is not something to match calls against.
    for entry in list {
        match read_standing_grant(entry) {
            Some(grant) => grants.push(grant),
            None => return StandingParse::Malformed,
        }
    }
    StandingParse::Ok(grants)
}

/// The session's grants, oldest first. Absent is empty; MALFORMED IS ALSO
/// EMPTY (fail closed for matching: unreadable permission data grants
/// nothing). Writers use [`write_standing_grant`], which refuses malformed
/// data instead.
pub fn standing_grants(metadata: &Value) -> Vec<Value> {
    match parse_standing(metadata) {
        StandingParse::Ok(grants) => grants,
        StandingParse::Malformed => vec![],
    }
}

/// The active (not revoked) grant covering this digest, when there is one.
pub fn find_active_standing_grant(metadata: &Value, digest: &str) -> Option<Value> {
    standing_grants(metadata).into_iter().find(|grant| {
        grant.get("digest").and_then(Value::as_str) == Some(digest)
            && grant.get("revokedAt").is_none_or(Value::is_null)
    })
}

pub fn with_standing(metadata: &Value, grants: Vec<Value>) -> Value {
    let mut next = metadata.as_object().cloned().unwrap_or_default();
    next.insert(
        STANDING_APPROVALS_METADATA_KEY.into(),
        json!({"version": 1, "grants": grants}),
    );
    Value::Object(next)
}

/// The session's grants for a WRITE: `None` when the stored list is
/// malformed. A read path may fail closed to an empty list
/// ([`standing_grants`]); a writer must never overwrite unreadable permission
/// data, so it refuses instead. The adapter names the session in its refusal.
pub fn writable_standing_grants(metadata: &Value) -> Option<Vec<Value>> {
    match parse_standing(metadata) {
        StandingParse::Ok(grants) => Some(grants),
        StandingParse::Malformed => None,
    }
}

/// The grants after making room for one more; revoked grants drop oldest
/// first. `None` when the list is full of ACTIVE grants — the adapter's
/// refusal names the cap and the session.
pub fn make_room(mut grants: Vec<Value>) -> Option<Vec<Value>> {
    while grants.len() >= MAX_STANDING_APPROVALS {
        let index = grants
            .iter()
            .position(|grant| grant.get("revokedAt").is_some_and(|value| !value.is_null()))?;
        grants.remove(index);
    }
    Some(grants)
}

/// The `standing` object of a request's content, validated.
pub fn read_standing_key(content: &Value) -> Option<(String, String)> {
    let standing = content.get("standing")?.as_object()?;
    let digest = standing.get("digest")?.as_str()?;
    let label = standing.get("label")?.as_str()?;
    if !is_hex64(digest) || label.is_empty() || label.chars().count() > 512 {
        return None;
    }
    Some((digest.to_owned(), label.to_owned()))
}

/// True when an answer to this request can only be `{ decision }`.
pub fn is_approval_request(content: &Value) -> bool {
    content.get("kind").and_then(Value::as_str) == Some("approval")
}

/// The plan a pending request asks to approve, per the tool-name policy
/// (mirrors `planRequestOf`).
pub fn plan_request_of(content: &Value, plan_tool_names: &[String]) -> Option<Value> {
    if plan_tool_names.is_empty() || content.get("kind").and_then(Value::as_str) != Some("approval")
    {
        return None;
    }
    let tool_name = content.get("toolName")?.as_str()?;
    if !plan_tool_names.iter().any(|name| name == tool_name) {
        return None;
    }
    let plan_digest = content.get("argsSha256")?.as_str()?;
    let mut plan = json!({"planDigest": plan_digest});
    if let Some(mode) = content.get("requestedMode").and_then(Value::as_str) {
        plan["requestedMode"] = mode.into();
    }
    if let Some(text) = content.get("prompt").and_then(Value::as_str) {
        plan["plan"] = text.into();
    }
    Some(plan)
}

/// The shape of a human's answer to an approval request.
#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    /// True when the human approved.
    pub approve: bool,
    /// The human's note, when they left one.
    pub note: Option<String>,
    /// The `remember` flag, carried only by an approval.
    pub remember: Option<bool>,
}

/// `{ decision: "approve" | "deny", note?, remember? }` only; `Err` carries
/// the refusal message the adapter reports as `invalid_input`. `remember` is
/// a boolean and only an approval may carry it: a denial has nothing to
/// remember, and accepting the field there would let a typo read as consent.
pub fn read_decision(response: &Value) -> Result<Decision, String> {
    let bad = || {
        "An approval answer must be { decision: \"approve\" | \"deny\", note?: string, remember?: boolean }; remember is only allowed with \"approve\".".to_owned()
    };
    let object = response.as_object().ok_or_else(bad)?;
    let decision = object.get("decision").and_then(Value::as_str);
    if decision != Some("approve") && decision != Some("deny") {
        return Err(bad());
    }
    for key in object.keys() {
        if !["decision", "note", "remember"].contains(&key.as_str()) {
            return Err(bad());
        }
    }
    let note = match object.get("note") {
        None | Some(Value::Null) => None,
        Some(Value::String(note)) => Some(note.clone()),
        Some(_) => return Err(bad()),
    };
    let remember = match object.get("remember") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(value)) => Some(*value),
        Some(_) => return Err(bad()),
    };
    if remember.is_some() && decision == Some("deny") {
        return Err(bad());
    }
    Ok(Decision {
        approve: decision == Some("approve"),
        note,
        remember,
    })
}

/// The session layer's pre-transaction mirror of the settlement's first
/// write-free act: an approval answer must read `{ decision, note?,
/// remember? }` before any stale gate speaks for the request, exactly as
/// TypeScript's `readDecision` precedes them in the answer transaction. The
/// message is the one the answer transaction itself raises, so the pre-check
/// and the transaction refuse identically.
pub fn approval_answer_refusal(request: &Value, response: &Value) -> Option<String> {
    if is_approval_request(request) {
        read_decision(response).err()
    } else {
        None
    }
}

/// The epoch-millis instant a request's `expiresAt` wire text names (UTC
/// when it carries no offset). The session layer's expiry gate and the
/// adapter's column check share ONE parser ([`parse_iso_millis`]) so both
/// boundaries read the field identically.
pub fn parse_answer_expiry(value: &str) -> Option<i64> {
    parse_iso_millis(value)
}

/// The chat metadata with its session descriptor replaced.
fn with_descriptor(metadata: &Value, descriptor: Value) -> Value {
    let mut next = metadata.as_object().cloned().unwrap_or_default();
    next.insert(SESSION_METADATA_KEY.into(), descriptor);
    Value::Object(next)
}

/// Why a session write refuses before it can land. The adapter maps these
/// onto its own fault (`is_not_found`, `invalid_input`), so the domain rule
/// reads the same against any store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionRefusal {
    /// The record the write targets is not an agent session.
    NotFound(String),
    /// The caller's input, or the record it would overwrite, is not valid.
    InvalidInput(String),
}

impl SessionRefusal {
    /// The refusal naming why the caller's input was rejected.
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }

    /// The refusal naming the record that is not there.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }
}

/// A session-metadata write the adapter applies inside one transaction, so
/// the busy/state checks and the write share a single actor job. The service
/// computes the value; the adapter only persists it.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionMutation {
    /// Patch the session descriptor in place (`set_model`, `configure`). The
    /// chat must already carry a readable descriptor, or the write refuses
    /// `session_not_found`; the patched descriptor must read back, or
    /// `invalid_input: {invalid_message}` — the message the caller's own
    /// validation owed the human. Patching (rather than replacing) keeps the
    /// read-modify-write inside the adapter's single job, so a concurrent
    /// turn cannot slip a descriptor change between the read and the write.
    PatchDescriptor {
        /// Keys set or replaced wholesale.
        replace: serde_json::Map<String, Value>,
        /// Keys whose stored object is EXTENDED with these entries — the
        /// workspace trust flag, which must keep its sibling keys. A key the
        /// descriptor does not already hold as an object is left alone,
        /// exactly as the closure this replaced did.
        merge: serde_json::Map<String, Value>,
        /// Keys dropped.
        remove: Vec<String>,
        /// The message for a patched descriptor that does not read back.
        invalid_message: String,
    },
    /// Stamp the descriptor of a freshly forked chat: the copied descriptor
    /// must read (`invalid_input: "forked descriptor unreadable"`), it is
    /// stamped with `createdAt` (from `now`) and `forkedFrom`, and the result
    /// must read back (`invalid_input: "forked descriptor invalid"`). Pending
    /// inputs and the provider continuation never travel, so the fork's own
    /// provenance has to be written with it.
    ForkDescriptor {
        /// The `forkedFrom` envelope: `{ sessionId, messageId }`.
        forked_from: Value,
    },
    /// An explicit, persisted mode switch by the host or the user — never by
    /// the model. Entering `plan` drops any grant. The chat must be an agent
    /// session; an empty `mode` refuses `invalid_input`.
    ExecutionMode {
        /// The mode to enter, non-empty.
        mode: String,
    },
}

/// Apply `mutation` to `chat` in place, refusing with the reason the caller
/// reports. `now` stamps the execution state's `updatedAt`.
pub fn apply_session_mutation(
    chat: &mut ChatRecord,
    mutation: &SessionMutation,
    now: i64,
) -> Result<(), SessionRefusal> {
    match mutation {
        SessionMutation::PatchDescriptor {
            replace,
            merge,
            remove,
            invalid_message,
        } => {
            let mut descriptor = crate::read_session_descriptor(&chat.metadata)
                .and_then(|descriptor| descriptor.as_object().cloned())
                .ok_or_else(|| {
                    SessionRefusal::not_found(format!(
                        "Session {} is not an agent session.",
                        chat.id
                    ))
                })?;
            for key in remove {
                descriptor.remove(key);
            }
            for (key, value) in merge {
                if let Some(existing) = descriptor.get_mut(key).and_then(Value::as_object_mut) {
                    for (inner, value) in value.as_object().into_iter().flatten() {
                        existing.insert(inner.clone(), value.clone());
                    }
                }
            }
            for (key, value) in replace {
                descriptor.insert(key.clone(), value.clone());
            }
            write_descriptor(chat, &Value::Object(descriptor), invalid_message)?;
            Ok(())
        }
        SessionMutation::ForkDescriptor { forked_from } => {
            let mut descriptor = crate::read_session_descriptor(&chat.metadata)
                .and_then(|descriptor| descriptor.as_object().cloned())
                .ok_or_else(|| SessionRefusal::invalid_input("forked descriptor unreadable"))?;
            let created_at = ConversationTimestamp::from_epoch_millis(now)
                .map_err(|error| SessionRefusal::invalid_input(error.to_string()))?
                .wire_text()
                .map_err(|error| SessionRefusal::invalid_input(error.to_string()))?
                .into_owned();
            descriptor.insert("createdAt".into(), created_at.into());
            descriptor.insert("forkedFrom".into(), forked_from.clone());
            write_descriptor(
                chat,
                &Value::Object(descriptor),
                "forked descriptor invalid",
            )?;
            Ok(())
        }
        SessionMutation::ExecutionMode { mode } => {
            require_descriptor(chat)?;
            let updated_at = ConversationTimestamp::from_epoch_millis(now)
                .map_err(|error| SessionRefusal::invalid_input(error.to_string()))?
                .wire_text()
                .map_err(|error| SessionRefusal::invalid_input(error.to_string()))?
                .into_owned();
            let state = execution_state_for_mode(
                mode,
                &updated_at,
                read_execution_state(&chat.metadata).as_ref(),
            )
            .ok_or_else(|| SessionRefusal::invalid_input("mode must be non-empty.".to_owned()))?;
            chat.metadata = with_execution(&chat.metadata, Some(state));
            Ok(())
        }
    }
}

fn require_descriptor(chat: &ChatRecord) -> Result<(), SessionRefusal> {
    if crate::read_session_descriptor(&chat.metadata).is_none() {
        return Err(SessionRefusal::not_found(format!(
            "Session {} is not an agent session.",
            chat.id
        )));
    }
    Ok(())
}

fn write_descriptor(
    chat: &mut ChatRecord,
    descriptor: &Value,
    invalid_message: &str,
) -> Result<(), SessionRefusal> {
    let metadata = with_descriptor(&chat.metadata, descriptor.clone());
    if crate::read_session_descriptor(&metadata).is_none() {
        return Err(SessionRefusal::invalid_input(invalid_message.to_owned()));
    }
    chat.metadata = metadata;
    Ok(())
}
