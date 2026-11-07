//! AGENTKIT-89: the pinned effective run profile — check-at-USE
//! validation over the durable facts a pinned profile references.
//!
//! The profile document is RESOLVED by the host (defaults + agent
//! definition revision + project selection + task overrides into ONE
//! immutable profile) and PINNED by [`crate::SqliteStore::admit_teammate_task`]
//! inside the existing admission transaction — this module adds no second
//! ledger, no parallel provider registry, and no parallel authorization
//! evaluator. Every fact it verifies is read through the services that
//! own it:
//!
//! - provider registration and capability evidence: the existing
//!   [`crate::provider_catalog`] settings namespaces, validated by its own
//!   validators;
//! - keyed provider authority: the existing
//!   [`crate::provider_connections::validate_generation`] fence, called
//!   on the caller's transaction;
//! - pinned skill versions: the existing `managed_skills` rows
//!   (AGENTKIT-80s skills) — a deleted or re-published skill no longer
//!   matches its pinned revision and is revoked;
//! - source access: the agent's OWN definition resources (project
//!   selection never grants access to unrelated memory or files);
//! - approvals: the existing standing-grant machinery
//!   ([`crate::find_active_standing_grant`]) — a revoked grant fences
//!   dispatch.
//!
//! Profile grammar (v1, camelCase; the host resolver constructs it and
//! the store fails closed on anything else):
//!
//! ```text
//! { "version": 1, "agentId": s, "agentRevision": int>0,
//!   "provider": { "providerId": s, "modelId": s,
//!                 "connectionId"?: s, "generation"?: int>0,
//!                 "localOnly": bool, "fallbacks": [s] },
//!   "capability": { "toolCalling": bool, "vision": bool, "streaming": bool,
//!                   "contextWindowTokens": int>0, "continuationCompatible": bool },
//!   "skills": [{ "id": s, "revision": s }],
//!   "contextSources": [{ "id": s, "kind": "skill"|"resource",
//!                        "revision"?: s, "tokens"?: int>=0,
//!                        "truncated"?: bool, "citations"?: [s] }],
//!   "permissionCeiling": { "mode": <permission vocabulary>, "tools": [s] },
//!   "budgets": { <positive ints, AgentBudgets keys> },
//!   "environment": { "vars": [{ "name": s, "valueDigest": hex64 }] },
//!   "truncation"?: { "mode": "explicit", "droppedUnits"?: int>=0,
//!                    "toolPairsPreserved": bool },
//!   "provenance"?: { <field>: <"default"|"agent"|"task"> } }
//! ```
//!
//! Secrets have no lawful home here: `environment` records names and
//! value digests only, provider rows are never copied, and unknown keys
//! are refused as if `deny_unknown_fields` ran on the JSON.

use crate::{
    AgentRecord, ChatRecord, Result, StoreError, TaskRecord, find_active_standing_grant, util,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const PERMISSION_VOCABULARY: [&str; 5] = ["read_only", "ask", "auto_edit", "full_auto", "plan"];

fn refused(reason: impl std::fmt::Display) -> StoreError {
    StoreError::InvalidInput(format!("run_profile_refused:{reason}"))
}

/// Canonical (recursively key-sorted) serialization digest of a profile
/// document. The single digest authority: the resolver seals with it, the
/// store recomputes it on every read, and a seal that does not match the
/// bytes fails closed.
pub fn run_profile_digest(profile: &Value) -> String {
    let canonical = canonicalize(profile);
    // Value serialization cannot fail; sorting keys makes it order-stable.
    let text = serde_json::to_string(&canonical).expect("canonical profile serialization");
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                out.insert(key.clone(), canonicalize(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

fn is_hex64(value: &Value) -> bool {
    value.as_str().is_some_and(|text| {
        text.len() == 64
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn str_field<'a>(object: &'a Value, key: &str) -> Result<&'a str> {
    object[key]
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| refused(format!("{key} must be a non-empty string")))
}

fn opt_str_field<'a>(object: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match &object[key] {
        Value::Null => Ok(None),
        value if value.is_string() => str_field(object, key).map(Some),
        _ => Err(refused(format!("{key} must be a string when present"))),
    }
}

fn positive_int(value: &Value, key: &str) -> Result<i64> {
    value
        .as_i64()
        .filter(|value| *value > 0 && *value <= 9_007_199_254_740_991)
        .ok_or_else(|| refused(format!("{key} must be a positive safe integer")))
}

fn bool_field(object: &Value, key: &str) -> Result<bool> {
    object[key]
        .as_bool()
        .ok_or_else(|| refused(format!("{key} must be a boolean")))
}

fn string_array(value: &Value, key: &str) -> Result<Vec<String>> {
    value
        .as_array()
        .filter(|items| {
            items
                .iter()
                .all(|item| item.as_str().is_some_and(|text| !text.is_empty()))
        })
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().expect("validated string").to_owned())
                .collect()
        })
        .ok_or_else(|| refused(format!("{key} must be an array of non-empty strings")))
}

/// The durable references a profile pins, extracted under the grammar.
pub(crate) struct ProfileRefs<'a> {
    pub agent_id: &'a str,
    pub agent_revision: i64,
    pub provider_id: &'a str,
    /// `provider.authority == "host"`: the embedding host owns the
    /// provider registry and re-checks it at execution.
    pub host_authority: bool,
    pub connection_id: Option<&'a str>,
    pub generation: Option<i64>,
    pub skills: Vec<(&'a str, &'a str)>,
    pub resource_sources: Vec<&'a str>,
    pub approvals: Vec<(&'a str, &'a str, &'a str)>,
}

/// Parse the grammar. Anything outside it is refused — the profile is a
/// sealed document, and a malformed seal is a failed seal, not a partial
/// read.
fn profile_refs(profile: &Value) -> Result<ProfileRefs<'_>> {
    let object = profile
        .as_object()
        .ok_or_else(|| refused("profile must be an object"))?;
    if object.keys().any(|key| {
        ![
            "version",
            "agentId",
            "agentRevision",
            "provider",
            "capability",
            "skills",
            "contextSources",
            "compiledContext",
            "compilationPolicy",
            "permissionCeiling",
            "budgets",
            "environment",
            "truncation",
            "provenance",
            "approvals",
        ]
        .contains(&key.as_str())
    }) {
        return Err(refused("unknown profile field"));
    }
    if profile["version"].as_i64() != Some(1) {
        return Err(refused("only profile version 1 exists"));
    }
    let agent_id = str_field(profile, "agentId")?;
    let agent_revision = positive_int(&profile["agentRevision"], "agentRevision")?;

    let provider = &profile["provider"];
    let provider_id = str_field(provider, "providerId")?;
    let _ = str_field(provider, "modelId")?;
    // A missing authority means the catalogue (backward compatible).
    let host_authority = match &provider["authority"] {
        Value::Null => false,
        Value::String(text) if text == "catalog" => false,
        Value::String(text) if text == "host" => true,
        _ => return Err(refused("provider.authority must be catalog or host")),
    };
    let connection_id = opt_str_field(provider, "connectionId")?;
    let generation = match &provider["generation"] {
        Value::Null => None,
        value => Some(positive_int(value, "provider.generation")?),
    };
    let _ = bool_field(provider, "localOnly")?;
    let _ = string_array(&provider["fallbacks"], "provider.fallbacks")?;

    let capability = &profile["capability"];
    let _ = bool_field(capability, "toolCalling")?;
    let _ = bool_field(capability, "vision")?;
    let _ = bool_field(capability, "streaming")?;
    let _ = bool_field(capability, "continuationCompatible")?;
    let _ = positive_int(
        &capability["contextWindowTokens"],
        "capability.contextWindowTokens",
    )?;

    let mut skills: Vec<(&str, &str)> = Vec::new();
    for entry in profile["skills"]
        .as_array()
        .ok_or_else(|| refused("skills must be an array"))?
    {
        skills.push((str_field(entry, "id")?, str_field(entry, "revision")?));
    }

    let mut resource_sources: Vec<&str> = Vec::new();
    let sources = profile["contextSources"]
        .as_array()
        .ok_or_else(|| refused("contextSources must be an array"))?;
    for source in sources {
        let id = str_field(source, "id")?;
        match source["kind"].as_str() {
            Some("skill") => {
                skills.push((id, str_field(source, "revision")?));
            }
            Some("resource") => resource_sources.push(id),
            _ => return Err(refused("contextSources kind must be skill or resource")),
        }
        let tokens = &source["tokens"];
        if !tokens.is_null()
            && !tokens
                .as_u64()
                .is_some_and(|value| value <= 9_007_199_254_740_991)
        {
            return Err(refused("contextSources tokens must be a safe integer"));
        }
        let truncated = &source["truncated"];
        if !truncated.is_null() && !truncated.is_boolean() {
            return Err(refused("contextSources truncated must be a boolean"));
        }
        if !source["contentDigest"].is_null() && !is_hex64(&source["contentDigest"]) {
            return Err(refused(
                "contextSources contentDigest must be 64-char lowercase hex",
            ));
        }
        let citations = &source["citations"];
        if !citations.is_null() {
            let _ = string_array(citations, "contextSources citations")?;
        }
    }

    let ceiling = &profile["permissionCeiling"];
    ceiling["mode"]
        .as_str()
        .filter(|mode| PERMISSION_VOCABULARY.contains(mode))
        .ok_or_else(|| refused("permissionCeiling mode is outside the permission vocabulary"))?;
    let _ = string_array(&ceiling["tools"], "permissionCeiling tools")?;

    let budgets = profile["budgets"]
        .as_object()
        .ok_or_else(|| refused("budgets must be an object"))?;
    for (key, value) in budgets {
        if !value.is_null() {
            let _ = positive_int(value, &format!("budgets.{key}"))?;
        }
    }

    let vars = profile["environment"]["vars"]
        .as_array()
        .ok_or_else(|| refused("environment.vars must be an array"))?;
    for entry in vars {
        let _ = str_field(entry, "name")?;
        if !is_hex64(&entry["valueDigest"]) {
            return Err(refused(
                "environment valueDigest must be 64-char lowercase hex",
            ));
        }
        if entry.as_object().is_some_and(|entry| {
            entry
                .keys()
                .any(|key| key != "name" && key != "valueDigest")
        }) {
            // A value smuggled beside the digest is exactly what the
            // grammar refuses: environment records NAMES and digests only.
            return Err(refused(
                "environment vars carry no material beyond name and valueDigest",
            ));
        }
    }

    let truncation = &profile["truncation"];
    if !truncation.is_null() {
        if truncation["mode"].as_str() != Some("explicit") {
            return Err(refused("truncation mode must be explicit"));
        }
        if truncation["droppedUnits"].is_number()
            && !truncation["droppedUnits"]
                .as_u64()
                .is_some_and(|value| value <= 9_007_199_254_740_991)
        {
            return Err(refused("truncation droppedUnits must be a safe integer"));
        }
        if !truncation["toolPairsPreserved"].is_boolean() {
            return Err(refused("truncation toolPairsPreserved must be a boolean"));
        }
    }

    let provenance = &profile["provenance"];
    if !provenance.is_null() {
        let provenance = provenance
            .as_object()
            .ok_or_else(|| refused("provenance must be an object"))?;
        for (key, value) in provenance {
            if !value.is_null() && !value.is_string() {
                return Err(refused(format!("provenance.{key} must be a string")));
            }
        }
    }

    let mut approvals: Vec<(&str, &str, &str)> = Vec::new();
    let grants = &profile["approvals"];
    if !grants.is_null() {
        let grants = grants
            .as_array()
            .ok_or_else(|| refused("approvals must be an array"))?;
        for grant in grants {
            let session_id = str_field(grant, "sessionId")?;
            let grant_id = str_field(grant, "grantId")?;
            let digest = str_field(grant, "digest")?;
            if !is_hex64(&grant["digest"]) {
                return Err(refused("approval digest must be 64-char lowercase hex"));
            }
            approvals.push((session_id, grant_id, digest));
        }
    }

    Ok(ProfileRefs {
        agent_id,
        agent_revision,
        provider_id,
        host_authority,
        connection_id,
        generation,
        skills,
        resource_sources,
        approvals,
    })
}

fn setting(db: &Connection, namespace: &str, key: &str) -> Result<Option<Value>> {
    db.query_row(
        "SELECT value FROM settings WHERE namespace=? AND key=?",
        params![namespace, key],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|text| crate::provider_catalog::parse_setting(&text, namespace))
    .transpose()
}

fn skill_revision(db: &Connection, id: &str) -> Result<Option<String>> {
    let Some((source, path)) = id.split_once(':') else {
        return Ok(None);
    };
    if source.is_empty() || path.is_empty() {
        return Ok(None);
    }
    db.query_row(
        "SELECT revision FROM managed_skills WHERE source_id=?1 AND path=?2",
        params![source, path],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(Into::into)
}

/// The check-at-USE over a sealed profile, on the caller's transaction.
/// Every reference is revalidated against the CURRENT durable fact:
/// nothing here trusts the pin to mean the reference still exists.
///
/// A profile with `provider.authority == "host"` skips the catalogue
/// provider/capability/connection-generation checks: the embedding host
/// owns that registry and re-checks it at execution. Digest, agent
/// binding, skills, resource sources and approvals are still verified.
/// A missing `authority` means the catalogue.
pub(crate) fn verify_run_profile(
    db: &Connection,
    bound_agent_id: Option<&str>,
    pin: &crate::PinnedRunProfile,
) -> Result<()> {
    if run_profile_digest(&pin.profile) != pin.digest {
        return Err(refused("digest_mismatch"));
    }
    let refs = profile_refs(&pin.profile)?;
    if bound_agent_id != Some(refs.agent_id) {
        return Err(refused("agent_binding_mismatch"));
    }
    if !refs.host_authority {
        // The provider must still be registered and enabled — deletion of a
        // referenced provider blocks new dispatch even for a pinned profile.
        let provider = setting(db, crate::PROVIDERS_NAMESPACE, refs.provider_id)?
            .ok_or_else(|| refused(format!("provider_deleted:{}", refs.provider_id)))?;
        if !crate::provider_catalog::provider(&provider)
            || provider["enabled"].as_bool() != Some(true)
        {
            return Err(refused(format!("provider_revoked:{}", refs.provider_id)));
        }
        // Capability evidence is admission-conditional; its revocation fences
        // dispatch too — fail closed, never re-probe a provider at dispatch.
        let capabilities = setting(db, crate::PROVIDER_CAPABILITIES_NAMESPACE, refs.provider_id)?
            .ok_or_else(|| {
            refused(format!(
                "provider_capabilities_revoked:{}",
                refs.provider_id
            ))
        })?;
        if !crate::provider_catalog::capabilities(&capabilities) {
            return Err(refused(format!(
                "provider_capabilities_revoked:{}",
                refs.provider_id
            )));
        }
        // The EXISTING keyed-authority fence: a revoked connection (or a
        // rotated credential advancing the generation) fails closed here,
        // inside the caller's transaction.
        crate::provider_connections::validate_generation(
            db,
            &json!({ "connectionId": refs.connection_id, "generation": refs.generation }),
        )?;
    }
    // Pinned skill versions must still be exactly what was pinned: a
    // deleted or re-published skill is a revoked context source.
    for (id, revision) in &refs.skills {
        let stored = skill_revision(db, id)?;
        if stored.as_deref() != Some(*revision) {
            return Err(refused(format!("skill_revoked:{id}")));
        }
    }
    // Resource sources are granted by the agent's own definition, and the
    // check runs on the CURRENT definition: removing a resource revokes
    // it at use. (A deleted agent is absence, exactly like the 88 gate.)
    let agent: Option<AgentRecord> = util::get(db, "agents", "agent_id", refs.agent_id)?;
    let Some(agent) = agent else {
        return Err(StoreError::NotFound(format!("agent {}", refs.agent_id)));
    };
    for id in &refs.resource_sources {
        if !agent
            .definition
            .resources
            .iter()
            .any(|granted| granted == id)
        {
            return Err(refused(format!("source_revoked:{id}")));
        }
    }
    // Approvals: the standing-grant authority answers at use. A revoked
    // (or vanished) grant fences dispatch; nothing caches the verdict.
    for (session_id, grant_id, digest) in &refs.approvals {
        let chat: Option<ChatRecord> = util::get(db, "chats", "id", session_id)?;
        let active = chat
            .as_ref()
            .and_then(|chat| find_active_standing_grant(&chat.metadata, digest));
        match active {
            Some(grant) if grant.get("grantId").and_then(Value::as_str) == Some(*grant_id) => {}
            _ => return Err(refused(format!("approval_revoked:{grant_id}"))),
        }
    }
    Ok(())
}

/// True when a task's pinned profile no longer verifies — its dispatch is
/// fenced. An unbound or un-pinned task is never fenced.
pub(crate) fn dispatch_fenced(db: &Connection, task: &TaskRecord) -> bool {
    task.run_profile
        .as_ref()
        .is_some_and(|pin| verify_run_profile(db, task.agent_id.as_deref(), pin).is_err())
}

/// The admission-time validation, INSIDE the admission transaction: the
/// pin seals a grammar-valid document bound to THIS agent at the CURRENT
/// definition revision (whose durable provenance row must exist), and
/// every reference verifies against the facts as they stand right now.
pub(crate) fn validate_at_admission(
    db: &Connection,
    agent: &AgentRecord,
    pin: Option<&crate::PinnedRunProfile>,
) -> Result<()> {
    let Some(pin) = pin else {
        return Ok(());
    };
    verify_run_profile(db, Some(&agent.agent_id), pin)?;
    let refs = profile_refs(&pin.profile)?;
    if refs.agent_revision != agent.current_revision {
        // The definition moved between resolution and admission: the
        // caller resolves again against the newer revision. Model changes
        // apply to FUTURE attempts — never a stale pin.
        return Err(refused("stale_revision"));
    }
    let sealed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_config_revisions WHERE agent_id=?1 AND revision=?2)",
        params![agent.agent_id, refs.agent_revision],
        |row| row.get(0),
    )?;
    if !sealed {
        return Err(refused("revision_provenance_missing"));
    }
    Ok(())
}

impl crate::SqliteStore {
    /// The safe dispatch verdict for one task's pinned profile: `None`
    /// when nothing fences the next dispatch (or no profile is pinned),
    /// otherwise the stable refusal code. Reads through the same check-at-
    /// use path `claim` runs — one evaluator, two readers.
    pub async fn run_profile_dispatch_refusal(&self, task_id: String) -> Result<Option<String>> {
        self.call(move |db| {
            let task: TaskRecord = util::required(db, "tasks", "id", &task_id)?;
            let Some(pin) = task.run_profile.as_ref() else {
                return Ok(None);
            };
            Ok(verify_run_profile(db, task.agent_id.as_deref(), pin)
                .err()
                .map(|error| error.to_string()))
        })
        .await
    }
}
