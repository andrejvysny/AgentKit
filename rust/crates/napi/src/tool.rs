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

    /// The registered definitions — the same list `contribute` hands the
    /// executor. The MCP server advertises exactly this catalogue, so a
    /// remote client sees precisely the tools a run can call.
    pub(crate) fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.definitions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The catalogue the MCP server advertises: exactly the definitions
    /// [`Self::lookup`] can resolve, deduplicated with the resolver's own
    /// first-match rule. Kept next to `lookup` on purpose — one rule, so a
    /// remote client can never be shown one callback and be served another.
    pub(crate) fn advertised_definitions(&self) -> Vec<ToolDefinition> {
        advertised(&self.tool_definitions())
    }

    /// One registered definition by tool name.
    pub(crate) fn lookup(&self, name: &str) -> Option<ToolDefinition> {
        resolve_first(&self.tool_definitions(), name).cloned()
    }

    /// Execute a registered definition through the exact `JsTool::execute`
    /// lane a run-scoped call takes: same bridge request, same pending
    /// kind, same result mapping. The caller supplies the execution
    /// context; this only supplies the tool.
    pub(crate) async fn invoke(
        &self,
        definition: ToolDefinition,
        context: ToolContext,
        input: Value,
    ) -> Result<ToolResult, CoreError> {
        JsTool {
            definition,
            bridge: Arc::clone(&self.bridge),
        }
        .execute(context, input)
        .await
    }
}

/// The resolver `call_tool` uses: the FIRST definition registered under a
/// name wins (a duplicate under a new version is a redefinition, and the
/// router only ever reaches the live registration under that name).
fn resolve_first<'a>(definitions: &'a [ToolDefinition], name: &str) -> Option<&'a ToolDefinition> {
    definitions
        .iter()
        .find(|definition| definition.name == name)
}

/// The catalogue a remote client may be shown: one entry per name, holding
/// exactly what [`resolve_first`] — and therefore `call_tool` — will execute.
/// Deriving it from the resolver's own rule is the point: advertising every
/// registered definition would let `t@1.0.0` and `t@2.0.0` both appear while
/// only the first is addressable, i.e. show one callback and serve another.
fn advertised(definitions: &[ToolDefinition]) -> Vec<ToolDefinition> {
    let mut seen: Vec<&str> = Vec::new();
    definitions
        .iter()
        .filter(|definition| {
            if seen.contains(&definition.name.as_str()) {
                false
            } else {
                seen.push(&definition.name);
                true
            }
        })
        .cloned()
        .collect()
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

#[cfg(test)]
mod tests {
    use super::{ToolDefinition, advertised, resolve_first};
    use serde_json::json;

    fn definition(name: &str, version: &str, description: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            version: version.into(),
            effect: "write".into(),
            capability: "general".into(),
            description: description.into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}}}),
            output_schema: None,
            timeout_ms: None,
        }
    }

    /// The exact scenario the streamable-HTTP server must never get wrong:
    /// `t@1.0.0` and a redefinition `t@2.0.0` are BOTH registered, but only
    /// one callback is addressable by name. What `list_tools` advertises
    /// (`advertised`) and what `call_tool` executes (`resolve_first`) have to
    /// be the same definition — never the definition the client was shown
    /// while the other one ran.
    #[test]
    fn the_advertised_catalog_is_exactly_what_the_resolver_executes() {
        let registered = vec![
            definition("t", "1.0.0", "first"),
            definition("u", "1.0.0", "other"),
            definition("t", "2.0.0", "replacement"),
        ];
        let shown = advertised(&registered);
        assert_eq!(
            shown.len(),
            2,
            "one catalogue entry per name — the unaddressable replacement is never shown"
        );
        assert_eq!(shown[0].name, "t");
        assert_eq!(shown[1].name, "u");
        let resolved = resolve_first(&registered, "t").expect("t resolves");
        let advertised_t = shown
            .iter()
            .find(|tool| tool.name == "t")
            .expect("t is shown");
        // Same definition, field for field: the client is shown the callback
        // it will actually invoke.
        assert_eq!(advertised_t.version, resolved.version);
        assert_eq!(advertised_t.description, resolved.description);
        assert_eq!(advertised_t.input_schema, resolved.input_schema);
        assert_eq!(resolved.version, "1.0.0", "the FIRST registration wins");
        assert_eq!(
            resolve_first(&registered, "u").map(|tool| tool.version.as_str()),
            Some("1.0.0")
        );
        assert!(resolve_first(&registered, "missing").is_none());
    }

    #[test]
    fn parse_rules_keep_the_taxonomy_and_schema_gates() {
        let parsed = super::parse_definition(&json!({
            "name": "t", "effect": "read", "inputSchema": {"type":"object"}
        }))
        .expect("a minimal definition parses");
        assert_eq!(parsed.version, "1.0.0", "a missing version defaults");
        assert_eq!(parsed.effect, "read");
        assert!(
            super::parse_definition(&json!({
                "name": "t", "effect": "danger", "inputSchema": {"type":"object"}
            }))
            .is_err(),
            "an effect outside the host taxonomy is refused"
        );
        assert!(
            super::parse_definition(&json!({"name": "t", "inputSchema": 7})).is_err(),
            "a non-object schema validates nothing and is refused"
        );
    }
}
