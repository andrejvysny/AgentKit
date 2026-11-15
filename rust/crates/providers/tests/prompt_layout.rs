//! CE-01 on the provider transports: the system segments S2-S4 reach the wire
//! as separate blocks, in segment order, ahead of history (ADR 0028).

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
struct Saved(Mutex<Vec<Value>>);
#[async_trait]
impl ContinuationStore for Saved {
    async fn save(&self, state: Value) -> Result<(), CoreError> {
        self.0.lock().unwrap().push(state);
        Ok(())
    }
}

fn layout() -> PromptLayout {
    PromptLayout::of(Some("policy"), Some("rules"), Some("memory"))
}
fn request() -> ChatRequest {
    let mut messages = layout().to_messages();
    messages.push(ChatMessage::text("user", "hi"));
    ChatRequest {
        run_id: "run".into(),
        model: "m".into(),
        messages,
        ..Default::default()
    }
}
fn sse(values: &[Value]) -> String {
    values
        .iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect()
}
fn answer() -> Value {
    json!({"id":"m1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok"}]})
}
fn completed(output: Vec<Value>) -> String {
    sse(&[json!({"type":"response.completed","response":{"status":"completed","output":output}})])
}

#[tokio::test]
async fn chat_completions_sends_each_segment_as_its_own_system_message_in_order() {
    let reply = sse(&[json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})])
        + "data: [DONE]\n\n";
    let transport = Transport::new("chat-completions", vec![reply]);
    let provider = ChatCompletionsProvider::new(transport.clone()).unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let sent = transport.sent.lock().unwrap();
    let wire: Vec<_> = sent[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| {
            (
                message["role"].as_str().unwrap(),
                message["content"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        wire,
        [
            ("system", "policy"),
            ("system", "rules"),
            ("system", "memory"),
            ("user", "hi")
        ]
    );
    assert!(sent[0]["messages"][0].get("metadata").is_none());
}

#[test]
fn responses_puts_s2_in_instructions_and_later_segments_in_leading_system_items() {
    let identity = Transport::new("responses", vec![]).identity.clone();
    let body = build_responses_request(&request(), &identity, false).unwrap();
    assert_eq!(body["instructions"], "policy");
    let input = body["input"].as_array().unwrap();
    let roles: Vec<_> = input
        .iter()
        .map(|item| item["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "system", "user"]);
    assert_eq!(input[0]["content"][0]["text"], "rules");
    assert_eq!(input[1]["content"][0]["text"], "memory");
    assert_eq!(input[2]["content"][0]["text"], "hi");
    // Wire key order is the layout order: tools, then instructions, then input.
    let keys: Vec<_> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let position = |name: &str| keys.iter().position(|key| *key == name).unwrap();
    assert!(position("instructions") < position("input"));
}

#[test]
fn responses_without_s3_and_s4_keeps_the_single_instructions_shape() {
    let identity = Transport::new("responses", vec![]).identity.clone();
    let mut input = request();
    input.messages = PromptLayout::of(Some("policy"), None, None).to_messages();
    input.messages.push(ChatMessage::text("user", "hi"));
    let body = build_responses_request(&input, &identity, false).unwrap();
    assert_eq!(body["instructions"], "policy");
    assert_eq!(body["input"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn responses_continuation_never_checkpoints_segment_items() {
    let reasoning = json!({"id":"r1","type":"reasoning","encrypted_content":"opaque"});
    let transport = Transport::new("responses", vec![completed(vec![reasoning, answer()])]);
    let provider = ResponsesProvider::new(transport.clone()).unwrap();
    let saved = Arc::new(Saved(Mutex::new(vec![])));
    let mut input = request();
    input.continuation_scope = Some(ContinuationScope {
        chat_id: "chat".into(),
        branch_id: "branch".into(),
    });
    input.continuation_store = Some(saved.clone());
    let _: Vec<_> = provider
        .stream_chat(input, CancellationToken::new())
        .collect()
        .await;
    let states = saved.0.lock().unwrap();
    let items = states[0]["inputItems"].as_array().unwrap();
    assert!(
        items.iter().all(|item| item["role"] != "system"),
        "{items:?}"
    );
    assert_eq!(items[0]["content"][0]["text"], "hi");
    assert!(items.iter().any(|item| item["type"] == "reasoning"));
}
