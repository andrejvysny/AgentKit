//! The AGENTKIT-90 consumer surface for persistent teammates: agent
//! lifecycle and definition versions, durable task submission, task
//! status/history, bounded event pages, and effective-run-profile
//! inspection — exposed as operations on THIS backend and delegating to
//! the ONLY execution owners: the host's [`AgentDefinitionService`],
//! [`TeammateService`], and [`RunProfileService`] over the single store
//! and runner. No parallel service, no second queue: this module parses,
//! authorizes, projects, and redacts — nothing else.
//!
//! The invariants this boundary keeps:
//! - **Scope authorization on every list/detail/event route.** The
//!   caller's scope is derived from the authenticated actor (never from
//!   the request body or path), and every agent is read through
//!   [`DesktopBackend::owned_agent`]: an agent outside the caller's
//!   scope answers exactly like an agent that does not exist (404), so
//!   cross-scope access is a refusal, never an oracle.
//! - **Reads create nothing.** Status, history, event pages, profile
//!   explanation, and the SSE resume are pure reads over durable rows;
//!   the only work-creating route is `submitTeammateTask`, whose
//!   `admissionKey` makes even its duplicate a replay.
//! - **Credential values never leave.** Definitions carry secret
//!   REFERENCES only; the durable event pages pass through
//!   [`agentkit_store::redact_event`], which replaces credential-named
//!   values and oversized tool payloads before a byte crosses the wire.
//! - **Unknown usage is unknown.** The projection never synthesizes a
//!   `0` for a counter no event reported (`null` means "not reported").

use super::*;
use agentkit_agents::{AgentDefinitionService, TeammateAdmission, TeammateService};
use agentkit_sessions::{
    ContextUnit, ContextUnitKind, ProjectSelection, RunProfileService, SelectedSource,
    SelectedSourceKind, TaskProfileOverrides,
};
use agentkit_store::{AgentDefinition, AgentRecord, SqliteStore, TaskRecord, redact_event};
use serde_json::{Value, json};

/// The caller's scope, derived ONLY from the authenticated actor: an
/// explicit `scope` in the actor metadata (a deployment that maps one
/// credential to several namespaces), else the actor id itself. Nothing
/// in a body or path can name a different scope — that is what makes
/// cross-scope access a refusal instead of a parameter choice.
pub(super) fn actor_scope(actor: &Actor) -> String {
    actor
        .metadata
        .get("scope")
        .and_then(Value::as_str)
        .filter(|scope| !scope.trim().is_empty())
        .unwrap_or(&actor.id)
        .to_owned()
}

fn encoded(value: impl serde::Serialize) -> Result<Value, Problem> {
    serde_json::to_value(value).map_err(|error| Problem::host("internal_error", error.to_string()))
}

impl DesktopBackend {
    /// The cheap-clone service views over this backend's own store and
    /// runtime — the same single execution owner, never a copy of it.
    fn agents(&self) -> AgentDefinitionService {
        AgentDefinitionService::with_runtime(self.host.store().clone(), self.host.runtime().clone())
    }

    fn teammates_service(&self) -> TeammateService {
        TeammateService::with_runtime(self.host.store().clone(), self.host.runtime().clone())
    }

    fn profiles(&self) -> RunProfileService<SqliteStore> {
        RunProfileService::new(
            self.host.store().clone(),
            self.run_profile_defaults.clone().unwrap_or_default(),
        )
    }

    /// The scope guard every agent route funnels through: the agent must
    /// exist, must not be a tombstone, and must live in the caller's
    /// scope. A foreign agent answers 404 exactly like an absent one.
    pub(super) async fn owned_agent(
        &self,
        agent_id: &str,
        scope: &str,
    ) -> Result<AgentRecord, Problem> {
        let record = self
            .agents()
            .get(agent_id)
            .await
            .map_err(teammate_problem)?;
        if record.scope != scope {
            return Err(missing("Agent", agent_id));
        }
        Ok(record)
    }

    /// The scope guard for every task route: the task must exist, must
    /// be agent-bound (chat runs are not agent tasks and are not
    /// reachable here), and its agent must be the caller's.
    pub(super) async fn owned_agent_task(
        &self,
        task_id: &str,
        scope: &str,
    ) -> Result<TaskRecord, Problem> {
        let task = self
            .host
            .store()
            .get_task(task_id)
            .await
            .map_err(store_error)?
            .filter(|task| task.agent_id.is_some())
            .ok_or_else(|| missing("Agent task", task_id))?;
        if let Some(agent_id) = &task.agent_id {
            self.owned_agent(agent_id, scope).await?;
        }
        Ok(task)
    }

    /// The single visibility question both stream lanes ask: may this
    /// actor watch this task? Chat-bound tasks answer as they always
    /// did; agent-bound tasks are visible only through the same agent
    /// scope guard every JSON task route uses. `None` is the shared
    /// "no such run" answer for absent, foreign, and unowned ids alike.
    pub(super) async fn stream_task(
        &self,
        actor: &Actor,
        task_id: &str,
    ) -> Result<Option<TaskRecord>, Problem> {
        let Some(task) = self
            .host
            .store()
            .get_task(task_id)
            .await
            .map_err(store_error)?
        else {
            return Ok(None);
        };
        if task.payload["chatId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
        {
            return Ok(Some(task));
        }
        if let Some(agent_id) = &task.agent_id {
            // The guard answers 404 for foreign agents; a present-and-
            // owned agent task is streamable.
            let agent_id = agent_id.clone();
            self.owned_agent(&agent_id, &actor_scope(actor)).await?;
            return Ok(Some(task));
        }
        Ok(None)
    }

    pub(super) async fn teammates(
        &self,
        input: &OperationRequest,
    ) -> Result<Option<OperationResponse>, Problem> {
        let scope = actor_scope(&input.actor);
        let output = match input.operation.as_str() {
            "createAgent" => {
                let definition = definition_from(&input.body["definition"])?;
                let record = self
                    .agents()
                    .create(agentkit_store::CreateAgent {
                        agent_id: None,
                        scope,
                        owner_id: input.actor.id.clone(),
                        definition,
                        now: 0,
                    })
                    .await
                    .map_err(teammate_problem)?;
                output(201, encoded(&record)?)
            }
            "listAgents" => {
                let records = self.agents().list(&scope).await.map_err(teammate_problem)?;
                let mut items = Vec::with_capacity(records.len().min(100));
                for record in records.into_iter().take(limit(input, 100)) {
                    items.push(encoded(&record)?);
                }
                output(200, json!(items))
            }
            "getAgent" => {
                let agent_id = &input.params["agentId"];
                let record = self.owned_agent(agent_id, &scope).await?;
                output(200, encoded(&record)?)
            }
            "updateAgent" => {
                let agent_id = &input.params["agentId"];
                self.owned_agent(agent_id, &scope).await?;
                let definition = definition_from(&input.body["definition"])?;
                let expected = expected_revision(&input.body)?;
                let record = self
                    .agents()
                    .update(agent_id, expected, definition)
                    .await
                    .map_err(teammate_problem)?;
                output(200, encoded(&record)?)
            }
            "setAgentModel" => {
                let agent_id = &input.params["agentId"];
                self.owned_agent(agent_id, &scope).await?;
                let expected = expected_revision(&input.body)?;
                let model = optional_string(&input.body, "model")?;
                let reasoning_effort = optional_string(&input.body, "reasoningEffort")?;
                let record = self
                    .agents()
                    .set_model(agent_id, expected, model, reasoning_effort)
                    .await
                    .map_err(teammate_problem)?;
                output(200, encoded(&record)?)
            }
            "archiveAgent" => {
                let agent_id = &input.params["agentId"];
                self.owned_agent(agent_id, &scope).await?;
                let record = self
                    .agents()
                    .archive(agent_id)
                    .await
                    .map_err(teammate_problem)?;
                output(200, encoded(&record)?)
            }
            "restoreAgent" => {
                let agent_id = &input.params["agentId"];
                self.owned_agent(agent_id, &scope).await?;
                let record = self
                    .agents()
                    .restore(agent_id)
                    .await
                    .map_err(teammate_problem)?;
                output(200, encoded(&record)?)
            }
            "listAgentRevisions" => {
                let agent_id = &input.params["agentId"];
                self.owned_agent(agent_id, &scope).await?;
                let revisions = self
                    .agents()
                    .revisions(agent_id)
                    .await
                    .map_err(teammate_problem)?;
                let mut items = Vec::with_capacity(revisions.len().min(100));
                for revision in revisions.into_iter().take(limit(input, 100)) {
                    items.push(encoded(&revision)?);
                }
                output(200, json!(items))
            }
            "submitTeammateTask" => self.submit_teammate_task(input, &scope).await?,
            "listAgentTasks" => {
                let agent_id = &input.params["agentId"];
                self.owned_agent(agent_id, &scope).await?;
                let tasks = self
                    .teammates_service()
                    .tasks(agent_id)
                    .await
                    .map_err(teammate_problem)?;
                let items: Vec<Value> = tasks
                    .into_iter()
                    .take(limit(input, 100))
                    .map(|task| projections::agent_task(&task))
                    .collect();
                output(200, json!(items))
            }
            "getAgentTask" => {
                let task_id = &input.params["taskId"];
                self.owned_agent_task(task_id, &scope).await?;
                let projection = self
                    .host
                    .store()
                    .task_projection(task_id, 0)
                    .await
                    .map_err(store_error)?
                    .ok_or_else(|| missing("Agent task", task_id))?;
                output(200, projections::agent_task_projection(&projection))
            }
            "cancelAgentTask" => {
                let task_id = &input.params["taskId"];
                self.owned_agent_task(task_id, &scope).await?;
                let task = self
                    .teammates_service()
                    .cancel(task_id)
                    .await
                    .map_err(teammate_problem)?;
                output(202, projections::agent_task(&task))
            }
            "listAgentTaskEvents" => {
                let task_id = &input.params["taskId"];
                self.owned_agent_task(task_id, &scope).await?;
                let after = input
                    .query
                    .get("after")
                    .map(|raw| raw.parse::<f64>())
                    .transpose()
                    .map_err(|_| {
                        Problem::new(400, "invalid_request", "Event cursor must be numeric.")
                    })?;
                let page = limit(input, 100).min(1000);
                let records = self
                    .host
                    .store()
                    .events_after_number(task_id, after, page)
                    .await
                    .map_err(store_error)?;
                let events: Vec<Value> = records
                    .iter()
                    .map(|record| redact_event(&record.event))
                    .collect();
                let mut body = json!({"events": events});
                if records.len() == page
                    && let Some(last) = records.last()
                {
                    body["nextCursor"] = json!(last.seq.to_string());
                }
                output(200, body)
            }
            "getAgentTaskRunProfile" => {
                let task_id = &input.params["taskId"];
                self.owned_agent_task(task_id, &scope).await?;
                let explained = self
                    .profiles()
                    .explain(task_id)
                    .await
                    .map_err(teammate_problem)?;
                match explained {
                    // Absence is the honest answer: the admission pinned
                    // no profile. Not an empty profile, not a zero.
                    None => output(200, json!({"taskId": task_id})),
                    Some(profile) => output(
                        200,
                        json!({
                            "taskId": task_id,
                            "digest": profile["profileDigest"],
                            "profile": profile,
                        }),
                    ),
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(output))
    }

    async fn submit_teammate_task(
        &self,
        input: &OperationRequest,
        scope: &str,
    ) -> Result<OperationResponse, Problem> {
        let agent_id = &input.params["agentId"];
        self.owned_agent(agent_id, scope).await?;
        let body = &input.body;
        let admission_key = body["admissionKey"]
            .as_str()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| {
                Problem::new(400, "invalid_request", "`admissionKey` must not be empty.")
            })?
            .to_owned();
        let kind = body["kind"]
            .as_str()
            .filter(|kind| !kind.trim().is_empty())
            .ok_or_else(|| Problem::new(400, "invalid_request", "`kind` must not be empty."))?
            .to_owned();
        let payload = body.get("payload").cloned().unwrap_or_else(|| json!({}));
        if !payload.is_object() {
            return Err(Problem::new(
                400,
                "invalid_request",
                "`payload` must be a JSON object.",
            ));
        }
        // Profile pinning is SERVER-side: when the caller asks for a
        // profile, the sealed value is the service's resolution, never
        // whatever the caller sent. Overrides may only narrow; `resolve`
        // refuses expansion, and the pin rides the admission
        // transaction — never a second ledger.
        let wants_profile = body.get("taskProfile").is_some()
            || body.get("projectSources").is_some()
            || body.get("approvals").is_some();
        let run_profile = if wants_profile {
            let project = project_selection(body.get("projectSources"))?;
            let overrides = match body.get("taskProfile") {
                None => TaskProfileOverrides::default(),
                Some(value) => serde_json::from_value::<TaskProfileOverrides>(value.clone())
                    .map_err(|error| Problem::new(400, "invalid_request", error.to_string()))?,
            };
            let approvals = match body.get("approvals") {
                None => Vec::new(),
                Some(values) => values
                    .as_array()
                    .ok_or_else(|| {
                        Problem::new(400, "invalid_request", "`approvals` must be an array.")
                    })?
                    .iter()
                    .map(
                        |entry| -> Result<agentkit_sessions::ApprovalGrantRef, Problem> {
                            Ok(agentkit_sessions::ApprovalGrantRef {
                                session_id: string_field(entry, "sessionId")?,
                                grant_id: string_field(entry, "grantId")?,
                                digest: string_field(entry, "digest")?,
                            })
                        },
                    )
                    .collect::<Result<_, _>>()?,
            };
            Some(
                self.profiles()
                    .resolve(
                        agent_id,
                        self.host.runtime().now_ms(),
                        &project,
                        &overrides,
                        &approvals,
                    )
                    .await
                    .map_err(teammate_problem)?,
            )
        } else {
            None
        };
        let admitted = self
            .teammates_service()
            .admit(TeammateAdmission {
                task_id: body
                    .get("taskId")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .map(str::to_owned),
                agent_id: agent_id.clone(),
                admission_key,
                kind,
                payload,
                priority: agentkit_contracts::javascript_number(&body["priority"])
                    .filter(|value| value.fract() == 0.0)
                    .map(|value| value as i32)
                    .unwrap_or(0),
                depends_on: body
                    .get("dependsOn")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
                parent_task_id: body
                    .get("parentTaskId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                run_profile,
            })
            .await
            .map_err(teammate_problem)?;
        Ok(output(
            201,
            json!({"task": projections::agent_task(&admitted.task), "replayed": admitted.replayed}),
        ))
    }
}

fn definition_from(value: &Value) -> Result<AgentDefinition, Problem> {
    serde_json::from_value::<AgentDefinition>(value.clone())
        .map_err(|error| Problem::new(400, "invalid_request", error.to_string()))
}

fn expected_revision(body: &Value) -> Result<i64, Problem> {
    agentkit_contracts::javascript_number(&body["expectedRevision"])
        .filter(|value| value.fract() == 0.0 && *value >= 1.0)
        .map(|value| value as i64)
        .ok_or_else(|| {
            Problem::new(
                400,
                "invalid_request",
                "`expectedRevision` must be a positive safe integer.",
            )
        })
}

fn optional_string(body: &Value, key: &str) -> Result<Option<String>, Problem> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(Problem::new(
            400,
            "invalid_request",
            format!("`{key}` must be a string or null."),
        )),
    }
}

fn string_field(entry: &Value, key: &str) -> Result<String, Problem> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            Problem::new(
                400,
                "invalid_request",
                format!("`{key}` must be a non-empty string."),
            )
        })
}

fn project_selection(value: Option<&Value>) -> Result<ProjectSelection, Problem> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(ProjectSelection::default());
    };
    let entries = value.as_array().ok_or_else(|| {
        Problem::new(400, "invalid_request", "`projectSources` must be an array.")
    })?;
    let mut sources = Vec::with_capacity(entries.len());
    for entry in entries {
        let id = string_field(entry, "id")?;
        let kind = match entry.get("kind").and_then(Value::as_str) {
            Some("skill") => SelectedSourceKind::Skill,
            Some("resource") => SelectedSourceKind::Resource,
            _ => {
                return Err(Problem::new(
                    400,
                    "invalid_request",
                    "`kind` must be \"skill\" or \"resource\".",
                ));
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
                            return Err(Problem::new(
                                400,
                                "invalid_request",
                                "unit `kind` must be \"text\" or \"toolPair\".",
                            ));
                        }
                    };
                    let text = unit
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            Problem::new(400, "invalid_request", "unit `text` must be a string.")
                        })?
                        .to_owned();
                    let citations = unit
                        .get("citations")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    units.push(ContextUnit {
                        kind,
                        text,
                        citations,
                        required: unit
                            .get("required")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    });
                }
                units
            }
            Some(_) => {
                return Err(Problem::new(
                    400,
                    "invalid_request",
                    "`units` must be an array.",
                ));
            }
        };
        sources.push(SelectedSource { id, kind, units });
    }
    Ok(ProjectSelection { sources })
}

#[cfg(test)]
mod teammate_error_tests;
