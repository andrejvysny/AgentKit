use agentkit_core::{providers::*, *};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

struct Transport {
    identity: ConnectionIdentity,
    response: String,
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
        _: Option<Value>,
        _: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        Ok(http::Response::builder()
            .status(200)
            .body(self.response.clone())
            .unwrap()
            .into())
    }
}
fn identity(protocol: &str) -> ConnectionIdentity {
    ConnectionIdentity {
        provider_id: "provider".into(),
        connection_id: "account".into(),
        protocol: protocol.into(),
        generation: 1,
    }
}
fn tool(schema: Value) -> ToolDefinition {
    ToolDefinition {
        name: "echo".into(),
        version: "1".into(),
        effect: "read".into(),
        capability: "test".into(),
        description: "Echo".into(),
        input_schema: schema,
        output_schema: None,
        timeout_ms: None,
    }
}
#[test]
fn responses_schema_overflow_matches_ajv_acceptance_then_javascript_parameter_clone() {
    let schema = agentkit_contracts::parse_javascript_json(r#"{"type":"object","properties":{"value":{"type":"number","minimum":1e400,"maximum":9007199254740993}}}"#).unwrap();
    let request = ChatRequest {
        model: "model".into(),
        tools: vec![tool(schema)],
        ..Default::default()
    };
    let body = build_responses_request(&request, &identity("responses"), false).unwrap();
    let parameters = &body["tools"][0]["tools"][0]["parameters"];
    assert!(parameters["properties"]["value"]["minimum"].is_null());
    assert_eq!(
        parameters["properties"]["value"]["maximum"],
        json!(9007199254740992u64)
    );
}
#[tokio::test]
async fn chat_indices_accept_javascript_numbers_and_keep_separate_accumulators() {
    for index in ["-1.5", "1e400", "-0"] {
        let first = r#"{"choices":[{"delta":{"tool_calls":[{"index":INDEX,"function":{"name":"echo","arguments":"{"}}]}}]}"#.replace("INDEX", index);
        let other = r#"{"choices":[{"delta":{"tool_calls":[{"index":2,"function":{"name":"other","arguments":"{}"}}]}}]}"#;
        let end = r#"{"choices":[{"delta":{"tool_calls":[{"index":INDEX,"function":{"arguments":"}"}}]},"finish_reason":"tool_calls"}]}"#.replace("INDEX", if index == "-0" { "0" } else { index });
        let response = format!("data: {first}\n\ndata: {other}\n\ndata: {end}\n\n");
        let provider = ChatCompletionsProvider::new(Arc::new(Transport {
            identity: identity("chat-completions"),
            response,
        }))
        .unwrap();
        let events: Vec<_> = provider
            .stream_chat(ChatRequest::default(), CancellationToken::new())
            .collect()
            .await;
        let calls: Vec<_> = events
            .into_iter()
            .filter_map(|event| match event.unwrap() {
                ProviderEvent::ToolRequested(call) => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "echo");
        assert_eq!(calls[0].arguments_json, "{}");
        assert_eq!(calls[1].name, "other");
        assert_eq!(calls[1].arguments_json, "{}");
    }
}
struct Saved(Mutex<Option<Value>>);
#[async_trait]
impl ContinuationStore for Saved {
    async fn save(&self, state: Value) -> Result<(), CoreError> {
        *self.0.lock().unwrap() = Some(state);
        Ok(())
    }
}
fn private_stream() -> String {
    r#"data: {"type":"response.completed","response":{"status":"completed","output":[{"id":"reasoning","type":"reasoning","encrypted_content":"private","rounded":9007199254740993,"overflow":1e400,"marker":{"$serde_json::private::Number":"123"}},{"id":"message","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"answer"}]}]}}

"#.into()
}
#[tokio::test]
async fn responses_server_failure_details_stay_redacted_before_host_projection() {
    let provider = ResponsesProvider::new(Arc::new(Transport {
        identity: identity("responses"),
        response: "data: {\"type\":\"error\",\"code\":\"invalid_api_key\",\"message\":\"secret-server-detail\"}\n\n".into(),
    })).unwrap();
    let events: Vec<_> = provider
        .stream_chat(ChatRequest::default(), CancellationToken::new())
        .collect()
        .await;
    let failure = events.into_iter().find_map(Result::err).unwrap();
    assert_eq!(failure.code, "authentication_required");
    assert_eq!(
        failure.message,
        "Responses provider failed: authentication_required"
    );
    assert!(!failure.message.contains("secret-server-detail"));
}
#[tokio::test]
async fn responses_parse_rounding_and_private_marker_objects_survive_callback_and_replay() {
    let saved = Arc::new(Saved(Mutex::new(None)));
    let mut request = ChatRequest {
        model: "model".into(),
        messages: vec![ChatMessage::text("user", "question")],
        continuation_scope: Some(ContinuationScope {
            chat_id: "chat".into(),
            branch_id: "branch".into(),
        }),
        continuation_store: Some(saved.clone()),
        ..Default::default()
    };
    let provider = ResponsesProvider::new(Arc::new(Transport {
        identity: identity("responses"),
        response: private_stream(),
    }))
    .unwrap();
    let events: Vec<_> = provider
        .stream_chat(request.clone(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let state = saved.0.lock().unwrap().clone().unwrap();
    let reasoning = &state["inputItems"][1];
    assert_eq!(reasoning["rounded"], json!(9007199254740992u64));
    assert_eq!(
        agentkit_contracts::javascript_number(&reasoning["overflow"]),
        Some(f64::INFINITY)
    );
    assert!(reasoning["marker"].is_object());
    assert_eq!(reasoning["marker"]["$serde_json::private::Number"], "123");
    request
        .messages
        .push(ChatMessage::text("assistant", "answer"));
    request.continuation = Some(state);
    request.continuation_required = true;
    let replay = build_responses_request(&request, &identity("responses"), false).unwrap();
    assert!(replay["input"][1]["marker"].is_object());
    assert_eq!(
        replay["input"][1]["marker"]["$serde_json::private::Number"],
        "123"
    );
}
