//! CE-07: cache fields reach the wire only when the preset declares them.

use agentkit_core::*;
use agentkit_providers::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Transport {
    sent: Mutex<Vec<Value>>,
}
#[async_trait]
impl TrustedTransport for Transport {
    fn identity(&self) -> ConnectionIdentity {
        ConnectionIdentity {
            provider_id: "test".into(),
            protocol: "chat-completions".into(),
            connection_id: "c".into(),
            generation: 1,
        }
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
        let reply = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})
        );
        Ok(http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(reply)
            .unwrap()
            .into())
    }
}

fn identity() -> ConnectionIdentity {
    ConnectionIdentity {
        provider_id: "p".into(),
        protocol: "responses".into(),
        connection_id: "c".into(),
        generation: 1,
    }
}
fn layout() -> PromptLayout {
    PromptLayout::of(Some("policy"), Some("rules"), Some("memory"))
}
fn prefix() -> agentkit_contracts::AiSessionPrefix {
    let digests = PrefixDigests::compute(&layout(), &[], &json!({"model":"m"}));
    freeze(None, digests, "chat", "branch", "task")
        .prefix()
        .clone()
}
/// A request planned the way the host plans it for a preset kind.
fn request_for(kind: &str) -> ChatRequest {
    let capability = preset_by_kind(kind).unwrap().cache.clone();
    let directives = capability.plan(&layout(), &prefix());
    let mut messages = layout().to_messages();
    messages.push(ChatMessage::text("user", "hi"));
    ChatRequest {
        run_id: "run".into(),
        model: "m".into(),
        messages,
        cache: (!directives.is_empty()).then_some(directives),
        ..Default::default()
    }
}
/// The Chat Completions body the provider puts on the wire.
async fn chat_body(input: &ChatRequest) -> Value {
    let transport = Arc::new(Transport {
        sent: Mutex::new(vec![]),
    });
    let provider = ChatCompletionsProvider::new(transport.clone()).unwrap();
    let events: Vec<_> = provider
        .stream_chat(input.clone(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    transport.sent.lock().unwrap()[0].clone()
}

#[tokio::test]
async fn openai_preset_sends_the_session_cache_key_on_both_lanes() {
    let key = prefix().cache_key;
    let input = request_for("openai");
    let responses = build_responses_request(&input, &identity(), false).unwrap();
    assert_eq!(responses["prompt_cache_key"], key.as_str());
    let chat = chat_body(&input).await;
    assert_eq!(chat["prompt_cache_key"], key.as_str());
    assert!(chat.get("session_id").is_none());
}

#[tokio::test]
async fn openrouter_preset_sends_session_id_only_on_chat_completions() {
    let key = prefix().cache_key;
    let input = request_for("openrouter");
    let chat = chat_body(&input).await;
    assert_eq!(chat["session_id"], key.as_str());
    assert!(chat.get("prompt_cache_key").is_none());
    let responses = build_responses_request(&input, &identity(), false).unwrap();
    assert!(responses.get("prompt_cache_key").is_none());
}

#[tokio::test]
async fn unknown_and_local_presets_send_no_cache_field() {
    for kind in ["openai-compatible", "ollama", "lmstudio", "omlx"] {
        let input = request_for(kind);
        let responses = build_responses_request(&input, &identity(), false).unwrap();
        assert!(responses.get("prompt_cache_key").is_none(), "{kind}");
        let chat = chat_body(&input).await;
        assert!(chat.get("prompt_cache_key").is_none(), "{kind}");
        assert!(chat.get("session_id").is_none(), "{kind}");
    }
}

#[test]
fn responses_instructions_never_carry_turn_context() {
    let mut input = request_for("openai");
    input.messages.last_mut().unwrap().content = "hi\n[turn context]".into();
    let body = build_responses_request(&input, &identity(), false).unwrap();
    assert_eq!(body["instructions"], "policy\n\nrules\n\nmemory");
    assert!(
        !body["instructions"]
            .as_str()
            .unwrap()
            .contains("turn context")
    );
    // The user-role turn context travels as an input item, after the prefix.
    assert!(body["input"].to_string().contains("[turn context]"));
}
