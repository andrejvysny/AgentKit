//! Hardening H1.5b: OpenAI's newer reasoning models refuse function tools on
//! /v1/chat/completions unless `reasoning_effort` is "none", even when the
//! request omits the field (live probe, 17 Nov 2026). The provider retries
//! such a refusal once with "none", remembers the model for its lifetime, and
//! says so with a `reasoning_effort_dropped` warning.
use agentkit_core::*;
use agentkit_providers::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const REFUSAL: &str = r#"{"error":{"message":"Function tools with reasoning_effort are not supported for m in /v1/chat/completions. To use function tools, use /v1/responses or set reasoning_effort to 'none'.","type":"invalid_request_error","param":"reasoning_effort","code":null}}"#;

struct Transport {
    identity: ConnectionIdentity,
    replies: Mutex<Vec<(u16, String)>>,
    sent: Mutex<Vec<Value>>,
}

impl Transport {
    fn new(provider_id: &str, replies: Vec<(u16, String)>) -> Arc<Self> {
        Arc::new(Self {
            identity: ConnectionIdentity {
                provider_id: provider_id.into(),
                protocol: "chat-completions".into(),
                connection_id: "c".into(),
                generation: 1,
            },
            replies: Mutex::new(replies),
            sent: Mutex::new(vec![]),
        })
    }
    fn sent(&self) -> Vec<Value> {
        self.sent.lock().unwrap().clone()
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
        let (status, body) = self.replies.lock().unwrap().remove(0);
        Ok(http::Response::builder()
            .status(status)
            .header("content-type", "text/event-stream")
            .body(body)
            .unwrap()
            .into())
    }
}

fn answer() -> String {
    [
        json!({"choices":[{"delta":{"content":"done"}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
    ]
    .iter()
    .map(|value| format!("data: {value}\n\n"))
    .chain(["data: [DONE]\n\n".to_owned()])
    .collect()
}

fn tool() -> ToolDefinition {
    serde_json::from_value(json!({
        "name": "read_file", "version": "1", "effect": "read", "capability": "test",
        "description": "Read a file.",
        "inputSchema": {"type":"object","properties":{"file_path":{"type":"string"}}}
    }))
    .unwrap()
}

fn request(tools: bool, effort: Option<ReasoningEffort>) -> ChatRequest {
    ChatRequest {
        run_id: "run".into(),
        model: "m".into(),
        messages: vec![ChatMessage::text("user", "hi")],
        tools: if tools { vec![tool()] } else { vec![] },
        reasoning_effort: effort,
        ..Default::default()
    }
}

async fn run(
    transport: &Arc<Transport>,
    input: ChatRequest,
) -> Vec<Result<ProviderEvent, CoreError>> {
    let provider = ChatCompletionsProvider::new(transport.clone()).unwrap();
    provider
        .stream_chat(input, CancellationToken::new())
        .collect()
        .await
}

fn completed(events: &[Result<ProviderEvent, CoreError>]) -> bool {
    events.iter().any(|event| {
        matches!(event, Ok(ProviderEvent::Event { kind, .. }) if kind == "run.message.completed")
    })
}

fn warned(events: &[Result<ProviderEvent, CoreError>]) -> bool {
    events.iter().any(|event| {
        matches!(event, Ok(ProviderEvent::Warning { code, .. }) if code == "reasoning_effort_dropped")
    })
}

#[tokio::test]
async fn a_tool_request_refused_for_its_effort_is_retried_once_with_none() {
    let transport = Transport::new("effort-retry", vec![(400, REFUSAL.into()), (200, answer())]);
    let events = run(&transport, request(true, Some(ReasoningEffort::High))).await;
    assert!(completed(&events), "{events:?}");
    assert!(warned(&events), "{events:?}");
    let sent = transport.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["reasoning_effort"], "high");
    assert_eq!(sent[1]["reasoning_effort"], "none");
    assert!(
        sent[1]["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
}

/// The live refusal also came for requests that sent no effort at all: the
/// model's default effort counts.
#[tokio::test]
async fn a_tool_request_without_an_effort_is_retried_with_none() {
    let transport = Transport::new(
        "effort-absent",
        vec![(400, REFUSAL.into()), (200, answer())],
    );
    let events = run(&transport, request(true, None)).await;
    assert!(completed(&events), "{events:?}");
    let sent = transport.sent();
    assert_eq!(sent.len(), 2);
    assert!(sent[0].get("reasoning_effort").is_none());
    assert_eq!(sent[1]["reasoning_effort"], "none");
}

#[tokio::test]
async fn a_refused_model_sends_none_from_the_start_afterwards() {
    let transport = Transport::new(
        "effort-remembered",
        vec![(400, REFUSAL.into()), (200, answer()), (200, answer())],
    );
    // The memory belongs to the provider instance, as a host keeps it.
    let provider = ChatCompletionsProvider::new(transport.clone()).unwrap();
    for _ in 0..2 {
        let events: Vec<_> = provider
            .stream_chat(
                request(true, Some(ReasoningEffort::Low)),
                CancellationToken::new(),
            )
            .collect()
            .await;
        assert!(completed(&events), "{events:?}");
        assert!(warned(&events), "each run says so: {events:?}");
    }
    let sent = transport.sent();
    assert_eq!(sent.len(), 3, "no second refusal");
    assert_eq!(sent[2]["reasoning_effort"], "none");

    // A new provider instance tries the configured effort again.
    let fresh = Transport::new("effort-remembered", vec![(200, answer())]);
    run(&fresh, request(true, Some(ReasoningEffort::Low))).await;
    assert_eq!(fresh.sent()[0]["reasoning_effort"], "low");
}

#[tokio::test]
async fn a_request_without_tools_keeps_its_effort() {
    let transport = Transport::new("effort-no-tools", vec![(400, REFUSAL.into())]);
    let events = run(&transport, request(false, Some(ReasoningEffort::High))).await;
    assert!(!completed(&events));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(error) if error.code == "400")),
        "{events:?}"
    );
    assert_eq!(transport.sent().len(), 1, "not retried");
}

#[tokio::test]
async fn another_400_is_not_retried() {
    let other = r#"{"error":{"message":"Invalid value for 'temperature'.","param":"temperature"}}"#;
    let transport = Transport::new("effort-other", vec![(400, other.into())]);
    let events = run(&transport, request(true, Some(ReasoningEffort::High))).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(error) if error.code == "400")),
        "{events:?}"
    );
    assert!(!warned(&events));
    assert_eq!(transport.sent().len(), 1, "not retried");
}

#[tokio::test]
async fn a_second_refusal_with_none_fails_without_looping() {
    let transport = Transport::new(
        "effort-twice",
        vec![(400, REFUSAL.into()), (400, REFUSAL.into())],
    );
    let events = run(&transport, request(true, Some(ReasoningEffort::High))).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(error) if error.code == "400")),
        "{events:?}"
    );
    assert_eq!(transport.sent().len(), 2);
}
