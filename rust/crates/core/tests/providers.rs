use agentkit_core::{providers::*, *};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
struct Transport {
    identity: ConnectionIdentity,
    replies: Mutex<Vec<(u16, String)>>,
    sent: Mutex<Vec<Value>>,
    auth: AuthKind,
    checks: AtomicUsize,
}
impl Transport {
    fn new(protocol: &str, replies: Vec<(u16, String)>) -> Arc<Self> {
        Arc::new(Self {
            identity: ConnectionIdentity {
                provider_id: "test".into(),
                protocol: protocol.into(),
                connection_id: "c".into(),
                generation: 1,
            },
            replies: Mutex::new(replies),
            sent: Mutex::new(vec![]),
            auth: AuthKind::ApiKey,
            checks: AtomicUsize::new(0),
        })
    }
}
#[async_trait]
impl TrustedTransport for Transport {
    fn identity(&self) -> ConnectionIdentity {
        self.identity.clone()
    }
    fn auth_kind(&self) -> AuthKind {
        self.auth
    }
    fn check_current(&self) -> Result<(), CoreError> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn request(
        &self,
        identity: &ConnectionIdentity,
        _: &str,
        body: Option<Value>,
        _: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        assert_eq!(identity, &self.identity);
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
fn sse(values: &[Value]) -> String {
    values
        .iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect()
}
fn request() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![ChatMessage::text("user", "hi")],
        ..Default::default()
    }
}
#[tokio::test]
async fn chat_stream_keeps_reasoning_separate_and_records_trailing_usage() {
    let stream = sse(&[
        json!({"choices":[{"delta":{"reasoning_content":"private thought","content":"yes"}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
    ]) + "data: [DONE]\n\n";
    let transport = Transport::new("chat-completions", vec![(200, stream)]);
    let provider = ChatCompletionsProvider::new(transport.clone()).unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    assert!(events.iter().any(|event|matches!(event,Ok(ProviderEvent::Event {data,..}) if data["content"]=="yes"&&data["reasoningContent"]=="private thought")));
    assert!(events
        .iter()
        .any(|event| matches!(event,Ok(ProviderEvent::Usage(data)) if data["totalTokens"]==5)));
    assert!(transport.checks.load(Ordering::SeqCst) >= 4);
}
#[tokio::test]
async fn chat_malformed_tool_frames_never_dispatch_tool_calls() {
    let stream = sse(&[
        json!({"choices":[{"delta":{"content":"partial","tool_calls":[{"index":0,"id":"c","function":{"name":"write","arguments":"{}"}}]}}]}),
    ]) + "data: {invalid\n\n"
        + &sse(&[json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})])
        + "data: [DONE]\n\n";
    let provider =
        ChatCompletionsProvider::new(Transport::new("chat-completions", vec![(200, stream)]))
            .unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events
        .iter()
        .any(|event| matches!(event,Err(error) if error.code=="incomplete_stream")));
    assert!(!events
        .iter()
        .any(|event| matches!(event, Ok(ProviderEvent::ToolRequested(_)))));
}
#[tokio::test]
async fn compatibility_fallback_reserves_each_request_and_never_exceeds_budget() {
    let transport = Transport::new(
        "chat-completions",
        vec![
            (400, "max_tokens unsupported".into()),
            (200, "unused".into()),
        ],
    );
    let provider = ChatCompletionsProvider::new(transport.clone()).unwrap();
    let mut input = request();
    input.max_output_tokens = Some(10);
    input.budget = Some(Arc::new(
        ExecutionBudget::new(
            BudgetLimits {
                provider_requests: Some(1),
                ..Default::default()
            },
            Arc::new(SystemRuntime),
            None,
        )
        .unwrap(),
    ));
    let events: Vec<_> = provider
        .stream_chat(input, CancellationToken::new())
        .collect()
        .await;
    assert_eq!(transport.sent.lock().unwrap().len(), 1);
    assert!(events
        .iter()
        .any(|event| matches!(event,Err(error) if error.code=="execution_budget_exhausted")));
}
fn terminal(output: Vec<Value>) -> Value {
    json!({"type":"response.completed","response":{"status":"completed","output":output,"usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}})
}
fn message() -> Value {
    json!({"id":"message1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"answer"}]})
}
#[tokio::test]
async fn responses_reconciles_live_prefix_and_terminal_snapshot() {
    let stream = sse(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"message1","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"item_id":"message1","content_index":0,"delta":"ans"}),
        terminal(vec![message()]),
    ]);
    let transport = Transport::new("responses", vec![(200, stream)]);
    let provider = ResponsesProvider::new(transport.clone()).unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    assert!(events.iter().any(
        |event| matches!(event,Ok(ProviderEvent::Completed {content,..}) if content=="answer")
    ));
    let deltas = events
        .iter()
        .filter_map(|event| {
            if let Ok(ProviderEvent::Delta(delta)) = event {
                Some(delta.as_str())
            } else {
                None
            }
        })
        .collect::<String>();
    assert_eq!(deltas, "answer");
    let body = &transport.sent.lock().unwrap()[0];
    assert_eq!(body["store"], false);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
}
#[tokio::test]
async fn responses_rejects_mismatched_output_and_unknown_events() {
    for (extra, code) in [
        (json!({"type":"unknown"}), "unsupported_stream_event"),
        (
            json!({"type":"response.output_text.delta","output_index":0,"item_id":"wrong","content_index":0,"delta":"x"}),
            "output_item_mismatch",
        ),
    ] {
        let stream = sse(&[
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"message1","type":"message"}}),
            extra,
            terminal(vec![message()]),
        ]);
        let provider =
            ResponsesProvider::new(Transport::new("responses", vec![(200, stream)])).unwrap();
        let events: Vec<_> = provider
            .stream_chat(request(), CancellationToken::new())
            .collect()
            .await;
        assert!(events
            .iter()
            .any(|event| matches!(event,Err(error) if error.code==code)));
        assert!(!events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::Completed { .. }))));
    }
}
struct Saved {
    states: Mutex<Vec<Value>>,
    fail: bool,
}
#[async_trait]
impl ContinuationStore for Saved {
    async fn save(&self, state: Value) -> Result<(), CoreError> {
        if self.fail {
            return Err(CoreError::new("store_error", "private save failed"));
        }
        self.states.lock().unwrap().push(state);
        Ok(())
    }
}
#[tokio::test]
async fn private_reasoning_continuation_is_persisted_then_replayed_under_exact_scope() {
    let output = vec![
        json!({"id":"r1","type":"reasoning","encrypted_content":"opaque"}),
        message(),
    ];
    let transport = Transport::new("responses", vec![(200, sse(&[terminal(output)]))]);
    let provider = ResponsesProvider::new(transport.clone()).unwrap();
    let saved = Arc::new(Saved {
        states: Mutex::new(vec![]),
        fail: false,
    });
    let mut input = request();
    input.continuation_scope = Some(ContinuationScope {
        chat_id: "chat".into(),
        branch_id: "branch".into(),
    });
    input.continuation_store = Some(saved.clone());
    let events: Vec<_> = provider
        .stream_chat(input.clone(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let continuation = saved.states.lock().unwrap()[0].clone();
    input.continuation = Some(continuation);
    input
        .messages
        .push(ChatMessage::text("assistant", "answer"));
    input.messages.push(ChatMessage::text("user", "next"));
    let body = build_responses_request(&input, &transport.identity, false).unwrap();
    assert!(body["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["type"] == "reasoning" && item["encrypted_content"] == "opaque"));
    input.continuation_scope.as_mut().unwrap().branch_id = "wrong".into();
    assert_eq!(
        build_responses_request(&input, &transport.identity, false)
            .unwrap_err()
            .code,
        "continuation_scope_mismatch"
    );
}
#[tokio::test]
async fn private_persistence_failure_preserves_usage_and_exposes_no_completion() {
    let transport = Transport::new(
        "responses",
        vec![(
            200,
            sse(&[terminal(vec![
                json!({"id":"r1","type":"reasoning","encrypted_content":"opaque"}),
                message(),
            ])]),
        )],
    );
    let provider = ResponsesProvider::new(transport).unwrap();
    let mut input = request();
    input.continuation_scope = Some(ContinuationScope {
        chat_id: "chat".into(),
        branch_id: "branch".into(),
    });
    input.continuation_store = Some(Arc::new(Saved {
        states: Mutex::new(vec![]),
        fail: true,
    }));
    let events: Vec<_> = provider
        .stream_chat(input, CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().any(|event|matches!(event,Ok(ProviderEvent::Usage(data)) if data["finalForCall"]==false&&data["totalTokens"]==5)));
    assert!(events
        .iter()
        .any(|event| matches!(event,Err(error) if error.code=="store_error")));
    assert!(!events
        .iter()
        .any(|event| matches!(event, Ok(ProviderEvent::Completed { .. }))));
}

struct CappedStreamTransport;
#[async_trait]
impl TrustedTransport for CappedStreamTransport {
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
        _: Option<Value>,
        _: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        let calls=(0..1025).map(|index|json!({"index":index,"id":format!("call{index}"),"function":{"name":"write","arguments":"{}"}})).collect::<Vec<_>>();
        let frame = sse(&[json!({"choices":[{"delta":{"tool_calls":calls}}]})]);
        let stream =
            futures_util::stream::once(async move { Ok::<_, std::io::Error>(frame.into_bytes()) })
                .chain(futures_util::stream::pending());
        Ok(http::Response::builder()
            .status(200)
            .body(reqwest::Body::wrap_stream(stream))
            .unwrap()
            .into())
    }
}
#[tokio::test]
async fn chat_limit_failure_terminates_without_waiting_for_hostile_socket_eof() {
    let provider = ChatCompletionsProvider::new(Arc::new(CappedStreamTransport)).unwrap();
    let events = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        provider
            .stream_chat(request(), CancellationToken::new())
            .collect::<Vec<_>>(),
    )
    .await
    .expect("cap failure must not wait for more bytes");
    assert!(events
        .iter()
        .any(|event| matches!(event,Err(error) if error.code=="stream_limit_exceeded")));
}

#[tokio::test]
async fn chat_unindexed_and_reused_index_assembly_matches_typescript_fixtures() {
    for deltas in [
        vec![
            json!({"id":"a","function":{"name":"one"}}),
            json!({"function":{"arguments":"{\"x\":1}"}}),
            json!({"id":"b","function":{"name":"two"}}),
            json!({"function":{"arguments":"{\"y\":2}"}}),
        ],
        vec![
            json!({"index":0,"function":{"name":"one","arguments":"{\"x\":1}"}}),
            json!({"index":0,"function":{"name":"two","arguments":"{\"y\":2}"}}),
        ],
        vec![
            json!({"function":{"name":"one","arguments":"{\"x\":1}"}}),
            json!({"function":{"name":"two","arguments":"{"}}),
            json!({"function":{"arguments":"\"y\":2}"}}),
        ],
    ] {
        let mut frames = deltas
            .into_iter()
            .map(|delta| json!({"choices":[{"delta":{"tool_calls":[delta]}}]}))
            .collect::<Vec<_>>();
        frames.push(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}));
        let provider = ChatCompletionsProvider::new(Transport::new(
            "chat-completions",
            vec![(200, sse(&frames) + "data: [DONE]\n\n")],
        ))
        .unwrap();
        let events = provider
            .stream_chat(request(), CancellationToken::new())
            .collect::<Vec<_>>()
            .await;
        let calls = events
            .iter()
            .find_map(|event| {
                if let Ok(ProviderEvent::Event { data, .. }) = event {
                    Some(data["toolCalls"].as_array().unwrap())
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["name"], "one");
        assert_eq!(calls[1]["name"], "two");
        assert_eq!(calls[0]["argumentsJson"], "{\"x\":1}");
        assert_eq!(calls[1]["argumentsJson"], "{\"y\":2}");
    }
}

#[tokio::test]
async fn trusted_model_catalogues_map_api_and_account_shapes_without_silent_account_fallback() {
    let compatible = ChatCompletionsProvider::new(Transport::new(
        "chat-completions",
        vec![(
            200,
            json!({"data":[{"id":"local","name":"Local"},{}]}).to_string(),
        )],
    ))
    .unwrap();
    let models = compatible
        .list_models(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["modelId"], "local");
    let mut transport=Transport::new("responses",vec![(200,json!({"models":[{"slug":"account-model","display_name":"Account","visibility":"list"},{"slug":"hidden","visibility":"hide"}]}).to_string())]);
    Arc::get_mut(&mut transport).unwrap().auth = AuthKind::ChatGptAccount;
    let provider = ResponsesProvider::new(transport).unwrap();
    let models = provider
        .list_models(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["modelId"], "account-model");
    assert_eq!(models[0]["displayName"], "Account");
    let mut transport = Transport::new(
        "responses",
        vec![(200, json!({"data":[{"id":"api-model"}]}).to_string())],
    );
    Arc::get_mut(&mut transport).unwrap().auth = AuthKind::ChatGptAccount;
    let provider = ResponsesProvider::new(transport).unwrap();
    assert_eq!(
        provider
            .list_models(CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        "malformed_model_catalog"
    );
}

#[tokio::test]
async fn multimodal_wire_preserves_absent_detail_newlines_and_silent_host_refs() {
    let transport = Transport::new(
        "chat-completions",
        vec![(
            200,
            sse(&[json!({"choices":[{"delta":{},"finish_reason":"stop"}]})]) + "data: [DONE]\n\n",
        )],
    );
    let provider = ChatCompletionsProvider::new(transport.clone()).unwrap();
    let mut input = request();
    input.messages=vec![serde_json::from_value(json!({"role":"system","content":[{"type":"text","text":"first"},{"type":"image","source":{"kind":"url","url":"https://example.test/a.png"}},{"type":"text","text":"second"}]})).unwrap(),serde_json::from_value(json!({"role":"assistant","content":[{"type":"image","source":{"kind":"data","base64":"aGVsbG8=","mediaType":"image/png"}},{"type":"image","source":{"kind":"ref","ref":"opaque"}}]})).unwrap()];
    let events = provider
        .stream_chat(input, CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let sent = transport.sent.lock().unwrap();
    assert_eq!(sent[0]["messages"][0]["content"], "first\nsecond");
    assert_eq!(
        sent[0]["messages"][1]["content"],
        json!([{"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8="}}])
    );
    let warnings = events
        .iter()
        .filter_map(|event| {
            if let Ok(ProviderEvent::Warning { message, .. }) = event {
                Some(message)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("system"));
    assert!(!warnings[0].contains("assistant"));
}
