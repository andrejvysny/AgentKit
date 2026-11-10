//! Summarization transcript and file details adapted from Pi, MIT,
//! Copyright (c) 2025 Mario Zechner, earendil-works/pi commit
//! 4a080252f06caf63a525d7873c5d94d3c23887e0.
use super::{compaction::Entry, compaction_prompts as prompts, context_budget::message_tokens};
use agentkit_core::ChatMessage;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(super) fn summary_messages(
    covered: &[Entry],
    previous: Option<&Value>,
    window: u64,
    reserve: u64,
) -> Option<(Vec<ChatMessage>, u64)> {
    let system = prompts::SUMMARIZATION_SYSTEM_PROMPT;
    let prompt = if previous.is_some() {
        format!(
            "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\n{}",
            prompts::UPDATE_SUMMARIZATION_INSTRUCTIONS
        )
    } else {
        prompts::SUMMARIZATION_PROMPT.into()
    };
    let previous = previous
        .map(|record| {
            format!(
                "\n\n<previous-summary>\n{}\n</previous-summary>",
                record["summary"].as_str().unwrap_or_default()
            )
        })
        .unwrap_or_default();
    let overhead = prompt.encode_utf16().count()
        + previous.encode_utf16().count()
        + system.encode_utf16().count()
        + "<conversation>\n\n</conversation>\n\n".len()
        + "[... earlier conversation omitted]\n\n".len()
        + 8;
    let budget = window
        .saturating_sub(reserve)
        .saturating_mul(4)
        .saturating_sub(overhead as u64) as usize;
    let conversation = bounded_conversation(covered, budget)?;
    let messages = vec![
        ChatMessage::text("system", system),
        ChatMessage::text(
            "user",
            &format!("<conversation>\n{conversation}\n</conversation>{previous}\n\n{prompt}"),
        ),
    ];
    let estimate: u64 = messages.iter().map(message_tokens).sum();
    let output = reserve.saturating_mul(8) / 10;
    let output = output
        .max(1)
        .min(reserve)
        .min(window.saturating_sub(estimate));
    (estimate.saturating_add(reserve) <= window && output > 0).then_some((messages, output))
}

fn bounded_conversation(covered: &[Entry], budget: usize) -> Option<String> {
    let conversation = super::compaction::serialize_conversation(covered);
    (conversation.encode_utf16().count() <= budget).then_some(conversation)
}

pub(super) fn file_details(covered: &[Entry], previous: Option<&Value>) -> (Value, Vec<String>) {
    let mut read = BTreeSet::new();
    let mut modified = BTreeSet::new();
    for (field, set) in [("readFiles", &mut read), ("modifiedFiles", &mut modified)] {
        if let Some(files) = previous.and_then(|p| p["details"][field].as_array()) {
            set.extend(files.iter().filter_map(Value::as_str).map(str::to_owned));
        }
    }
    let mut commands = previous
        .and_then(|p| p["summary"].as_str())
        .and_then(|s| s.split_once("<commands-run>\n"))
        .and_then(|(_, s)| s.split_once("\n</commands-run>"))
        .map(|(body, _)| body.lines().map(str::to_owned).collect::<Vec<_>>())
        .unwrap_or_default();
    for (message, _) in covered {
        for call in &message.tool_calls {
            let Ok(args) = serde_json::from_str::<Value>(&call.arguments_json) else {
                continue;
            };
            if let Some(path) = args["path"].as_str().filter(|path| !path.is_empty()) {
                match call.name.as_str() {
                    "read_file" | "code_search" => {
                        read.insert(path.into());
                    }
                    "edit_file" | "write_file" => {
                        modified.insert(path.into());
                    }
                    _ => {}
                }
            }
            if matches!(call.name.as_str(), "run_command" | "run_tests")
                && let Some(command) = args["command"].as_str().filter(|value| !value.is_empty())
            {
                commands.push(command.chars().take(200).collect());
            }
        }
    }
    read.retain(|path| !modified.contains(path));
    if commands.len() > 20 {
        commands.drain(..commands.len() - 20);
    }
    (
        json!({"readFiles":read, "modifiedFiles":modified}),
        commands,
    )
}

pub(super) fn add_details(text: &str, details: &Value, commands: &[String]) -> String {
    let mut blocks = vec![];
    for (field, tag) in [
        ("readFiles", "read-files"),
        ("modifiedFiles", "modified-files"),
    ] {
        if let Some(files) = details[field].as_array().filter(|v| !v.is_empty()) {
            blocks.push(format!(
                "<{tag}>\n{}\n</{tag}>",
                files
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
    }
    if !commands.is_empty() {
        blocks.push(format!(
            "<commands-run>\n{}\n</commands-run>",
            commands.join("\n")
        ));
    }
    if blocks.is_empty() {
        text.into()
    } else {
        format!("{text}\n\n{}", blocks.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_transcript_cannot_claim_full_coverage() {
        let covered = vec![(
            ChatMessage::text("user", &format!("{}newest", "a".repeat(20_000))),
            Some("id".into()),
        )];
        assert!(summary_messages(&covered, None, 2048, 512).is_none());
    }

    #[test]
    fn file_details_promote_reads_to_writes() {
        let mut message = ChatMessage::text("assistant", "");
        message.tool_calls = vec![
            agentkit_core::ToolCall {
                id: "a".into(),
                name: "read_file".into(),
                arguments_json: r#"{"path":"a.txt"}"#.into(),
            },
            agentkit_core::ToolCall {
                id: "b".into(),
                name: "write_file".into(),
                arguments_json: r#"{"path":"a.txt"}"#.into(),
            },
            agentkit_core::ToolCall {
                id: "c".into(),
                name: "run_tests".into(),
                arguments_json: r#"{"command":"bun test"}"#.into(),
            },
        ];
        let (details, commands) = file_details(&[(message, Some("id".into()))], None);
        assert_eq!(details["readFiles"], json!([]));
        assert_eq!(details["modifiedFiles"], json!(["a.txt"]));
        assert_eq!(commands, vec!["bun test"]);
    }
}
