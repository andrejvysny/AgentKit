//! The session descriptor (ADR 0016 §1): what a chat records once it is an
//! agent session. Port of `sessions/descriptor.ts`.
//!
//! Every reader answers `None` when the key is absent and `Some(invalid)`
//! semantics through the outer `Option` when it is present but malformed:
//! a bad optional field invalidates the descriptor the same way a bad
//! `profile` does instead of being silently dropped.

use agentkit_store::SESSION_METADATA_KEY;
use serde_json::{Map, Value};

pub const CHAT_TURN_TASK_KIND: &str = "chat.turn";
pub const REASONING_EFFORTS: [&str; 4] = ["minimal", "low", "medium", "high"];

fn is_record(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn is_str(value: Option<&Value>) -> Option<&str> {
    value?.as_str()
}

fn is_non_empty(value: Option<&Value>) -> Option<&str> {
    value?.as_str().filter(|text| !text.is_empty())
}

/// The chat's session descriptor, or `None` when it is absent or malformed.
pub fn read_session_descriptor(metadata: &Value) -> Option<Value> {
    let raw = is_record(metadata.get(SESSION_METADATA_KEY)?)?;
    if raw.get("version")?.as_i64()? != 1 {
        return None;
    }
    let profile = is_record(raw.get("profile")?)?;
    let id = is_non_empty(profile.get("id"))?;
    let mut next = Map::new();
    next.insert("version".into(), Value::from(1));
    let mut profile_next = Map::new();
    profile_next.insert("id".into(), id.into());
    if let Some(digest) = profile.get("digest") {
        profile_next.insert("digest".into(), Value::from(is_str(Some(digest))?));
    }
    next.insert("profile".into(), Value::Object(profile_next));
    for key in ["providerId", "model"] {
        if let Some(value) = raw.get(key) {
            next.insert(key.into(), Value::from(is_str(Some(value))?));
        }
    }
    if let Some(effort) = raw.get("reasoningEffort") {
        let effort = is_str(Some(effort))?;
        if !REASONING_EFFORTS.contains(&effort) {
            return None;
        }
        next.insert("reasoningEffort".into(), effort.into());
    }
    next.insert(
        "createdAt".into(),
        Value::from(is_str(raw.get("createdAt"))?),
    );
    // Absent optional keys are valid and stay omitted; a malformed one
    // (present but rejected) invalidates the whole descriptor.
    if let Some(workspace) = read_workspace(raw.get("workspace")) {
        next.insert("workspace".into(), workspace?);
    }
    if let Some(context) = read_context(raw.get("context")) {
        next.insert("context".into(), context?);
    }
    if let Some(skills) = read_skills(raw.get("skills")) {
        next.insert("skills".into(), skills?);
    }
    if let Some(kind) = raw.get("turnKind") {
        let kind = is_str(Some(kind))?;
        if !is_valid_session_turn_kind(kind) {
            return None;
        }
        next.insert("turnKind".into(), kind.into());
    }
    Some(Value::Object(next))
}

/// Three-state reader: outer `None` = absent, inner `None` = present but
/// malformed, `Some(Some(value))` = valid.
fn read_workspace(raw: Option<&Value>) -> Option<Option<Value>> {
    let raw = raw?;
    let raw = if raw.is_null() {
        return Some(None);
    } else {
        is_record(raw)?
    };
    let root = is_non_empty(raw.get("root"))?;
    let trusted = raw.get("trusted")?.as_bool()?;
    let mut out = Map::new();
    out.insert("root".into(), root.into());
    out.insert("trusted".into(), Value::Bool(trusted));
    Some(Some(Value::Object(out)))
}

fn read_positive_int(raw: Option<&Value>) -> Option<Option<Value>> {
    let raw = raw?;
    if raw.is_null() {
        return Some(None);
    }
    let value = raw.as_i64()?;
    Some((value > 0).then(|| Value::from(value)))
}

fn read_context(raw: Option<&Value>) -> Option<Option<Value>> {
    let raw = raw?;
    let raw = if raw.is_null() {
        return Some(None);
    } else {
        is_record(raw)?
    };
    let mut out = Map::new();
    if let Some(policy) = raw.get("policy") {
        let policy = policy.as_str()?;
        if policy != "final_answers" && policy != "transcript" {
            return Some(None);
        }
        out.insert("policy".into(), policy.into());
    }
    for key in ["windowTokens", "reserveTokens", "keepRecentTokens"] {
        if let Some(value) = read_positive_int(raw.get(key)) {
            out.insert(key.into(), value?);
        }
    }
    if let Some(elision) = raw.get("toolResultElision") {
        if elision.is_boolean() {
            if elision.as_bool() != Some(false) {
                return Some(None);
            }
            out.insert("toolResultElision".into(), Value::Bool(false));
        } else {
            let elision = is_record(elision)?;
            let mut out_elision = Map::new();
            for key in ["keepRecentTurns", "maxChars"] {
                if let Some(value) = read_positive_int(elision.get(key)) {
                    out_elision.insert(key.into(), value?);
                }
            }
            out.insert("toolResultElision".into(), Value::Object(out_elision));
        }
    }
    Some(Some(Value::Object(out)))
}

fn read_skills(raw: Option<&Value>) -> Option<Option<Value>> {
    let raw = raw?;
    if raw.is_null() {
        return Some(None);
    }
    let entries = raw.as_array()?;
    let mut skills = vec![];
    for entry in entries {
        let entry = is_record(entry)?;
        skills.push(Value::Object(Map::from_iter([
            ("id".to_owned(), Value::from(is_str(entry.get("id"))?)),
            (
                "revision".to_owned(),
                Value::from(is_str(entry.get("revision"))?),
            ),
        ])));
    }
    Some(Some(Value::Array(skills)))
}

/// Whether `kind` may name a session's turn tasks: the default, or a host
/// kind in dotted lowercase form outside the reserved `chat.*` /
/// `agentkit.*` prefixes (`^[a-z][a-z0-9_-]*(\.[a-z0-9_-]+)*$`).
pub fn is_valid_session_turn_kind(kind: &str) -> bool {
    if kind == CHAT_TURN_TASK_KIND {
        return true;
    }
    for segment in kind.split('.') {
        let mut chars = segment.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !first.is_ascii_lowercase() {
            return false;
        }
        if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
            return false;
        }
    }
    let root = kind.split('.').next().unwrap_or(kind);
    root != "chat" && root != "agentkit"
}

/// The task kind this session's turns are created as.
pub fn session_turn_kind(descriptor: &Value) -> String {
    descriptor["turnKind"]
        .as_str()
        .filter(|kind| !kind.is_empty())
        .unwrap_or(CHAT_TURN_TASK_KIND)
        .to_owned()
}

/// The run-content selection a session's turns carry: its carried-over
/// Skills. `None` when there are none, so a session without Skills submits
/// exactly the payload it did before.
pub fn session_content_selection(descriptor: &Value) -> Option<Value> {
    let skills = descriptor["skills"].as_array()?;
    if skills.is_empty() {
        return None;
    }
    Some(serde_json::json!({ "skills": skills }))
}
