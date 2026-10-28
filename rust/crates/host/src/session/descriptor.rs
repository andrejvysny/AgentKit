//! The session descriptor (ADR 0016 §1): what a chat records once it is an
//! agent session. Port of `sessions/descriptor.ts`.
//!
//! Grammar honored (frozen wire authority `packages/host/src/sessions/descriptor.ts`
//! plus `isValidSessionTurnKind` in `packages/host/src/tasks/kinds.ts`):
//!
//! - the descriptor is the record at `metadata["agentkit.session"]` whose
//!   `version` is the number `1` (the TS check is `raw["version"] !== 1`, so any
//!   JSON number equal to `1` — including `1.0` — is accepted);
//! - `profile` is a record; `profile.id` a non-empty string; `profile.digest`
//!   absent or a string; extra profile keys are dropped;
//! - `providerId` / `model` are absent or strings;
//! - `reasoningEffort` is absent or one of [`REASONING_EFFORTS`];
//! - `createdAt` is a required string;
//! - `workspace` is absent, or `{ root: non-empty string, trusted: boolean }`.
//!   Anything else that is present — `null`, a scalar, an array — is malformed
//!   (`isRecord` rejects all three in TS), never silently dropped;
//! - `context` is absent or a record with `policy` ∈ {`final_answers`,
//!   `transcript`}, positive integers (`Number.isInteger(n) && n > 0`) for
//!   `windowTokens` / `reserveTokens` / `keepRecentTokens`, and
//!   `toolResultElision` absent, `false`, or a record of positive integers
//!   `keepRecentTurns` / `maxChars` (`true` is malformed);
//! - `skills` is absent or an array of records each carrying string `id` AND
//!   string `revision`;
//! - `turnKind` is absent, exactly `chat.turn`, or a host kind matching the TS
//!   `HOST_TASK_KIND` regex `^[a-z][a-z0-9_-]*(\.[a-z0-9_-]+)*$` — a lowercase
//!   first segment, later segments drawn from `[a-z0-9_-]` and non-empty (so
//!   `project.1` and `project._worker` are legal) — whose root segment is
//!   neither `chat` nor `agentkit`.
//!
//! Absent optional fields are accepted and stay omitted with their defaults;
//! any field that is PRESENT but malformed invalidates the whole descriptor —
//! the readers answer through the three-state [`Field`] (`Absent` /
//! `Malformed` / `Valid`), mirroring the TS readers' `undefined`-absent /
//! `null`-malformed split, so a malformed value can never be confused with an
//! absent one.

use agentkit_store::SESSION_METADATA_KEY;
use serde_json::{Map, Value};

pub const CHAT_TURN_TASK_KIND: &str = "chat.turn";
pub const REASONING_EFFORTS: [&str; 4] = ["minimal", "low", "medium", "high"];

/// Three-state field read mirroring the TS readers' `undefined` / `null` /
/// value split: `Absent` means the key is not present (valid, default),
/// `Malformed` means the key is present but rejected (invalidates the whole
/// descriptor), `Valid` carries the normalized value.
enum Field<T> {
    Absent,
    Malformed,
    Valid(T),
}

/// The chat's session descriptor, or `None` when it is absent or malformed.
pub fn read_session_descriptor(metadata: &Value) -> Option<Value> {
    let raw = metadata.get(SESSION_METADATA_KEY)?.as_object()?;
    // TS: `raw["version"] !== 1` — a JS number comparison, so `1.0` passes too.
    if as_integral(raw.get("version")?).and_then(Value::as_f64) != Some(1.0) {
        return None;
    }
    let profile = raw.get("profile")?.as_object()?;
    let id = profile
        .get("id")?
        .as_str()
        .filter(|text| !text.is_empty())?;
    let mut profile_next = Map::new();
    profile_next.insert("id".into(), id.into());
    if let Some(digest) = profile.get("digest") {
        // Present-but-non-string (including `null`) invalidates the descriptor.
        profile_next.insert("digest".into(), digest.as_str()?.into());
    }
    let mut next = Map::new();
    next.insert("version".into(), Value::from(1));
    next.insert("profile".into(), Value::Object(profile_next));
    for key in ["providerId", "model"] {
        if let Some(value) = raw.get(key) {
            next.insert(key.into(), value.as_str()?.into());
        }
    }
    if let Some(effort) = raw.get("reasoningEffort") {
        let effort = effort.as_str()?;
        if !REASONING_EFFORTS.contains(&effort) {
            return None;
        }
        next.insert("reasoningEffort".into(), effort.into());
    }
    next.insert("createdAt".into(), raw.get("createdAt")?.as_str()?.into());
    // Absent optional keys stay omitted; a malformed one (present but
    // rejected) invalidates the whole descriptor instead of being dropped.
    for (key, field) in [
        ("workspace", read_workspace(raw.get("workspace"))),
        ("context", read_context(raw.get("context"))),
        ("skills", read_skills(raw.get("skills"))),
    ] {
        match field {
            Field::Absent => {}
            Field::Malformed => return None,
            Field::Valid(value) => {
                next.insert(key.into(), value);
            }
        };
    }
    if let Some(kind) = raw.get("turnKind") {
        let kind = kind.as_str()?;
        if !is_valid_session_turn_kind(kind) {
            return None;
        }
        next.insert("turnKind".into(), kind.into());
    }
    Some(Value::Object(next))
}

/// The JS integer test behind `raw["version"] !== 1` and
/// `Number.isInteger(n) && n > 0`: any JSON number with an integral value,
/// including floats written as `1.0`. Returns the original number so the
/// normalized descriptor keeps exactly the value TypeScript would store.
fn as_integral(value: &Value) -> Option<&Value> {
    if value.is_i64() || value.is_u64() {
        return Some(value);
    }
    let number = value.as_f64()?;
    (number.is_finite() && number.fract() == 0.0).then_some(value)
}

fn read_positive_int(raw: Option<&Value>) -> Field<Value> {
    let Some(value) = raw else {
        return Field::Absent;
    };
    match as_integral(value) {
        // Non-numbers, non-integers, zero and negatives are malformed-present.
        None => Field::Malformed,
        Some(number) if number.as_f64().is_some_and(|n| n > 0.0) => Field::Valid(number.clone()),
        Some(_) => Field::Malformed,
    }
}

fn read_workspace(raw: Option<&Value>) -> Field<Value> {
    // `null` is not a record in TS (`isRecord`), so an explicit null is
    // malformed, not absent.
    let Some(value) = raw else {
        return Field::Absent;
    };
    let Some(raw) = value.as_object() else {
        return Field::Malformed;
    };
    let Some(root) = raw
        .get("root")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    else {
        return Field::Malformed;
    };
    let Some(trusted) = raw.get("trusted").and_then(Value::as_bool) else {
        return Field::Malformed;
    };
    Field::Valid(Value::Object(Map::from_iter([
        ("root".to_owned(), Value::from(root)),
        ("trusted".to_owned(), Value::Bool(trusted)),
    ])))
}

fn read_context(raw: Option<&Value>) -> Field<Value> {
    let Some(value) = raw else {
        return Field::Absent;
    };
    let Some(raw) = value.as_object() else {
        return Field::Malformed;
    };
    let mut out = Map::new();
    if let Some(policy) = raw.get("policy") {
        match policy.as_str() {
            Some(policy @ ("final_answers" | "transcript")) => {
                out.insert("policy".into(), policy.into());
            }
            _ => return Field::Malformed,
        }
    }
    for key in ["windowTokens", "reserveTokens", "keepRecentTokens"] {
        match read_positive_int(raw.get(key)) {
            Field::Absent => {}
            Field::Malformed => return Field::Malformed,
            Field::Valid(number) => {
                out.insert(key.into(), number);
            }
        };
    }
    if let Some(elision) = raw.get("toolResultElision") {
        if elision == &Value::Bool(false) {
            out.insert("toolResultElision".into(), Value::Bool(false));
        } else {
            // `true`, `null`, scalars and arrays are all malformed here.
            let Some(elision) = elision.as_object() else {
                return Field::Malformed;
            };
            let mut out_elision = Map::new();
            for key in ["keepRecentTurns", "maxChars"] {
                match read_positive_int(elision.get(key)) {
                    Field::Absent => {}
                    Field::Malformed => return Field::Malformed,
                    Field::Valid(number) => {
                        out_elision.insert(key.into(), number);
                    }
                };
            }
            out.insert("toolResultElision".into(), Value::Object(out_elision));
        }
    }
    Field::Valid(Value::Object(out))
}

fn read_skills(raw: Option<&Value>) -> Field<Value> {
    let Some(value) = raw else {
        return Field::Absent;
    };
    let Some(entries) = value.as_array() else {
        return Field::Malformed;
    };
    let mut skills = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(entry) = entry.as_object() else {
            return Field::Malformed;
        };
        let id = entry.get("id").and_then(Value::as_str);
        let revision = entry.get("revision").and_then(Value::as_str);
        let (Some(id), Some(revision)) = (id, revision) else {
            return Field::Malformed;
        };
        skills.push(Value::Object(Map::from_iter([
            ("id".to_owned(), Value::from(id)),
            ("revision".to_owned(), Value::from(revision)),
        ])));
    }
    Field::Valid(Value::Array(skills))
}

/// Whether `kind` may name a session's turn tasks: the default, or a host
/// kind in dotted lowercase form outside the reserved `chat.*` /
/// `agentkit.*` prefixes. Grammar is the TS `HOST_TASK_KIND` regex
/// `^[a-z][a-z0-9_-]*(\.[a-z0-9_-]+)*$`: the FIRST segment must begin with an
/// ASCII lowercase letter, later segments must be non-empty and drawn from
/// `[a-z0-9_-]` — so `project.1` and `project._worker` are legal host kinds.
pub fn is_valid_session_turn_kind(kind: &str) -> bool {
    if kind == CHAT_TURN_TASK_KIND {
        return true;
    }
    let is_host_char =
        |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
    let mut segments = kind.split('.');
    let first = segments.next().unwrap_or_default();
    let mut chars = first.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_lowercase()) || !chars.all(is_host_char) {
        return false;
    }
    if !segments.all(|segment| !segment.is_empty() && segment.chars().all(is_host_char)) {
        return false;
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
