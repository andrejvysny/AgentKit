use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

struct Script {
    turns: Mutex<Vec<Vec<Result<ProviderEvent, CoreError>>>>,
}
#[async_trait]
impl ChatProvider for Script {
    fn id(&self) -> &str {
        "mock"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        Box::pin(futures_util::stream::iter(
            self.turns.lock().unwrap().remove(0),
        ))
    }
}
struct Echo {
    definition: ToolDefinition,
    hangs: bool,
}
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        if self.definition.name == "counter" {
            struct Fails;
            impl serde::Serialize for Fails {
                fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                    Err(serde::ser::Error::custom(
                        "JSON.stringify cannot serialize BigInt.",
                    ))
                }
            }
            ToolResult::serialize_data("counter", &Fails)?;
        }
        if self.hangs {
            return futures_util::future::pending().await;
        }
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"echoed":input["text"]})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
        })
    }
}
fn text_turn(text: &str) -> Vec<Result<ProviderEvent, CoreError>> {
    vec![
        Ok(ProviderEvent::Started),
        Ok(ProviderEvent::Delta(text.into())),
        Ok(ProviderEvent::Event {
            kind: "run.message.completed".into(),
            data: json!({"content":text,"toolCallCount":0}),
        }),
    ]
}
fn call_turn(calls: Vec<ToolCall>) -> Vec<Result<ProviderEvent, CoreError>> {
    let mut events = vec![
        Ok(ProviderEvent::Started),
        Ok(ProviderEvent::Event {
            kind: "run.message.completed".into(),
            data: json!({"content":"","toolCallCount":calls.len()}),
        }),
    ];
    events.extend(
        calls
            .into_iter()
            .map(|call| Ok(ProviderEvent::ToolRequested(call))),
    );
    events
}
fn call(id: &str, name: &str, text: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: if name == "hangs" {
            "{}".into()
        } else {
            json!({"text":text}).to_string()
        },
    }
}
fn normalized(mut event: Value) -> Value {
    let object = event.as_object_mut().unwrap();
    object.remove("timestamp");
    object.remove("eventId");
    normalize_numbers(&mut event);
    event
}
fn normalize_numbers(value: &mut Value) {
    match value {
        Value::Number(number) => {
            if let Some(number) = number.as_f64() {
                if number.fract() == 0.0 && number >= 0.0 {
                    *value = Value::from(number as u64);
                }
            }
        }
        Value::Object(object) => {
            for value in object.values_mut() {
                normalize_numbers(value);
            }
        }
        Value::Array(items) => {
            for value in items {
                normalize_numbers(value);
            }
        }
        _ => {}
    }
}
#[tokio::test]
async fn nine_native_scenarios_match_frozen_typescript_goldens() {
    for name in [
        "chat-only",
        "tool-run",
        "cancelled-run",
        "failed-run",
        "usage-run",
        "tool-cap-run",
        "duplicate-id-run",
        "tool-timeout-run",
        "unserializable-run",
    ] {
        let mut options = RunOptions {
            run_id: Some(format!("run-golden-{name}")),
            ..Default::default()
        };
        options.request.model = "gpt-golden".into();
        options.request.messages = vec![ChatMessage::text("user", "hi")];
        let mut registry = ToolRegistry::new();
        let turns = match name {
            "chat-only" => vec![text_turn("Hello from the golden chat-only trace.")],
            "cancelled-run" => {
                options.cancellation.cancel();
                vec![text_turn("never sent")]
            }
            "failed-run" => vec![vec![
                Ok(ProviderEvent::Started),
                Ok(ProviderEvent::Delta("half an ans".into())),
                Err(CoreError::new("provider_error", "socket hang up")),
            ]],
            "usage-run" => {
                let mut turn = text_turn("Usage-tracked answer.");
                turn.push(Ok(ProviderEvent::Usage(json!({"callId":"call-usage-0","attempt":1,"step":0,"model":"gpt-golden","promptTokens":10,"completionTokens":5,"totalTokens":15,"source":"stream","finalForCall":true}))));
                vec![turn]
            }
            "tool-run" => vec![
                call_turn(vec![call("call-golden-1", "echo", "golden")]),
                text_turn("Done."),
            ],
            "tool-cap-run" => {
                options.max_tool_calls_per_iteration = 1;
                vec![
                    call_turn(vec![
                        call("call-golden-cap-1", "echo", "a"),
                        call("call-golden-cap-2", "echo", "b"),
                    ]),
                    text_turn("Done."),
                ]
            }
            "duplicate-id-run" => vec![
                call_turn(vec![
                    call("call-golden-dup", "echo", "one"),
                    call("call-golden-dup", "echo", "two"),
                ]),
                text_turn("Done."),
            ],
            "unserializable-run" => vec![
                call_turn(vec![ToolCall {
                    id: "call-golden-unserializable".into(),
                    name: "counter".into(),
                    arguments_json: "{}".into(),
                }]),
                text_turn("Done."),
            ],
            "tool-timeout-run" => {
                options.default_tool_timeout_ms = Some(5);
                vec![
                    call_turn(vec![call("call-golden-timeout", "hangs", "")]),
                    text_turn("Done."),
                ]
            }
            _ => unreachable!(),
        };
        if name.contains("tool") || name == "duplicate-id-run" || name == "unserializable-run" {
            let hangs = name == "tool-timeout-run";
            let definition=serde_json::from_value(json!({"name":if hangs {"hangs"} else if name == "unserializable-run" {"counter"} else {"echo"},"version":"1","effect":"read","capability":"test","description":"Echoes its input back.","inputSchema":if hangs || name == "unserializable-run" {json!({"type":"object","properties":{}})} else {json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]})}})).unwrap();
            registry
                .register(Arc::new(Echo { definition, hangs }))
                .unwrap();
        }
        let handle = run_chat(
            options,
            Arc::new(Script {
                turns: Mutex::new(turns),
            }),
            Arc::new(registry),
        );
        let actual = handle
            .events
            .map(|event| normalized(serde_json::to_value(event).unwrap()))
            .collect::<Vec<_>>()
            .await;
        handle.result.await.unwrap();
        let path = format!(
            "{}/../../../packages/testing/src/golden/traces/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let expected: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            actual,
            expected.into_iter().map(normalized).collect::<Vec<_>>(),
            "{name}"
        );
    }
}
