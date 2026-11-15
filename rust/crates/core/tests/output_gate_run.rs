//! Roadmap 2.1 (ADR 0034) through the run loop: a tool result over its
//! inline limit is stored before the model sees it, the tool message holds
//! the preview and the handle, and without a gate the old byte budget (and
//! its marker) still bounds the result.
use agentkit_core::*;
use agentkit_testkit::{InMemoryOutputStore, ManualClock};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

fn memory() -> Arc<InMemoryOutputStore> {
    Arc::new(InMemoryOutputStore::new(ManualClock::new(
        1_760_000_000_000,
    )))
}

/// One tool call, then a final answer.
struct OneCall(AtomicUsize);

#[async_trait]
impl ChatProvider for OneCall {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        let (content, tool_calls, finish) = if first {
            (
                String::new(),
                vec![ToolCall {
                    id: "call1".into(),
                    name: "big".into(),
                    arguments_json: "{}".into(),
                }],
                "tool_calls",
            )
        } else {
            ("done".into(), vec![], "stop")
        };
        Box::pin(futures_util::stream::iter(vec![
            Ok(ProviderEvent::Started),
            Ok(ProviderEvent::Completed {
                content,
                tool_calls,
                finish_reason: finish.into(),
            }),
        ]))
    }
}

struct Big {
    definition: ToolDefinition,
    saw_flag: Arc<Mutex<Option<Value>>>,
}

#[async_trait]
impl Tool for Big {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        *self.saw_flag.lock().unwrap() = context.metadata.get(OUTPUT_STORE_METADATA_KEY).cloned();
        let text: String = (1..=40_000).map(|n| format!("row {n}\n")).collect();
        Ok(ToolResult {
            ok: true,
            data: Some(Value::String(text)),
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

fn registry(saw_flag: Arc<Mutex<Option<Value>>>) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    let definition = serde_json::from_value(json!({"name":"big","version":"1","effect":"read","capability":"test","description":"Big","inputSchema":{"type":"object"}})).unwrap();
    registry
        .register(Arc::new(Big {
            definition,
            saw_flag,
        }))
        .unwrap();
    Arc::new(registry)
}

async fn run(output: Option<Arc<OutputGate>>, flag: Arc<Mutex<Option<Value>>>) -> RunResult {
    let handle = run_chat(
        RunOptions {
            scope_id: Some("s1".into()),
            output,
            ..Default::default()
        },
        Arc::new(OneCall(AtomicUsize::new(0))),
        registry(flag),
    );
    let _: Vec<_> = handle.events.collect().await;
    handle.result.await.unwrap()
}

fn tool_text(result: &RunResult) -> String {
    let message = result
        .appended_messages
        .iter()
        .find(|message| message.role == "tool")
        .expect("tool message");
    message.content.as_str().expect("tool text").to_owned()
}

#[tokio::test]
async fn a_large_result_is_stored_and_the_model_reads_a_preview() {
    let store = memory();
    let flag = Arc::new(Mutex::new(None));
    let result = run(Some(Arc::new(OutputGate::new(store.clone()))), flag.clone()).await;
    assert_eq!(result.terminal, Terminal::Completed);
    assert_eq!(*flag.lock().unwrap(), Some(json!(true)));
    let text = tool_text(&result);
    let envelope: Value = serde_json::from_str(&text).expect("envelope json");
    let preview = envelope["data"].as_str().expect("preview text");
    assert!(preview.len() <= DEFAULT_INLINE_LIMIT);
    assert!(preview.contains("[output stored: "));
    assert!(preview.contains("lines hidden …]"));
    assert_eq!(envelope["truncated"], false);
    assert!(!text.contains("[...truncated]"));
    assert_eq!(store.stored("s1"), 1);
    let handle = OutputHandle::parse(&preview[preview.rfind("handle out_").unwrap() + 7..][..36])
        .expect("footer handle");
    let info = store.info(&handle).await.expect("stored row");
    assert_eq!(
        (
            info.meta.session.as_str(),
            info.meta.tool.as_str(),
            info.meta.call_id.as_str()
        ),
        ("s1", "big", "call1")
    );
    assert_eq!(info.line_count, 40_000);
}

#[tokio::test]
async fn without_a_gate_the_byte_budget_still_applies() {
    let flag = Arc::new(Mutex::new(None));
    let result = run(None, flag.clone()).await;
    assert_eq!(*flag.lock().unwrap(), None);
    let text = tool_text(&result);
    assert!(text.len() <= ToolLimits::default().max_bytes);
    assert!(text.contains("[...truncated]"));
}

#[tokio::test]
async fn a_run_without_chat_or_scope_stores_under_its_run_id() {
    let store = memory();
    let handle = run_chat(
        RunOptions {
            run_id: Some("run-solo".into()),
            output: Some(Arc::new(OutputGate::new(store.clone()))),
            ..Default::default()
        },
        Arc::new(OneCall(AtomicUsize::new(0))),
        registry(Arc::new(Mutex::new(None))),
    );
    let _: Vec<_> = handle.events.collect().await;
    handle.result.await.unwrap();
    assert_eq!(store.stored("run-solo"), 1);
}
