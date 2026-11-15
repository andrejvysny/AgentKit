//! Call-time legacy tool names (T5.2, R-API-12): a retired name maps to its
//! replacement or teaches the model, and never shows up in any listing.
use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Script {
    calls: Vec<ToolCall>,
    turns: Mutex<usize>,
}
#[async_trait]
impl ChatProvider for Script {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let mut turns = self.turns.lock().unwrap();
        *turns += 1;
        let event = if *turns == 1 {
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
    received: Arc<Mutex<Vec<Value>>>,
}
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    fn arg_aliases(&self) -> Vec<(String, String)> {
        vec![("block".into(), "content".into())]
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        self.received.lock().unwrap().push(input);
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

fn legacy(old: &str, new: Option<&str>, teach: &str) -> LegacyName {
    LegacyName {
        old: old.into(),
        new: new.map(str::to_owned),
        teach: teach.into(),
    }
}

fn registry(received: &Arc<Mutex<Vec<Value>>>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for name in ["artifact_edit", "editor_replace"] {
        let definition = serde_json::from_value(json!({"name":name,"version":"1","effect":"write",
            "capability":"test","description":"d","inputSchema":{"type":"object",
            "required":["content"],"properties":{"content":{"type":"string"}}}}))
        .unwrap();
        registry
            .register(Arc::new(Echo {
                definition,
                received: received.clone(),
            }))
            .unwrap();
    }
    assert!(registry.register_legacy_name(legacy("write_block", Some("artifact_edit"), "unused")));
    assert!(registry.register_legacy_name(legacy(
        "edit_content",
        None,
        "It was split: call {{tool:editor_replace}} to change text."
    )));
    assert!(registry.register_legacy_name(legacy(
        "show_widget",
        Some("visual_show_widget"),
        "Widgets are not offered here; use {{tool:artifact_edit}} instead."
    )));
    registry
}

struct Deny {
    seen: Arc<Mutex<Vec<Value>>>,
}
#[async_trait]
impl RunHooks for Deny {
    fn indexes(&self, name: &str) -> Vec<usize> {
        if name == "beforeToolCall" { vec![0] } else { vec![] }
    }
    async fn call(&self, _: &str, _: usize, context: Value, _: &CancellationToken) -> Result<Value, CoreError> {
        self.seen.lock().unwrap().push(context);
        Ok(json!({"value":{"action":"deny","reason":"guard says no"},"defined":true}))
    }
}

async fn run(
    registry: ToolRegistry,
    calls: Vec<ToolCall>,
    hooks: Option<Arc<dyn RunHooks>>,
) -> Vec<Value> {
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                ..Default::default()
            },
            tool_call_records: true,
            max_tool_calls_per_iteration: 8,
            hooks,
            ..Default::default()
        },
        Arc::new(Script {
            calls,
            turns: Mutex::new(0),
        }),
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
fn of_type<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["type"] == kind)
        .map(|event| &event["data"])
        .collect()
}
fn call(name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments_json: args.into(),
    }
}

#[tokio::test]
async fn mapped_call_runs_as_the_new_tool_with_its_aliases_and_is_recorded() {
    let received = Arc::new(Mutex::new(vec![]));
    let events = run(registry(&received), vec![call("write_block", r#"{"block":"hi"}"#)], None).await;
    assert_eq!(received.lock().unwrap()[0], json!({"content":"hi"}));
    assert_eq!(of_type(&events, "run.tool.succeeded")[0]["toolName"], "artifact_edit");
    let record = of_type(&events, "run.tool.record")[0];
    assert_eq!(record["outcomeClass"], "ok");
    assert_eq!(
        record["aliasesUsed"],
        json!(["legacy_name:write_block->artifact_edit", "block->content"])
    );
}

#[tokio::test]
async fn mapped_call_gets_the_same_guard_verdict_as_a_direct_call() {
    let mut verdicts = vec![];
    for name in ["artifact_edit", "write_block"] {
        let received = Arc::new(Mutex::new(vec![]));
        let seen = Arc::new(Mutex::new(vec![]));
        let hooks: Arc<dyn RunHooks> = Arc::new(Deny { seen: seen.clone() });
        let events = run(registry(&received), vec![call(name, r#"{"content":"x"}"#)], Some(hooks)).await;
        assert!(received.lock().unwrap().is_empty(), "{name} must not run");
        let failed = of_type(&events, "run.tool.failed")[0];
        verdicts.push((failed["errorCode"].clone(), failed["errorMessage"].clone()));
        // The guard saw the canonical new call, never the old name.
        assert_eq!(seen.lock().unwrap()[0]["toolName"], "artifact_edit", "{name}");
    }
    assert_eq!(verdicts[0], verdicts[1]);
    assert_eq!(verdicts[0].0, "tool_hook_denied");
}

#[tokio::test]
async fn unmapped_legacy_name_teaches_with_the_replacement_named() {
    let received = Arc::new(Mutex::new(vec![]));
    let events = run(registry(&received), vec![call("edit_content", "{}")], None).await;
    assert!(received.lock().unwrap().is_empty());
    let failed = of_type(&events, "run.tool.failed")[0];
    assert_eq!(failed["errorCode"], "tool_renamed");
    let message = failed["errorMessage"].as_str().unwrap();
    assert!(message.contains("edit_content was retired"), "{message}");
    assert!(message.contains("call editor_replace to change text"), "{message}");
    assert!(!message.contains("{{"), "{message}");
    assert_eq!(of_type(&events, "run.tool.record")[0]["outcomeClass"], "precondition");
}

#[tokio::test]
async fn mapping_to_a_tool_that_is_not_offered_teaches_instead() {
    let received = Arc::new(Mutex::new(vec![]));
    let events = run(registry(&received), vec![call("show_widget", "{}")], None).await;
    assert!(received.lock().unwrap().is_empty());
    let failed = of_type(&events, "run.tool.failed")[0];
    assert_eq!(failed["errorCode"], "tool_renamed");
    assert!(failed["errorMessage"].as_str().unwrap().contains("use artifact_edit instead"));
}

#[tokio::test]
async fn a_withheld_replacement_is_not_offered() {
    let received = Arc::new(Mutex::new(vec![]));
    let mut registry = registry(&received);
    registry.withhold("artifact_edit", "read-only turn");
    let events = run(registry, vec![call("write_block", r#"{"content":"x"}"#)], None).await;
    assert!(received.lock().unwrap().is_empty());
    assert_eq!(of_type(&events, "run.tool.failed")[0]["errorCode"], "tool_renamed");
}

#[test]
fn legacy_names_are_not_listed_anywhere() {
    let received = Arc::new(Mutex::new(vec![]));
    let registry = registry(&received);
    let old = ["write_block", "edit_content", "show_widget"];
    let definitions = registry.definitions();
    assert_eq!(definitions.len(), 2);
    registry.freeze_surface(definitions.clone(), vec![]);
    assert!(registry.freeze_catalog(&[]));
    let listings = [
        registry.names(),
        registry.presented_names(),
        registry.definitions().into_iter().map(|d| d.name).collect(),
        registry.catalog().unwrap().names(),
    ];
    for listing in listings {
        for name in old {
            assert!(!listing.iter().any(|entry| entry == name), "{name} in {listing:?}");
        }
    }
    assert!(registry.get("write_block").is_none());
}

#[test]
fn a_real_tool_keeps_its_name_and_a_legacy_name_registers_once() {
    let received = Arc::new(Mutex::new(vec![]));
    let mut registry = registry(&received);
    assert!(!registry.register_legacy_name(legacy("artifact_edit", Some("editor_replace"), "t")));
    assert!(!registry.register_legacy_name(legacy("write_block", None, "again")));
    assert!(!registry.register_legacy_name(legacy("same", Some("same"), "t")));
}
