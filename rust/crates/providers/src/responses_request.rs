use crate::chat_completions_request::{history_arguments, sanitized_note};
use crate::content;
use agentkit_core::{
    ChatMessage, ChatRequest, ConnectionIdentity, CoreError, OPENAI_PATCH_NAME, OPENAI_SHELL_NAME,
    ProfileNote, ReasoningSummary, ToolCall, ToolChoice, builtin_tool_type, is_strict_compliant,
    policy_tool_choice,
};
use agentkit_kernel::sha256_hex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
const MAX_BYTES: usize = 1024 * 1024;
/// Stable code of the warning that records a dropped continuation (a cache break).
const CONTINUATION_RESET_HISTORY: &str = "continuation_reset:history_rewritten";
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct State {
    version: u8,
    scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_content: Option<RunContentBinding>,
    message_count: usize,
    message_digest: String,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    input_items: Vec<Value>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunContentBinding {
    run_id: String,
    digest: String,
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
    build_responses_request_with_warnings(input, identity, account).map(|(body, _)| body)
}
/// Like [`build_responses_request`], also returning one message per tool that
/// was left out of the request because its schema is unsupported.
pub fn build_responses_request_with_warnings(
    input: &ChatRequest,
    identity: &ConnectionIdentity,
    account: bool,
) -> Result<(Value, Vec<String>), CoreError> {
    build_responses_request_notes(input, identity, account)
        .map(|(body, warnings, _)| (body, warnings))
}
/// Like [`build_responses_request_with_warnings`], also returning the notes
/// the request builder itself produced: history arguments it replaced and a
/// tool choice the session's profile downgraded.
pub fn build_responses_request_notes(
    input: &ChatRequest,
    identity: &ConnectionIdentity,
    account: bool,
) -> Result<(Value, Vec<String>, Vec<ProfileNote>), CoreError> {
    content::validate_selected(&input.selected_content)?;
    if account && (input.temperature.is_some() || input.max_output_tokens.is_some()) {
        return Err(CoreError::responses("unsupported_parameter"));
    }
    let mut items = vec![];
    let mut offset = 0;
    let mut notes: Vec<ProfileNote> = vec![];
    if let Some(state) = &input.continuation {
        bounded(state)?;
        let state: State = serde_json::from_value(state.clone())
            .map_err(|_| CoreError::responses("invalid_continuation"))?;
        check_state(&state, input, identity)?;
        let intact = history_intact(&state, input)?;
        let digest = selected_digest(input)?;
        let previous = state.run_content.as_ref();
        let same_run = previous.is_some_and(|binding| binding.run_id == input.run_id);
        // Inside one run nothing may change under the checkpoint.
        if same_run
            && (!intact || previous.map(|binding| binding.digest.as_str()) != digest.as_deref())
        {
            return Err(CoreError::responses("continuation_history_mismatch"));
        }
        // On a later run a changed selection is appended content: the prior
        // provider-native items stay valid, so they are kept. Only a history
        // that was rewritten under the checkpoint drops it, and says so.
        if intact {
            offset = state.message_count;
            items = state.input_items;
        } else {
            notes.push(ProfileNote {
                code: CONTINUATION_RESET_HISTORY,
                message: "The provider continuation was dropped because the history before it changed; the request replays the canonical history and the provider cache restarts.".into(),
            });
        }
    } else if input.continuation_required {
        return Err(CoreError::responses("continuation_required"));
    }
    // Selected content rides inside the turn's last user message (CE-01),
    // never ahead of the system segments. The chat executor persists it there
    // as a turn-context part instead, so this fold only serves callers that
    // still pass `selected_content`.
    let builtins = builtin_names(input);
    let mut replaced = vec![];
    let folded = agentkit_core::fold_selected_content(&input.messages, &input.selected_content);
    items.extend(map_messages(&folded[offset..], &builtins, &mut replaced)?);
    if items.len() > 1024 {
        return Err(CoreError::responses("state_limit_exceeded"));
    }
    // S2-S4 are joined in segment order into `instructions`; turn context
    // rides the user side and is never hoisted here (CE-07).
    let instructions = input
        .messages
        .iter()
        .filter(|message| message.role == "system")
        .map(|message| content::text(message, true))
        .collect::<Result<Vec<_>, _>>()?
        .join("\n\n");
    let (tools, warnings) = tools(input)?;
    // Wire order: tools, instructions (S2-S4), then history. Selected content
    // sits inside the last user message (CE-01), so no request-local items
    // precede the history.
    let wire_items = items;
    let mut body = json!({"model":input.model});
    notes.extend(sanitized_note(&replaced));
    if !tools.is_empty() {
        let (choice, mut downgraded) = match &input.profile {
            Some(profile) => policy_tool_choice(profile, &input.tool_choice),
            None => (input.tool_choice.clone(), vec![]),
        };
        // Auto is the Responses default: only a changed choice is written.
        if choice != ToolChoice::Auto || !downgraded.is_empty() {
            body["tool_choice"] = responses_tool_choice(&choice, &builtins);
        }
        notes.append(&mut downgraded);
        body["tools"] = tools.into();
    }
    if !instructions.is_empty() {
        body["instructions"] = instructions.into();
    }
    if let Some(cache) = &input.cache
        && cache.key_field == Some(agentkit_core::CacheHint::PromptCacheKey)
        && let Some(key) = &cache.cache_key
    {
        body["prompt_cache_key"] = key.clone().into();
    }
    body["input"] = wire_items.into();
    body["stream"] = true.into();
    body["store"] = false.into();
    body["include"] = json!(["reasoning.encrypted_content"]);
    let mut reasoning = serde_json::Map::new();
    if let Some(value) = input.reasoning_effort {
        reasoning.insert("effort".into(), json!(value));
    }
    if input.reasoning_summary != ReasoningSummary::Off {
        reasoning.insert("summary".into(), json!(input.reasoning_summary));
    }
    if !reasoning.is_empty() {
        body["reasoning"] = reasoning.into();
    }
    if let Some(value) = input.temperature {
        body["temperature"] = value.into();
    }
    if let Some(value) = input.max_output_tokens {
        body["max_output_tokens"] = value.into();
    }
    bounded(&body)?;
    Ok((body, warnings, notes))
}
/// The Responses wire form of a tool choice: a named function is
/// `{"type":"function","name":..}`, a vendor built-in is `{"type":<name>}`.
fn responses_tool_choice(choice: &ToolChoice, builtins: &[&str]) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Named(name) if builtins.contains(&name.as_str()) => json!({"type": name}),
        ToolChoice::Named(name) => json!({"type": "function", "name": name}),
    }
}
fn check_state(
    state: &State,
    input: &ChatRequest,
    identity: &ConnectionIdentity,
) -> Result<(), CoreError> {
    let Some(scope) = &input.continuation_scope else {
        return Err(CoreError::responses("invalid_continuation"));
    };
    if state.run_content.as_ref().is_some_and(|binding| {
        binding.run_id.is_empty()
            || binding.digest.len() != 64
            || !binding
                .digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        return Err(CoreError::responses("invalid_continuation"));
    }
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
    for item in &state.input_items {
        match item["type"].as_str() {
            Some(
                "message"
                | "function_call"
                | "function_call_output"
                | "apply_patch_call"
                | "apply_patch_call_output"
                | "shell_call"
                | "shell_call_output",
            ) => {}
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
/// Whether the messages the checkpoint covered are still the same bytes.
fn history_intact(state: &State, input: &ChatRequest) -> Result<bool, CoreError> {
    Ok(state.message_count <= input.messages.len()
        && state.message_digest == message_digest(&input.messages[..state.message_count])?)
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
    // Nothing request-local to strip: S3/S4 ride in `instructions` (CE-07) and
    // selected content lives inside the user message, which the continuation
    // keeps so a replayed turn matches the persisted history.
    items.extend_from_slice(output);
    if items.len() > 1024 {
        return Err(CoreError::responses("state_limit_exceeded"));
    }
    let state = State {
        version: 1,
        run_content: selected_digest(input)?.map(|digest| RunContentBinding {
            run_id: input.run_id.clone(),
            digest,
        }),
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
fn selected_digest(input: &ChatRequest) -> Result<Option<String>, CoreError> {
    if let Some(digest) = &input.content_binding_digest {
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(CoreError::responses("invalid_request"));
        }
        return Ok(Some(digest.clone()));
    }
    message_digest(&input.selected_content).map(Some)
}
fn message_digest(messages: &[ChatMessage]) -> Result<String, CoreError> {
    let stable: Vec<Value> = messages
        .iter()
        .map(|message| {
            let mut item = json!({"role":message.role,"content":message.content});
            if let Some(id) = &message.tool_call_id
                && !id.is_empty()
            {
                item["toolCallId"] = id.clone().into();
            }
            if !message.tool_calls.is_empty() {
                item["toolCalls"] =
                    serde_json::to_value(&message.tool_calls).expect("JSON values only");
            }
            item
        })
        .collect();
    let bytes = bounded(&Value::Array(stable))?;
    Ok(sha256_hex(bytes.as_bytes()))
}
/// The vendor built-in tools this request offers, by the name a call carries.
fn builtin_names(input: &ChatRequest) -> Vec<&'static str> {
    [OPENAI_PATCH_NAME, OPENAI_SHELL_NAME]
        .into_iter()
        .filter(|name| {
            input
                .tools
                .iter()
                .any(|tool| tool.name == *name && builtin_tool_type(tool) == Some(*name))
        })
        .collect()
}
fn map_messages(
    messages: &[ChatMessage],
    builtins: &[&str],
    replaced: &mut Vec<String>,
) -> Result<Vec<Value>, CoreError> {
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
            let text = content::text(message, true)?;
            let built_in = message
                .name
                .as_deref()
                .filter(|name| builtins.contains(name));
            items.push(match built_in {
                Some(name) => builtin_output(name, id, &text),
                None => json!({"type":"function_call_output","call_id":id,"output":text}),
            });
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
            if builtins.contains(&call.name.as_str()) {
                items.push(builtin_call(call)?);
                continue;
            }
            items.push(json!({"type":"function_call","call_id":call.id,"namespace":"agentkit","name":call.name,"arguments":history_arguments(call, replaced)}));
        }
    }
    Ok(items)
}
fn tools(input: &ChatRequest) -> Result<(Vec<Value>, Vec<String>), CoreError> {
    let mut seen = std::collections::HashSet::new();
    let mut functions = vec![];
    let mut built_in = vec![];
    let mut warnings = vec![];
    let strict = input
        .profile
        .as_ref()
        .is_some_and(|profile| agentkit_core::strict_dialect(&profile.dialect.id));
    for tool in &input.tools {
        if let Some(kind) = builtin_tool_type(tool) {
            // The vendor's own typed tool; it has no schema to check here.
            if kind != tool.name || !seen.insert(&tool.name) {
                return Err(CoreError::responses("unsupported_tool"));
            }
            built_in.push(match kind {
                k if k == OPENAI_PATCH_NAME => json!({"type":"apply_patch"}),
                k if k == OPENAI_SHELL_NAME => {
                    json!({"type":"shell","environment":{"type":"local"}})
                }
                _ => return Err(CoreError::responses("unsupported_tool")),
            });
            continue;
        }
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
        // One tool with an exotic schema (common with MCP servers) must not
        // fail the whole turn: leave it out and tell the run.
        let Some(schema) = supported_schema(&tool.input_schema) else {
            warnings.push(format!(
                "Tool {} was excluded from the request: {}.",
                tool.name,
                unsupported_reason(&tool.input_schema)
            ));
            continue;
        };
        functions.push(json!({"type":"function","name":tool.name,"description":agentkit_core::presented_description(tool, agentkit_core::ExampleLane::Responses),"parameters":agentkit_contracts::javascript_json_value(&schema),"strict":strict && is_strict_compliant(&schema)}));
    }
    let mut tools = built_in;
    if !functions.is_empty() {
        tools.push(
            json!({"type":"namespace","name":"agentkit","description":"Tools supplied by the application.","tools":functions}),
        );
    }
    Ok((tools, warnings))
}
/// Lanes whose schema acceptance can be linted at build time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaLane {
    Responses,
    ChatCompletions,
    Anthropic,
}
/// The keys a lane accepts, or `None` when the lane takes any JSON Schema
/// keyword (no allow-list exists for it).
pub fn allowed_schema_keys(lane: SchemaLane) -> Option<&'static [&'static str]> {
    match lane {
        SchemaLane::Responses => Some(RESPONSES_SCHEMA_KEYS),
        SchemaLane::ChatCompletions | SchemaLane::Anthropic => None,
    }
}
/// Lint helper: every schema key of `schema` (annotation-only keywords are
/// ignored, as the request builder strips them) that `lane` cannot carry, as
/// `path: key` strings. Empty means the lane offers the tool as defined.
pub fn schema_lane_violations(lane: SchemaLane, schema: &Value) -> Vec<String> {
    let Some(allowed) = allowed_schema_keys(lane) else {
        return vec![];
    };
    let mut schema = schema.clone();
    strip_annotations(&mut schema);
    let mut out = vec![];
    collect_unknown_keys(&schema, allowed, "$", &mut out);
    out
}
fn collect_unknown_keys(schema: &Value, allowed: &[&str], path: &str, out: &mut Vec<String>) {
    let Some(map) = schema.as_object() else {
        return;
    };
    for (key, value) in map {
        if !allowed.contains(&key.as_str()) {
            out.push(format!("{path}: unsupported key `{key}`"));
            continue;
        }
        match key.as_str() {
            "properties" => {
                if let Some(children) = value.as_object() {
                    for (name, child) in children {
                        collect_unknown_keys(child, allowed, &format!("{path}.{name}"), out);
                    }
                }
            }
            "items" => collect_unknown_keys(value, allowed, &format!("{path}[]"), out),
            "additionalProperties" if !value.is_boolean() => {
                collect_unknown_keys(value, allowed, &format!("{path}.*"), out)
            }
            "anyOf" | "oneOf" | "allOf" => {
                for (index, child) in value.as_array().into_iter().flatten().enumerate() {
                    collect_unknown_keys(child, allowed, &format!("{path}.{key}[{index}]"), out);
                }
            }
            _ => {}
        }
    }
}
fn unsupported_reason(schema: &Value) -> String {
    let violations = schema_lane_violations(SchemaLane::Responses, schema);
    if violations.is_empty() {
        "its input schema is not supported by the Responses lane".into()
    } else {
        format!(
            "its input schema is not supported by the Responses lane ({})",
            violations.join("; ")
        )
    }
}
fn supported_schema(schema: &Value) -> Option<Value> {
    let mut schema = schema.clone();
    strip_annotations(&mut schema);
    (validate_schema(&schema, 0).is_ok()
        && schema["type"] == "object"
        && agentkit_contracts::compile_tool_schema(&schema).is_ok())
    .then_some(schema)
}
/// Drops annotation-only keywords at schema positions. Property names under
/// `properties` are data, not keywords, so only the subschemas are visited.
fn strip_annotations(schema: &mut Value) {
    const ANNOTATIONS: &[&str] = &[
        "$schema",
        "$id",
        "$comment",
        "examples",
        "deprecated",
        "readOnly",
        "writeOnly",
    ];
    let Some(map) = schema.as_object_mut() else {
        return;
    };
    for key in ANNOTATIONS {
        map.remove(*key);
    }
    for (key, value) in map.iter_mut() {
        match key.as_str() {
            "properties" => {
                if let Some(children) = value.as_object_mut() {
                    children.values_mut().for_each(strip_annotations);
                }
            }
            "items" | "additionalProperties" => strip_annotations(value),
            "anyOf" | "oneOf" | "allOf" => {
                if let Some(children) = value.as_array_mut() {
                    children.iter_mut().for_each(strip_annotations);
                }
            }
            _ => {}
        }
    }
}
/// Schema keywords the Responses lane carries; any other key makes a tool
/// unofferable there (see [`schema_lane_violations`]).
pub const RESPONSES_SCHEMA_KEYS: &[&str] = &[
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
    "uniqueItems",
    "anyOf",
    "oneOf",
    "allOf",
    "const",
    "title",
    "format",
    "pattern",
];
fn validate_schema(schema: &Value, depth: usize) -> Result<(), CoreError> {
    let schema = schema
        .as_object()
        .ok_or_else(|| CoreError::responses("unsupported_tool_schema"))?;
    if depth > 32 {
        return Err(CoreError::responses("unsupported_tool_schema"));
    }
    for (key, value) in schema {
        if !RESPONSES_SCHEMA_KEYS.contains(&key.as_str()) {
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

/// A built-in call the model made, shown back as the vendor's call item. The
/// stored arguments are the vendor's own item payload (`operation`, `action`).
fn builtin_call(call: &ToolCall) -> Result<Value, CoreError> {
    let payload = agentkit_contracts::parse_javascript_json(&call.arguments_json)
        .map_err(|_| CoreError::responses("invalid_tool_history"))?;
    Ok(if call.name == OPENAI_PATCH_NAME {
        json!({"type":"apply_patch_call","call_id":call.id,"status":"completed","operation":payload})
    } else {
        json!({"type":"shell_call","call_id":call.id,"status":"completed","action":payload})
    })
}
/// The vendor's output item for the result envelope of a built-in call.
/// `ok` and `summary` come from the envelope; a text that is not an envelope
/// is the output as it stands.
fn builtin_output(name: &str, call_id: &str, content: &str) -> Value {
    let envelope: Option<Value> = serde_json::from_str(content).ok();
    let ok = envelope
        .as_ref()
        .and_then(|value| value["ok"].as_bool())
        .unwrap_or(true);
    let summary = envelope
        .as_ref()
        .and_then(|value| value["summary"].as_str())
        .unwrap_or(content);
    if name == OPENAI_PATCH_NAME {
        return json!({
            "type":"apply_patch_call_output",
            "call_id":call_id,
            "status": if ok { "completed" } else { "failed" },
            "output": summary,
        });
    }
    // `run_command` merges both streams into its text; a call that never
    // reached an exit code (refused, failed to start) reports exit 1 with the
    // failure text as stderr.
    let text = envelope
        .as_ref()
        .and_then(|value| value["data"].as_str())
        .unwrap_or("");
    let timed_out = summary.starts_with("timed out");
    let exit_code = summary
        .strip_prefix("exit code ")
        .and_then(|code| code.trim().parse::<i64>().ok());
    let (stdout, stderr, outcome) = match (timed_out, exit_code) {
        (true, _) => (text, "", json!({"type":"timeout"})),
        (_, Some(code)) => (text, "", json!({"type":"exit","exit_code":code})),
        _ => ("", summary, json!({"type":"exit","exit_code":1})),
    };
    json!({
        "type":"shell_call_output",
        "call_id":call_id,
        "output":[{"stdout":stdout,"stderr":stderr,"outcome":outcome}],
    })
}

#[cfg(test)]
mod selected_content_tests {
    use super::*;
    use agentkit_core::{ContinuationScope, ReasoningEffort};

    #[test]
    fn content_binding_fences_same_run_and_changed_next_turn_keeps_the_checkpoint() {
        let identity = ConnectionIdentity {
            provider_id: "p".into(),
            protocol: "responses".into(),
            connection_id: "c".into(),
            generation: 1,
        };
        let mut input = ChatRequest {
            run_id: "run-1".into(),
            model: "model".into(),
            messages: vec![ChatMessage::text("user", "canonical")],
            selected_content: vec![ChatMessage::text("user", "selected first")],
            reasoning_effort: Some(ReasoningEffort::High),
            continuation_scope: Some(ContinuationScope {
                chat_id: "chat".into(),
                branch_id: "branch".into(),
            }),
            ..Default::default()
        };
        let body = build_responses_request(&input, &identity, false).unwrap();
        assert_eq!(body["reasoning"]["effort"], "high");
        // The selection rides inside the user message, never ahead of it.
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        let first = body["input"][0]["content"][0]["text"].as_str().unwrap();
        assert!(first.starts_with("canonical\n\n<turn_context>"), "{first}");
        assert!(first.contains("selected first"), "{first}");
        let output = vec![
            json!({"type":"reasoning","encrypted_content":"opaque"}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}),
        ];
        let state = make_continuation(&input, &identity, &body, &output, "answer", &[])
            .unwrap()
            .unwrap();
        assert_eq!(state["messageCount"], 2);
        assert_eq!(state["runContent"]["runId"], "run-1");
        assert!(state["inputItems"].to_string().contains("selected first"));
        input
            .messages
            .push(ChatMessage::text("assistant", "answer"));
        input.messages.push(ChatMessage::text("user", "next"));
        input.continuation = Some(state.clone());
        let continued = build_responses_request(&input, &identity, false).unwrap();
        assert_eq!(continued["input"].as_array().unwrap().len(), 4);
        input.selected_content = vec![ChatMessage::text("user", "selected second")];
        assert_eq!(
            build_responses_request(&input, &identity, false)
                .unwrap_err()
                .code,
            "continuation_history_mismatch"
        );
        input.run_id = "run-2".into();
        // A later run keeps the checkpoint: the new selection is appended
        // and the provider-native items before it are replayed unchanged.
        let (replay, _, notes) = build_responses_request_notes(&input, &identity, false).unwrap();
        assert!(notes.is_empty());
        assert!(replay["input"].to_string().contains("opaque"));
        assert!(replay["input"].to_string().contains("selected first"));
        assert!(replay["input"].to_string().contains("selected second"));
        assert_eq!(
            replay["input"].as_array().unwrap()[..3],
            state["inputItems"].as_array().unwrap()[..]
        );
    }
    #[test]
    fn a_later_run_whose_history_was_rewritten_drops_the_checkpoint_and_says_so() {
        let identity = ConnectionIdentity {
            provider_id: "p".into(),
            protocol: "responses".into(),
            connection_id: "c".into(),
            generation: 1,
        };
        let mut input = ChatRequest {
            run_id: "run-1".into(),
            model: "model".into(),
            messages: vec![ChatMessage::text("user", "first")],
            continuation_scope: Some(ContinuationScope {
                chat_id: "chat".into(),
                branch_id: "branch".into(),
            }),
            ..Default::default()
        };
        let body = build_responses_request(&input, &identity, false).unwrap();
        let state = make_continuation(&input, &identity, &body, &[], "answer", &[])
            .unwrap()
            .unwrap();
        input.messages = vec![
            ChatMessage::text("user", "edited"),
            ChatMessage::text("assistant", "answer"),
            ChatMessage::text("user", "next"),
        ];
        input.continuation = Some(state);
        assert_eq!(
            build_responses_request(&input, &identity, false)
                .unwrap_err()
                .code,
            "continuation_history_mismatch"
        );
        input.run_id = "run-2".into();
        let (body, _, notes) = build_responses_request_notes(&input, &identity, false).unwrap();
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].code, "continuation_reset:history_rewritten");
    }
    #[test]
    fn selecting_a_skill_on_turn_two_replays_turn_one_items_byte_for_byte() {
        let identity = ConnectionIdentity {
            provider_id: "p".into(),
            protocol: "responses".into(),
            connection_id: "c".into(),
            generation: 1,
        };
        let scope = ContinuationScope {
            chat_id: "chat".into(),
            branch_id: "branch".into(),
        };
        let call = ToolCall {
            id: "call_1".into(),
            name: "lookup".into(),
            arguments_json: "{\"q\":1}".into(),
        };
        let mut turn_one = ChatRequest {
            run_id: "run-1".into(),
            model: "model".into(),
            messages: vec![ChatMessage::text("user", "q1")],
            content_binding_digest: Some("a".repeat(64)),
            continuation_scope: Some(scope.clone()),
            ..Default::default()
        };
        let body = build_responses_request(&turn_one, &identity, false).unwrap();
        // The provider's own function_call item: a private id the canonical
        // history cannot rebuild.
        let output = vec![
            json!({"type":"reasoning","encrypted_content":"opaque"}),
            json!({"type":"function_call","id":"fc_private","call_id":"call_1","name":"lookup","namespace":"agentkit","arguments":"{\"q\":1}"}),
        ];
        let state = make_continuation(
            &turn_one,
            &identity,
            &body,
            &output,
            "",
            std::slice::from_ref(&call),
        )
        .unwrap()
        .unwrap();
        let mut assistant = ChatMessage::text("assistant", "");
        assistant.tool_calls = vec![call];
        let mut result = ChatMessage::text("tool", "found");
        result.tool_call_id = Some("call_1".into());
        turn_one.messages.extend([
            assistant,
            result,
            ChatMessage::text("assistant", "done"),
            ChatMessage::text(
                "user",
                "q2\n\n<turn_context>\n<skill name=\"s\">body</skill>\n</turn_context>",
            ),
        ]);
        let turn_two = ChatRequest {
            run_id: "run-2".into(),
            content_binding_digest: Some("b".repeat(64)),
            continuation: Some(state.clone()),
            ..turn_one
        };
        let (second, _, notes) =
            build_responses_request_notes(&turn_two, &identity, false).unwrap();
        assert!(notes.is_empty());
        let before = state["inputItems"].as_array().unwrap();
        assert_eq!(
            second["input"].as_array().unwrap()[..before.len()],
            before[..]
        );
        assert!(
            second["input"]
                .to_string()
                .contains("\"id\":\"fc_private\"")
        );
    }
    #[test]
    fn empty_selection_is_bound_within_the_same_run() {
        let identity = ConnectionIdentity {
            provider_id: "p".into(),
            protocol: "responses".into(),
            connection_id: "c".into(),
            generation: 1,
        };
        let mut input = ChatRequest {
            run_id: "run".into(),
            model: "model".into(),
            messages: vec![ChatMessage::text("user", "question")],
            continuation_scope: Some(agentkit_core::ContinuationScope {
                chat_id: "chat".into(),
                branch_id: "branch".into(),
            }),
            ..Default::default()
        };
        let body = build_responses_request(&input, &identity, false).unwrap();
        let checkpoint = make_continuation(&input, &identity, &body, &[], "answer", &[])
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint["runContent"]["runId"], "run");
        input
            .messages
            .push(ChatMessage::text("assistant", "answer"));
        input
            .selected_content
            .push(ChatMessage::text("system", "new instructions"));
        input.continuation = Some(checkpoint);
        assert_eq!(
            build_responses_request(&input, &identity, false)
                .unwrap_err()
                .code,
            "continuation_history_mismatch"
        );
        input.run_id = "next-run".into();
        assert!(build_responses_request(&input, &identity, false).is_ok());
    }

    #[test]
    fn dropped_tool_produces_a_diagnostic_naming_tool_and_key() {
        let identity = ConnectionIdentity {
            provider_id: "p".into(),
            protocol: "responses".into(),
            connection_id: "c".into(),
            generation: 1,
        };
        let tool = |name: &str, schema: Value| {
            serde_json::from_value(json!({"name":name,"version":"1","effect":"read","capability":"test","description":"d","inputSchema":schema})).unwrap()
        };
        let input = ChatRequest {
            run_id: "run".into(),
            model: "model".into(),
            messages: vec![ChatMessage::text("user", "hi")],
            tools: vec![
                tool(
                    "good",
                    json!({"type":"object","properties":{"a":{"type":"string"}}}),
                ),
                tool(
                    "bad",
                    json!({"type":"object","properties":{"a":{"type":"string"}},"minProperties":1}),
                ),
            ],
            ..Default::default()
        };
        let (body, warnings) =
            build_responses_request_with_warnings(&input, &identity, false).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("bad"), "{}", warnings[0]);
        assert!(warnings[0].contains("minProperties"), "{}", warnings[0]);
        let names = body["tools"][0]["tools"].as_array().unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0]["name"], "good");
    }
    #[test]
    fn lane_lint_reports_keys_outside_the_allow_list() {
        let schema = json!({"type":"object","properties":{"a":{"type":"array","items":{"type":"string","contentEncoding":"x"}}},"minProperties":1});
        let found = schema_lane_violations(SchemaLane::Responses, &schema);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().any(|v| v.contains("minProperties")));
        assert!(found.iter().any(|v| v.contains("contentEncoding")));
        assert!(
            schema_lane_violations(
                SchemaLane::Responses,
                &json!({"type":"object","$schema":"x","properties":{}})
            )
            .is_empty()
        );
        assert!(schema_lane_violations(SchemaLane::Anthropic, &schema).is_empty());
    }
}
