use crate::{McpClientError, McpConnection, model};
use agentkit_core::{CoreError, Tool, ToolContext, ToolDefinition, ToolResult};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("invalid MCP tool identity or alias")]
    Identity,
    #[error("MCP canonical or registry tool identity collision")]
    Collision,
    #[error(transparent)]
    Client(#[from] McpClientError),
}

pub struct McpToolSet {
    alias: String,
    aliases: BTreeMap<String, String>,
    client: Arc<dyn McpConnection>,
}
impl McpToolSet {
    pub fn new(
        alias: &str,
        aliases: BTreeMap<String, String>,
        client: Arc<dyn McpConnection>,
    ) -> Result<Self, BridgeError> {
        let alias = alias.trim();
        if !server_alias(alias) {
            return Err(BridgeError::Identity);
        }
        let mut targets = BTreeSet::new();
        let mut normalized = BTreeMap::new();
        for (original, replacement) in aliases {
            let original = original.trim().to_owned();
            let replacement = replacement.trim().to_owned();
            if !tool_name(&original) || !tool_name(&replacement) {
                return Err(BridgeError::Identity);
            }
            if !targets.insert(replacement.clone())
                || normalized.insert(original, replacement).is_some()
            {
                return Err(BridgeError::Collision);
            }
        }
        Ok(Self {
            alias: alias.into(),
            aliases: normalized,
            client,
        })
    }
    /// Validate the complete batch before returning any tool to a host registry.
    pub async fn contribute(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<Arc<dyn Tool>>, BridgeError> {
        let listed = self.client.list_tools(cancellation).await?;
        let mut names = BTreeSet::new();
        let mut canonical = BTreeSet::new();
        let mut result: Vec<Arc<dyn Tool>> = Vec::new();
        for tool in listed {
            let tool = self.project(tool)?;
            if !canonical.insert(tool.definition.capability.clone())
                || !names.insert(tool.definition.name.clone())
            {
                return Err(BridgeError::Collision);
            }
            result.push(Arc::new(tool));
        }
        Ok(result)
    }
    /// Also catch cross-server collisions before exposing any partial catalogue.
    pub async fn contribute_all(
        sets: &[Self],
        cancellation: CancellationToken,
    ) -> Result<Vec<Arc<dyn Tool>>, BridgeError> {
        let mut result = Vec::new();
        let mut names = BTreeSet::new();
        for set in sets {
            for tool in set.contribute(cancellation.clone()).await? {
                if !names.insert(tool.definition().name.clone()) {
                    return Err(BridgeError::Collision);
                }
                result.push(tool);
            }
        }
        Ok(result)
    }
    fn project(&self, tool: model::Tool) -> Result<RemoteTool, BridgeError> {
        let original = tool.name.trim();
        if !tool_name(original) {
            return Err(BridgeError::Identity);
        }
        let effective = self
            .aliases
            .get(original)
            .map(String::as_str)
            .unwrap_or(original);
        let canonical = format!("mcp.{}.{effective}", self.alias);
        let definition = ToolDefinition {
            name: canonical.replace('.', "__"),
            version: "1.0.0".into(),
            effect: if tool
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.read_only_hint)
                == Some(true)
            {
                "read".into()
            } else {
                "write".into()
            },
            capability: canonical,
            description: tool
                .description
                .map(|text| text.into_owned())
                .unwrap_or_else(|| format!("MCP tool {original} from {}", self.alias)),
            input_schema: Value::Object((*tool.input_schema).clone()),
            output_schema: None,
            timeout_ms: None,
        };
        Ok(RemoteTool {
            definition,
            client: self.client.clone(),
            original: original.into(),
            alias: self.alias.clone(),
        })
    }
}

struct RemoteTool {
    definition: ToolDefinition,
    client: Arc<dyn McpConnection>,
    original: String,
    alias: String,
}
#[async_trait]
impl Tool for RemoteTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        let arguments = input.as_object().cloned().unwrap_or_default();
        match self
            .client
            .call_tool_guarded(
                &self.original,
                arguments,
                context.cancellation.clone(),
                context.dispatch_guard.clone(),
            )
            .await
        {
            Ok(result) => Ok(self.outcome(context, result)),
            Err(error) => Ok(self.failure(context, error)),
        }
    }
}
impl RemoteTool {
    fn outcome(&self, context: ToolContext, result: model::CallToolResult) -> ToolResult {
        let content = serde_json::to_value(result.content).unwrap_or_else(|_| json!([]));
        let mut texts = Vec::new();
        let mut non_text: Vec<String> = Vec::new();
        for part in content.as_array().into_iter().flatten() {
            match (part["type"].as_str(), part["text"].as_str()) {
                (Some("text"), Some(text)) => texts.push(text),
                (Some(kind), _) if !non_text.iter().any(|entry| entry == kind) => {
                    non_text.push(kind.into())
                }
                _ => {}
            }
        }
        let text = texts.join("\n");
        let is_error = result.is_error == Some(true);
        let message = if is_error && text.is_empty() {
            format!("MCP tool {} reported an error.", self.definition.capability)
        } else {
            text.clone()
        };
        let (capped, truncated) = truncate(&message, context.limits.max_bytes);
        let mut data = json!({"canonicalId":self.definition.capability,"serverAlias":self.alias,"toolName":self.original,
            "text":text,"content":content,"nonTextTypes":non_text,"isError":is_error});
        let structured = result.structured_content.filter(Value::is_object);
        if let Some(structured) = &structured {
            data["structuredContent"] = structured.clone();
        }
        let mut model = if is_error {
            json!({"errorCode":"mcp_tool_error","errorMessage":capped,"retryable":false})
        } else {
            json!({"text":capped})
        };
        if !is_error && let Some(structured) = structured {
            model["structured"] = structured;
        }
        let mut warnings = Vec::new();
        if !is_error && !non_text.is_empty() {
            warnings.push(format!("MCP tool returned non-text content ({}); only text parts were passed to the model.", non_text.join(", ")));
        }
        if !is_error && truncated {
            warnings.push(format!(
                "MCP tool result was truncated to {} bytes.",
                context.limits.max_bytes
            ));
        }
        ToolResult {
            ok: !is_error,
            data: Some(data),
            model_data: Some(model),
            summary: Some(if message.is_empty() {
                format!("{} returned no text.", self.definition.capability)
            } else {
                first_line(&message)
            }),
            status: (!is_error).then(|| "ok".into()),
            sources: vec![],
            warnings,
            truncated,
            limits: context.limits,
        }
    }
    fn failure(&self, context: ToolContext, error: McpClientError) -> ToolResult {
        let code = error.code();
        let (message, truncated) = truncate(&error.to_string(), context.limits.max_bytes);
        let data = json!({"errorCode":code,"errorMessage":message,"retryable":false});
        ToolResult {
            ok: false,
            data: Some(data.clone()),
            model_data: Some(data),
            summary: Some(format!(
                "{} failed: {}",
                self.definition.capability,
                first_line(&message)
            )),
            status: None,
            sources: vec![],
            warnings: vec![message],
            truncated,
            limits: context.limits,
        }
    }
}
pub(crate) fn server_alias(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}
pub(crate) fn tool_name(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value.split('.').all(|segment| {
            !segment.is_empty()
                && segment.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'-')
                })
        })
}
fn truncate(value: &str, max: usize) -> (String, bool) {
    const MARKER: &str = "\n[...truncated]";
    if value.len() <= max {
        return (value.into(), false);
    }
    if max <= MARKER.len() {
        return (MARKER[..max].into(), true);
    }
    let mut end = max - MARKER.len();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}{MARKER}", &value[..end]), true)
}
fn first_line(value: &str) -> String {
    let line = value.lines().next().unwrap_or_default();
    if line.chars().count() <= 200 {
        line.into()
    } else {
        format!("{}…", line.chars().take(199).collect::<String>())
    }
}
