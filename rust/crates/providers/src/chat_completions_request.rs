//! The Chat Completions request body, with the session's frozen model
//! profile applied (T2.2): message rules, reasoning sent back, sampling and
//! template options, tool choice, and the history sanitiser.

use crate::content;
use agentkit_core::{
    ChatMessage, ChatRequest, CoreError, ProfileNote, REASONING_HISTORY_FIELD,
    REASONING_METADATA_KEY, ToolCall, apply_message_rules, resolve_request_settings,
    wire_tool_choice,
};
use serde_json::{Value, json};

/// Arguments text the next request may carry. A template that maps the
/// arguments needs an object; anything else is replaced by `{}` and counted.
fn history_arguments(call: &ToolCall, replaced: &mut Vec<String>) -> String {
    match agentkit_contracts::parse_javascript_json(&call.arguments_json) {
        Ok(Value::Object(_)) => call.arguments_json.clone(),
        _ => {
            replaced.push(call.id.clone());
            "{}".into()
        }
    }
}
fn history_reasoning(message: &ChatMessage) -> Option<&str> {
    message
        .metadata
        .as_ref()?
        .get(REASONING_METADATA_KEY)?
        .as_str()
        .filter(|text| !text.is_empty())
}
/// The body, the roles whose parts were flattened to text, and the notes to
/// report as warnings.
pub(crate) fn build_body(
    input: &ChatRequest,
) -> Result<(Value, Vec<String>, Vec<ProfileNote>), CoreError> {
    content::validate_selected(&input.selected_content)?;
    let history: Vec<ChatMessage> = input
        .selected_content
        .iter()
        .chain(input.messages.iter())
        .cloned()
        .collect();
    let (history, mut notes) = match &input.profile {
        Some(profile) => apply_message_rules(history, profile),
        None => (history, vec![]),
    };
    let send_back = input
        .profile
        .as_ref()
        .is_some_and(|profile| profile.reasoning.send_back);
    let mut messages = vec![];
    let mut roles = vec![];
    let mut replaced = vec![];
    for message in &history {
        let (content, dropped) = content::parts(message, false, true)?;
        if dropped && !roles.contains(&message.role) {
            roles.push(message.role.clone());
        }
        let mut mapped = json!({
            "role": message.role,
            "content": content,
        });
        if let Some(name) = &message.name {
            mapped["name"] = name.clone().into();
        }
        if let Some(id) = &message.tool_call_id {
            mapped["tool_call_id"] = id.clone().into();
        }
        if send_back
            && message.role == "assistant"
            && let Some(reasoning) = history_reasoning(message)
        {
            mapped[REASONING_HISTORY_FIELD] = reasoning.into();
        }
        if !message.tool_calls.is_empty() {
            mapped["tool_calls"] = message
                .tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": history_arguments(call, &mut replaced),
                        },
                    })
                })
                .collect();
        }
        messages.push(mapped);
    }
    if !replaced.is_empty() {
        notes.push(ProfileNote {
            code: "history_arguments_sanitized",
            message: format!(
                "Replaced malformed arguments with {{}} in the history of {} earlier tool call(s): {}.",
                replaced.len(),
                replaced.join(", ")
            ),
        });
    }
    let mut body = json!({
        "model": input.model,
        "messages": messages,
        "stream": true,
        "stream_options": {
            "include_usage": true,
        },
    });
    let tools_present = !input.tools.is_empty();
    match &input.profile {
        Some(profile) => {
            let settings = resolve_request_settings(
                profile,
                input.reasoning_effort,
                input.reasoning_off,
                input.temperature,
                tools_present,
                &input.tool_choice,
            );
            for (name, value) in settings.fields {
                body[name] = value;
            }
            if let Some(choice) = settings.tool_choice {
                body["tool_choice"] = choice;
            }
            notes.extend(settings.notes);
        }
        None => {
            if let Some(value) = input.reasoning_effort {
                body["reasoning_effort"] = serde_json::to_value(value).expect("enum serialization");
            }
            if let Some(value) = input.temperature {
                body["temperature"] = value.into();
            }
            if tools_present {
                body["tool_choice"] = wire_tool_choice(&input.tool_choice);
            }
        }
    }
    if let Some(value) = input.max_output_tokens {
        body["max_tokens"] = value.into();
    }
    if tools_present {
        // A strict dialect (T3.3) asks for strict calls on the tools whose
        // schema is strict; other tools stay non-strict.
        let strict = input
            .profile
            .as_ref()
            .is_some_and(|profile| agentkit_core::strict_dialect(&profile.dialect.id));
        body["tools"] = input
            .tools
            .iter()
            .map(|tool| {
                let mut wire = json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": agentkit_core::presented_description(
                            tool,
                            agentkit_core::ExampleLane::ChatCompletions,
                        ),
                        "parameters": tool.input_schema,
                    },
                });
                if strict && agentkit_core::is_strict_compliant(&tool.input_schema) {
                    wire["function"]["strict"] = true.into();
                }
                wire
            })
            .collect();
    }
    Ok((body, roles, notes))
}
