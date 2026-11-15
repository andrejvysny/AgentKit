//! CE-01 for selected content: nothing selected for a turn is sent ahead of
//! the system segments. A persisted `turn_context` part rides inside its user
//! message, and the legacy `selected_content` carrier is folded into the last
//! user message. Golden wire fixtures per transport (ADR 0036).

use agentkit_core::*;
use agentkit_providers::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::Mutex;

const TURN_CONTEXT: &str = "<turn_context>\n<date>2026-11-14</date>\n<skill name=\"release-notes\" revision=\"r1\">\nSKILL BODY\n</skill>\n</turn_context>";

struct Transport {
    identity: ConnectionIdentity,
    replies: Mutex<Vec<String>>,
    sent: Mutex<Vec<Value>>,
}
impl Transport {
    fn new(protocol: &str, replies: Vec<String>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            identity: ConnectionIdentity {
                provider_id: "test".into(),
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

fn golden(name: &str) -> Value {
    let text = match name {
        "chat_completions" => include_str!("fixtures/turn_context/chat_completions.json"),
        "responses" => include_str!("fixtures/turn_context/responses.json"),
        other => panic!("no fixture {other}"),
    };
    serde_json::from_str(text).unwrap()
}
fn system() -> Vec<ChatMessage> {
    PromptLayout::of(Some("policy"), Some("rules"), None).to_messages()
}
/// Two turns, the first carrying its persisted turn context.
fn persisted() -> ChatRequest {
    let mut first = ChatMessage::text("user", "");
    first.content = json!([
        {"type": "text", "text": "q1"},
        {"type": "turn_context", "text": TURN_CONTEXT}
    ]);
    let mut messages = system();
    messages.extend([
        first,
        ChatMessage::text("assistant", "a1"),
        ChatMessage::text("user", "q2"),
    ]);
    ChatRequest {
        run_id: "run".into(),
        model: "m".into(),
        messages,
        ..Default::default()
    }
}
/// A caller that still hands the selection over as `selected_content`.
fn legacy() -> ChatRequest {
    let mut messages = system();
    messages.push(ChatMessage::text("user", "hi"));
    ChatRequest {
        run_id: "run".into(),
        model: "m".into(),
        messages,
        selected_content: vec![ChatMessage::text("system", "SKILL BODY")],
        ..Default::default()
    }
}
async fn chat_completions_body(request: ChatRequest) -> Value {
    let reply = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})
    );
    let transport = Transport::new("chat-completions", vec![reply]);
    let provider = ChatCompletionsProvider::new(transport.clone()).unwrap();
    let events: Vec<_> = provider
        .stream_chat(request, CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let sent = transport.sent.lock().unwrap();
    sent[0].clone()
}
fn responses_body(request: &ChatRequest) -> Value {
    let identity = Transport::new("responses", vec![]).identity.clone();
    build_responses_request(request, &identity, false).unwrap()
}
/// Where `needle` first appears in the serialized `items`.
fn first_mention(items: &[Value], needle: &str) -> Option<usize> {
    items
        .iter()
        .position(|item| item.to_string().contains(needle))
}

#[tokio::test]
async fn chat_completions_matches_the_golden_wire_with_the_context_inside_the_user_message() {
    let golden = golden("chat_completions");
    let body = chat_completions_body(persisted()).await;
    assert_eq!(body["messages"], golden["persisted"]["messages"]);
    let body = chat_completions_body(legacy()).await;
    assert_eq!(body["messages"], golden["legacy"]["messages"]);
}

#[tokio::test]
async fn chat_completions_sends_no_selected_content_before_the_system_messages() {
    for request in [persisted(), legacy()] {
        let body = chat_completions_body(request).await;
        let messages = body["messages"].as_array().unwrap();
        let skill = first_mention(messages, "SKILL BODY").expect("the skill is sent");
        let last_system = messages
            .iter()
            .rposition(|message| message["role"] == "system")
            .unwrap();
        assert_eq!(messages[0]["content"], "policy");
        assert!(
            skill > last_system,
            "selected content precedes system: {messages:?}"
        );
        assert_eq!(messages[skill]["role"], "user");
    }
}

#[test]
fn responses_matches_the_golden_wire_with_the_context_inside_the_user_item() {
    let golden = golden("responses");
    let body = responses_body(&persisted());
    assert_eq!(body["instructions"], golden["persisted"]["instructions"]);
    assert_eq!(body["input"], golden["persisted"]["input"]);
    let body = responses_body(&legacy());
    assert_eq!(body["instructions"], golden["legacy"]["instructions"]);
    assert_eq!(body["input"], golden["legacy"]["input"]);
}

#[test]
fn responses_keeps_selected_content_out_of_instructions_and_ahead_of_nothing() {
    for request in [persisted(), legacy()] {
        let body = responses_body(&request);
        assert!(!body["instructions"].to_string().contains("SKILL BODY"));
        let input = body["input"].as_array().unwrap();
        let skill = first_mention(input, "SKILL BODY").expect("the skill is sent");
        let last_system = input
            .iter()
            .rposition(|item| item["role"] == "system")
            .unwrap();
        assert!(
            skill > last_system,
            "selected content precedes system: {input:?}"
        );
        assert_eq!(input[skill]["role"], "user");
    }
}
