use regex::Regex;
use serde_json::Value;
use std::{collections::HashSet, sync::LazyLock};

pub const EMULATED_TOOL_CALL_MESSAGE: &str = "This model wrote tool calls as text instead of calling the tools, so nothing was actually run. Pick a model with native tool-calling support, or check that the provider has tool calling enabled.";

static BLOCKS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?is)```(?:json|tool_call|json5)?\s*\n?(.*?)```",
        r"(?is)<tool_call>(.*?)</tool_call>",
        r"(?is)<function_call>(.*?)</function_call>",
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("fixed expression"))
    .collect()
});

pub fn looks_like_emulated_tool_call(content: &str, staged: &HashSet<String>) -> bool {
    if content.is_empty() || staged.is_empty() {
        return false;
    }
    BLOCKS.iter().any(|pattern| {
        pattern
            .captures_iter(content)
            .any(|capture| names_staged_tool(capture[1].trim(), staged))
    }) || names_staged_tool(content.trim(), staged)
}

fn names_staged_tool(block: &str, staged: &HashSet<String>) -> bool {
    let Ok(value) = agentkit_contracts::parse_javascript_json(block) else {
        return false;
    };
    let candidates = match &value {
        Value::Array(items) => items.as_slice(),
        _ => std::slice::from_ref(&value),
    };
    candidates.iter().any(|candidate| {
        is_staged_call(candidate, staged)
            || candidate
                .get("function")
                .is_some_and(|nested| is_staged_call(nested, staged))
    })
}

fn is_staged_call(value: &Value, staged: &HashSet<String>) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    ["arguments", "parameters", "args", "input"]
        .iter()
        .any(|key| object.contains_key(*key))
        && ["name", "tool", "tool_name", "function"].iter().any(|key| {
            object
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|name| staged.contains(name))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_closed_parseable_blocks_naming_staged_tools() {
        let staged = HashSet::from(["search_docs".into()]);
        for answer in [
            r#"{"name":"search_docs","arguments":{}}"#,
            "```JSON\n{\"tool\":\"search_docs\",\"input\":{}}\n```",
            r#"<tool_call>{"function":{"name":"search_docs","arguments":"{}"}}</tool_call>"#,
            r#"<FUNCTION_CALL>[{"tool_name":"search_docs","args":{}}]</FUNCTION_CALL>"#,
        ] {
            assert!(looks_like_emulated_tool_call(answer, &staged), "{answer}");
        }
        for answer in [
            r#"Example {"name":"search_docs","arguments":{}} in prose."#,
            r#"{"name":"not_staged","arguments":{}}"#,
            r#"{"name":"search_docs"}"#,
            r#"<tool_call>{"name":"search_docs","arguments":{}}"#,
            r#"<tool_call>{"name":"search_docs","arguments":{}}</function_call>"#,
            "```json {broken}```",
            "",
        ] {
            assert!(!looks_like_emulated_tool_call(answer, &staged), "{answer}");
        }
    }
}
