//! The Anthropic dialect when the session lacks an editor prerequisite
//! (T3.2): rendering falls back to the common forms, and the frozen dialect
//! must be the table that was rendered, or the advertised tools cannot be
//! called. Calls go through a filtered registry, not rendering alone.
use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

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

struct Echo {
    definition: ToolDefinition,
    ran: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        self.ran
            .lock()
            .unwrap()
            .push(format!("{}:{input}", self.definition.name));
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"ok":true})),
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

/// A registry holding `names`, frozen the way the chat executor freezes it:
/// the dialect is the effective table for exactly these tools.
fn frozen(names: &[&str], ran: &Arc<Mutex<Vec<String>>>) -> (ToolRegistry, Vec<ToolDefinition>) {
    let mut registry = ToolRegistry::new();
    for name in names {
        let definition = serde_json::from_value(json!({"name":name,"version":"1","effect":"read",
            "capability":"test","description":"d","inputSchema":{"type":"object","properties":{
                "path":{"type":"string"},"file_path":{"type":"string"},
                "old_string":{"type":"string"},"new_string":{"type":"string"},
                "content":{"type":"string"}}}}))
        .unwrap();
        registry
            .register(Arc::new(Echo {
                definition,
                ran: ran.clone(),
            }))
            .unwrap();
    }
    let canonical = registry.definitions();
    let dialect = anthropic_dialect().effective_for(&canonical);
    let rendered = dialect.render(&canonical);
    assert!(registry.freeze_presented(rendered.clone(), vec![], dialect));
    (registry, rendered)
}

async fn call_all(registry: ToolRegistry, calls: Vec<ToolCall>) -> Vec<Value> {
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
            max_tool_calls_per_iteration: 8,
            ..Default::default()
        },
        provider,
        Arc::new(registry),
    );
    handle
        .events
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect()
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: args.to_string(),
    }
}

fn failed(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|event| event["type"] == "run.tool.failed")
        .collect()
}

#[tokio::test]
async fn a_read_only_session_advertises_list_directory_and_it_runs() {
    let ran = Arc::new(Mutex::new(vec![]));
    // No write or edit tool: the editor needs all of read, write and edit.
    let (registry, rendered) = frozen(&["read_file", "list_dir"], &ran);
    let names: Vec<&str> = rendered.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["read_file", "list_directory"]);
    assert!(rendered.iter().all(|d| builtin_tool_type(d).is_none()));
    let events = call_all(
        registry,
        vec![call("c1", "list_directory", json!({"path":"src"}))],
    )
    .await;
    assert!(failed(&events).is_empty(), "{events:?}");
    assert_eq!(*ran.lock().unwrap(), [r#"list_dir:{"path":"src"}"#]);
}

#[tokio::test]
async fn a_partial_editor_set_advertises_edit_and_it_runs() {
    let ran = Arc::new(Mutex::new(vec![]));
    let (registry, rendered) = frozen(&["edit_file", "run_command"], &ran);
    let names: Vec<&str> = rendered.iter().map(|d| d.name.as_str()).collect();
    // `edit_file` is a common form; bash stays the vendor built-in.
    assert_eq!(names, ["edit", "bash"]);
    let events = call_all(
        registry,
        vec![
            call(
                "c1",
                "edit",
                json!({"file_path":"a","old_string":"x","new_string":"y"}),
            ),
            call("c2", "bash", json!({"command":"ls"})),
        ],
    )
    .await;
    assert!(failed(&events).is_empty(), "{events:?}");
    let ran = ran.lock().unwrap().clone();
    assert!(ran[0].starts_with("edit_file:"), "{ran:?}");
    assert_eq!(ran[1], r#"run_command:{"command":"ls"}"#);
}

#[tokio::test]
async fn the_complete_editor_set_still_uses_the_built_in_editor() {
    let ran = Arc::new(Mutex::new(vec![]));
    let (registry, rendered) = frozen(&["read_file", "write_file", "edit_file", "list_dir"], &ran);
    let names: Vec<&str> = rendered.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["str_replace_based_edit_tool"]);
    let events = call_all(
        registry,
        vec![call(
            "c1",
            "str_replace_based_edit_tool",
            json!({"command":"view","path":"src/"}),
        )],
    )
    .await;
    assert!(failed(&events).is_empty(), "{events:?}");
    assert_eq!(*ran.lock().unwrap(), [r#"list_dir:{"path":"src"}"#]);
}

#[test]
fn an_unoffered_built_in_name_is_not_translated_and_history_keeps_the_common_form() {
    let canonical: Vec<ToolDefinition> = ["read_file", "list_dir"]
        .iter()
        .map(|name| {
            serde_json::from_value(json!({"name":name,"version":"1","effect":"read",
                "capability":"test","description":"d","inputSchema":{"type":"object"}}))
            .unwrap()
        })
        .collect();
    let dialect = anthropic_dialect().effective_for(&canonical);
    let translated = dialect
        .translate(
            "str_replace_based_edit_tool",
            json!({"command":"view","path":"a"}),
        )
        .unwrap();
    assert_eq!(translated.name, "str_replace_based_edit_tool");
    let shown = dialect
        .wire_call("list_dir", &json!({"path":"src"}))
        .unwrap();
    assert_eq!(shown.0, "list_directory");
}
