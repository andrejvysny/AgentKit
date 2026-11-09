use super::service::{SpawnRequest, SubagentService};
use agentkit_core::{CoreError, Tool, ToolContext, ToolDefinition, ToolResult};
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

fn definition(
    name: &str,
    description: &str,
    input_schema: Value,
    timeout_ms: u64,
) -> ToolDefinition {
    ToolDefinition {
        name: name.to_owned(),
        version: "1".to_owned(),
        effect: "read".to_owned(),
        capability: CAPABILITY.to_owned(),
        description: description.to_owned(),
        input_schema,
        output_schema: None,
        timeout_ms: Some(timeout_ms),
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
    ToolResult {
        ok: false,
        data: Some(Value::String(reason.clone())),
        model_data: Some(Value::String(reason)),
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
}

type Body = Arc<dyn Fn(SubagentService, ToolContext, Value) -> BodyFuture + Send + Sync>;
type BodyFuture = std::pin::Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>;

#[async_trait]
impl Tool for SubagentTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        if !self.service.config().enabled {
            return Ok(refused("subagents_disabled".into(), &context));
        }
        match (self.body)(self.service.clone(), context.clone(), input).await {
            Ok(data) => Ok(payload(data, &context)),
            Err(refusal) => Ok(refused(refusal, &context)),
        }
    }
}

fn tool(
    service: &SubagentService,
    name: &str,
    description: &str,
    schema: Value,
    timeout_ms: u64,
    body: impl Fn(SubagentService, ToolContext, Value) -> BodyFuture + Send + Sync + 'static,
) -> Arc<dyn Tool> {
    Arc::new(SubagentTool {
        definition: definition(name, description, schema, timeout_ms),
        service: service.clone(),
        body: Arc::new(body),
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
impl agentkit_host::ToolContributor for SubagentContributor {
    fn namespace(&self) -> &str {
        "subagent"
    }
    async fn contribute(
        &self,
        _context: &agentkit_host::ToolContributionContext,
    ) -> agentkit_host::GovernanceResult<Vec<agentkit_host::ContributedTool>> {
        if !self.service.config().enabled {
            return Ok(vec![]);
        }
        let service = self.service.clone();
        Ok(vec![
            agentkit_host::ContributedTool::Read(tool(
                &service,
                "subagent_spawn",
                "Spawn a child session that runs one delegated assignment in its own context. \
                 Roles: researcher, explorer, implementer, reviewer, general. Independent \
                 children should be spawned together and collected with subagent_await. The \
                 child inherits your workspace and permission ceiling; read-only roles cannot \
                 write. Set exclusiveWorkspace for an implementer whose edits must not race any \
                 other writer on the workspace.",
                json!({"type":"object","required":["role","prompt"],"properties":{
                    "role":{"type":"string"},
                    "prompt":{"type":"string","description":"Self-contained assignment: the child sees nothing of your context."},
                    "title":{"type":"string"},
                    "model":{"type":"string","description":"Override the role's model."},
                    "exclusiveWorkspace":{"type":"boolean"},
                    "maxIterations":{"type":"integer","minimum":1},
                    "maxProviderRequests":{"type":"integer","minimum":1},
                    "overallTimeoutMs":{"type":"integer","minimum":1000},
                    "spawnKey":{"type":"string","description":"Stable idempotency key; omit to derive it from this call."}
                }}),
                30_000,
                |service, context, input| {
                    Box::pin(async move {
                        let (parent_chat_id, parent_task_id) =
                            parent_coordinates(&context).map_err(|error| error.code)?;
                        // Without a logical call id the derived key would
                        // collapse every spawn of one role into one child.
                        if context.tool_call_id.is_none()
                            && input.get("spawnKey").and_then(Value::as_str).is_none()
                        {
                            return Err(
                                "spawn_key_required: no tool-call identity; pass an explicit \
                                 spawnKey"
                                    .into(),
                            );
                        }
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
                            exclusive_workspace: input["exclusiveWorkspace"]
                                .as_bool()
                                .unwrap_or(false),
                            max_iterations: input["maxIterations"].as_u64().map(|v| v as u32),
                            max_provider_requests: input["maxProviderRequests"].as_u64(),
                            overall_timeout_ms: input["overallTimeoutMs"].as_u64(),
                        };
                        let child = service
                            .spawn(&request)
                            .await
                            .map_err(|error| error.to_string())?;
                        serde_json::to_value(child).map_err(|error| error.to_string())
                    })
                },
            )),
            agentkit_host::ContributedTool::Read(tool(
                &service,
                "subagent_list",
                "List your delegated children with live status (queued/running/completed/failed/\
                 cancelled), role, result and error.",
                json!({"type":"object","properties":{}}),
                15_000,
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
            agentkit_host::ContributedTool::Read(tool(
                &service,
                "subagent_status",
                "One child's live detail: status, final result, error, task status, waiting \
                 state and queued inputs.",
                json!({"type":"object","required":["childId"],"properties":{
                    "childId":{"type":"string"}
                }}),
                15_000,
                |service, context, input| {
                    Box::pin(async move {
                        let (parent_chat_id, _) =
                            parent_coordinates(&context).map_err(|error| error.code)?;
                        let child = input["childId"]
                            .as_str()
                            .ok_or_else(|| "childId is required".to_owned())?;
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
            agentkit_host::ContributedTool::Read(tool(
                &service,
                "subagent_message",
                "Send a follow-up to one of your children: mode \"queue\" appends an input that \
                 runs as its next turn; mode \"steer\" injects into the live turn at its next \
                 tool boundary.",
                json!({"type":"object","required":["childId","content"],"properties":{
                    "childId":{"type":"string"},
                    "content":{"type":"string"},
                    "mode":{"type":"string","enum":["queue","steer"]},
                    "messageKey":{"type":"string","description":"Stable idempotency key for this message."}
                }}),
                15_000,
                |service, context, input| {
                    Box::pin(async move {
                        let (parent_chat_id, _) =
                            parent_coordinates(&context).map_err(|error| error.code)?;
                        let child = input["childId"]
                            .as_str()
                            .ok_or_else(|| "childId is required".to_owned())?;
                        let content = input["content"]
                            .as_str()
                            .ok_or_else(|| "content is required".to_owned())?;
                        let mode = input["mode"].as_str().unwrap_or("queue");
                        let message_key = input["messageKey"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| spawn_key(&context, &input));
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
            agentkit_host::ContributedTool::Read(tool(
                &service,
                "subagent_await",
                "Wait for your children to settle and collect their results. Omit childIds to \
                 await every active child. Returns on the first settled moment, on timeout \
                 (timedOut:true, partial results), or when the turn is cancelled — a timeout \
                 is not a failure: keep working or await again.",
                json!({"type":"object","properties":{
                    "childIds":{"type":"array","items":{"type":"string"}},
                    "timeoutMs":{"type":"integer","minimum":0}
                }}),
                660_000,
                |service, context, input| {
                    Box::pin(async move {
                        let (parent_chat_id, _) =
                            parent_coordinates(&context).map_err(|error| error.code)?;
                        let ids: Option<Vec<String>> = input["childIds"].as_array().map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        });
                        let outcome = service
                            .await_children(
                                &parent_chat_id,
                                ids.as_deref(),
                                input["timeoutMs"].as_u64(),
                                &context.cancellation,
                            )
                            .await
                            .map_err(|error| error.to_string())?;
                        serde_json::to_value(outcome).map_err(|error| error.to_string())
                    })
                },
            )),
            agentkit_host::ContributedTool::Read(tool(
                &service,
                "subagent_cancel",
                "Cooperatively cancel one of your children's live turn. Cancellation is \
                 durable; the child settles cancelled at its next checkpoint.",
                json!({"type":"object","required":["childId"],"properties":{
                    "childId":{"type":"string"}
                }}),
                15_000,
                |service, context, input| {
                    Box::pin(async move {
                        let (parent_chat_id, _) =
                            parent_coordinates(&context).map_err(|error| error.code)?;
                        let child = input["childId"]
                            .as_str()
                            .ok_or_else(|| "childId is required".to_owned())?;
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
            agentkit_host::ContributedTool::Read(tool(
                &service,
                "subagent_resume",
                "Resume one of your children: an interrupted turn continues in place; a failed \
                 or cancelled one is re-dispatched as a new turn that continues the \
                 assignment.",
                json!({"type":"object","required":["childId"],"properties":{
                    "childId":{"type":"string"},
                    "resumeKey":{"type":"string","description":"Stable idempotency key for this resume."}
                }}),
                15_000,
                |service, context, input| {
                    Box::pin(async move {
                        let (parent_chat_id, parent_task_id) =
                            parent_coordinates(&context).map_err(|error| error.code)?;
                        let child = input["childId"]
                            .as_str()
                            .ok_or_else(|| "childId is required".to_owned())?;
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
                        let resume_key = input["resumeKey"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| {
                                format!("{parent_task_id}:{}", spawn_key(&context, &input))
                            });
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
