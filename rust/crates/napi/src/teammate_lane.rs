//! The persistent-teammate lane (AGENTKIT-90): thin JSON-over-boundary
//! wrappers over the host's OWN persistent-agent services —
//! [`AgentDefinitionService`], [`TeammateService`] and [`RunProfileService`]
//! — over the same single [`SqliteStore`] the native store handle owns.
//! Nothing here re-implements admission, the revision ledger, or the
//! projection: the binding moves JSON across the boundary and every error
//! carries a code from `error-codes.json`. Scope is host-assigned exactly
//! as the services take it: `scope` rides only the create/list calls the
//! services themselves accept; every other call addresses the agent by its
//! immutable id and the services enforce lifecycle and ledger rules.

use std::sync::{Arc, Mutex};

use napi::bindgen_prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use agentkit_agents::{AgentDefinitionService, AgentsError, TeammateAdmission, TeammateService};
use agentkit_host::{
    ApprovalGrantRef, ContextUnit, ContextUnitKind, ProjectSelection, RunProfileDefaults,
    RunProfileService, Runtime, SelectedSource, SelectedSourceKind, SharedRuntime, SystemRuntime,
    TaskProfileOverrides,
};
use agentkit_store::{AgentDefinition, CreateAgent, SqliteStore, redact_event};

use crate::errors;
use crate::store::NativeStore;

/// The teammate surface over one store: the three host services built on
/// the store's single actor and the shared system runtime. Cheap to build;
/// one per caller, all sharing the store behind it. `close()` releases
/// every store clone this lane holds, so a caller can close the store and
/// reopen the same file deterministically without waiting on the GC.
#[napi(js_name = "NativeTeammates")]
pub struct NativeTeammates {
    inner: Mutex<Option<Arc<TeammateServices>>>,
}

/// The owned state behind the closed gate: cloned (cheaply — every field
/// is a handle) per call so no lock is ever held across an await.
struct TeammateServices {
    store: SqliteStore,
    agents: AgentDefinitionService,
    teammates: TeammateService,
    profiles: RunProfileService,
}

impl NativeTeammates {
    /// The services, or `closed` once [`Self::close`] released them.
    fn services(&self) -> Result<Arc<TeammateServices>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| errors::closed("the teammate lane is closed"))
    }
}

#[napi]
impl NativeStore {
    /// Create the persistent-teammate surface over this store: the host's
    /// definition service, teammate admission service, and run-profile
    /// service — the SAME services the HTTP host routes to, no second
    /// ledger. `optionsJson` may carry `runProfileDefaults` (the host's
    /// default layer for profile resolution: provider id, model,
    /// permission mode, tool set, budget ceilings, environment digests,
    /// routing policy); an unsupported shape fails with `invalid_input`.
    #[napi]
    pub async fn create_teammates(&self, options_json: Option<String>) -> Result<NativeTeammates> {
        let store = self.handle()?;
        let defaults = parse_run_profile_defaults(options_json.as_deref())?;
        let runtime: SharedRuntime = std::sync::Arc::new(SystemRuntime);
        Ok(NativeTeammates {
            inner: Mutex::new(Some(Arc::new(TeammateServices {
                agents: AgentDefinitionService::with_runtime(store.clone(), runtime.clone()),
                teammates: TeammateService::with_runtime(store.clone(), runtime),
                profiles: RunProfileService::new(store.clone(), defaults),
                store,
            }))),
        })
    }
}

#[napi]
impl NativeTeammates {
    /// Release every store clone this lane holds. Idempotent; later calls
    /// through this handle fail with `closed`, exactly like the store's
    /// own gate.
    #[napi]
    pub fn close(&self) -> Result<()> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        Ok(())
    }

    // -- agent definitions -------------------------------------------------

    /// Create a persistent agent in the caller-named scope under
    /// `ownerId`. `definitionJson` is the `AgentDefinition` document;
    /// unknown fields are refused (`deny_unknown_fields`), so a payload can
    /// never smuggle a secret value or a foreign scope. Returns the
    /// `AgentRecord` JSON at revision 1.
    #[napi]
    pub async fn create_agent(
        &self,
        scope: String,
        owner_id: String,
        definition_json: String,
    ) -> Result<String> {
        let s = self.services()?;
        if scope.trim().is_empty() {
            return Err(errors::invalid("scope must be a non-empty string"));
        }
        if owner_id.trim().is_empty() {
            return Err(errors::invalid("ownerId must be a non-empty string"));
        }
        let definition: AgentDefinition = parse_json(definition_json, "definition")?;
        let record = s
            .agents
            .create(CreateAgent {
                agent_id: None,
                scope,
                owner_id,
                definition,
                now: 0, // the service stamps the runtime clock
            })
            .await
            .map_err(map_host)?;
        serialized(&record, "agent")
    }

    /// The live agents (active and archived) of one scope. Other scopes are
    /// absence, not an error: the listing never leaks them.
    #[napi]
    pub async fn list_agents(&self, scope: String) -> Result<String> {
        let s = self.services()?;
        if scope.trim().is_empty() {
            return Err(errors::invalid("scope must be a non-empty string"));
        }
        let records = s
            .agents
            .list(&scope)
            .await
            .map_err(map_host)?
            .into_iter()
            .map(record_value)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Value::Array(records).to_string())
    }

    /// One live agent; archived is readable, tombstoned and unknown are
    /// `not_found`.
    #[napi]
    pub async fn get_agent(&self, agent_id: String) -> Result<String> {
        let s = self.services()?;
        let record = s.agents.get(&agent_id).await.map_err(map_host)?;
        serialized(&record, "agent")
    }

    /// Replace the effective definition under optimistic concurrency: the
    /// caller must name the revision it read; a stale `expectedRevision`
    /// conflicts with code `revision_conflict` and applies NOTHING.
    #[napi]
    pub async fn update_agent(
        &self,
        agent_id: String,
        expected_revision: f64,
        definition_json: String,
    ) -> Result<String> {
        let s = self.services()?;
        let expected_revision = revision(expected_revision)?;
        let definition: AgentDefinition = parse_json(definition_json, "definition")?;
        let record = s
            .agents
            .update(&agent_id, expected_revision, definition)
            .await
            .map_err(map_host)?;
        serialized(&record, "agent")
    }

    /// Move only the model preference of an existing agent; identity,
    /// scope, owner, and the whole ledger stay, and the append carries
    /// `model_change` provenance. Stale revisions conflict like
    /// [`Self::update_agent`].
    #[napi]
    pub async fn set_agent_model(
        &self,
        agent_id: String,
        expected_revision: f64,
        model: Option<String>,
        reasoning_effort: Option<String>,
    ) -> Result<String> {
        let s = self.services()?;
        let expected_revision = revision(expected_revision)?;
        let record = s
            .agents
            .set_model(&agent_id, expected_revision, model, reasoning_effort)
            .await
            .map_err(map_host)?;
        serialized(&record, "agent")
    }

    /// Archive an agent: new admission refuses, history stays readable,
    /// the row and its ledger stay durable. Idempotent.
    #[napi]
    pub async fn archive_agent(&self, agent_id: String) -> Result<String> {
        let s = self.services()?;
        let record = s.agents.archive(&agent_id).await.map_err(map_host)?;
        serialized(&record, "agent")
    }

    /// Restore an archived agent to active, recording a `restore` revision.
    /// Idempotent.
    #[napi]
    pub async fn restore_agent(&self, agent_id: String) -> Result<String> {
        let s = self.services()?;
        let record = s.agents.restore(&agent_id).await.map_err(map_host)?;
        serialized(&record, "agent")
    }

    /// The append-only configuration ledger of one agent, oldest first.
    #[napi]
    pub async fn list_agent_revisions(&self, agent_id: String) -> Result<String> {
        let s = self.services()?;
        let revisions = s
            .agents
            .revisions(&agent_id)
            .await
            .map_err(map_host)?
            .into_iter()
            .map(record_value)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Value::Array(revisions).to_string())
    }

    // -- admission and tasks -----------------------------------------------

    /// Admit work under an agent — the only admission path. The admission
    /// JSON: `{ agentId, admissionKey, kind, payload?, priority?,
    /// dependsOn?, parentTaskId?, taskId? }` plus, for profile pinning,
    /// `taskProfile?`, `projectSources?`, `approvals?`. Profile pinning is
    /// SERVER-side: when the caller asks for a profile the sealed value is
    /// [`RunProfileService::resolve`]'s resolution, never whatever the
    /// caller sent, and the pin rides the admission transaction. Returns
    /// `{ task, replayed }`: a duplicate `admissionKey` answers with the
    /// SAME logical task and `replayed: true`, creating nothing.
    #[napi]
    pub async fn submit_teammate_task(&self, admission_json: String) -> Result<String> {
        let s = self.services()?;
        let input: Value = parse_json(admission_json, "admission")?;
        let agent_id = string_field(&input, "agentId")?;
        let admission_key = string_field(&input, "admissionKey")?;
        let kind = string_field(&input, "kind")?;
        let payload = match input.get("payload") {
            None | Some(Value::Null) => json!({}),
            Some(Value::Object(_)) => input["payload"].clone(),
            Some(_) => return Err(errors::invalid("`payload` must be a JSON object")),
        };
        let priority = match input.get("priority") {
            None | Some(Value::Null) => 0,
            Some(value) => i32::try_from(
                value
                    .as_i64()
                    .ok_or_else(|| errors::invalid("`priority` must be an integer"))?,
            )
            .map_err(|_| errors::invalid("`priority` is out of range"))?,
        };
        let depends_on = match input.get("dependsOn") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .filter(|text| !text.trim().is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            errors::invalid("`dependsOn` entries must be non-empty strings")
                        })
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(errors::invalid("`dependsOn` must be an array")),
        };
        let parent_task_id = optional_string(&input, "parentTaskId")?;
        let task_id = optional_string(&input, "taskId")?;
        // Profile pinning is opt-in by presence, exactly like the HTTP
        // host: any of the three profile layers asks the service to seal.
        let wants_profile = input.get("taskProfile").is_some()
            || input.get("projectSources").is_some()
            || input.get("approvals").is_some();
        let run_profile = if wants_profile {
            let project = project_selection(input.get("projectSources"))?;
            let overrides = match input.get("taskProfile") {
                None | Some(Value::Null) => TaskProfileOverrides::default(),
                Some(value) => serde_json::from_value::<TaskProfileOverrides>(value.clone())
                    .map_err(|error| errors::invalid(format!("invalid `taskProfile`: {error}")))?,
            };
            let approvals = match input.get("approvals") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(entries)) => entries
                    .iter()
                    .map(|entry| {
                        Ok(ApprovalGrantRef {
                            session_id: string_field(entry, "sessionId")?,
                            grant_id: string_field(entry, "grantId")?,
                            digest: string_field(entry, "digest")?,
                        })
                    })
                    .collect::<Result<_, _>>()?,
                Some(_) => return Err(errors::invalid("`approvals` must be an array")),
            };
            Some(
                s.profiles
                    .resolve(
                        &agent_id,
                        SystemRuntime.now_ms(),
                        &project,
                        &overrides,
                        &approvals,
                    )
                    .await
                    .map_err(map_host)?,
            )
        } else {
            None
        };
        let admitted = s
            .teammates
            .admit(TeammateAdmission {
                task_id,
                agent_id,
                admission_key,
                kind,
                payload,
                priority,
                depends_on,
                parent_task_id,
                run_profile,
            })
            .await
            .map_err(map_host)?;
        Ok(json!({
            "task": record_value(&admitted.task)?,
            "replayed": admitted.replayed,
        })
        .to_string())
    }

    /// The task records of one agent, in admission order.
    #[napi]
    pub async fn list_agent_tasks(&self, agent_id: String) -> Result<String> {
        let s = self.services()?;
        let tasks = s
            .teammates
            .tasks(&agent_id)
            .await
            .map_err(map_host)?
            .into_iter()
            .map(record_value)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Value::Array(tasks).to_string())
    }

    /// The bounded projection of one task: the canonical row plus the
    /// derived event-window views. PURE and recomputed on every read, so
    /// it reconstructs identically after a store reopen. Unknown usage
    /// stays unknown: counters are `null`, never a reassuring `0`.
    #[napi]
    pub async fn get_agent_task_projection(
        &self,
        task_id: String,
        window: Option<f64>,
    ) -> Result<String> {
        let s = self.services()?;
        let window = match window {
            None => 0,
            Some(value) => usize::try_from(whole(value, "`window`")?)
                .map_err(|_| errors::invalid("`window` is out of range"))?,
        };
        let projection = s
            .store
            .task_projection(&task_id, window)
            .await
            .map_err(errors::store)?
            .ok_or_else(|| {
                errors::store(agentkit_store::StoreError::NotFound(format!(
                    "agent task {task_id}"
                )))
            })?;
        serialized(&projection, "projection")
    }

    /// Request cancellation: a queued, parked, or backoff-parked task
    /// settles NOW; a running attempt observes the durable flag through
    /// its lease. Idempotent — a settled task answers with the same
    /// record, never an error.
    #[napi]
    pub async fn cancel_agent_task(&self, task_id: String) -> Result<String> {
        let s = self.services()?;
        let task = s.teammates.cancel(&task_id).await.map_err(map_host)?;
        serialized(&task, "task")
    }

    /// One page of a task's durable event log, REDACTED: credential-named
    /// values and oversized tool text never cross the boundary; the
    /// durable record stays untouched. `after` is the numeric sequence
    /// cursor from a previous page's `nextCursor`.
    #[napi]
    pub async fn list_agent_task_events(
        &self,
        task_id: String,
        after: Option<f64>,
        limit: Option<f64>,
    ) -> Result<String> {
        let s = self.services()?;
        let after = match after {
            None => None,
            Some(number) if number.is_finite() => Some(number),
            Some(_) => return Err(errors::invalid("event cursor must be numeric")),
        };
        let page = match limit {
            None => 100,
            Some(value) => usize::try_from(whole(value, "`limit`")?)
                .map_err(|_| errors::invalid("`limit` is out of range"))?
                .max(1),
        }
        .min(1000);
        let records = s
            .store
            .events_after_number(&task_id, after, page)
            .await
            .map_err(errors::store)?;
        let events: Vec<Value> = records
            .iter()
            .map(|record| redact_event(&record.event))
            .collect();
        let mut body = json!({ "events": events });
        if records.len() == page
            && let Some(last) = records.last()
        {
            body["nextCursor"] = json!(last.seq.to_string());
        }
        Ok(body.to_string())
    }

    /// The explanation of one admitted task's sealed run profile — the
    /// SAME pinned value, never a re-resolution. Honest absence: a task
    /// admitted without a profile answers `null`, not an empty profile.
    #[napi]
    pub async fn get_agent_task_run_profile(&self, task_id: String) -> Result<Option<String>> {
        let s = self.services()?;
        let explained = s.profiles.explain(&task_id).await.map_err(map_host)?;
        Ok(explained.map(|value| value.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Parsing and mapping: JSON in, JSON out; every refusal carries a
// contract code.
// ---------------------------------------------------------------------------

/// Map a host error the way this lane's namespaces promise: the optimistic
/// concurrency conflict keeps the established `revision_conflict` code (the
/// same code the HTTP host answers with); admission/validation sentinels
/// the services raise through `HostError::Execution` are caller fixes
/// (`invalid_input`); store transition and duplicate legality keep this
/// lane's namespace codes; everything else maps exactly as the crate-wide
/// host mapping.
fn map_host(error: impl Into<AgentsError>) -> Error {
    use agentkit_host::HostError as H;
    use agentkit_store::StoreError as S;
    let error = match error.into() {
        AgentsError::Host(inner) => inner,
        refusal @ (AgentsError::InvalidAgent(_) | AgentsError::InvalidAdmission(_)) => {
            return errors::invalid(refusal.to_string());
        }
        other => return errors::agents(other),
    };
    // Raised by the host-owned run-profile resolver, not the agents context.
    if let H::Execution(message) = &error
        && (message.starts_with("invalid_overrides") || message.starts_with("run_profile_refused"))
    {
        return errors::invalid(message.clone());
    }
    match error {
        H::Store(S::Conflict(message)) => errors::stable("revision_conflict", message, None),
        H::Store(inner) => errors::store_port(inner, "invalid_task_transition", "duplicate_task"),
        _ => errors::host(error),
    }
}

fn parse_json<T: serde::de::DeserializeOwned>(raw: String, what: &str) -> Result<T> {
    serde_json::from_str(&raw)
        .map_err(|error| errors::invalid(format!("`{what}` is not valid JSON: {error}")))
}

fn serialized<T: serde::Serialize>(value: &T, what: &str) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|error| errors::internal(&format!("{what} serialization: {error}")))
}

fn record_value<T: serde::Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value)
        .map_err(|error| errors::internal(&format!("record serialization: {error}")))
}

/// A revision the caller read: a whole, non-negative integer.
fn revision(value: f64) -> Result<i64> {
    i64::try_from(whole(value, "`expectedRevision`")?)
        .map_err(|_| errors::invalid("`expectedRevision` is out of range"))
}

/// A whole, non-negative count the caller named: JSON numbers cross the
/// boundary as f64, so wholeness and range are validated, never truncated.
fn whole(value: f64, what: &str) -> Result<u64> {
    if !value.is_finite() || value.fract() != 0.0 || value < 0.0 || value > u64::MAX as f64 {
        return Err(errors::invalid(format!(
            "`{what}` must be a non-negative integer"
        )));
    }
    Ok(value as u64)
}

fn string_field(input: &Value, key: &str) -> Result<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| errors::invalid(format!("`{key}` must be a non-empty string")))
}

fn optional_string(input: &Value, key: &str) -> Result<Option<String>> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.clone())),
        Some(_) => Err(errors::invalid(format!(
            "`{key}` must be a non-empty string"
        ))),
    }
}

/// The `projectSources` shape, parsed exactly as the HTTP host parses it:
/// each entry names a skill (pinned in the agent's selection) or a granted
/// resource, with its desired compilation units.
fn project_selection(value: Option<&Value>) -> Result<ProjectSelection> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(ProjectSelection::default());
    };
    let entries = value
        .as_array()
        .ok_or_else(|| errors::invalid("`projectSources` must be an array"))?;
    let mut sources = Vec::with_capacity(entries.len());
    for entry in entries {
        let id = string_field(entry, "id")?;
        let kind = match entry.get("kind").and_then(Value::as_str) {
            Some("skill") => SelectedSourceKind::Skill,
            Some("resource") => SelectedSourceKind::Resource,
            _ => {
                return Err(errors::invalid("`kind` must be \"skill\" or \"resource\"."));
            }
        };
        let units = match entry.get("units") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut units = Vec::with_capacity(items.len());
                for unit in items {
                    let kind = match unit.get("kind").and_then(Value::as_str) {
                        Some("text") => ContextUnitKind::Text,
                        Some("toolPair") => ContextUnitKind::ToolPair,
                        _ => {
                            return Err(errors::invalid(
                                "unit `kind` must be \"text\" or \"toolPair\".",
                            ));
                        }
                    };
                    let text = unit
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| errors::invalid("unit `text` must be a string"))?
                        .to_owned();
                    let citations = match unit.get("citations") {
                        None | Some(Value::Null) => Vec::new(),
                        Some(Value::Array(cites)) => cites
                            .iter()
                            .map(|cite| {
                                cite.as_str()
                                    .map(str::to_owned)
                                    .ok_or_else(|| errors::invalid("citation must be a string"))
                            })
                            .collect::<Result<_, _>>()?,
                        Some(_) => return Err(errors::invalid("`citations` must be an array")),
                    };
                    let required = match unit.get("required") {
                        None | Some(Value::Null) => false,
                        Some(Value::Bool(flag)) => *flag,
                        Some(_) => return Err(errors::invalid("`required` must be a boolean")),
                    };
                    units.push(ContextUnit {
                        kind,
                        text,
                        citations,
                        required,
                    });
                }
                units
            }
            Some(_) => return Err(errors::invalid("`units` must be an array")),
        };
        sources.push(SelectedSource { id, kind, units });
    }
    Ok(ProjectSelection { sources })
}

/// The mirror of the host's [`RunProfileDefaults`] over the boundary:
/// `deny_unknown_fields` keeps the options document honest — there is no
/// field that could smuggle a credential through the lane.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
struct RunProfileDefaultsInput {
    provider_id: String,
    model: Option<String>,
    permission_mode: String,
    tools: Vec<String>,
    budgets: agentkit_store::AgentBudgets,
    environment: std::collections::BTreeMap<String, String>,
    local_only: bool,
    fallback_providers: Vec<String>,
}

fn parse_run_profile_defaults(options_json: Option<&str>) -> Result<RunProfileDefaults> {
    let Some(raw) = options_json.filter(|text| !text.trim().is_empty()) else {
        return Ok(RunProfileDefaults::default());
    };
    let options: Value = parse_json(raw.to_owned(), "options")?;
    match options.get("runProfileDefaults") {
        None | Some(Value::Null) => Ok(RunProfileDefaults::default()),
        Some(value) => {
            let parsed: RunProfileDefaultsInput =
                serde_json::from_value(value.clone()).map_err(|error| {
                    errors::invalid(format!("invalid `runProfileDefaults`: {error}"))
                })?;
            let host_default = RunProfileDefaults::default();
            Ok(RunProfileDefaults {
                provider_id: if parsed.provider_id.is_empty() {
                    host_default.provider_id
                } else {
                    parsed.provider_id
                },
                model: parsed.model,
                permission_mode: if parsed.permission_mode.is_empty() {
                    host_default.permission_mode
                } else {
                    parsed.permission_mode
                },
                tools: parsed.tools,
                budgets: parsed.budgets,
                environment: parsed.environment,
                local_only: parsed.local_only,
                fallback_providers: parsed.fallback_providers,
            })
        }
    }
}
