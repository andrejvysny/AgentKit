//! History sent back to the model. Stored messages keep canonical tool
//! calls; the model reads each call as it emitted it (its wire call, kept in
//! the message metadata), or, for a call stored without one, through the
//! session's dialect. The emitted form matters beyond style: a Responses
//! continuation hashes the calls exactly as the provider returned them, so
//! the next request must show those bytes again.

use super::Dialect;
use crate::{ChatMessage, ToolCall};
use serde_json::Value;
use std::collections::HashMap;

/// Where a stored assistant message keeps the tool calls as the model emitted
/// them, parallel to its canonical `tool_calls` (same ids, same order). Set
/// only when they differ from the canonical calls.
pub const WIRE_CALLS_METADATA_KEY: &str = "wireToolCalls";

/// The emitted calls recorded on `message`, when they pair with its stored
/// calls id by id.
fn wire_calls(message: &ChatMessage) -> Option<Vec<ToolCall>> {
    let value = message.metadata.as_ref()?.get(WIRE_CALLS_METADATA_KEY)?;
    let wire: Vec<ToolCall> = serde_json::from_value(value.clone()).ok()?;
    let paired = wire.len() == message.tool_calls.len()
        && wire
            .iter()
            .zip(&message.tool_calls)
            .all(|(emitted, stored)| emitted.id == stored.id);
    paired.then_some(wire)
}

/// `messages` with each tool call shown as the model emitted it, for a
/// session without a dialect.
pub fn restore_wire_calls(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    present(None, messages)
}

/// `messages` with each tool call shown as the model emitted it, else as the
/// wire tool that expresses it. A call no wire tool can express (or whose
/// arguments are not JSON) stays as stored. Tool results take the name of the
/// call they answer.
pub fn present_history(dialect: &Dialect, messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    present(Some(dialect), messages)
}

fn present(dialect: Option<&Dialect>, messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut shown: HashMap<String, String> = HashMap::new();
    let mut out = Vec::with_capacity(messages.len());
    for mut message in messages {
        if let Some(emitted) = wire_calls(&message) {
            for call in &emitted {
                shown.insert(call.id.clone(), call.name.clone());
            }
            message.tool_calls = emitted;
        } else if let Some(dialect) = dialect {
            for call in &mut message.tool_calls {
                if let Some(presented) = present_call(dialect, call) {
                    shown.insert(presented.id.clone(), presented.name.clone());
                    *call = presented;
                }
            }
        }
        if message.role == "tool"
            && let Some(id) = &message.tool_call_id
        {
            if let Some(name) = shown.get(id) {
                message.name = Some(name.clone());
            } else if let Some(dialect) = dialect
                && let Some(name) = &message.name
                && let Some(wire) = dialect.wire_names_of(name).into_iter().next()
            {
                message.name = Some(wire);
            }
        }
        out.push(message);
    }
    out
}

fn present_call(dialect: &Dialect, call: &ToolCall) -> Option<ToolCall> {
    let args: Value = agentkit_contracts::parse_javascript_json(&call.arguments_json).ok()?;
    let (name, shown) = dialect.wire_call(&call.name, &args)?;
    let arguments_json = if shown == args {
        call.arguments_json.clone()
    } else {
        crate::envelope::stringify(&shown)
    };
    Some(ToolCall {
        id: call.id.clone(),
        name,
        arguments_json,
    })
}
