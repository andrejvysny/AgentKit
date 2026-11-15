//! History sent back to the model. Stored messages keep canonical tool
//! names; the model must read the names it was offered, so every request
//! shows earlier calls and results through the session's dialect.

use super::Dialect;
use crate::{ChatMessage, ToolCall};
use serde_json::Value;
use std::collections::HashMap;

/// `messages` with each tool call shown as the wire tool that expresses it.
/// A call no wire tool can express (or whose arguments are not JSON) stays
/// as stored. Tool results take the name of the call they answer.
pub fn present_history(dialect: &Dialect, messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut shown: HashMap<String, String> = HashMap::new();
    let mut out = Vec::with_capacity(messages.len());
    for mut message in messages {
        for call in &mut message.tool_calls {
            if let Some(presented) = present_call(dialect, call) {
                shown.insert(presented.id.clone(), presented.name.clone());
                *call = presented;
            }
        }
        if message.role == "tool"
            && let Some(id) = &message.tool_call_id
        {
            if let Some(name) = shown.get(id) {
                message.name = Some(name.clone());
            } else if let Some(name) = &message.name
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
