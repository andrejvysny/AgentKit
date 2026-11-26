//! One failure type for every tool (TU-10) and the repair budget (TU-11).
use agentkit_core::failure_catalog::{self, STOP_RETRYING};
use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && !path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("test"))
        {
            out.push(path);
        }
    }
}

/// String literal at the start of `text` (after whitespace), if it is a code.
fn leading_code(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix('"')?;
    let end =
        rest.find(|ch: char| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'))?;
    (end > 0 && rest[..end].starts_with(|ch: char| ch.is_ascii_lowercase())).then(|| &rest[..end])
}

/// Codes raised in non-test source, found by constructor.
fn codes_in_source() -> BTreeSet<String> {
    let constructors = [
        "CodingError::new(",
        "CoreError::new(",
        " denied(",
        "Refused(format!(",
        "Refused(",
    ];
    let mut files = vec![];
    for krate in [
        "core",
        "toolpacks-coding",
        "agents",
        "governance",
        "runtime",
        "mcp",
        "lsp",
    ] {
        rust_files(&crates_dir().join(krate).join("src"), &mut files);
    }
    let mut found = BTreeSet::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        // Unit tests sit after this marker; they raise made-up codes.
        let text = text.split("#[cfg(test)]").next().unwrap();
        for constructor in constructors {
            for (at, _) in text.match_indices(constructor) {
                let after = &text[at + constructor.len()..];
                let Some(code) = leading_code(after) else {
                    continue;
                };
                // `Refused("...")` also carries sentences; only `code:` or a
                // bare code counts as a code.
                let end = after.trim_start()[1..].find('"').unwrap_or(0);
                let literal = &after.trim_start()[1..1 + end];
                if constructor.starts_with("Refused")
                    && !(literal == code || literal[code.len()..].starts_with(':'))
                {
                    continue;
                }
                found.insert(code.to_owned());
            }
        }
    }
    found.extend(sandbox_codes_in_source());
    found
}

/// Codes the sandbox pack raises: `Refusal::new("code", ..)` in a body and
/// `refusal_text("code", ..)` in its guard. The workspace service's codes
/// are a `match`; the runtime's sandbox lane test checks those.
fn sandbox_codes_in_source() -> BTreeSet<String> {
    let mut files = vec![];
    rust_files(
        &crates_dir().join("toolpacks-sandbox").join("src"),
        &mut files,
    );
    let mut found = BTreeSet::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let text = text.split("#[cfg(test)]").next().unwrap();
        for constructor in ["Refusal::new(", "refusal_text("] {
            for (at, _) in text.match_indices(constructor) {
                if let Some(code) = leading_code(&text[at + constructor.len()..]) {
                    found.insert(code.to_owned());
                }
            }
        }
    }
    found
}

#[test]
fn every_code_used_in_source_is_in_the_table() {
    let listed: BTreeSet<&str> = failure_catalog::catalog().map(|row| row.code).collect();
    let unlisted: Vec<String> = codes_in_source()
        .into_iter()
        .filter(|code| !listed.contains(code.as_str()))
        .collect();
    assert!(
        unlisted.is_empty(),
        "failure codes missing from core/src/failure_table.rs: {unlisted:?}"
    );
}

#[test]
fn the_scan_finds_codes_so_the_check_cannot_pass_by_finding_nothing() {
    let found = codes_in_source();
    for code in [
        "tool_missing",
        "edit_no_match",
        "not_found",
        "read_only",
        "export_stale",
        "network_approval_required",
    ] {
        assert!(found.contains(code), "scan lost {code}: {found:?}");
    }
}

#[test]
fn every_row_has_a_next_step_a_sentence_and_a_unique_code() {
    let mut seen = BTreeSet::new();
    for row in failure_catalog::catalog() {
        assert!(seen.insert(row.code), "duplicate code {}", row.code);
        assert!(row.next_step.len() > 8, "{} has no next step", row.code);
        assert!(
            row.message.ends_with('.'),
            "{} message is no sentence",
            row.code
        );
    }
}

#[test]
fn the_rows_the_spec_names_are_filled() {
    let next = |code: &str| failure_catalog::spec(code).unwrap().next_step;
    assert!(next("not_found").contains("List the folder"));
    assert!(next("approval_denied").contains("ask the user"));
    assert!(next("read_only").contains("Describe the change"));
    assert!(next("tool_timeout").contains("longer timeout"));
    assert!(next("tool_not_allowed").contains("available"));
    assert!(next("subagents_disabled").contains("yourself"));
    assert!(failure_catalog::spec("spawn_key_required").is_some());
}

struct Script {
    calls: Vec<ToolCall>,
    requests: Mutex<Vec<Vec<ChatMessage>>>,
}
#[async_trait]
impl ChatProvider for Script {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, request: ChatRequest, _: CancellationToken) -> ProviderStream {
        let mut seen = self.requests.lock().unwrap();
        seen.push(request.messages);
        let event = if seen.len() == 1 {
            ProviderEvent::Completed {
                content: String::new(),
                tool_calls: self.calls.clone(),
                finish_reason: "tool_calls".into(),
            }
        } else {
            ProviderEvent::Completed {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            }
        };
        Box::pin(futures_util::stream::iter([Ok(event)]))
    }
}

struct Edit {
    definition: ToolDefinition,
}
#[async_trait]
impl Tool for Edit {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: true,
            data: Some(input),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
            model_content: None,
        })
    }
}

fn call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: args.into(),
    }
}

async fn run(calls: Vec<ToolCall>) -> (Vec<Value>, Vec<ChatMessage>) {
    let mut registry = ToolRegistry::new();
    for name in ["edit_file", "read_file"] {
        let definition = serde_json::from_value(json!({"name":name,"version":"1","effect":"read",
            "capability":"test","description":"d","inputSchema":
            {"type":"object","required":["file_path"],"properties":{"file_path":{"type":"string"}}}}))
        .unwrap();
        registry.register(Arc::new(Edit { definition })).unwrap();
    }
    let provider = Arc::new(Script {
        calls,
        requests: Mutex::new(vec![]),
    });
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                ..Default::default()
            },
            tool_call_records: true,
            max_tool_calls_per_iteration: 8,
            ..Default::default()
        },
        provider.clone(),
        Arc::new(registry),
    );
    let events = handle
        .events
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    // The results the run recorded: a repeated identical failure ends the
    // run before the provider is asked again.
    let messages = handle.result.await.unwrap().appended_messages;
    (events, messages)
}

fn envelopes(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .filter(|message| message.role == "tool")
        .map(|message| serde_json::from_str(message.content.as_str().unwrap()).unwrap())
        .collect()
}

fn records(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|event| event["type"] == "run.tool.record")
        .map(|event| &event["data"])
        .collect()
}

#[tokio::test]
async fn a_failed_call_carries_message_why_next_and_the_structured_failure() {
    let (_, messages) = run(vec![call("c1", "no_such_tool", "{}")]).await;
    let envelope = &envelopes(&messages)[0];
    let summary = envelope["summary"].as_str().unwrap();
    assert!(
        summary.contains("Tool not registered: no_such_tool"),
        "{summary}"
    );
    assert!(summary.contains("\nNext: "), "{summary}");
    let failure = &envelope["data"]["failure"];
    assert_eq!(failure["code"], "tool_missing");
    assert_eq!(failure["class"], "unknown_tool");
    assert_eq!(failure["retryable"], true);
    assert!(
        failure["nextStep"]
            .as_str()
            .unwrap()
            .contains("tool_search")
    );
    assert_eq!(envelope["data"]["errorCode"], "tool_missing");
}

#[tokio::test]
async fn second_failure_in_one_class_stops_retrying_and_third_identical_call_is_a_repeat() {
    let (events, messages) = run(vec![
        call("c1", "edit_file", "{}"),
        call("c2", "edit_file", r#"{"limit":1}"#),
        call("c3", "edit_file", "{}"),
        call("c4", "edit_file", "{}"),
    ])
    .await;
    let results = envelopes(&messages);
    let summary = |at: usize| results[at]["summary"].as_str().unwrap().to_owned();
    assert!(
        !summary(0).contains(STOP_RETRYING),
        "first failure: {}",
        summary(0)
    );
    for at in 1..4 {
        assert!(
            summary(at).contains(STOP_RETRYING),
            "failure {at}: {}",
            summary(at)
        );
    }
    let classes: Vec<&str> = records(&events)
        .iter()
        .map(|record| record["outcomeClass"].as_str().unwrap())
        .collect();
    assert_eq!(
        classes,
        [
            "invalid_structure",
            "invalid_structure",
            "invalid_structure",
            "repeat_identical"
        ]
    );
}

#[tokio::test]
async fn the_stop_message_is_per_tool_and_a_success_clears_it() {
    let (_, messages) = run(vec![
        call("c1", "edit_file", "{}"),
        call("c2", "read_file", "{}"),
        call("c3", "edit_file", r#"{"file_path":"a"}"#),
        call("c4", "edit_file", "{}"),
    ])
    .await;
    for result in envelopes(&messages) {
        let text = result["summary"].as_str().unwrap_or_default();
        assert!(!text.contains(STOP_RETRYING), "{text}");
    }
}
