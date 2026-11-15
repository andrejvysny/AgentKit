//! Message rules of a profile (T2.2, TU-18): one system message and it comes
//! first, at least one user message, text that imitates a tool result is
//! escaped, images only where the profile allows them.
//!
//! Pure, on the request's messages before a transport maps them to its wire
//! format. Every change that is not a plain layout merge is returned as a
//! note, so nothing is silent.

use super::request::ProfileNote;
use crate::ChatMessage;
use agentkit_contracts::{AiModelImagePosition, AiModelToolProfile};
use serde_json::Value;

/// The tags Qwen's chat template treats as a tool result when a user turn
/// begins and ends with them.
pub const TOOL_RESPONSE_OPEN: &str = "<tool_response>";
pub const TOOL_RESPONSE_CLOSE: &str = "</tool_response>";
const ESCAPED_OPEN: &str = "&lt;tool_response&gt;";
const ESCAPED_CLOSE: &str = "&lt;/tool_response&gt;";

/// The text with its leading and trailing tool-response tags escaped, or
/// `None` when it does not both begin and end with them.
pub fn escape_tool_response_tags(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.len() < TOOL_RESPONSE_OPEN.len() + TOOL_RESPONSE_CLOSE.len()
        || !trimmed.starts_with(TOOL_RESPONSE_OPEN)
        || !trimmed.ends_with(TOOL_RESPONSE_CLOSE)
    {
        return None;
    }
    let start = text.find(TOOL_RESPONSE_OPEN)?;
    let end = text.rfind(TOOL_RESPONSE_CLOSE)?;
    Some(format!(
        "{}{ESCAPED_OPEN}{}{ESCAPED_CLOSE}{}",
        &text[..start],
        &text[start + TOOL_RESPONSE_OPEN.len()..end],
        &text[end + TOOL_RESPONSE_CLOSE.len()..]
    ))
}

/// The text of a message body: a string, or the text parts of a part list.
fn plain_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn allowed_images(profile: &AiModelToolProfile, role: &str) -> bool {
    let positions = &profile.message_rules.image_positions;
    match role {
        "user" => positions.contains(&AiModelImagePosition::User),
        "tool" => positions.contains(&AiModelImagePosition::ToolResult),
        _ => false,
    }
}

/// Remove image parts from messages whose role the profile does not allow
/// them in. Returns how many parts were removed.
fn drop_images(profile: &AiModelToolProfile, messages: &mut [ChatMessage]) -> usize {
    let mut dropped = 0;
    for message in messages {
        if allowed_images(profile, &message.role) {
            continue;
        }
        let Value::Array(parts) = &mut message.content else {
            continue;
        };
        let before = parts.len();
        parts.retain(|part| part["type"] != "image");
        let removed = before - parts.len();
        dropped += removed;
        if removed > 0 && parts.is_empty() {
            message.content = Value::String(String::new());
        }
    }
    dropped
}

/// Escape user and tool-result text that imitates a tool result. Returns how
/// many messages changed.
fn escape_messages(messages: &mut [ChatMessage]) -> usize {
    let mut changed = 0;
    for message in messages {
        if !matches!(message.role.as_str(), "user" | "tool") {
            continue;
        }
        match &mut message.content {
            Value::String(text) => {
                if let Some(escaped) = escape_tool_response_tags(text) {
                    *text = escaped;
                    changed += 1;
                }
            }
            Value::Array(parts) => {
                let mut touched = false;
                for part in parts.iter_mut().filter(|part| part["type"] == "text") {
                    if let Some(escaped) = part["text"].as_str().and_then(escape_tool_response_tags)
                    {
                        part["text"] = Value::String(escaped);
                        touched = true;
                    }
                }
                changed += usize::from(touched);
            }
            _ => {}
        }
    }
    changed
}

fn text_message(role: &str, text: String) -> ChatMessage {
    ChatMessage::text(role, &text)
}

/// One system message first. The leading run of system messages (the layout
/// blocks S2-S4 and managed instructions) is merged in order, separated by a
/// blank line; a system message after the first non-system message is moved
/// into a user turn unchanged. Returns the number of moved messages.
fn system_first(messages: Vec<ChatMessage>) -> (Vec<ChatMessage>, usize) {
    let lead = messages
        .iter()
        .take_while(|message| message.role == "system")
        .count();
    let mut output = Vec::with_capacity(messages.len());
    let mut moved = 0;
    let mut iter = messages.into_iter();
    let leading: Vec<ChatMessage> = iter.by_ref().take(lead).collect();
    match leading.len() {
        0 => {}
        1 => output.extend(leading),
        _ => {
            let text = leading
                .iter()
                .map(|message| plain_text(&message.content))
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            output.push(text_message("system", text));
        }
    }
    for mut message in iter {
        if message.role == "system" {
            message.role = "user".into();
            message.metadata = None;
            moved += 1;
        }
        output.push(message);
    }
    (output, moved)
}

/// Apply the profile's message rules. A profile with no rules returns the
/// messages unchanged.
pub fn apply_message_rules(
    messages: Vec<ChatMessage>,
    profile: &AiModelToolProfile,
) -> (Vec<ChatMessage>, Vec<ProfileNote>) {
    let rules = &profile.message_rules;
    let mut messages = messages;
    let mut notes = vec![];
    let images = drop_images(profile, &mut messages);
    if images > 0 {
        notes.push(ProfileNote::new(
            "image_position_dropped",
            format!(
                "Dropped {images} image part(s): profile {} allows images only in {}.",
                profile.id,
                if allowed_images(profile, "user") {
                    "user messages"
                } else {
                    "no message position"
                }
            ),
        ));
    }
    if rules.escape_tool_result_tags {
        let escaped = escape_messages(&mut messages);
        if escaped > 0 {
            notes.push(ProfileNote::new(
                "tool_result_tags_escaped",
                format!(
                    "Escaped tool-response tags in {escaped} message(s); the model's template would read them as a tool result."
                ),
            ));
        }
    }
    if !rules.system_first_only {
        return (messages, notes);
    }
    let (mut messages, moved) = system_first(messages);
    if moved > 0 {
        notes.push(ProfileNote::new(
            "system_message_moved",
            format!(
                "Moved {moved} later system message(s) into user turns: profile {} allows one system message, first.",
                profile.id
            ),
        ));
    }
    if !messages.iter().any(|message| message.role == "user") {
        if messages
            .first()
            .is_some_and(|message| message.role == "system")
        {
            let mut first = messages.remove(0);
            first.role = "user".into();
            first.metadata = None;
            messages.insert(0, first);
            notes.push(ProfileNote::new(
                "system_moved_to_user",
                format!(
                    "The request had no user message, which profile {} requires; the system text was sent as the user turn.",
                    profile.id
                ),
            ));
        } else {
            notes.push(ProfileNote::new(
                "no_user_message",
                format!(
                    "The request has no user message, which profile {} requires.",
                    profile.id
                ),
            ));
        }
    }
    (messages, notes)
}
