//! Recovery of tool-call text the server did not parse (T2.2, TU-21).
//!
//! Qwen's XML-style call shape:
//!
//! ```text
//! <tool_call>
//! <function=name>
//! <parameter=key>
//! value
//! </parameter>
//! </function>
//! </tool_call>
//! ```
//!
//! Values are raw text and stay strings: the call pipeline converts types the
//! tool's schema asks for and records each conversion. A block that is not
//! well formed is left in the text, never half-parsed.

use serde_json::{Map, Value};

const CALL_OPEN: &str = "<tool_call>";
const CALL_CLOSE: &str = "</tool_call>";
const FUNCTION_OPEN: &str = "<function=";
const FUNCTION_CLOSE: &str = "</function>";
const PARAMETER_OPEN: &str = "<parameter=";
const PARAMETER_CLOSE: &str = "</parameter>";

#[derive(Clone, Debug, PartialEq)]
pub struct SalvagedCall {
    pub name: String,
    pub arguments: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Salvage {
    pub calls: Vec<SalvagedCall>,
    /// The reply with the parsed blocks removed, trimmed.
    pub text: String,
}

/// One parameter value as the template wrote it: a single newline after the
/// opening tag and before the closing tag is layout, not content.
fn value(raw: &str) -> &str {
    let raw = raw.strip_prefix('\n').unwrap_or(raw);
    raw.strip_suffix('\n').unwrap_or(raw)
}

/// The text up to `end`, and the rest after it.
fn split_at_tag<'a>(text: &'a str, end: &str) -> Option<(&'a str, &'a str)> {
    let at = text.find(end)?;
    Some((&text[..at], &text[at + end.len()..]))
}

fn parse_function(block: &str) -> Option<SalvagedCall> {
    let after = block.trim_start().strip_prefix(FUNCTION_OPEN)?;
    let (name, rest) = split_at_tag(after, ">")?;
    let (mut body, _) = split_at_tag(rest, FUNCTION_CLOSE)?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let mut arguments = Map::new();
    loop {
        body = body.trim_start();
        if body.is_empty() {
            break;
        }
        let after = body.strip_prefix(PARAMETER_OPEN)?;
        let (key, rest) = split_at_tag(after, ">")?;
        let (raw, rest) = split_at_tag(rest, PARAMETER_CLOSE)?;
        let key = key.trim();
        if key.is_empty() {
            return None;
        }
        arguments.insert(key.into(), Value::String(value(raw).into()));
        body = rest;
    }
    Some(SalvagedCall {
        name: name.into(),
        arguments,
    })
}

/// Parse every well-formed tool-call block in `content`, or `None` when there
/// is none. Blocks that do not parse stay in the returned text.
pub fn salvage_tool_calls(content: &str) -> Option<Salvage> {
    let mut calls = vec![];
    let mut text = String::new();
    let mut rest = content;
    while let Some(start) = rest.find(CALL_OPEN) {
        let after = &rest[start + CALL_OPEN.len()..];
        let (block, tail) = match split_at_tag(after, CALL_CLOSE) {
            Some((block, tail)) => (block, tail),
            // A reply cut off after the function closed still carries a call.
            None => match split_at_tag(after, FUNCTION_CLOSE) {
                Some((head, tail)) => (&after[..head.len() + FUNCTION_CLOSE.len()], tail),
                None => break,
            },
        };
        match parse_function(block) {
            Some(call) => {
                text.push_str(&rest[..start]);
                calls.push(call);
                rest = tail;
            }
            None => {
                let kept = rest.len() - after.len() + block.len();
                text.push_str(&rest[..kept]);
                rest = &rest[kept..];
            }
        }
    }
    text.push_str(rest);
    if calls.is_empty() {
        return None;
    }
    Some(Salvage {
        calls,
        text: text.trim().to_owned(),
    })
}

/// The nudge hook: a message to send after the model announced a call and
/// emitted none. Off until experiment X8 shows it helps, so this returns
/// `None` whatever the profile says.
pub fn nudge_after_announced_call(
    _profile: &agentkit_contracts::AiModelToolProfile,
    _content: &str,
) -> Option<String> {
    None
}
