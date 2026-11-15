use super::config::{BUILTIN_ROLES, SubagentConfig};
use super::service::{
    AwaitDecision, AwaitMode, SUBAGENT_ITERATIONS_CEILING, SUBAGENT_OVERALL_MS_CEILING,
    SUBAGENT_PROVIDER_REQUESTS_CEILING, SpawnRequest, SubagentService,
};
use agentkit_core::{
    AliasTable, CoreError, Tool, ToolContext, ToolDefinition, ToolResult, failure_catalog,
    normalise,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::future::Future;
use std::sync::Arc;

/// Every tool this pack contributes; the binding's unbound-name check and
/// the role guard both speak these names.
pub const SUBAGENT_TOOL_NAMES: [&str; 7] = [
    "subagent_spawn",
    "subagent_list",
    "subagent_status",
    "subagent_message",
    "subagent_await",
    "subagent_cancel",
    "subagent_resume",
];

const CAPABILITY: &str = "orchestration";

/// Upper bound the schema states for the child-id list.
const MAX_CHILD_IDS: usize = 64;

/// Old parameter names as `(alias, canonical)`, accepted for one release
/// (call pipeline aliases).
pub const SUBAGENT_ARG_ALIASES: &[(&str, &str)] = &[
    ("childId", "child_id"),
    ("childIds", "child_ids"),
    ("exclusiveWorkspace", "exclusive_workspace"),
    ("maxIterations", "max_iterations"),
    ("maxProviderRequests", "max_provider_requests"),
    ("overallTimeoutMs", "timeout"),
    ("timeoutMs", "timeout"),
];

/// The seven static definitions, with the built-in roles and default await
/// bound. The contributor builds the same definitions with the host's
/// configured roles and bounds.
pub fn subagent_tool_definitions() -> Vec<ToolDefinition> {
    let roles: Vec<String> = BUILTIN_ROLES
        .iter()
        .map(|role| (*role).to_owned())
        .collect();
    let max_await_ms = SubagentConfig::default().limits.max_await_ms;
    SUBAGENT_TOOL_NAMES
        .iter()
        .map(|name| definition_for(name, &roles, max_await_ms))
        .collect()
}

fn child_id_schema() -> Value {
    json!({"type": "string", "minLength": 1,
           "description": "Id of a child you spawned, from subagent_spawn or subagent_list."})
}

fn definition_for(name: &str, roles: &[String], max_await_ms: u64) -> ToolDefinition {
    let (description, input_schema, timeout_ms, examples): (String, Value, u64, Vec<Value>) =
        match name {
            "subagent_spawn" => (
                format!(
                    "Spawn a child session that runs one delegated assignment in its own \
                     context; it sees nothing of yours except the prompt. Roles: {}. Read-only \
                     roles cannot write, and the child inherits your workspace and permission \
                     ceiling. Spawn independent children together, then collect them with \
                     subagent_await. Returns the child id for subagent_status, subagent_message, \
                     subagent_cancel and subagent_resume. Set exclusive_workspace for an \
                     implementer whose edits must not race another writer.",
                    roles.join(", ")
                ),
                json!({"type":"object","required":["role","prompt"],"additionalProperties":false,
                    "properties":{
                    "role":{"type":"string","enum":roles,"description":"What kind of child to spawn."},
                    "prompt":{"type":"string","minLength":1,"description":"Self-contained assignment: the child sees nothing of your context."},
                    "title":{"type":"string","description":"Short label for the child."},
                    "model":{"type":"string","description":"Model for this child instead of the default."},
                    "exclusive_workspace":{"type":"boolean","description":"Give the child the workspace to itself while it runs."},
                    "max_iterations":{"type":"integer","minimum":1,"maximum":SUBAGENT_ITERATIONS_CEILING,"description":"Upper bound on the child's tool iterations."},
                    "max_provider_requests":{"type":"integer","minimum":1,"maximum":SUBAGENT_PROVIDER_REQUESTS_CEILING,"description":"Upper bound on the child's provider requests."},
                    "timeout":{"type":"integer","minimum":1000,"maximum":SUBAGENT_OVERALL_MS_CEILING,"description":"Overall time limit for the child, in milliseconds."}
                }}),
                30_000,
                vec![
                    json!({"role": "explorer", "prompt": "Find where retries are configured and report the file paths and defaults."}),
                    json!({"role": "implementer", "prompt": "Add a retry limit to src/client.rs and run the tests.", "exclusive_workspace": true, "timeout": 600000}),
                ],
            ),
            "subagent_list" => (
                "List your delegated children with live status (queued, running, completed, \
                 failed, cancelled), role, result and error. Use it to see what is still running \
                 before you await or cancel; use subagent_status for the full detail of one \
                 child. It takes no parameters and shows only the children you spawned."
                    .to_owned(),
                json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
                15_000,
                vec![json!({})],
            ),
            "subagent_status" => (
                "Read one child's live detail: status, final result, error, task status, waiting \
                 state and queued inputs. Use it when subagent_list is not enough or to read the \
                 result of a finished child; to wait for several children use subagent_await. \
                 Only children you spawned can be read."
                    .to_owned(),
                json!({"type":"object","required":["child_id"],"additionalProperties":false,
                    "properties":{"child_id":child_id_schema()}}),
                15_000,
                vec![json!({"child_id": "child-1"})],
            ),
            "subagent_message" => (
                "Send a follow-up to one of your running children. Mode queue (the default) \
                 appends an input that runs as the child's next turn; mode steer injects it into \
                 the live turn at its next tool boundary. Use it to correct or extend an \
                 assignment; to stop a child use subagent_cancel. Returns a receipt for the \
                 message."
                    .to_owned(),
                json!({"type":"object","required":["child_id","content"],"additionalProperties":false,
                    "properties":{
                    "child_id":child_id_schema(),
                    "content":{"type":"string","minLength":1,"description":"The follow-up text for the child."},
                    "mode":{"type":"string","enum":["queue","steer"],"description":"queue (default) or steer."}
                }}),
                15_000,
                vec![
                    json!({"child_id": "child-1", "content": "Also check the tests directory.", "mode": "steer"}),
                ],
            ),
            "subagent_await" => (
                "Wait for your children to settle and collect their results. Leave child_ids out \
                 to await every active child. Your turn parks while you wait and frees its \
                 slot; it resumes when all awaited children settled (mode all, the default) or \
                 the first one did (mode any), on timeout (timedOut true, with partial \
                 results), or when the owner sends a message. A timeout is not a failure: keep \
                 working or await again. Spawn the children first with subagent_spawn."
                    .to_owned(),
                json!({"type":"object","required":[],"additionalProperties":false,
                    "properties":{
                    "child_ids":{"type":"array","items":{"type":"string","minLength":1},"minItems":1,"maxItems":MAX_CHILD_IDS,"description":"Children to await. Default: every active child."},
                    "timeout":{"type":"integer","minimum":0,"maximum":max_await_ms,"description":"How long to wait, in milliseconds."},
                    "mode":{"type":"string","enum":["all","any"],"description":"all (default) wakes when every awaited child settled; any wakes on the first."}
                }}),
                660_000,
                vec![
                    json!({}),
                    json!({"child_ids": ["child-1", "child-2"], "timeout": 120000}),
                ],
            ),
            "subagent_cancel" => (
                "Cooperatively cancel one of your children's live turn. Cancellation is durable: \
                 the child settles as cancelled at its next checkpoint. Use it for work that is \
                 no longer needed; to change a child's assignment use subagent_message instead, \
                 and to continue a cancelled child use subagent_resume."
                    .to_owned(),
                json!({"type":"object","required":["child_id"],"additionalProperties":false,
                    "properties":{"child_id":child_id_schema()}}),
                15_000,
                vec![json!({"child_id": "child-1"})],
            ),
            _ => (
                "Resume one of your children. An interrupted turn continues in place; a failed \
                 or cancelled child is re-dispatched as a new turn that continues the \
                 assignment. Use it after subagent_status shows the child stopped before \
                 finishing; to give it new instructions use subagent_message."
                    .to_owned(),
                json!({"type":"object","required":["child_id"],"additionalProperties":false,
                    "properties":{"child_id":child_id_schema()}}),
                15_000,
                vec![json!({"child_id": "child-1"})],
            ),
        };
    ToolDefinition {
        name: name.to_owned(),
        version: "1".to_owned(),
        effect: "read".to_owned(),
        capability: CAPABILITY.to_owned(),
        description,
        input_schema,
        output_schema: None,
        timeout_ms: Some(timeout_ms),
        examples,
    }
}

fn payload(data: Value, context: &ToolContext) -> ToolResult {
    ToolResult {
        ok: true,
        data: Some(data.clone()),
        model_data: Some(Value::String(data.to_string())),
        sources: vec![],
        warnings: vec![],
        truncated: false,
        limits: context.limits.clone(),
        summary: None,
        status: None,
        model_content: None,
    }
}

fn refused(reason: String, context: &ToolContext) -> ToolResult {
    // Bare codes such as `spawn_key_required` become full sentences with a
    // next step (TU-10); the model reads the rendered failure.
    let text = failure_catalog::render(&failure_catalog::from_refusal(&reason));
    ToolResult {
        ok: false,
        data: Some(Value::String(text.clone())),
        model_data: Some(Value::String(text)),
        sources: vec![],
        warnings: vec![],
        truncated: false,
        limits: context.limits.clone(),
        summary: None,
        status: Some("refused".into()),
        model_content: None,
    }
}

/// One parent turn's stable identity for child naming. `tool_call_id` is
/// the model's logical call id — identical across attempt retries, so a
/// retried spawn re-admits the same child instead of forking a second one.
fn spawn_key(context: &ToolContext, input: &Value) -> String {
    if let Some(key) = input.get("spawnKey").and_then(Value::as_str) {
        return format!("{}|k:{key}", context.tool_call_id.as_deref().unwrap_or(""));
    }
    format!(
        "{}|a:{}",
        context.tool_call_id.as_deref().unwrap_or(""),
        input["role"].as_str().unwrap_or(""),
    )
}

/// Idempotency key of one logical call: the turn (run id) and the model's
/// call id. A retry of the same call yields the same key; the same call id
/// in another turn does not collide. Models never supply it (TU-06); the
/// old `*Key` parameters stay accepted for one release and win when sent.
fn call_key(context: &ToolContext) -> String {
    format!(
        "{}|{}",
        context.run_id,
        context.tool_call_id.as_deref().unwrap_or("")
    )
}

/// Without a logical call id the derived key would collapse every call of
/// one tool into one; only an explicitly sent legacy key can stand in.
fn require_call_identity(
    context: &ToolContext,
    input: &Value,
    legacy_param: &str,
) -> Result<(), String> {
    let sent = input.get(legacy_param).and_then(Value::as_str).is_some();
    if context.tool_call_id.is_none() && !sent {
        return Err(
            "spawn_key_required: this call carries no tool-call identity, so the runtime \
             cannot tell a retry from a second call. Why: the idempotency key is derived \
             from the turn and the call id. Next: call the tool again from a normal tool \
             turn."
                .into(),
        );
    }
    Ok(())
}

/// The caller's coordinates: only a session turn with a live task may
/// coordinate children, and it may coordinate only its OWN.
fn parent_coordinates(context: &ToolContext) -> Result<(String, String), CoreError> {
    let chat_id = context.chat_id.clone().ok_or_else(|| {
        CoreError::new(
            "subagent_parent_required",
            "subagent tools run inside a durable session turn",
        )
    })?;
    let task_id = context.run_id.clone();
    Ok((chat_id, task_id))
}

/// Generic tool shape for the pack: validates, dispatches, shapes business
/// refusals as `ok:false` results (the model reads them) and infrastructure
/// faults as `CoreError` (the run records them).
struct SubagentTool {
    definition: ToolDefinition,
    service: SubagentService,
    body: Body,
    alias_table: AliasTable,
}

type Body = Arc<dyn Fn(SubagentService, ToolContext, Value) -> BodyFuture + Send + Sync>;
type BodyFuture = std::pin::Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>;

#[async_trait]
impl Tool for SubagentTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    fn arg_aliases(&self) -> Vec<(String, String)> {
        SUBAGENT_ARG_ALIASES
            .iter()
            .map(|(alias, canonical)| ((*alias).to_owned(), (*canonical).to_owned()))
            .collect()
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        if !self.service.config().enabled {
            return Ok(refused("subagents_disabled".into(), &context));
        }
        // The call pipeline normalises real calls; direct callers get the
        // same canonical names.
        let input = normalise(
            &self.alias_table,
            &self.definition.name,
            &self.definition.input_schema,
            input,
        )
        .args;
        match (self.body)(self.service.clone(), context.clone(), input).await {
            Ok(data) => Ok(payload(data, &context)),
            Err(refusal) => Ok(refused(refusal, &context)),
        }
    }
}

fn tool(
    service: &SubagentService,
    definition: ToolDefinition,
    body: impl Fn(SubagentService, ToolContext, Value) -> BodyFuture + Send + Sync + 'static,
) -> Arc<dyn Tool> {
    let mut alias_table = AliasTable::standard();
    for (alias, canonical) in SUBAGENT_ARG_ALIASES {
        alias_table.add_for_tool(&definition.name, alias, canonical);
    }
    Arc::new(SubagentTool {
        definition,
        service: service.clone(),
        body: Arc::new(body),
        alias_table,
    })
}

/// Contributes the seven coordination tools to EVERY session turn (Chat and
/// Coding alike) while the subsystem is enabled; the durable guard chain
/// still judges each call, and children only see what their role allows.
pub struct SubagentContributor {
    service: SubagentService,
}

impl SubagentContributor {
    pub fn new(service: SubagentService) -> Self {
        Self { service }
    }
}

#[async_trait]
impl agentkit_governance::ToolContributor for SubagentContributor {
    fn namespace(&self) -> &str {
        "subagent"
    }
    fn toolsets(&self) -> Vec<agentkit_core::Toolset> {
        super::toolsets::delegation_toolsets()
    }
    async fn contribute(
        &self,
        _context: &agentkit_governance::ToolContributionContext,
    ) -> agentkit_governance::GovernanceResult<Vec<agentkit_governance::ContributedTool>> {
        if !self.service.config().enabled {
            return Ok(vec![]);
        }
        let service = self.service.clone();
        let config = service.config();
        let roles: Vec<String> = if config.roles.is_empty() {
            BUILTIN_ROLES
                .iter()
                .map(|role| (*role).to_owned())
                .collect()
        } else {
            config.roles.keys().cloned().collect()
        };
        let max_await_ms = config.limits.max_await_ms;
        Ok(vec![
            agentkit_governance::ContributedTool::Read(tool(
                &service,
                definition_for("subagent_spawn", &roles, max_await_ms),
                |service, context, input| {
                    Box::pin(async move {
                        let (parent_chat_id, parent_task_id) =
                            parent_coordinates(&context).map_err(|error| error.code)?;
                        require_call_identity(&context, &input, "spawnKey")?;
                        let request = SpawnRequest {
                            parent_chat_id,
                            parent_task_id,
                            spawn_key: spawn_key(&context, &input),
                            role: input["role"]
                                .as_str()
                                .ok_or_else(|| "role is required".to_owned())?
                                .to_owned(),
                            prompt: input["prompt"]
                                .as_str()
                                .ok_or_else(|| "prompt is required".to_owned())?
                                .to_owned(),
                            title: input["title"].as_str().map(str::to_owned),
                            model: input["model"].as_str().map(str::to_owned),
                            exclusive_workspace: input["exclusive_workspace"]
                                .as_bool()
                                .unwrap_or(false),
                            max_iterations: input["max_iterations"].as_u64().map(|v| v as u32),
                            max_provider_requests: input["max_provider_requests"].as_u64(),
                            overall_timeout_ms: input["timeout"].as_u64(),
                        };
                        let child = service
                            .spawn(&request)
                            .await
                            .map_err(|error| error.to_string())?;
                        serde_json::to_value(child).map_err(|error| error.to_string())
                    })
                },
            )),
            agentkit_governance::ContributedTool::Read(tool(
                &service,
                definition_for("subagent_list", &roles, max_await_ms),
                |service, context, _input| {
                    Box::pin(async move {
                        let (parent_chat_id, _) =
                            parent_coordinates(&context).map_err(|error| error.code)?;
                        let views = service
                            .list(&parent_chat_id)
                            .await
                            .map_err(|error| error.to_string())?;
                        Ok(json!({"children": serde_json::to_value(views)
                            .map_err(|error| error.to_string())?}))
                    })
                },
            )),
            agentkit_governance::ContributedTool::Read(tool(
                &service,
                definition_for("subagent_status", &roles, max_await_ms),
                |service, context, input| {
                    Box::pin(async move {
                        let (parent_chat_id, _) =
                            parent_coordinates(&context).map_err(|error| error.code)?;
                        let child = input["child_id"]
                            .as_str()
                            .ok_or_else(|| "child_id is required".to_owned())?;
                        let view = service
                            .status(child)
                            .await
                            .map_err(|error| error.to_string())?;
                        if view.parent_chat_id != parent_chat_id {
                            return Err(
                                "subagent_not_yours: coordinate only your own children".into()
                            );
                        }
                        serde_json::to_value(view).map_err(|error| error.to_string())
                    })
                },
            )),
            agentkit_governance::ContributedTool::Read(tool(
                &service,
                definition_for("subagent_message", &roles, max_await_ms),
                |service, context, input| {
                    Box::pin(async move {
                        let (parent_chat_id, _) =
                            parent_coordinates(&context).map_err(|error| error.code)?;
                        let child = input["child_id"]
                            .as_str()
                            .ok_or_else(|| "child_id is required".to_owned())?;
                        let content = input["content"]
                            .as_str()
                            .ok_or_else(|| "content is required".to_owned())?;
                        let mode = input["mode"].as_str().unwrap_or("queue");
                        require_call_identity(&context, &input, "messageKey")?;
                        let message_key = input["messageKey"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| call_key(&context));
                        let row = service
                            .store()
                            .get_subagent(child)
                            .await
                            .map_err(|error| error.to_string())?;
                        match row {
                            Some(row) if row.parent_chat_id == parent_chat_id => {}
                            Some(_) => {
                                return Err(
                                    "subagent_not_yours: coordinate only your own children".into(),
                                );
                            }
                            None => return Err(format!("subagent_not_found: {child}")),
                        }
                        let receipt = service
                            .message(child, Value::String(content.to_owned()), mode, &message_key)
                            .await
                            .map_err(|error| error.to_string())?;
                        serde_json::to_value(receipt).map_err(|error| error.to_string())
                    })
                },
            )),
            agentkit_governance::ContributedTool::Read(tool(
                &service,
                definition_for("subagent_await", &roles, max_await_ms),
                |service, context, input| {
                    Box::pin(async move {
                        let (parent_chat_id, parent_task_id) =
                            parent_coordinates(&context).map_err(|error| error.code)?;
                        let ids: Option<Vec<String>> = input["child_ids"].as_array().map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        });
                        let mode = AwaitMode::from_arg(input["mode"].as_str())
                            .map_err(|error| error.to_string())?;
                        let timeout = input["timeout"].as_u64();
                        // A durable turn parks (freeing its engine slot);
                        // a host that cannot park, or a call with no
                        // identity, keeps the in-turn poll.
                        if context.suspension.is_some() && context.tool_call_id.is_some() {
                            let decision = service
                                .plan_await(
                                    &parent_chat_id,
                                    &parent_task_id,
                                    &context.waiting_request_id(),
                                    ids.as_deref(),
                                    timeout,
                                    mode,
                                )
                                .await
                                .map_err(|error| error.to_string())?;
                            match decision {
                                AwaitDecision::Ready(outcome) => {
                                    return serde_json::to_value(outcome)
                                        .map_err(|error| error.to_string());
                                }
                                AwaitDecision::Park(spec) => {
                                    if context.request_human(spec) {
                                        return Ok(json!({"parked": true}));
                                    }
                                }
                            }
                        }
                        let outcome = service
                            .await_children_mode(
                                &parent_chat_id,
                                ids.as_deref(),
                                timeout,
                                mode,
                                &context.cancellation,
                            )
                            .await
                            .map_err(|error| error.to_string())?;
                        serde_json::to_value(outcome).map_err(|error| error.to_string())
                    })
                },
            )),
            agentkit_governance::ContributedTool::Read(tool(
                &service,
                definition_for("subagent_cancel", &roles, max_await_ms),
                |service, context, input| {
                    Box::pin(async move {
                        let (parent_chat_id, _) =
                            parent_coordinates(&context).map_err(|error| error.code)?;
                        let child = input["child_id"]
                            .as_str()
                            .ok_or_else(|| "child_id is required".to_owned())?;
                        let row = service
                            .store()
                            .get_subagent(child)
                            .await
                            .map_err(|error| error.to_string())?;
                        match row {
                            Some(row) if row.parent_chat_id == parent_chat_id => {}
                            Some(_) => {
                                return Err(
                                    "subagent_not_yours: coordinate only your own children".into(),
                                );
                            }
                            None => return Err(format!("subagent_not_found: {child}")),
                        }
                        let view = service
                            .cancel(child)
                            .await
                            .map_err(|error| error.to_string())?;
                        serde_json::to_value(view).map_err(|error| error.to_string())
                    })
                },
            )),
            agentkit_governance::ContributedTool::Read(tool(
                &service,
                definition_for("subagent_resume", &roles, max_await_ms),
                |service, context, input| {
                    Box::pin(async move {
                        let (parent_chat_id, parent_task_id) =
                            parent_coordinates(&context).map_err(|error| error.code)?;
                        let child = input["child_id"]
                            .as_str()
                            .ok_or_else(|| "child_id is required".to_owned())?;
                        let row = service
                            .store()
                            .get_subagent(child)
                            .await
                            .map_err(|error| error.to_string())?;
                        match row {
                            Some(row) if row.parent_chat_id == parent_chat_id => {}
                            Some(_) => {
                                return Err(
                                    "subagent_not_yours: coordinate only your own children".into(),
                                );
                            }
                            None => return Err(format!("subagent_not_found: {child}")),
                        }
                        require_call_identity(&context, &input, "resumeKey")?;
                        let resume_key = input["resumeKey"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| call_key(&context));
                        let view = service
                            .resume_for_parent(child, &resume_key, Some(&parent_task_id))
                            .await
                            .map_err(|error| error.to_string())?;
                        serde_json::to_value(view).map_err(|error| error.to_string())
                    })
                },
            )),
        ])
    }
}
