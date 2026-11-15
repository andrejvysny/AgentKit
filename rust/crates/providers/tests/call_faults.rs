//! TU-09 and TU-12 on the chat-completions and Responses stream parsers: a
//! call with bad JSON, an empty name or an empty id still reaches the run loop
//! and gets a failure result; results of one turn follow in call order.
//! The Anthropic parser has the same fixtures in OneMind (`stream_tests.rs`).
use agentkit_core::*;
use agentkit_providers::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Transport {
    identity: ConnectionIdentity,
    replies: Mutex<Vec<String>>,
    sent: Mutex<Vec<Value>>,
}
impl Transport {
    fn new(protocol: &str, replies: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            identity: ConnectionIdentity {
                provider_id: "faults".into(),
                protocol: protocol.into(),
                connection_id: "c".into(),
                generation: 1,
            },
            replies: Mutex::new(replies),
            sent: Mutex::new(vec![]),
        })
    }
}
#[async_trait]
impl TrustedTransport for Transport {
    fn identity(&self) -> ConnectionIdentity {
        self.identity.clone()
    }
    fn auth_kind(&self) -> AuthKind {
        AuthKind::ApiKey
    }
    fn check_current(&self) -> Result<(), CoreError> {
        Ok(())
    }
    async fn request(
        &self,
        _: &ConnectionIdentity,
        _: &str,
        body: Option<Value>,
        _: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        self.sent.lock().unwrap().push(body.unwrap_or(Value::Null));
        let body = self.replies.lock().unwrap().remove(0);
        Ok(http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(body)
            .unwrap()
            .into())
    }
}

fn sse(values: &[Value]) -> String {
    values
        .iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect()
}

struct Write;
#[async_trait]
impl Tool for Write {
    fn definition(&self) -> &ToolDefinition {
        static DEFINITION: std::sync::OnceLock<ToolDefinition> = std::sync::OnceLock::new();
        DEFINITION.get_or_init(|| {
            serde_json::from_value(json!({"name":"write","version":"1","effect":"read",
                "capability":"test","description":"d","inputSchema":{"type":"object"}}))
            .unwrap()
        })
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"wrote":true})),
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

/// One chat-completions tool call delta.
fn chat_call(index: u64, id: Option<&str>, name: Option<&str>, args: &str) -> Value {
    let mut call = json!({"index":index,"function":{"arguments":args}});
    if let Some(id) = id {
        call["id"] = id.into();
    }
    if let Some(name) = name {
        call["function"]["name"] = name.into();
    }
    call
}
fn chat_turn(calls: Vec<Value>) -> String {
    sse(&[
        json!({"choices":[{"delta":{"tool_calls":calls}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ]) + "data: [DONE]\n\n"
}
fn chat_answer() -> String {
    sse(&[json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})])
        + "data: [DONE]\n\n"
}

fn function_call(slot: u64, id: &str, name: &str, args: &str) -> Value {
    json!({"id":format!("fc{slot}"),"type":"function_call","call_id":id,"name":name,
        "arguments":args,"namespace":"agentkit"})
}
fn responses_turn(items: Vec<Value>) -> String {
    sse(&[json!({"type":"response.completed","response":{"status":"completed","output":items}})])
}
fn responses_answer() -> String {
    sse(&[
        json!({"type":"response.completed","response":{"status":"completed","output":[
        {"id":"message1","type":"message","role":"assistant","status":"completed",
         "content":[{"type":"output_text","text":"done"}]}]}}),
    ])
}

struct Run {
    events: Vec<Value>,
    second_request: Value,
}
async fn run(provider: Arc<dyn ChatProvider>, transport: &Transport) -> Run {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Write)).unwrap();
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                messages: vec![ChatMessage::text("user", "go")],
                ..Default::default()
            },
            tool_call_records: true,
            ..Default::default()
        },
        provider,
        Arc::new(registry),
    );
    let events = handle
        .events
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    let second_request = transport.sent.lock().unwrap()[1].clone();
    Run {
        events,
        second_request,
    }
}
fn data<'a>(run: &'a Run, kind: &str) -> Vec<&'a Value> {
    run.events
        .iter()
        .filter(|event| event["type"] == kind)
        .map(|event| &event["data"])
        .collect()
}

/// (stream with one faulty call, expected id, expected failure code)
fn chat_faults() -> Vec<(&'static str, String, &'static str, &'static str)> {
    vec![
        (
            "bad json",
            chat_turn(vec![chat_call(0, Some("c1"), Some("write"), "{\"a\":")]),
            "c1",
            "bad_args",
        ),
        (
            "empty name",
            chat_turn(vec![chat_call(0, Some("c1"), None, "{}")]),
            "c1",
            "tool_name_missing",
        ),
        (
            "empty id",
            chat_turn(vec![chat_call(0, None, Some("missing_tool"), "{}")]),
            "call_0",
            "tool_missing",
        ),
    ]
}
fn responses_faults() -> Vec<(&'static str, String, &'static str, &'static str)> {
    vec![
        (
            "bad json",
            responses_turn(vec![function_call(1, "c1", "write", "{\"a\":")]),
            "c1",
            "bad_args",
        ),
        (
            "empty name",
            responses_turn(vec![function_call(1, "c1", "", "{}")]),
            "c1",
            "tool_name_missing",
        ),
        (
            "empty id",
            responses_turn(vec![function_call(1, "", "write", "{\"a\":")]),
            "call_0",
            "bad_args",
        ),
    ]
}

#[tokio::test]
async fn chat_completions_faulty_calls_each_get_a_failure_result() {
    for (fault, stream, id, code) in chat_faults() {
        let transport = Transport::new("chat-completions", vec![stream, chat_answer()]);
        let provider = ChatCompletionsProvider::new(transport.clone()).unwrap();
        let run = run(Arc::new(provider), &transport).await;
        let failed = data(&run, "run.tool.failed");
        assert_eq!(failed.len(), 1, "{fault}: {:?}", run.events);
        assert_eq!(failed[0]["toolCallId"], id, "{fault}");
        assert_eq!(failed[0]["errorCode"], code, "{fault}");
        let tool_message = run.second_request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool")
            .unwrap_or_else(|| panic!("{fault}: no result in next request"));
        assert_eq!(tool_message["tool_call_id"], id, "{fault}");
    }
}

#[tokio::test]
async fn responses_faulty_calls_each_get_a_failure_result() {
    for (fault, stream, id, code) in responses_faults() {
        let transport = Transport::new("responses", vec![stream, responses_answer()]);
        let provider = ResponsesProvider::new(transport.clone()).unwrap();
        let run = run(Arc::new(provider), &transport).await;
        let failed = data(&run, "run.tool.failed");
        assert_eq!(failed.len(), 1, "{fault}: {:?}", run.events);
        assert_eq!(failed[0]["toolCallId"], id, "{fault}");
        assert_eq!(failed[0]["errorCode"], code, "{fault}");
        let output = run.second_request["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .unwrap_or_else(|| panic!("{fault}: no result in next request"));
        assert_eq!(output["call_id"], id, "{fault}");
    }
}

#[tokio::test]
async fn chat_completions_results_follow_call_order_with_a_failure_between() {
    let stream = chat_turn(vec![
        chat_call(0, Some("c1"), Some("write"), "{}"),
        chat_call(1, Some("c2"), Some("write"), "{\"a\":"),
        chat_call(2, Some("c3"), Some("write"), "{}"),
    ]);
    let transport = Transport::new("chat-completions", vec![stream, chat_answer()]);
    let provider = ChatCompletionsProvider::new(transport.clone()).unwrap();
    let run = run(Arc::new(provider), &transport).await;
    let messages = run.second_request["messages"].as_array().unwrap();
    let first = messages.iter().position(|m| m["role"] == "tool").unwrap();
    let ids: Vec<_> = messages[first..]
        .iter()
        .map(|message| {
            assert_eq!(message["role"], "tool");
            message["tool_call_id"].as_str().unwrap()
        })
        .collect();
    assert_eq!(ids, ["c1", "c2", "c3"]);
    assert_eq!(data(&run, "run.tool.failed").len(), 1);
}

#[tokio::test]
async fn responses_results_follow_call_order_with_a_failure_between() {
    let stream = responses_turn(vec![
        function_call(1, "c1", "write", "{}"),
        function_call(2, "c2", "write", "{\"a\":"),
        function_call(3, "c3", "write", "{}"),
    ]);
    let transport = Transport::new("responses", vec![stream, responses_answer()]);
    let provider = ResponsesProvider::new(transport.clone()).unwrap();
    let run = run(Arc::new(provider), &transport).await;
    let input = run.second_request["input"].as_array().unwrap();
    let first = input
        .iter()
        .position(|item| item["type"] == "function_call_output")
        .unwrap();
    let ids: Vec<_> = input[first..]
        .iter()
        .map(|item| {
            assert_eq!(item["type"], "function_call_output");
            item["call_id"].as_str().unwrap()
        })
        .collect();
    assert_eq!(ids, ["c1", "c2", "c3"]);
    assert_eq!(data(&run, "run.tool.failed").len(), 1);
}
