//! Host-owned agent definitions (AGENTKIT-87): the service boundary over
//! the durable agent identity/revision store.
//!
//! The store keeps the rows; THIS layer is the authority that decides what
//! a valid definition means: it validates the semantic vocabulary the
//! store deliberately treats as bounded strings (permission ceilings from
//! the core permission vocabulary, reasoning efforts from the core model
//! vocabulary), derives the revision provenance (`model_change` only when
//! the model preference actually moved), and exposes the lifecycle a host
//! runs agents through: create → edit → archive → restore → delete, plus
//! credential-free duplication and bounded versioned export/import.
//!
//! Invariants this boundary guarantees:
//! - **Optimistic concurrency.** Every mutation carries the revision the
//!   caller read; concurrent editors get an explicit
//!   [`agentkit_store::StoreError::Conflict`], never a silent overwrite.
//! - **Model change ≠ new agent.** `set_model` bumps the configuration
//!   revision and validates the new metadata; identity, scope, owner, and
//!   the whole revision ledger are untouched. There is no path through
//!   this service that replaces an identity.
//! - **Admission is checked, not guessed.** Hosts call
//!   [`AgentDefinitionService::check_admission`] before admitting work
//!   under an agent; an archived agent refuses there, so archive means
//!   "no NEW runs", never "erase history".
//! - **Retention before deletion.** Deletion is a tombstone and refuses
//!   while the receipt store holds active (admitted, unresolved)
//!   executions; [`AgentDefinitionService::deletion_preview`] shows the
//!   retention picture first.
//! - **Secret references only.** No call here stores a secret VALUE;
//!   credential material enters only as scheme-qualified `secretRefs`
//!   references, duplicate strips them, and export/import never carries
//!   them across a boundary.

use crate::{AgentsError, AgentsResult};
use agentkit_core::{ReasoningEffort, permission::CodingMode};
use agentkit_host::{SharedRuntime, SystemRuntime};
use agentkit_store::{
    AgentDefinition, AgentDeletionPreview, AgentExportEnvelope, AgentRecord, AgentRevision,
    AgentRevisionSource, CreateAgent, DuplicateAgent, SetAgentModel, SqliteStore, UpdateAgent,
};

fn invalid(message: impl std::fmt::Display) -> AgentsError {
    AgentsError::InvalidAgent(message.to_string())
}

fn not_found(agent_id: &str) -> AgentsError {
    AgentsError::not_found(format!("agent_not_found: {agent_id}"))
}

/// Validate the semantic vocabulary the durable store keeps as bounded
/// strings: the permission ceiling must be a core permission mode, the
/// reasoning effort a core effort level, and a named model a real string.
pub fn validate_agent_definition(definition: &AgentDefinition) -> AgentsResult<()> {
    if definition.name.trim().is_empty() {
        return Err(invalid("name must not be blank"));
    }
    if CodingMode::parse(&definition.permission_mode).is_none() {
        return Err(invalid(format!(
            "permissionMode {:?} is not a core permission mode",
            definition.permission_mode
        )));
    }
    if let Some(effort) = &definition.reasoning_effort {
        serde_json::from_value::<ReasoningEffort>(serde_json::Value::String(effort.clone()))
            .map_err(|_| invalid(format!("reasoningEffort {effort:?} is unknown")))?;
    }
    if definition.model.as_deref().is_some_and(str::is_empty) {
        return Err(invalid("model, when set, must not be empty"));
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
        if limit <= 0 {
            return Err(invalid("budget limits must be positive"));
        }
    }
    Ok(())
}

/// The host service boundary for persistent agent definitions. Cheap to
/// clone: everything shared is the store handle and the injected runtime.
#[derive(Clone)]
pub struct AgentDefinitionService {
    store: SqliteStore,
    runtime: SharedRuntime,
}

impl AgentDefinitionService {
    /// Bind the service to a durable store with the system runtime clock.
    pub fn new(store: SqliteStore) -> Self {
        Self::with_runtime(store, std::sync::Arc::new(SystemRuntime))
    }

    /// Bind the service with an injected clock/identity runtime, matching
    /// the host's other services.
    pub fn with_runtime(store: SqliteStore, runtime: SharedRuntime) -> Self {
        Self { store, runtime }
    }

    /// The underlying durable store (for hosts that compose these rows
    /// into their own admission paths).
    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    /// Create a durable agent identity. The definition's vocabulary is
    /// validated here; the store enforces the structural bounds beneath.
    pub async fn create(&self, mut input: CreateAgent) -> AgentsResult<AgentRecord> {
        validate_agent_definition(&input.definition)?;
        input.now = self.runtime.now_ms();
        Ok(self.store.create_agent(input).await?)
    }

    /// Read a live agent; tombstones and unknown ids are
    /// [`AgentsError::Host`] with the stable `agent_not_found` code.
    pub async fn get(&self, agent_id: &str) -> AgentsResult<AgentRecord> {
        self.store
            .get_agent(agent_id.to_owned())
            .await?
            .ok_or_else(|| not_found(agent_id))
    }

    /// List the live agents (active and archived) of one scope.
    pub async fn list(&self, scope: &str) -> AgentsResult<Vec<AgentRecord>> {
        Ok(self.store.list_agents(scope.to_owned()).await?)
    }

    /// The append-only configuration ledger of one agent, oldest first.
    pub async fn revisions(&self, agent_id: &str) -> AgentsResult<Vec<AgentRevision>> {
        Ok(self.store.list_agent_revisions(agent_id.to_owned()).await?)
    }

    /// Replace the effective definition under optimistic concurrency. The
    /// revision provenance is DERIVED here, not trusted from callers: the
    /// append is `model_change` exactly when the model preference moved,
    /// otherwise `edit`. A stale `expected_revision` conflicts and
    /// applies nothing.
    pub async fn update(
        &self,
        agent_id: &str,
        expected_revision: i64,
        definition: AgentDefinition,
    ) -> AgentsResult<AgentRecord> {
        validate_agent_definition(&definition)?;
        let current = self.get(agent_id).await?;
        let source = if definition.model != current.definition.model
            || definition.reasoning_effort != current.definition.reasoning_effort
        {
            AgentRevisionSource::ModelChange
        } else {
            AgentRevisionSource::Edit
        };
        if source == AgentRevisionSource::ModelChange
            && definition.model.as_deref().is_some_and(str::is_empty)
        {
            return Err(invalid("model must not be blank"));
        }
        Ok(self
            .store
            .update_agent(UpdateAgent {
                agent_id: agent_id.to_owned(),
                expected_revision,
                definition,
                source,
                now: self.runtime.now_ms(),
            })
            .await?)
    }

    /// Change the model preference of an existing agent. Identity and
    /// history are preserved by construction — the same immutable id, the
    /// same scope and owner, the full ledger — while the validated new
    /// metadata appends one `model_change` revision. This never creates
    /// or replaces an identity.
    pub async fn set_model(
        &self,
        agent_id: &str,
        expected_revision: i64,
        model: Option<String>,
        reasoning_effort: Option<String>,
    ) -> AgentsResult<AgentRecord> {
        if let Some(model) = &model
            && model.trim().is_empty()
        {
            return Err(invalid("model must not be blank"));
        }
        if let Some(effort) = &reasoning_effort {
            serde_json::from_value::<ReasoningEffort>(serde_json::Value::String(effort.clone()))
                .map_err(|_| invalid(format!("reasoningEffort {effort:?} is unknown")))?;
        }
        Ok(self
            .store
            .set_agent_model(SetAgentModel {
                agent_id: agent_id.to_owned(),
                expected_revision,
                model,
                reasoning_effort,
                now: self.runtime.now_ms(),
            })
            .await?)
    }

    /// Archive an agent: [`Self::check_admission`] refuses new work from
    /// now on, while the row, its configuration, and its whole ledger
    /// stay durable. Idempotent.
    pub async fn archive(&self, agent_id: &str) -> AgentsResult<AgentRecord> {
        Ok(self
            .store
            .archive_agent(agent_id.to_owned(), self.runtime.now_ms())
            .await?)
    }

    /// Restore an archived agent to active, recording a `restore`
    /// revision so the lifecycle trajectory is auditable. Idempotent.
    pub async fn restore(&self, agent_id: &str) -> AgentsResult<AgentRecord> {
        Ok(self
            .store
            .restore_agent(agent_id.to_owned(), self.runtime.now_ms())
            .await?)
    }

    /// The admission gate every execution entry point calls BEFORE
    /// admitting new work under an agent: Ok carries the current record
    /// to pin the run against; an archived agent refuses with
    /// [`agentkit_store::StoreError::InvalidTransition`], an unknown or
    /// deleted one with [`agentkit_store::StoreError::NotFound`].
    pub async fn check_admission(&self, agent_id: &str) -> AgentsResult<AgentRecord> {
        Ok(self
            .store
            .check_agent_admission(agent_id.to_owned())
            .await?)
    }

    /// Retention preview for an explicit delete: what survives (identity,
    /// configuration, the entire ledger) and what refuses (active
    /// execution receipts). Unknown agents are absence.
    pub async fn deletion_preview(
        &self,
        agent_id: &str,
    ) -> AgentsResult<Option<AgentDeletionPreview>> {
        Ok(self
            .store
            .preview_agent_deletion(agent_id.to_owned())
            .await?)
    }

    /// Explicitly delete an agent: a retention-gated tombstone. Active
    /// execution receipts refuse it (see
    /// [`Self::deletion_preview`]); nothing is erased — the reserved
    /// identity and the full configuration history stay durable.
    pub async fn delete(&self, agent_id: &str) -> AgentsResult<AgentRecord> {
        Ok(self
            .store
            .delete_agent(agent_id.to_owned(), self.runtime.now_ms())
            .await?)
    }

    /// Duplicate an agent under a NEW identity in the source's scope:
    /// display metadata, purpose, instructions, resources, skills, model
    /// preference, permission ceiling, and budgets are copied; credentials
    /// (secret references), grants, and private memory are excluded by
    /// construction and never ride along. The copy starts its own
    /// one-entry ledger.
    pub async fn duplicate(
        &self,
        agent_id: &str,
        owner_id: Option<String>,
    ) -> AgentsResult<AgentRecord> {
        Ok(self
            .store
            .duplicate_agent(DuplicateAgent {
                agent_id: agent_id.to_owned(),
                new_agent_id: None,
                owner_id,
                now: self.runtime.now_ms(),
            })
            .await?)
    }

    /// Export a scope's live agents as the versioned, bounded envelope
    /// (secret references stripped; tombstones never exported).
    pub async fn export(&self, scope: &str) -> AgentsResult<String> {
        Ok(self.store.export_agents(scope.to_owned()).await?)
    }

    /// Import a versioned envelope into one scope, verifying the format
    /// version, the schema, the ledger provenance, and the scope, and
    /// refusing duplicate identities or smuggled credential references.
    /// The batch lands in one transaction: a refusal leaves no partial
    /// import.
    pub async fn import(
        &self,
        envelope: &str,
        expected_scope: &str,
    ) -> AgentsResult<Vec<AgentRecord>> {
        Ok(self
            .store
            .import_agents(envelope.to_owned(), expected_scope.to_owned())
            .await?)
    }

    /// The envelope this service's export produces, for callers that want
    /// the typed value rather than the JSON text.
    pub fn parse_envelope(envelope: &str) -> AgentsResult<AgentExportEnvelope> {
        serde_json::from_str(envelope).map_err(|error| invalid(error.to_string()))
    }
}
