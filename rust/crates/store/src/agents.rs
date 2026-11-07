//! Persistent agent identity, definition revisions, and lifecycle
//! (AGENTKIT-87, schema version 14).
//!
//! One row per HOST-OWNED agent: an immutable `agent_id`, the owning
//! `owner_id`/`scope`, the effective configuration (display metadata,
//! purpose, instructions, scoped resources, selected skills, model
//! preference, permission ceiling, budgets), a monotonically increasing
//! `current_revision`, and a lifecycle state. This table is the durable
//! definition of WHAT an agent is — it is deliberately separate from the
//! sessions, tasks, and run attempts that EXECUTE under a definition; an
//! agent's definition revisions are configuration history, never task or
//! attempt history.
//!
//! Every configuration change appends one row to `agent_config_revisions`:
//! an append-only ledger of full effective-definition snapshots keyed by
//! `(agent_id, revision)` with the `source` that produced it
//! (`create | edit | model_change | restore`). The current definition and
//! its newest snapshot are written in ONE transaction, so a torn write can
//! never leave an agent pointing at a revision that was not recorded (or a
//! recorded revision with no agent). Reads that follow the ledger always
//! see the pair.
//!
//! Lifecycle is exactly three CHECK-constrained states. `deleted` is a
//! TOMBSTONE, not an erase: it preserves the identity (the id stays
//! reserved and a create with it is a duplicate), the configuration, and
//! the whole revision ledger for audit, and it reads as absence everywhere.
//! No secret VALUES live in these rows: the definition has no field that
//! could carry one, and credential material enters only as opaque
//! `secretRefs` entries — scheme-qualified references resolved elsewhere.
//! Duplicate strips them by default, and export never carries them.
//!
//! Concurrency: updates carry the `expected_revision` the caller read; a
//! stale expectation answers [`StoreError::Conflict`] and applies nothing,
//! so concurrent editors converge through explicit re-read-and-retry
//! instead of last-writer-wins overwrites.

use crate::{Result, SqliteStore, StoreError, util};
use agentkit_contracts::JsonDeserialize;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// The envelope format this build exports and imports. An envelope names
/// its version so a future format is refused with a useful error instead
/// of half-parsed; a reader never silently accepts bytes it cannot prove
/// it understands.
pub const AGENT_EXPORT_FORMAT_VERSION: i64 = 1;
/// Hard byte cap on one import envelope: bounded input is a precondition
/// of validation, not an optimization.
pub const MAX_AGENT_IMPORT_BYTES: usize = 8 * 1024 * 1024;
/// Hard agent-count cap on one import envelope.
pub const MAX_AGENT_IMPORT_AGENTS: usize = 512;

const MAX_ID_CHARS: usize = 128;
const MAX_SCOPE_CHARS: usize = 256;
const MAX_NAME_CHARS: usize = 256;
const MAX_AVATAR_CHARS: usize = 2048;
const MAX_PURPOSE_CHARS: usize = 8 * 1024;
const MAX_INSTRUCTIONS_CHARS: usize = 128 * 1024;
const MAX_MODEL_CHARS: usize = 256;
const MAX_EFFORT_CHARS: usize = 64;
const MAX_PERMISSION_MODE_CHARS: usize = 64;
const MAX_RESOURCE_CHARS: usize = 1024;
const MAX_RESOURCE_COUNT: usize = 512;
const MAX_TOOL_CHARS: usize = 128;
const MAX_TOOL_COUNT: usize = 512;
const MAX_SKILL_ID_CHARS: usize = 256;
const MAX_SKILL_COUNT: usize = 256;
const MAX_SECRET_REF_CHARS: usize = 512;
const MAX_SECRET_REF_COUNT: usize = 64;
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// The complete lifecycle vocabulary. `Deleted` is a tombstone: identity,
/// configuration, and revision ledger stay durable while every read path
/// treats the row as absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentLifecycle {
    Active,
    Archived,
    Deleted,
}

/// What produced one configuration revision. The ledger vocabulary is
/// closed: `create` is written only by identity creation (including a
/// duplicate's fresh first revision), `model_change` only by a model
/// preference change, `restore` only by an archive→active restore, `edit`
/// by every other accepted definition update.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRevisionSource {
    Create,
    Edit,
    ModelChange,
    Restore,
}

/// One selected skill under an immutable revision, the same
/// `{id, revision}` pairing the managed-content store keys published
/// skill documents by.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSkillSelection {
    pub id: String,
    pub revision: String,
}

/// Budget ceilings of a definition. Every present limit is a positive
/// safe integer; absence means "this ceiling is unset, the host default
/// governs".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_iterations: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls_per_iteration: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_budget_ms: Option<i64>,
}

/// The effective configuration of one agent — the snapshot unit the
/// revision ledger appends. `deny_unknown_fields` is the smuggling gate:
/// there is no field that could carry a secret value or a foreign scope,
/// and an envelope or payload that tries answers with a validation error.
/// Credential material enters only through `secret_refs`: opaque,
/// scheme-qualified references resolved by the secret store, never values.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub instructions: String,
    /// Scoped resource references this agent may use.
    #[serde(default)]
    pub resources: Vec<String>,
    /// Skills selected under pinned revisions.
    #[serde(default)]
    pub skills: Vec<AgentSkillSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// The permission CEILING (`read_only|ask|auto_edit|full_auto|plan`,
    /// the core permission vocabulary the host service validates); a
    /// ceiling narrows what a run under this agent may ever attempt, it
    /// grants nothing by itself.
    #[serde(default)]
    pub permission_mode: String,
    /// Tool allow-list ceiling; empty means the host default set.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub budgets: AgentBudgets,
    /// Secret REFERENCES only (e.g. `vault://...`); a value here is refused
    /// at write time. Duplicate strips these by default.
    #[serde(default)]
    pub secret_refs: Vec<String>,
}

agentkit_contracts::impl_json_via_serde!(AgentDefinition);

/// One durable agent row: identity, lifecycle, the current effective
/// definition, and the timestamps. Serialized whole into `agents.record`;
/// the indexed columns mirror exactly these fields at write time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRecord {
    pub agent_id: String,
    pub scope: String,
    pub owner_id: String,
    pub lifecycle: AgentLifecycle,
    pub current_revision: i64,
    pub definition: AgentDefinition,
    pub created_at: i64,
    pub updated_at: i64,
}

agentkit_contracts::impl_json_via_serde!(AgentRecord);

/// One row of the append-only configuration ledger: the full effective
/// definition snapshot at `revision`, with the source that produced it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRevision {
    pub agent_id: String,
    pub revision: i64,
    pub snapshot: AgentDefinition,
    pub source: AgentRevisionSource,
    pub created_at: i64,
}

agentkit_contracts::impl_json_via_serde!(AgentRevision);

/// The create payload: identity fields plus the definition. `agent_id`
/// may be supplied to pin a deterministic identity (a duplicate or an
/// import reuses the shared insert); `None` mints a fresh id.
#[derive(Clone, Debug)]
pub struct CreateAgent {
    pub agent_id: Option<String>,
    pub scope: String,
    pub owner_id: String,
    pub definition: AgentDefinition,
    pub now: i64,
}

/// The optimistic-concurrency update payload: the caller must name the
/// revision it read. A stale `expected_revision` conflicts, it never
/// overwrites.
#[derive(Clone, Debug)]
pub struct UpdateAgent {
    pub agent_id: String,
    pub expected_revision: i64,
    pub definition: AgentDefinition,
    pub source: AgentRevisionSource,
    pub now: i64,
}

/// The model-preference change payload: identity and history are
/// untouched, only the model fields of the effective definition move, and
/// the append carries `model_change` provenance.
#[derive(Clone, Debug)]
pub struct SetAgentModel {
    pub agent_id: String,
    pub expected_revision: i64,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub now: i64,
}

/// The duplicate payload: a NEW identity in the source's scope. The copy
/// carries display metadata, purpose, instructions, resources, skills,
/// model preference, permission ceiling, and budgets — and by
/// construction NOTHING ELSE: credentials (secret references), grants,
/// and private memory live outside the definition and never ride along.
#[derive(Clone, Debug)]
pub struct DuplicateAgent {
    pub agent_id: String,
    /// Optional pinned identity for the copy (imports/tests); `None` mints
    /// a fresh id.
    pub new_agent_id: Option<String>,
    /// Reattributes the copy; `None` keeps the source owner.
    pub owner_id: Option<String>,
    pub now: i64,
}

/// What an explicit delete would retain and what stands in its way.
/// Callers preview before deleting; the delete itself re-checks the same
/// evidence inside the tombstone transaction, so a preview can never
/// authorize a delete the durable state would refuse.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDeletionPreview {
    pub agent_id: String,
    pub scope: String,
    pub owner_id: String,
    pub lifecycle: AgentLifecycle,
    pub current_revision: i64,
    /// Revision ledger rows that survive the tombstone (deletion never
    /// erases configuration history).
    pub revision_count: i64,
    /// Admitted-without-result execution receipts currently held in the
    /// receipt store. Any number above zero refuses the delete.
    pub unresolved_executions: i64,
    pub deletable: bool,
}

/// One exported agent: the definition, the ledger, and the identity
/// columns. No secret reference ever lands in an entry (export strips
/// them), and the scope travels as a declared field an import verifies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentExportEntry {
    pub agent_id: String,
    pub scope: String,
    pub owner_id: String,
    pub lifecycle: AgentLifecycle,
    pub created_at: i64,
    pub updated_at: i64,
    pub definition: AgentDefinition,
    pub revisions: Vec<AgentRevisionExport>,
}

/// One ledger row as exported. `currentRevision` is deliberately NOT an
/// envelope field: it is derived as the ledger length, so an envelope
/// cannot claim a current revision the appended snapshots do not support.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentRevisionExport {
    pub revision: i64,
    pub snapshot: AgentDefinition,
    pub source: AgentRevisionSource,
    pub created_at: i64,
}

/// The bounded, versioned import/export envelope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentExportEnvelope {
    pub format_version: i64,
    pub agents: Vec<AgentExportEntry>,
}

agentkit_contracts::impl_json_via_serde!(AgentExportEnvelope);

fn invalid(reason: impl Into<String>) -> StoreError {
    StoreError::InvalidInput(format!("invalid_agent: {}", reason.into()))
}

fn too_long(field: &str, max: usize) -> StoreError {
    invalid(format!("{field} exceeds {max} characters"))
}

fn bounded_text(field: &str, value: &str, max: usize, required: bool) -> Result<()> {
    if value.is_empty() {
        if required {
            return Err(invalid(format!("{field} must not be empty")));
        }
        return Ok(());
    }
    if value.chars().count() > max {
        return Err(too_long(field, max));
    }
    Ok(())
}

fn unique_strings(field: &str, values: &[String], max_each: usize, max_count: usize) -> Result<()> {
    if values.len() > max_count {
        return Err(invalid(format!(
            "{field} exceeds the {max_count}-entry bound"
        )));
    }
    let mut seen = HashSet::with_capacity(values.len());
    for value in values {
        bounded_text(field, value, max_each, true)?;
        if !seen.insert(value) {
            return Err(invalid(format!("{field} contains a duplicate entry")));
        }
    }
    Ok(())
}

/// Store-side structural validation: bounds, shapes, and the
/// reference-not-value discipline on secret entries. Semantic vocabulary
/// (permission modes, reasoning efforts) is the host service's layer; the
/// store's CHECKs and this gate stand underneath it so no caller — host
/// or raw store — lands an unbounded or credential-shaped payload.
fn validate_definition(definition: &AgentDefinition) -> Result<()> {
    bounded_text("name", &definition.name, MAX_NAME_CHARS, true)?;
    if definition.name.trim().is_empty() {
        return Err(invalid("name must not be blank"));
    }
    if let Some(avatar) = &definition.avatar {
        bounded_text("avatar", avatar, MAX_AVATAR_CHARS, true)?;
    }
    bounded_text("purpose", &definition.purpose, MAX_PURPOSE_CHARS, false)?;
    bounded_text(
        "instructions",
        &definition.instructions,
        MAX_INSTRUCTIONS_CHARS,
        false,
    )?;
    if let Some(model) = &definition.model {
        bounded_text("model", model, MAX_MODEL_CHARS, true)?;
    }
    if let Some(effort) = &definition.reasoning_effort {
        bounded_text("reasoningEffort", effort, MAX_EFFORT_CHARS, true)?;
    }
    bounded_text(
        "permissionMode",
        &definition.permission_mode,
        MAX_PERMISSION_MODE_CHARS,
        false,
    )?;
    unique_strings(
        "resources",
        &definition.resources,
        MAX_RESOURCE_CHARS,
        MAX_RESOURCE_COUNT,
    )?;
    unique_strings(
        "allowedTools",
        &definition.allowed_tools,
        MAX_TOOL_CHARS,
        MAX_TOOL_COUNT,
    )?;
    if definition.skills.len() > MAX_SKILL_COUNT {
        return Err(invalid(format!(
            "skills exceed the {MAX_SKILL_COUNT}-entry bound"
        )));
    }
    let mut seen = HashSet::with_capacity(definition.skills.len());
    for skill in &definition.skills {
        bounded_text("skill.id", &skill.id, MAX_SKILL_ID_CHARS, true)?;
        bounded_text("skill.revision", &skill.revision, MAX_SKILL_ID_CHARS, true)?;
        if !seen.insert((skill.id.as_str(), skill.revision.as_str())) {
            return Err(invalid("skills contain a duplicate selection"));
        }
    }
    for limit in [
        definition.budgets.max_tool_iterations,
        definition.budgets.max_tool_calls_per_iteration,
        definition.budgets.max_output_tokens,
        definition.budgets.attempt_budget_ms,
    ]
    .into_iter()
    .flatten()
    {
        if limit <= 0 || limit > MAX_SAFE_INTEGER {
            return Err(invalid(
                "budget limits must be positive safe integers (1..=2^53-1)",
            ));
        }
    }
    unique_strings(
        "secretRefs",
        &definition.secret_refs,
        MAX_SECRET_REF_CHARS,
        MAX_SECRET_REF_COUNT,
    )?;
    for reference in &definition.secret_refs {
        // Reference-not-value discipline: an entry must be scheme-qualified
        // (`vault://...`, `env-name?opaque` is NOT a reference). A raw
        // secret has no scheme; refusing the shape refuses the value.
        if !reference.contains("://") {
            return Err(invalid(
                "secretRefs entries must be scheme-qualified references, never values",
            ));
        }
    }
    Ok(())
}

/// Identity-column validation shared by every write path.
fn validate_identity(scope: &str, owner_id: &str) -> Result<()> {
    bounded_text("scope", scope, MAX_SCOPE_CHARS, true)?;
    bounded_text("ownerId", owner_id, MAX_SCOPE_CHARS, true)?;
    Ok(())
}

fn validate_agent_id(agent_id: &str) -> Result<()> {
    bounded_text("agentId", agent_id, MAX_ID_CHARS, true)
}

fn read_record(db: &Connection, sql: &str, id: &str) -> Result<Option<AgentRecord>> {
    let json: Option<String> = db
        .query_row(sql, params![id], |row| row.get(0))
        .optional()?;
    json.map(|json| {
        agentkit_contracts::parse_javascript_json(&json)
            .and_then(AgentRecord::from_json)
            .map_err(StoreError::from)
    })
    .transpose()
}

/// Read the live row (tombstones read as absence: a deleted agent does
/// not exist for any caller).
fn select_agent(db: &Connection, agent_id: &str) -> Result<Option<AgentRecord>> {
    read_record(
        db,
        "SELECT record FROM agents WHERE agent_id=? AND lifecycle<>'deleted'",
        agent_id,
    )
}

/// Read including the tombstone — identity reservation only: create and
/// duplicate must collide with a deleted id, never resurrect it.
fn select_any(db: &Connection, agent_id: &str) -> Result<Option<AgentRecord>> {
    read_record(db, "SELECT record FROM agents WHERE agent_id=?", agent_id)
}

fn required_agent(db: &Connection, agent_id: &str) -> Result<AgentRecord> {
    select_agent(db, agent_id)?.ok_or_else(|| StoreError::NotFound(format!("agent {agent_id}")))
}

fn list_records(db: &Connection, sql: &str, arg: &str) -> Result<Vec<AgentRecord>> {
    let mut statement = db.prepare(sql)?;
    let rows = statement.query_map([arg], |row| row.get::<_, String>(0))?;
    rows.map(|row| {
        agentkit_contracts::parse_javascript_json(&row?)
            .and_then(AgentRecord::from_json)
            .map_err(StoreError::from)
    })
    .collect()
}

/// The ledger read: full snapshots in revision order. A malformed ledger
/// row is a hard error, never a silent omission — the ledger IS the audit
/// trail, so dropping a row would fabricate absence.
fn select_revisions(db: &Connection, agent_id: &str) -> Result<Vec<AgentRevision>> {
    let mut statement = db.prepare(
        "SELECT revision, snapshot, source, created_at FROM agent_config_revisions \
         WHERE agent_id=? ORDER BY revision",
    )?;
    let rows = statement.query_map([agent_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    rows.map(|row| {
        let (revision, snapshot, source, created_at) = row?;
        let snapshot: AgentDefinition = agentkit_contracts::parse_javascript_json(&snapshot)
            .and_then(AgentDefinition::from_json)
            .map_err(StoreError::from)?;
        let source = match source.as_str() {
            "create" => AgentRevisionSource::Create,
            "edit" => AgentRevisionSource::Edit,
            "model_change" => AgentRevisionSource::ModelChange,
            "restore" => AgentRevisionSource::Restore,
            other => {
                return Err(StoreError::RecoveryUnreadable(format!(
                    "agent {agent_id} revision {revision} carries unknown source {other:?}"
                )));
            }
        };
        Ok(AgentRevision {
            agent_id: agent_id.to_owned(),
            revision,
            snapshot,
            source,
            created_at,
        })
    })
    .collect()
}

/// One insert path shared by create, duplicate, and import: the agent row
/// and its first ledger snapshot commit together or not at all.
fn insert_agent_pair(
    db: &Connection,
    record: &AgentRecord,
    source: AgentRevisionSource,
) -> Result<()> {
    let source_text = match source {
        AgentRevisionSource::Create => "create",
        AgentRevisionSource::Edit => "edit",
        AgentRevisionSource::ModelChange => "model_change",
        AgentRevisionSource::Restore => "restore",
    };
    db.execute(
        "INSERT INTO agents(agent_id,scope,owner_id,lifecycle,current_revision,record,created_at,updated_at) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            record.agent_id,
            record.scope,
            record.owner_id,
            serde_name(record.lifecycle),
            record.current_revision,
            util::json(db, record)?,
            record.created_at,
            record.updated_at,
        ],
    )?;
    let snapshot = util::json(db, &record.definition)?;
    db.execute(
        "INSERT INTO agent_config_revisions(agent_id,revision,snapshot,source,created_at) \
         VALUES(?1,?2,?3,?4,?5)",
        params![
            record.agent_id,
            record.current_revision,
            snapshot,
            source_text,
            record.updated_at
        ],
    )?;
    Ok(())
}

fn serde_name(lifecycle: AgentLifecycle) -> &'static str {
    match lifecycle {
        AgentLifecycle::Active => "active",
        AgentLifecycle::Archived => "archived",
        AgentLifecycle::Deleted => "deleted",
    }
}

/// Optimistic-concurrency guard shared by every revisioned mutation: the
/// row must exist live, be ACTIVE for configuration changes, and still
/// carry the revision the caller read. Anything else is a typed refusal:
/// absence, an archived ceiling, or an explicit [`StoreError::Conflict`].
fn require_revision(
    db: &Connection,
    agent_id: &str,
    expected_revision: i64,
    action: &str,
) -> Result<AgentRecord> {
    let current = required_agent(db, agent_id)?;
    if current.lifecycle != AgentLifecycle::Active {
        return Err(StoreError::InvalidTransition(format!(
            "agent {agent_id} is {}; {action} is refused",
            serde_name(current.lifecycle)
        )));
    }
    if current.current_revision != expected_revision {
        return Err(StoreError::Conflict(format!(
            "agent {agent_id}: expected revision {expected_revision}, current is {}",
            current.current_revision
        )));
    }
    Ok(current)
}

/// Append the next revision and repoint the agent row: the pair write
/// every revisioned mutation funnels through, executed inside the
/// caller's already-open transaction.
fn append_revision(
    db: &Connection,
    current: &AgentRecord,
    definition: AgentDefinition,
    source: AgentRevisionSource,
    now: i64,
) -> Result<AgentRecord> {
    let next = current.current_revision + 1;
    let updated = AgentRecord {
        lifecycle: AgentLifecycle::Active,
        current_revision: next,
        definition,
        updated_at: now,
        ..current.clone()
    };
    db.execute(
        "UPDATE agents SET current_revision=?2, record=?3, updated_at=?4 WHERE agent_id=?1",
        params![current.agent_id, next, util::json(db, &updated)?, now],
    )?;
    let source_text = match source {
        AgentRevisionSource::Create => "create",
        AgentRevisionSource::Edit => "edit",
        AgentRevisionSource::ModelChange => "model_change",
        AgentRevisionSource::Restore => "restore",
    };
    db.execute(
        "INSERT INTO agent_config_revisions(agent_id,revision,snapshot,source,created_at) \
         VALUES(?1,?2,?3,?4,?5)",
        params![
            current.agent_id,
            next,
            util::json(db, &updated.definition)?,
            source_text,
            now
        ],
    )?;
    Ok(updated)
}

fn duplicate_definition(definition: &AgentDefinition) -> AgentDefinition {
    AgentDefinition {
        // Credentials never ride along: references resolve in the SOURCE's
        // secret context, so the copy starts with none (grants and private
        // memory live outside the definition and cannot be copied by it).
        secret_refs: Vec::new(),
        ..definition.clone()
    }
}

/// The refusal shape for an explicit delete while the receipt store still
/// holds admitted-without-result executions: deletion of an agent must
/// never erase the evidence an active execution was admitted under. The
/// count is the receipt store's own admitted-status query — this module
/// never invents a second receipt notion.
fn delete_blocked(unresolved: i64) -> StoreError {
    StoreError::InvalidTransition(format!(
        "agent_delete_blocked_active_executions: {unresolved} unresolved execution receipt(s) \
         must complete or resolve first"
    ))
}

impl SqliteStore {
    /// Create a durable agent identity: one transaction writes the agent
    /// row AND its `create` revision snapshot, so a half-created identity
    /// cannot be observed. A re-used id (live or tombstoned) is a
    /// [`StoreError::Duplicate`], never a resurrection.
    pub async fn create_agent(&self, input: CreateAgent) -> Result<AgentRecord> {
        validate_identity(&input.scope, &input.owner_id)?;
        validate_definition(&input.definition)?;
        let agent_id = match &input.agent_id {
            Some(id) => {
                validate_agent_id(id)?;
                id.clone()
            }
            None => util::id(),
        };
        let record = AgentRecord {
            agent_id,
            scope: input.scope,
            owner_id: input.owner_id,
            lifecycle: AgentLifecycle::Active,
            current_revision: 1,
            definition: input.definition,
            created_at: input.now,
            updated_at: input.now,
        };
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            if select_any(&transaction, &record.agent_id)?.is_some() {
                return Err(StoreError::Duplicate(format!("agent {}", record.agent_id)));
            }
            insert_agent_pair(&transaction, &record, AgentRevisionSource::Create)?;
            transaction.commit()?;
            Ok(record)
        })
        .await
    }

    /// Read one live agent; tombstones read as absence.
    pub async fn get_agent(&self, agent_id: String) -> Result<Option<AgentRecord>> {
        self.call(move |db| select_agent(db, &agent_id)).await
    }

    /// List the live agents of one scope (active and archived; deleted
    /// tombstones never surface), oldest first.
    pub async fn list_agents(&self, scope: String) -> Result<Vec<AgentRecord>> {
        self.call(move |db| {
            list_records(
                db,
                "SELECT record FROM agents WHERE scope=? AND lifecycle<>'deleted' ORDER BY rowid",
                &scope,
            )
        })
        .await
    }

    /// The append-only configuration ledger in revision order. The ledger
    /// is audit data: a malformed row fails the read instead of
    /// presenting a shorter history.
    pub async fn list_agent_revisions(&self, agent_id: String) -> Result<Vec<AgentRevision>> {
        self.call(move |db| {
            required_agent(db, &agent_id)?;
            select_revisions(db, &agent_id)
        })
        .await
    }

    /// Replace the effective definition under optimistic concurrency.
    /// A stale `expected_revision` answers [`StoreError::Conflict`] with
    /// NOTHING applied — concurrent editors re-read and retry, they never
    /// silently overwrite each other. Archived agents refuse configuration
    /// changes; `create` is not an update source.
    pub async fn update_agent(&self, input: UpdateAgent) -> Result<AgentRecord> {
        if input.source == AgentRevisionSource::Create {
            return Err(invalid("create is not an update source"));
        }
        validate_definition(&input.definition)?;
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let current = require_revision(
                &transaction,
                &input.agent_id,
                input.expected_revision,
                "updates",
            )?;
            let updated = append_revision(
                &transaction,
                &current,
                input.definition,
                input.source,
                input.now,
            )?;
            transaction.commit()?;
            Ok(updated)
        })
        .await
    }

    /// Change the model preference. Identity and history are preserved by
    /// construction — the agent keeps its `agentId`, its scope, its whole
    /// ledger — and the append records `model_change` provenance over the
    /// validated snapshot. This is the only path that writes a
    /// `model_change` revision.
    pub async fn set_agent_model(&self, input: SetAgentModel) -> Result<AgentRecord> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let current = require_revision(
                &transaction,
                &input.agent_id,
                input.expected_revision,
                "model changes",
            )?;
            let mut definition = current.definition.clone();
            definition.model = input.model;
            definition.reasoning_effort = input.reasoning_effort;
            validate_definition(&definition)?;
            let updated = append_revision(
                &transaction,
                &current,
                definition,
                AgentRevisionSource::ModelChange,
                input.now,
            )?;
            transaction.commit()?;
            Ok(updated)
        })
        .await
    }

    /// Archive an agent: admission is refused from this moment (see
    /// [`SqliteStore::check_agent_admission`]), the row, its configuration,
    /// and its whole ledger stay durable. Idempotent: an already-archived
    /// agent answers with its unchanged record.
    pub async fn archive_agent(&self, agent_id: String, now: i64) -> Result<AgentRecord> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let current = required_agent(&transaction, &agent_id)?;
            if current.lifecycle == AgentLifecycle::Archived {
                transaction.commit()?;
                return Ok(current);
            }
            let updated = AgentRecord {
                lifecycle: AgentLifecycle::Archived,
                updated_at: now,
                ..current.clone()
            };
            transaction.execute(
                "UPDATE agents SET lifecycle='archived', record=?2, updated_at=?3 WHERE agent_id=?1",
                params![agent_id, util::json(&transaction, &updated)?, now],
            )?;
            transaction.commit()?;
            Ok(updated)
        })
        .await
    }

    /// Restore an archived agent to active. The transition is recorded in
    /// the ledger as a `restore` revision carrying the effective
    /// definition, so the lifecycle trajectory is auditable. Idempotent on
    /// an already-active agent.
    pub async fn restore_agent(&self, agent_id: String, now: i64) -> Result<AgentRecord> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let current = required_agent(&transaction, &agent_id)?;
            if current.lifecycle == AgentLifecycle::Active {
                transaction.commit()?;
                return Ok(current);
            }
            let updated = append_revision(
                &transaction,
                &current,
                current.definition.clone(),
                AgentRevisionSource::Restore,
                now,
            )?;
            // `append_revision` writes the row back ACTIVE with the ledger
            // pair; the lifecycle move IS the restore.
            transaction.commit()?;
            Ok(updated)
        })
        .await
    }

    /// The admission check every execution entry point must call before
    /// admitting new work under an agent: an archived agent refuses
    /// admission with [`StoreError::InvalidTransition`], an unknown or
    /// deleted one is [`StoreError::NotFound`], and an active agent hands
    /// back its current record for the caller to pin the run against.
    pub async fn check_agent_admission(&self, agent_id: String) -> Result<AgentRecord> {
        self.call(move |db| {
            let current = required_agent(db, &agent_id)?;
            if current.lifecycle == AgentLifecycle::Archived {
                return Err(StoreError::InvalidTransition(format!(
                    "agent_archived: {agent_id} refuses new admission"
                )));
            }
            Ok(current)
        })
        .await
    }

    /// Duplicate an agent under a NEW identity. The copy receives the
    /// source's display metadata, purpose, instructions, scoped resources,
    /// skill selections, model preference, permission ceiling, and budgets
    /// — and NOTHING ELSE by construction: secret references are stripped
    /// (a reference resolves in the source's secret context; carrying it
    /// would smuggle credential material into a new identity), and grants
    /// and private memory live outside the definition, so a definition
    /// projection cannot copy them even by accident. The copy starts its
    /// own ledger with one `create` revision; source history never
    /// transfers.
    pub async fn duplicate_agent(&self, input: DuplicateAgent) -> Result<AgentRecord> {
        let new_id = match &input.new_agent_id {
            Some(id) => {
                validate_agent_id(id)?;
                id.clone()
            }
            None => util::id(),
        };
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let source = required_agent(&transaction, &input.agent_id)?;
            if new_id == source.agent_id {
                return Err(StoreError::Duplicate(format!("agent {new_id}")));
            }
            if select_any(&transaction, &new_id)?.is_some() {
                return Err(StoreError::Duplicate(format!("agent {new_id}")));
            }
            let owner_id = input.owner_id.as_deref().unwrap_or(&source.owner_id);
            validate_identity(&source.scope, owner_id)?;
            let definition = duplicate_definition(&source.definition);
            validate_definition(&definition)?;
            let copy = AgentRecord {
                agent_id: new_id,
                scope: source.scope.clone(),
                owner_id: owner_id.to_owned(),
                lifecycle: AgentLifecycle::Active,
                current_revision: 1,
                definition,
                created_at: input.now,
                updated_at: input.now,
            };
            insert_agent_pair(&transaction, &copy, AgentRevisionSource::Create)?;
            transaction.commit()?;
            Ok(copy)
        })
        .await
    }

    /// Explicit delete: a retention-gated TOMBSTONE. Configuration history
    /// and identity are never erased — the row keeps its definition,
    /// ledger, and reserved id — but the delete refuses while the receipt
    /// store holds admitted-without-result executions, because deleting
    /// then would erase what an active execution was admitted under. The
    /// check runs on the receipt store's own admitted-status query inside
    /// the same transaction as the tombstone write, so a receipt admitted
    /// concurrently cannot slip past it.
    pub async fn delete_agent(&self, agent_id: String, now: i64) -> Result<AgentRecord> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let current = required_agent(&transaction, &agent_id)?;
            let unresolved = crate::execution_receipts::count_admitted(&transaction)?;
            if unresolved > 0 {
                return Err(delete_blocked(unresolved));
            }
            let updated = AgentRecord {
                lifecycle: AgentLifecycle::Deleted,
                updated_at: now,
                ..current.clone()
            };
            transaction.execute(
                "UPDATE agents SET lifecycle='deleted', record=?2, updated_at=?3 WHERE agent_id=?1",
                params![agent_id, util::json(&transaction, &updated)?, now],
            )?;
            transaction.commit()?;
            Ok(updated)
        })
        .await
    }

    /// Retention preview for an explicit delete: what survives (the whole
    /// ledger) and what refuses (unresolved execution receipts). The
    /// delete re-checks the same evidence itself; this is the read-only
    /// half callers present before asking for confirmation.
    pub async fn preview_agent_deletion(
        &self,
        agent_id: String,
    ) -> Result<Option<AgentDeletionPreview>> {
        self.call(move |db| {
            let Some(current) = select_agent(db, &agent_id)? else {
                return Ok(None);
            };
            let revision_count: i64 = db.query_row(
                "SELECT count(*) FROM agent_config_revisions WHERE agent_id=?",
                params![agent_id],
                |row| row.get(0),
            )?;
            let unresolved = crate::execution_receipts::count_admitted(db)?;
            Ok(Some(AgentDeletionPreview {
                agent_id: current.agent_id,
                scope: current.scope,
                owner_id: current.owner_id,
                lifecycle: current.lifecycle,
                current_revision: current.current_revision,
                revision_count,
                unresolved_executions: unresolved,
                deletable: unresolved == 0,
            }))
        })
        .await
    }

    /// Export a scope's live agents as a versioned, bounded envelope
    /// (definition + full ledger per agent). Secret references are
    /// stripped on the way out: an export leaves the secret context that
    /// gives a reference meaning, so what travels is configuration only.
    /// Deleted tombstones never export — a retained identity is host-local
    /// audit data.
    pub async fn export_agents(&self, scope: String) -> Result<String> {
        self.call(move |db| {
            let records = list_records(
                db,
                "SELECT record FROM agents WHERE scope=? AND lifecycle<>'deleted' ORDER BY rowid",
                &scope,
            )?;
            let mut agents = Vec::with_capacity(records.len());
            for record in records {
                let revisions = select_revisions(db, &record.agent_id)?;
                agents.push(AgentExportEntry {
                    agent_id: record.agent_id,
                    scope: record.scope,
                    owner_id: record.owner_id,
                    lifecycle: record.lifecycle,
                    created_at: record.created_at,
                    updated_at: record.updated_at,
                    definition: AgentDefinition {
                        secret_refs: Vec::new(),
                        ..record.definition
                    },
                    revisions: revisions
                        .into_iter()
                        .map(|revision| AgentRevisionExport {
                            revision: revision.revision,
                            snapshot: AgentDefinition {
                                secret_refs: Vec::new(),
                                ..revision.snapshot
                            },
                            source: revision.source,
                            created_at: revision.created_at,
                        })
                        .collect(),
                });
            }
            util::json(
                db,
                &AgentExportEnvelope {
                    format_version: AGENT_EXPORT_FORMAT_VERSION,
                    agents,
                },
            )
        })
        .await
    }

    /// Import a versioned envelope into one scope: bounded size, strict
    /// schema, contiguous ledger provenance, scope match against
    /// `expected_scope`, reserved-identity collision refusal, and a hard
    /// deny-all on credential material (no entry may carry a secret
    /// reference, so an envelope cannot smuggle one). Every row lands in
    /// ONE transaction: a rejected entry leaves no partial import.
    pub async fn import_agents(
        &self,
        envelope: String,
        expected_scope: String,
    ) -> Result<Vec<AgentRecord>> {
        if envelope.len() > MAX_AGENT_IMPORT_BYTES {
            return Err(StoreError::InvalidImport {
                reason: format!(
                    "envelope exceeds the {}-byte import bound",
                    MAX_AGENT_IMPORT_BYTES
                ),
            });
        }
        let envelope: AgentExportEnvelope = agentkit_contracts::parse_javascript_json(&envelope)
            .and_then(AgentExportEnvelope::from_json)
            .map_err(|error| StoreError::InvalidImport {
                reason: format!("malformed envelope: {error}"),
            })?;
        if envelope.format_version != AGENT_EXPORT_FORMAT_VERSION {
            return Err(StoreError::InvalidImport {
                reason: format!(
                    "unsupported export format version {}; this build understands {}",
                    envelope.format_version, AGENT_EXPORT_FORMAT_VERSION
                ),
            });
        }
        if envelope.agents.len() > MAX_AGENT_IMPORT_AGENTS {
            return Err(StoreError::InvalidImport {
                reason: format!(
                    "envelope exceeds the {MAX_AGENT_IMPORT_AGENTS}-agent import bound"
                ),
            });
        }
        let entries = envelope.agents;
        self.call(move |db| {
            let mut imported = Vec::with_capacity(entries.len());
            let mut seen: HashSet<String> = HashSet::with_capacity(entries.len());
            let transaction = crate::transaction::atomic(db)?;
            for entry in entries {
                let reason = |message: String| StoreError::InvalidImport { reason: message };
                validate_agent_id(&entry.agent_id).map_err(|error| {
                    reason(format!("agent {}: {error}", entry.agent_id))
                })?;
                if !seen.insert(entry.agent_id.clone()) {
                    return Err(reason(format!(
                        "envelope carries agent {} twice",
                        entry.agent_id
                    )));
                }
                if entry.scope != expected_scope {
                    return Err(reason(format!(
                        "cross-scope import refused: agent {} declares scope {:?}, expected {expected_scope:?}",
                        entry.agent_id, entry.scope
                    )));
                }
                validate_identity(&entry.scope, &entry.owner_id)
                    .map_err(|error| reason(format!("agent {}: {error}", entry.agent_id)))?;
                if entry.lifecycle == AgentLifecycle::Deleted {
                    return Err(reason(format!(
                        "agent {}: deleted tombstones cannot be imported",
                        entry.agent_id
                    )));
                }
                // Deny-all on credential material: even a reference would
                // resolve in the EXPORTER's secret context, so imports
                // refuse every entry that tries to carry one.
                if !entry.definition.secret_refs.is_empty()
                    || entry
                        .revisions
                        .iter()
                        .any(|revision| !revision.snapshot.secret_refs.is_empty())
                {
                    return Err(reason(format!(
                        "agent {}: imports may not carry secret references",
                        entry.agent_id
                    )));
                }
                validate_definition(&entry.definition)
                    .map_err(|error| reason(format!("agent {}: {error}", entry.agent_id)))?;
                if entry.revisions.is_empty() {
                    return Err(reason(format!(
                        "agent {}: the configuration ledger may not be empty",
                        entry.agent_id
                    )));
                }
                if entry.created_at < 0 || entry.updated_at < entry.created_at {
                    return Err(reason(format!(
                        "agent {}: timestamps must be non-negative and ordered",
                        entry.agent_id
                    )));
                }
                let mut previous_created = entry.created_at;
                for (index, revision) in entry.revisions.iter().enumerate() {
                    let expected = index as i64 + 1;
                    if revision.revision != expected {
                        return Err(reason(format!(
                            "agent {}: ledger must be contiguous from revision 1, found {} at position {expected}",
                            entry.agent_id, revision.revision
                        )));
                    }
                    if (index == 0) != (revision.source == AgentRevisionSource::Create) {
                        return Err(reason(format!(
                            "agent {}: revision 1 must be the create revision and no later revision may claim create",
                            entry.agent_id
                        )));
                    }
                    validate_definition(&revision.snapshot)
                        .map_err(|error| reason(format!("agent {}: {error}", entry.agent_id)))?;
                    if revision.created_at < previous_created {
                        return Err(reason(format!(
                            "agent {}: ledger timestamps must not go backwards",
                            entry.agent_id
                        )));
                    }
                    previous_created = revision.created_at;
                }
                // The claimed effective definition IS the newest snapshot:
                // the ledger, not the envelope, decides currency.
                if entry.revisions.last().expect("checked non-empty").snapshot
                    != entry.definition
                {
                    return Err(reason(format!(
                        "agent {}: definition does not match the newest revision snapshot",
                        entry.agent_id
                    )));
                }
                if entry.updated_at < previous_created {
                    return Err(reason(format!(
                        "agent {}: updatedAt precedes its newest revision",
                        entry.agent_id
                    )));
                }
                if select_any(&transaction, &entry.agent_id)?.is_some() {
                    return Err(StoreError::Duplicate(format!(
                        "agent {} already exists in this store",
                        entry.agent_id
                    )));
                }
                let record = AgentRecord {
                    agent_id: entry.agent_id.clone(),
                    scope: entry.scope,
                    owner_id: entry.owner_id,
                    lifecycle: entry.lifecycle,
                    current_revision: entry.revisions.len() as i64,
                    definition: entry.definition,
                    created_at: entry.created_at,
                    updated_at: entry.updated_at,
                };
                transaction.execute(
                    "INSERT INTO agents(agent_id,scope,owner_id,lifecycle,current_revision,record,created_at,updated_at) \
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![
                        record.agent_id,
                        record.scope,
                        record.owner_id,
                        serde_name(record.lifecycle),
                        record.current_revision,
                        util::json(&transaction, &record)?,
                        record.created_at,
                        record.updated_at,
                    ],
                )?;
                for revision in &entry.revisions {
                    let source_text = match revision.source {
                        AgentRevisionSource::Create => "create",
                        AgentRevisionSource::Edit => "edit",
                        AgentRevisionSource::ModelChange => "model_change",
                        AgentRevisionSource::Restore => "restore",
                    };
                    transaction.execute(
                        "INSERT INTO agent_config_revisions(agent_id,revision,snapshot,source,created_at) \
                         VALUES(?1,?2,?3,?4,?5)",
                        params![
                            record.agent_id,
                            revision.revision,
                            util::json(&transaction, &revision.snapshot)?,
                            source_text,
                            revision.created_at,
                        ],
                    )?;
                }
                imported.push(record);
            }
            transaction.commit()?;
            Ok(imported)
        })
        .await
    }
}
