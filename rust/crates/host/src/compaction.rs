//! Derived transcript compaction. Cut/serialization logic is adapted from Pi,
//! MIT, Copyright (c) 2025 Mario Zechner, earendil-works/pi commit
//! 4a080252f06caf63a525d7873c5d94d3c23887e0.
use agentkit_core::ChatMessage;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(super) type Entry = (ChatMessage, Option<String>);
pub(super) const REVISION: &str = "agentkit-compaction-v1";

pub(super) fn hash_source(entries: &[Entry]) -> Option<String> {
    let rows: Vec<Value> = entries
        .iter()
        .map(|(message, id)| {
            Some(json!([
                id.as_ref()?,
                message.role,
                message.content,
                message.tool_call_id,
                message.tool_calls
            ]))
        })
        .collect::<Option<_>>()?;
    let encoded = agentkit_contracts::stringify_javascript_value(&json!([
        "agentkit-compaction-source-v2",
        rows
    ]))
    .ok()?;
    Some(format!("{:x}", Sha256::digest(encoded.as_bytes())))
}

pub(super) fn apply_summary(entries: &[Entry], record: &Value) -> Option<Vec<Entry>> {
    if record["summarizerRevision"].as_str()? != REVISION {
        return None;
    }
    let body_start = usize::from(
        entries
            .first()
            .is_some_and(|entry| entry.0.role == "system"),
    );
    let body = &entries[body_start..];
    let through = body
        .iter()
        .position(|entry| entry.1.as_deref() == record["coversThroughMessageId"].as_str())?;
    let kept = through.checked_add(1)?;
    if body.get(kept)?.1.as_deref() != record["firstKeptMessageId"].as_str() {
        return None;
    }
    if hash_source(&body[..kept])? != record["sourceHash"].as_str()? {
        return None;
    }
    let text = format!(
        "The conversation history before this point was compacted into the following summary:\n<summary>\n{}\n</summary>",
        record["summary"].as_str()?
    );
    let mut result = entries[..body_start].to_vec();
    result.push((
        ChatMessage::text("user", &text),
        Some(format!("summary:{}", record["summaryId"].as_str()?)),
    ));
    result.extend_from_slice(&body[kept..]);
    Some(result)
}

pub(super) fn find_cut(entries: &[Entry], from: usize, keep_tokens: u64) -> usize {
    let starts: Vec<usize> = (from..entries.len())
        .filter(|&i| entries[i].0.role == "user" && (i == 0 || entries[i - 1].0.role != "tool"))
        .collect();
    let Some(&last) = starts.last() else {
        return from;
    };
    let mut kept = starts.len() - 1;
    let mut total = entries[last..]
        .iter()
        .map(|entry| super::context_budget::message_tokens(&entry.0))
        .sum::<u64>();
    while kept > 0 {
        let cost = entries[starts[kept - 1]..starts[kept]]
            .iter()
            .map(|entry| super::context_budget::message_tokens(&entry.0))
            .sum::<u64>();
        if total.saturating_add(cost) > keep_tokens {
            break;
        }
        total += cost;
        kept -= 1;
    }
    starts[kept]
}

pub(super) fn serialize_conversation(entries: &[Entry]) -> String {
    let mut output = Vec::new();
    for (message, _) in entries {
        let mut text = super::hooks::message_text(&message.content);
        match message.role.as_str() {
            "user" if !text.is_empty() => output.push(format!("[User]: {text}")),
            "assistant" => {
                if !text.is_empty() {
                    output.push(format!("[Assistant]: {text}"));
                }
                if !message.tool_calls.is_empty() {
                    let calls = message
                        .tool_calls
                        .iter()
                        .map(|call| {
                            format!(
                                "{}({})",
                                call.name,
                                describe_arguments(&call.arguments_json)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    output.push(format!("[Assistant tool calls]: {calls}"));
                }
            }
            "tool" if !text.is_empty() => {
                let count = text.encode_utf16().count();
                if count > 2000 {
                    text = text
                        .chars()
                        .scan(0usize, |n, ch| {
                            *n += ch.len_utf16();
                            (*n <= 2000).then_some(ch)
                        })
                        .collect();
                    text.push_str(&format!(
                        "\n\n[... {} more characters truncated]",
                        count - 2000
                    ));
                }
                output.push(format!("[Tool result]: {text}"));
            }
            _ => {}
        }
    }
    output.join("\n\n")
}

fn describe_arguments(raw: &str) -> String {
    let Ok(Value::Object(object)) = agentkit_contracts::parse_javascript_json(raw) else {
        return raw.into();
    };
    object
        .into_iter()
        .map(|(key, value)| {
            format!(
                "{key}={}",
                agentkit_contracts::stringify_javascript_value(&value)
                    .unwrap_or_else(|_| "null".into())
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(id: &str, role: &str, text: &str) -> Entry {
        (ChatMessage::text(role, text), Some(id.into()))
    }
    #[test]
    fn summary_invalidates_for_edits_or_changed_path() {
        let input = vec![
            entry("a", "user", "one"),
            entry("b", "assistant", "two"),
            entry("c", "user", "three"),
        ];
        let summary = json!({"summaryId":"s", "summarizerRevision":REVISION, "coversThroughMessageId":"b", "firstKeptMessageId":"c", "sourceHash":hash_source(&input[..2]).unwrap(), "summary":"checkpoint"});
        assert_eq!(apply_summary(&input, &summary).unwrap().len(), 2);
        let mut edited = input.clone();
        edited[0].0.content = json!("changed");
        assert!(apply_summary(&edited, &summary).is_none());
        edited = input.clone();
        edited[2].1 = Some("other".into());
        assert!(apply_summary(&edited, &summary).is_none());
    }
    #[test]
    fn summary_invalidates_when_tool_call_changes() {
        let mut input = vec![entry("a", "assistant", ""), entry("b", "user", "next")];
        input[0].0.tool_calls.push(agentkit_core::ToolCall {
            id: "c".into(),
            name: "read_file".into(),
            arguments_json: "{}".into(),
        });
        let summary = json!({"summaryId":"s", "summarizerRevision":REVISION, "coversThroughMessageId":"a", "firstKeptMessageId":"b", "sourceHash":hash_source(&input[..1]).unwrap(), "summary":"checkpoint"});
        assert!(apply_summary(&input, &summary).is_some());
        input[0].0.tool_calls[0].arguments_json = "{\"path\":\"changed\"}".into();
        assert!(apply_summary(&input, &summary).is_none());
        let mut legacy = summary;
        let legacy_json =
            agentkit_contracts::stringify_javascript_value(&json!([["a", "assistant", "", null]]))
                .unwrap();
        legacy["sourceHash"] = json!(format!("{:x}", Sha256::digest(legacy_json.as_bytes())));
        assert!(apply_summary(&input, &legacy).is_none());
    }
    #[test]
    fn cut_preserves_whole_latest_turn_and_tool_pair() {
        let input = vec![
            entry("a", "user", "first"),
            entry("b", "assistant", "call"),
            entry("c", "tool", "result"),
            entry("d", "assistant", "done"),
            entry("e", "user", "last"),
        ];
        assert_eq!(find_cut(&input, 0, 0), 4);
        assert_eq!(find_cut(&input, 0, 100), 0);
    }
}
