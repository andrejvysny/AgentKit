//! JS-contributed tools.
//!
//! A JS tool is a definition plus a bridge call: the executor sees a real
//! `Tool`, and `execute` hands `{name, toolCallId, attemptId, runId, input}`
//! to the JS router and awaits the wrapper's settle. The tool honors the
//! task lease cancellation; a rejected callback is a tool failure, so the
//! durable execution path records an uncertain/failed effect instead of a
//! fake success. The tool's effect context carries its identity and input
//! schema, so a redefinition invalidates replayed receipts.

use std::sync::{Arc, Mutex};

use agentkit_core::{CoreError, Tool, ToolContext, ToolDefinition, ToolResult};
use agentkit_host::{ContributedTool, ToolContributionContext, ToolContributor};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::jsbridge::{JsBridge, PendingKind};

struct JsTool {
    definition: ToolDefinition,
    bridge: Arc<JsBridge>,
}

#[async_trait]
impl Tool for JsTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn effect_context(&self) -> Option<Value> {
        Some(json!({
            "kind": "js-tool",
            "name": self.definition.name,
            "version": self.definition.version,
            "inputSchema": self.definition.input_schema,
        }))
    }

    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        let call_id = uuid::Uuid::new_v4().to_string();
        let payload = json!({
            "name": self.definition.name,
            "toolCallId": context.tool_call_id,
            "attemptId": context.attempt_id,
            "runId": context.run_id,
            "input": input,
        });
        let value = Arc::clone(&self.bridge)
            .request(PendingKind::Tool, &call_id, &payload, &context.cancellation)
            .await
            .map_err(|message| CoreError::new("tool_callback_failed", &message))?;
        Ok(map_result(&value))
    }
}

/// The JS tool result contract: `{ ok?, data?, sources?, warnings?,
/// truncated?, limits?, summary?, status? }`. Missing fields take the
/// host defaults; anything unparseable lands in the defaults rather than
/// fabricating values.
fn map_result(value: &Value) -> ToolResult {
    ToolResult {
        ok: value.get("ok").and_then(Value::as_bool).unwrap_or(true),
        data: value.get("data").cloned().filter(|v| !v.is_null()),
        sources: value
            .get("sources")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        warnings: value
            .get("warnings")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        truncated: value
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        limits: value
            .get("limits")
            .cloned()
            .and_then(|limits| serde_json::from_value(limits).ok())
            .unwrap_or_default(),
        model_data: value.get("modelData").cloned().filter(|v| !v.is_null()),
        summary: value
            .get("summary")
            .and_then(Value::as_str)
            .map(str::to_owned),
        status: value
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

pub(crate) struct JsToolContributor {
    definitions: Mutex<Vec<ToolDefinition>>,
    bridge: Arc<JsBridge>,
}

impl JsToolContributor {
    pub(crate) fn new(bridge: Arc<JsBridge>) -> Self {
        Self {
            definitions: Mutex::new(Vec::new()),
            bridge,
        }
    }

    /// Parse and register one JS tool definition. Rejects anything the
    /// executor could mis-execute: duplicate names, non-object schemas,
    /// effects outside the read/write taxonomy.
    pub(crate) fn register(&self, definition_json: &str) -> std::result::Result<(), String> {
        let parsed: Value = serde_json::from_str(definition_json)
            .map_err(|error| format!("the tool definition is not valid JSON: {error}"))?;
        let definition = parse_definition(&parsed)?;
        let mut definitions = self
            .definitions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if definitions
            .iter()
            .any(|seen| seen.name == definition.name && seen.version == definition.version)
        {
            return Err(format!(
                "tool {}@{} is already registered",
                definition.name, definition.version
            ));
        }
        definitions.push(definition);
        Ok(())
    }
}

fn required_string<'a>(object: &'a Value, key: &str) -> std::result::Result<&'a str, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| format!("the tool definition requires a non-empty string {key}"))
}

fn parse_definition(parsed: &Value) -> std::result::Result<ToolDefinition, String> {
    let name = required_string(parsed, "name")?.to_owned();
    let version = parsed
        .get("version")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .unwrap_or("1.0.0")
        .to_owned();
    let effect = match parsed.get("effect") {
        None => "read".to_owned(),
        Some(Value::String(effect)) if effect == "read" || effect == "write" => effect.clone(),
        Some(other) => {
            return Err(format!(
                "tool effect must be \"read\" or \"write\", got {other}"
            ));
        }
    };
    let capability = parsed
        .get("capability")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .unwrap_or("general")
        .to_owned();
    let description = parsed
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let input_schema = parsed
        .get("inputSchema")
        .filter(|schema| schema.is_object())
        .cloned()
        .ok_or_else(|| "the tool definition requires an object inputSchema".to_owned())?;
    let output_schema = parsed
        .get("outputSchema")
        .filter(|schema| schema.is_object())
        .cloned();
    let timeout_ms = parsed.get("timeoutMs").and_then(Value::as_u64);
    Ok(ToolDefinition {
        name,
        version,
        effect,
        capability,
        description,
        input_schema,
        output_schema,
        timeout_ms,
    })
}

#[async_trait]
impl ToolContributor for JsToolContributor {
    fn namespace(&self) -> &str {
        "js"
    }

    async fn contribute(
        &self,
        _context: &ToolContributionContext,
    ) -> agentkit_host::HostResult<Vec<ContributedTool>> {
        let definitions = self
            .definitions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Ok(definitions
            .into_iter()
            .map(|definition| {
                let read = definition.effect == "read";
                let tool: Arc<dyn Tool> = Arc::new(JsTool {
                    definition,
                    bridge: Arc::clone(&self.bridge),
                });
                if read {
                    ContributedTool::Read(tool)
                } else {
                    ContributedTool::DirectWrite(tool)
                }
            })
            .collect())
    }
}
