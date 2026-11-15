//! The assistant call a run stores is the canonical prepared identity that
//! guards, approvals and receipts hash (T3.1): dialect translated, legacy
//! name mapped, aliases and text-typed values normalised. The call as the
//! model emitted it rides in message metadata, and the next request shows it
//! again.
use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Capture(Mutex<Vec<ChatMessage>>);

#[async_trait]
impl RunEventSink for Capture {
    async fn append(&self, _: &AiRunEvent) -> Result<(), CoreError> {
        Ok(())
    }
    async fn append_messages(
        &self,
        _: &AiRunEvent,
        messages: &[ChatMessage],
    ) -> Result<(), CoreError> {
        self.0.lock().unwrap().extend(messages.iter().cloned());
        Ok(())
    }
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

struct Echo {
    definition: ToolDefinition,
    received: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
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

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: args.to_string(),
    }
}

fn registry(received: &Arc<Mutex<Vec<Value>>>, meta: bool) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for (name, schema) in [
        (
            "edit_file",
            json!({"type":"object","required":["file_path"],"properties":{
                "file_path":{"type":"string"},"limit":{"type":"integer"},
                "timeout":{"type":"integer"},"command":{"type":"string"}}}),
        ),
        (
            "read_file",
            json!({"type":"object","properties":{"file_path":{"type":"string"}}}),
        ),
    ] {
        let definition = serde_json::from_value(json!({"name":name,"version":"1","effect":"read",
            "capability":"test","description":"d","inputSchema":schema}))
        .unwrap();
        registry
            .register(Arc::new(Echo {
                definition,
                received: received.clone(),
            }))
            .unwrap();
    }
    assert!(registry.register_legacy_name(LegacyName {
        old: "old_edit".into(),
        new: Some("edit_file".into()),
        teach: "Use edit_file.".into(),
    }));
    if meta {
        registry.install_meta_tools().unwrap();
        let classification = Classification {
            direct: DirectSet::names(["read_file"]),
            max_tokens: 0,
            max_definitions: 0,
            ..Classification::default()
        };
        let surface =
            ToolSurface::reconcile_with(None, &registry.definitions(), Some(&classification));
        registry.freeze_surface(
            surface.manifest.definitions().to_vec(),
            surface.deferred_names(),
        );
        registry.freeze_catalog(&surface.deferred);
    }
    registry
}

struct Outcome {
    stored: Vec<ChatMessage>,
    requests: Vec<Vec<ChatMessage>>,
}

async fn run(registry: ToolRegistry, calls: Vec<ToolCall>) -> Outcome {
    let provider = Arc::new(Script {
        calls,
        requests: Mutex::new(vec![]),
    });
    let sink = Arc::new(Capture(Mutex::new(vec![])));
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                ..Default::default()
            },
            event_sink: Some(sink.clone()),
            max_tool_calls_per_iteration: 8,
            ..Default::default()
        },
        provider.clone(),
        Arc::new(registry),
    );
    handle.events.collect::<Vec<_>>().await;
    let stored = sink.0.lock().unwrap().clone();
    let requests = provider.requests.lock().unwrap().clone();
    Outcome { stored, requests }
}

fn assistant(outcome: &Outcome) -> &ChatMessage {
    outcome
        .stored
        .iter()
        .find(|message| message.role == "assistant" && !message.tool_calls.is_empty())
        .expect("stored assistant message with calls")
}

fn args(call: &ToolCall) -> Value {
    serde_json::from_str(&call.arguments_json).unwrap()
}

#[tokio::test]
async fn a_direct_call_is_stored_as_the_canonical_call_the_receipt_hashes() {
    let received = Arc::new(Mutex::new(vec![]));
    // Legacy name, alias (`path`, `cmd`, `timeout_sec`) and a string integer.
    let emitted = call(
        "c1",
        "old_edit",
        json!({"path":"a.txt","cmd":"x","limit":"5","timeout_sec":2}),
    );
    let outcome = run(registry(&received, false), vec![emitted.clone()]).await;
    let canonical = json!({"file_path":"a.txt","command":"x","limit":5,"timeout":2000});
    assert_eq!(received.lock().unwrap()[0], canonical);
    let stored = assistant(&outcome);
    assert_eq!(stored.tool_calls[0].id, "c1");
    assert_eq!(stored.tool_calls[0].name, "edit_file");
    assert_eq!(args(&stored.tool_calls[0]), canonical);
    // The emitted call is kept beside it.
    let wire: Vec<ToolCall> =
        serde_json::from_value(stored.metadata.as_ref().unwrap()[WIRE_CALLS_METADATA_KEY].clone())
            .unwrap();
    assert_eq!(wire, vec![emitted.clone()]);
    // The next request shows the model the call it emitted, not the canonical one.
    let shown = outcome.requests[1]
        .iter()
        .find(|message| message.role == "assistant" && !message.tool_calls.is_empty())
        .unwrap();
    assert_eq!(shown.tool_calls, vec![emitted]);
}

#[tokio::test]
async fn an_indirect_call_is_stored_with_the_canonical_inner_call() {
    let received = Arc::new(Mutex::new(vec![]));
    let emitted = call(
        "c1",
        "tool_call",
        json!({"name":"old_edit","arguments":{"path":"b.txt","limit":"7"}}),
    );
    let outcome = run(registry(&received, true), vec![emitted]).await;
    assert_eq!(
        received.lock().unwrap()[0],
        json!({"file_path":"b.txt","limit":7})
    );
    let stored = assistant(&outcome);
    assert_eq!(stored.tool_calls[0].name, "tool_call");
    // Hosts unwrap the stored call to compare it with approvals and receipts.
    let inner = unwrap_tool_call(&stored.tool_calls[0]).expect("inner call");
    assert_eq!(inner.id, "c1");
    assert_eq!(inner.name, "edit_file");
    assert_eq!(args(&inner), json!({"file_path":"b.txt","limit":7}));
}

#[tokio::test]
async fn a_call_that_needs_no_normalising_is_stored_as_emitted() {
    let received = Arc::new(Mutex::new(vec![]));
    let emitted = call("c1", "read_file", json!({"file_path":"a.txt"}));
    let outcome = run(registry(&received, false), vec![emitted.clone()]).await;
    let stored = assistant(&outcome);
    assert_eq!(stored.tool_calls, vec![emitted]);
    assert!(
        stored
            .metadata
            .as_ref()
            .is_none_or(|metadata| !metadata.contains_key(WIRE_CALLS_METADATA_KEY))
    );
}
