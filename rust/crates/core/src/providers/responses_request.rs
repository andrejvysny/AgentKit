use super::{content, ConnectionIdentity};
use crate::{ChatMessage, ChatRequest, CoreError, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
const MAX_BYTES: usize = 1024 * 1024;
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct State {
    version: u8,
    scope: Scope,
    message_count: usize,
    message_digest: String,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    input_items: Vec<Value>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Scope {
    provider_id: String,
    protocol: String,
    connection_id: String,
    generation: u64,
    chat_id: String,
    branch_id: String,
    model: String,
}
pub fn build_responses_request(
    input: &ChatRequest,
    identity: &ConnectionIdentity,
    account: bool,
) -> Result<Value, CoreError> {
    if account && (input.temperature.is_some() || input.max_output_tokens.is_some()) {
        return Err(CoreError::responses("unsupported_parameter"));
    }
    let mut items = vec![];
    let mut offset = 0;
    if let Some(state) = &input.continuation {
        bounded(state)?;
        let state: State = serde_json::from_value(state.clone())
            .map_err(|_| CoreError::responses("invalid_continuation"))?;
        check_state(&state, input, identity)?;
        offset = state.message_count;
        items = state.input_items;
    } else if input.continuation_required {
        return Err(CoreError::responses("continuation_required"));
    }
    items.extend(map_messages(&input.messages[offset..])?);
    if items.len() > 1024 {
        return Err(CoreError::responses("state_limit_exceeded"));
    }
    let instructions = input
        .messages
        .iter()
        .filter(|message| message.role == "system")
        .map(|message| content::text(message, true))
        .collect::<Result<Vec<_>, _>>()?
        .join("\n\n");
    let tools = tools(input)?;
    let mut body = json!({"model":input.model,"input":items,"stream":true,"store":false,"include":["reasoning.encrypted_content"]});
    if !instructions.is_empty() {
        body["instructions"] = instructions.into();
    }
    if !tools.is_empty() {
        body["tools"] = tools.into();
    }
    if let Some(value) = input.temperature {
        body["temperature"] = value.into();
    }
    if let Some(value) = input.max_output_tokens {
        body["max_output_tokens"] = value.into();
    }
    bounded(&body)?;
    Ok(body)
}
fn check_state(
    state: &State,
    input: &ChatRequest,
    identity: &ConnectionIdentity,
) -> Result<(), CoreError> {
    let Some(scope) = &input.continuation_scope else {
        return Err(CoreError::responses("invalid_continuation"));
    };
    if state.version != 1
        || state.input_items.len() > 1024
        || state.message_digest.len() != 64
        || !state
            .message_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || state.scope.protocol != "responses"
        || state.scope.provider_id.is_empty()
        || state.scope.connection_id.is_empty()
        || state.scope.chat_id.is_empty()
        || state.scope.branch_id.is_empty()
        || state.scope.model.is_empty()
    {
        return Err(CoreError::responses("invalid_continuation"));
    }
    if state.scope.provider_id != identity.provider_id
        || state.scope.protocol != identity.protocol
        || state.scope.connection_id != identity.connection_id
        || state.scope.generation != identity.generation
        || state.scope.chat_id != scope.chat_id
        || state.scope.branch_id != scope.branch_id
        || state.scope.model != input.model
    {
        return Err(CoreError::responses("continuation_scope_mismatch"));
    }
    if state.message_count > input.messages.len()
        || state.message_digest != message_digest(&input.messages[..state.message_count])?
    {
        return Err(CoreError::responses("continuation_history_mismatch"));
    }
    for item in &state.input_items {
        match item["type"].as_str() {
            Some("message" | "function_call" | "function_call_output") => {}
            Some("reasoning") => {
                if item["encrypted_content"].as_str().is_none_or(str::is_empty) {
                    return Err(CoreError::responses("continuation_required"));
                }
            }
            _ => return Err(CoreError::responses("unsupported_continuation_item")),
        }
    }
    Ok(())
}
pub(crate) fn make_continuation(
    input: &ChatRequest,
    identity: &ConnectionIdentity,
    body: &Value,
    output: &[Value],
    content: &str,
    calls: &[ToolCall],
) -> Result<Option<Value>, CoreError> {
    let Some(scope) = &input.continuation_scope else {
        if output.iter().any(|item| item["type"] == "reasoning") {
            return Err(CoreError::responses("continuation_scope_required"));
        }
        return Ok(None);
    };
    let mut assistant = ChatMessage::text("assistant", content);
    assistant.tool_calls = calls.to_vec();
    let mut messages = input.messages.clone();
    messages.push(assistant);
    let mut items = body["input"]
        .as_array()
        .cloned()
        .ok_or_else(|| CoreError::responses("invalid_request"))?;
    items.extend_from_slice(output);
    if items.len() > 1024 {
        return Err(CoreError::responses("state_limit_exceeded"));
    }
    let state = State {
        version: 1,
        scope: Scope {
            provider_id: identity.provider_id.clone(),
            protocol: identity.protocol.clone(),
            connection_id: identity.connection_id.clone(),
            generation: identity.generation,
            chat_id: scope.chat_id.clone(),
            branch_id: scope.branch_id.clone(),
            model: input.model.clone(),
        },
        message_count: messages.len(),
        message_digest: message_digest(&messages)?,
        input_items: items,
    };
    let value =
        serde_json::to_value(state).map_err(|_| CoreError::responses("invalid_continuation"))?;
    bounded(&value)?;
    Ok(Some(value))
}
fn message_digest(messages: &[ChatMessage]) -> Result<String, CoreError> {
    let stable: Vec<Value> = messages
        .iter()
        .map(|message| {
            let mut item = json!({"role":message.role,"content":message.content});
            if let Some(id) = &message.tool_call_id {
                if !id.is_empty() {
                    item["toolCallId"] = id.clone().into();
                }
            }
            if !message.tool_calls.is_empty() {
                item["toolCalls"] =
                    serde_json::to_value(&message.tool_calls).expect("JSON values only");
            }
            item
        })
        .collect();
    let bytes = bounded(&Value::Array(stable))?;
    Ok(format!("{:x}", Sha256::digest(bytes.as_bytes())))
}
fn map_messages(messages: &[ChatMessage]) -> Result<Vec<Value>, CoreError> {
    let mut items = vec![];
    for message in messages {
        if message.role == "system" {
            content::text(message, true)?;
            continue;
        }
        if message.role == "tool" {
            let id = message
                .tool_call_id
                .as_ref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| CoreError::responses("missing_tool_call_id"))?;
            items.push(json!({"type":"function_call_output","call_id":id,"output":content::text(message,true)?}));
            continue;
        }
        if !["user", "assistant"].contains(&message.role.as_str()) {
            return Err(CoreError::responses("invalid_request"));
        }
        let (parts, _) = content::parts(message, true, true)?;
        if parts.as_array().is_some_and(|parts| !parts.is_empty()) {
            items.push(json!({"type":"message","role":message.role,"content":parts}));
        }
        for call in &message.tool_calls {
            if message.role != "assistant" || call.id.is_empty() || call.name.is_empty() {
                return Err(CoreError::responses("invalid_tool_history"));
            }
            items.push(json!({"type":"function_call","call_id":call.id,"namespace":"agentkit","name":call.name,"arguments":call.arguments_json}));
        }
    }
    Ok(items)
}
fn tools(input: &ChatRequest) -> Result<Vec<Value>, CoreError> {
    let mut seen = std::collections::HashSet::new();
    let mut functions = vec![];
    for tool in &input.tools {
        if tool.name.is_empty()
            || tool.name.len() > 64
            || !tool
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            || !seen.insert(&tool.name)
        {
            return Err(CoreError::responses("unsupported_tool"));
        }
        validate_schema(&tool.input_schema, 0)?;
        if tool.input_schema["type"] != "object"
            || agentkit_contracts::compile_tool_schema(&tool.input_schema).is_err()
        {
            return Err(CoreError::responses("unsupported_tool_schema"));
        }
        functions.push(json!({"type":"function","name":tool.name,"description":tool.description,"parameters":agentkit_contracts::javascript_json_value(&tool.input_schema),"strict":false}));
    }
    Ok(if functions.is_empty() {
        vec![]
    } else {
        vec![
            json!({"type":"namespace","name":"agentkit","description":"Tools supplied by the application.","tools":functions}),
        ]
    })
}
fn validate_schema(schema: &Value, depth: usize) -> Result<(), CoreError> {
    let schema = schema
        .as_object()
        .ok_or_else(|| CoreError::responses("unsupported_tool_schema"))?;
    if depth > 32 {
        return Err(CoreError::responses("unsupported_tool_schema"));
    }
    const KEYS: &[&str] = &[
        "type",
        "description",
        "properties",
        "required",
        "items",
        "enum",
        "additionalProperties",
        "default",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "anyOf",
        "oneOf",
        "allOf",
        "const",
        "title",
    ];
    for (key, value) in schema {
        if !KEYS.contains(&key.as_str()) {
            return Err(CoreError::responses("unsupported_tool_schema"));
        }
        match key.as_str() {
            "properties" => {
                for child in value
                    .as_object()
                    .ok_or_else(|| CoreError::responses("unsupported_tool_schema"))?
                    .values()
                {
                    validate_schema(child, depth + 1)?;
                }
            }
            "items" => validate_schema(value, depth + 1)?,
            "additionalProperties" if !value.is_boolean() => validate_schema(value, depth + 1)?,
            "anyOf" | "oneOf" | "allOf" => {
                let children = value
                    .as_array()
                    .filter(|children| !children.is_empty())
                    .ok_or_else(|| CoreError::responses("unsupported_tool_schema"))?;
                for child in children {
                    validate_schema(child, depth + 1)?;
                }
            }
            _ => {}
        }
    }
    bounded(&Value::Object(schema.clone()))?;
    Ok(())
}
pub(crate) fn bounded(value: &Value) -> Result<String, CoreError> {
    let encoded = agentkit_contracts::stringify_javascript_value(value)
        .map_err(|_| CoreError::responses("invalid_request"))?;
    if encoded.len() > MAX_BYTES {
        return Err(CoreError::responses("state_limit_exceeded"));
    }
    Ok(encoded)
}
