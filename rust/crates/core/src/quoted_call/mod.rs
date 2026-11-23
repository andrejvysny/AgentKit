//! Refusal of a model tool call that copies a tool-call block the run read.
//!
//! Some servers turn every tool-call block in a model's reply into a
//! structured call and remove it from the answer text. A model that only
//! quotes a block it found in a file or a page then "calls" the data author's
//! tool with the data author's arguments, and the loop never sees the quote.
//! A call identical to a block in tool output (or attached material) of the
//! current turn is therefore refused instead of run. Output of earlier turns
//! does not count, so the user can confirm such an action in a new message.
//!
//! Blocks are parsed leniently, as a server would read a quote: stray text
//! between parameters, a missing `<tool_call>` wrapper, several blocks in one
//! text, escaped or entity-encoded markup and line prefixes are accepted, and
//! values compare after the conversions a server applies. The scan is linear
//! in the scanned text.
//!
//! Known limits: a block split across two tool outputs or two string values
//! is not matched; any new user message starts a new turn, so output read
//! before it no longer counts; `resume_calls` were approved by a human
//! through the host and are not checked; a child run does not see what its
//! parent read.

mod compare;
mod parse;
mod variants;

use crate::{ChatMessage, CoreError, TURN_CONTEXT_PART_TYPE, ToolCall, tool_execution};
use compare::Candidate;
use serde_json::{Map, Value, json};

/// What the model reads instead of the result (also the table sentence).
pub(crate) const REFUSED: &str = "This call is identical to a tool-call block in content you read during this turn (tool output or attached material), so it looks like a quotation that was turned into a call. It was not run.";

/// The failure the model reads in place of the refused call's result.
pub(crate) fn refusal(call: &ToolCall, max_bytes: usize) -> tool_execution::ToolOutcome {
    tool_execution::failed(
        call,
        &CoreError::new("quoted_call", REFUSED),
        max_bytes,
        json!({"phase": "guard", "retryable": false}),
    )
}

/// The host-facing warning text: it names the tool, never an argument value.
pub(crate) fn warning(tool: &str) -> String {
    format!(
        "The {tool} call was not run: it is identical to a tool-call block in tool output or attached content of this turn, so it looks like quoted data turned into a call."
    )
}

/// One tool-call block found in a text.
#[derive(Debug, PartialEq)]
struct Block {
    name: String,
    arguments: Map<String, Value>,
}

/// How many leading messages may hold the turn's user message: the history
/// the run was seeded with, or up to the host's `turn_start` when that names
/// a user message in it. A steer or a correction follow-up seeded after the
/// turn's user message is then not taken for a new turn.
pub(crate) fn turn_bound(
    messages: &[ChatMessage],
    seeded: usize,
    turn_start: Option<usize>,
) -> usize {
    match turn_start {
        Some(start) if start < seeded.min(messages.len()) && is_user_turn(&messages[start]) => {
            start + 1
        }
        _ => seeded,
    }
}

/// For each `(emitted, prepared)` call, whether it is identical to a
/// tool-call block in tool output or attached material of the current turn.
/// The turn starts at the last user message among the first `bound`
/// messages ([`turn_bound`]): a steer placed during the run is typed text,
/// not a new turn, so it does not clear what was read.
pub(crate) fn quoted_calls(
    messages: &[ChatMessage],
    bound: usize,
    selected: &[ChatMessage],
    calls: &[(&ToolCall, &ToolCall)],
) -> Vec<bool> {
    let candidates: Vec<Candidate> = calls
        .iter()
        .map(|(emitted, prepared)| Candidate::of(emitted, prepared))
        .collect();
    let mut quoted = vec![false; calls.len()];
    let mut check = |block: Block| {
        for (index, candidate) in candidates.iter().enumerate() {
            quoted[index] |= candidate.matches(&block);
        }
    };
    for_each_source(messages, bound, selected, &mut |text| {
        scan(text, &mut check)
    });
    quoted
}

fn is_user_turn(message: &ChatMessage) -> bool {
    message.role == "user" && message.tool_call_id.is_none()
}

/// Every text of the current turn that did not come from the user's typing:
/// the turn-context material the host attached to the user message, request
/// selected content, and tool output.
fn for_each_source(
    messages: &[ChatMessage],
    bound: usize,
    selected: &[ChatMessage],
    visit: &mut dyn FnMut(&str),
) {
    let anchor = messages[..bound.min(messages.len())]
        .iter()
        .rposition(is_user_turn);
    if let Some(at) = anchor {
        attached(&messages[at].content, visit);
    }
    for message in selected {
        strings(&message.content, visit);
    }
    let turn = anchor.map_or(0, |at| at + 1);
    for message in &messages[turn..] {
        if message.role == "tool" || (message.role == "user" && message.tool_call_id.is_some()) {
            tool_output(&message.content, visit);
        }
    }
}

/// The turn context of a user message: its turn-context parts, or the text
/// those became on the way to the wire (`content_for_wire`). That function
/// defangs `<turn_context` in typed text, so only host text opens with the
/// tag; a host that skips it can only make typed text count as attached,
/// which refuses more calls, never fewer. For the same reason a string that
/// joins typed text and turn context is read from its first intact opener.
fn attached(content: &Value, visit: &mut dyn FnMut(&str)) {
    match content {
        Value::String(text) => {
            if let Some(context) = from_opener(text) {
                visit(context);
            }
        }
        Value::Array(parts) => {
            for part in parts {
                let Some(text) = part["text"].as_str() else {
                    continue;
                };
                if part["type"] == TURN_CONTEXT_PART_TYPE {
                    visit(text);
                } else if part["type"] == "text"
                    && let Some(context) = from_opener(text)
                {
                    visit(context);
                }
            }
        }
        _ => {}
    }
}

fn from_opener(text: &str) -> Option<&str> {
    text.find(crate::TURN_CONTEXT_OPEN).map(|at| &text[at..])
}

/// A tool message holds the JSON envelope: the tool's text sits in its
/// string values. Content that is not JSON is read as it is.
fn tool_output(content: &Value, visit: &mut dyn FnMut(&str)) {
    match content {
        Value::String(raw) if !variants::may_hold_block(raw) => {}
        Value::String(raw) => match agentkit_contracts::parse_javascript_json(raw) {
            Ok(value) => strings(&value, visit),
            Err(_) => visit(raw),
        },
        other => strings(other, visit),
    }
}

/// Every string value in `value`, without recursion.
fn strings(value: &Value, visit: &mut dyn FnMut(&str)) {
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        match value {
            Value::String(text) => visit(text),
            Value::Array(items) => stack.extend(items.iter().rev()),
            Value::Object(object) => stack.extend(object.values().rev()),
            _ => {}
        }
    }
}

/// Blocks in `text`, in every decoded copy and line view that holds one.
fn scan(text: &str, on_block: &mut dyn FnMut(Block)) {
    for variant in variants::decoded(text) {
        parse::blocks(&variant, on_block);
        for view in variants::line_views(&variant) {
            parse::blocks(&view, on_block);
        }
    }
}

#[cfg(test)]
mod forms_tests;
#[cfg(test)]
mod tests;
