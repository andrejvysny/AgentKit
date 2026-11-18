//! Hardening H1.6 (B6), end to end through the run loop: a closed-schema
//! tool refuses a bad call with a message that names the property and the
//! reason, and a documented old parameter name is accepted before the
//! closed-schema check and recorded on the call's record.
use agentkit_core::{
    CancellationToken, ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream,
    Runtime, Tool, ToolCall, ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_eval::run::{Run, drive, options};
use agentkit_testkit::ManualClock;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// A closed-schema tool whose `text` parameter was once called `body`.
struct NoteSave {
    definition: ToolDefinition,
    received: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl Tool for NoteSave {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    fn arg_aliases(&self) -> Vec<(String, String)> {
        vec![("body".into(), "text".into())]
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        self.received.lock().unwrap().push(input);
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"ok": true})),
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

/// Sends `calls` once, then answers; keeps every request it was sent.
struct Script {
    calls: Vec<ToolCall>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl ChatProvider for Script {
    fn id(&self) -> &str {
        "eval-script"
    }
    fn stream_chat(&self, request: ChatRequest, _: CancellationToken) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        requests.push(request);
        let event = if requests.len() == 1 {
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

fn call(id: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "note_save".into(),
        arguments_json: arguments.to_string(),
    }
}

/// The text the model was sent back for `id`.
fn result_text(request: &ChatRequest, id: &str) -> String {
    let messages = serde_json::to_value(&request.messages).unwrap();
    let message = messages
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message.to_string().contains(id))
        .unwrap_or_else(|| panic!("no result for {id}: {messages}"));
    message["content"].as_str().unwrap_or_default().to_owned()
}

fn note_save() -> (Arc<ToolRegistry>, Arc<Mutex<Vec<Value>>>) {
    let definition = serde_json::from_value(json!({
        "name": "note_save", "version": "1", "effect": "write", "capability": "eval.fixture",
        "description": "Save a note.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "text": {"type": "string", "minLength": 1},
                "scope": {"type": "string", "enum": ["user", "project"]}
            },
            "required": ["text", "scope"],
            "additionalProperties": false
        }
    }))
    .unwrap();
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(NoteSave {
            definition,
            received: Arc::clone(&received),
        }))
        .unwrap();
    (Arc::new(registry), received)
}

struct Played {
    run: Run,
    script: Arc<Script>,
    received: Arc<Mutex<Vec<Value>>>,
}

async fn play(calls: Vec<ToolCall>) -> Played {
    let (registry, received) = note_save();
    let script = Arc::new(Script {
        calls,
        requests: Mutex::new(vec![]),
    });
    let clock: Arc<dyn Runtime> = ManualClock::new(1_000);
    let provider: Arc<dyn ChatProvider> = script.clone();
    let run = drive(options(&registry, clock), provider, registry).await;
    assert!(run.types().contains(&"run.completed"), "{:?}", run.types());
    Played {
        run,
        script,
        received,
    }
}

/// A documented old name (`Tool::arg_aliases`) is mapped before the closed
/// schema is checked, so the tool runs, and the record names the alias.
#[tokio::test(flavor = "current_thread")]
async fn an_old_parameter_name_passes_a_closed_schema_and_is_recorded() {
    let played = play(vec![call("c_old", json!({"body": "a", "scope": "user"}))]).await;
    assert_eq!(
        *played.received.lock().unwrap(),
        [json!({"text": "a", "scope": "user"})]
    );
    let record = played
        .run
        .data("run.tool.record")
        .into_iter()
        .find(|record| record["toolCallId"] == "c_old")
        .expect("record for c_old");
    assert_eq!(record["aliasesUsed"], json!(["body->text"]), "{record}");
    assert_eq!(record["outcomeClass"], "ok", "{record}");
}

/// The refusal the model reads names the property and the reason; an extra
/// property comes with the properties the tool accepts.
#[tokio::test(flavor = "current_thread")]
async fn a_closed_schema_refusal_names_the_property_and_the_reason() {
    let played = play(vec![
        call(
            "c_extra",
            json!({"text": "a", "scope": "user", "reason": "r"}),
        ),
        call("c_enum", json!({"text": "a", "scope": "global"})),
    ])
    .await;
    assert!(played.received.lock().unwrap().is_empty(), "nothing ran");
    let requests = played.script.requests.lock().unwrap();
    let extra = result_text(&requests[1], "c_extra");
    assert!(extra.contains("/reason"), "{extra}");
    assert!(extra.contains("is not allowed"), "{extra}");
    assert!(extra.contains("\\\"text\\\""), "lists the allowed: {extra}");
    assert!(
        extra.contains("\\\"scope\\\""),
        "lists the allowed: {extra}"
    );
    let bad = result_text(&requests[1], "c_enum");
    assert!(bad.contains("/scope"), "{bad}");
    assert!(bad.contains("global"), "{bad}");
    assert!(!bad.contains("ECMAScript"), "{bad}");
}
