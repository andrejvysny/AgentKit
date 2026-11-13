use agentkit_core::*;
use agentkit_providers::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
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
        run_id: "provider-run".into(),
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
    assert!(
        events
            .iter()
            .any(|event| matches!(event,Ok(ProviderEvent::Usage(data)) if data["totalTokens"]==5))
    );
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
    assert!(
        events
            .iter()
            .any(|event| matches!(event,Err(error) if error.code=="incomplete_stream"))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::ToolRequested(_))))
    );
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
    assert!(
        events
            .iter()
            .any(|event| matches!(event,Err(error) if error.code=="execution_budget_exhausted"))
    );
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
    // Completed surfaces as the public `run.message.completed` event shape
    // (same as the chat-completions lane) so `reasoningContent` can ride it.
    assert!(events.iter().any(|event| matches!(event,
        Ok(ProviderEvent::Event { kind, data })
            if kind == "run.message.completed" && data["content"] == "answer")));
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
async fn responses_accepts_reencrypted_reasoning_in_terminal_output() {
    // OpenAI re-encrypts reasoning per event, so the done item and the
    // terminal output carry different ciphertexts. The terminal copy wins.
    let stream = sse(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"reason1","type":"reasoning","encrypted_content":"added"}}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"id":"reason1","type":"reasoning","encrypted_content":"done"}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"message1","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.output_text.delta","output_index":1,"item_id":"message1","content_index":0,"delta":"answer"}),
        json!({"type":"response.completed","response":{"status":"completed","output":[
            {"id":"reason1","type":"reasoning","encrypted_content":"terminal"},
            message()]}}),
    ]);
    let transport = Transport::new("responses", vec![(200, stream)]);
    let provider = ResponsesProvider::new(transport).unwrap();
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
        .stream_chat(input, CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    assert!(
        saved.states.lock().unwrap()[0]
            .to_string()
            .contains("terminal")
    );
}
fn tool_progress(events: &[Result<ProviderEvent, CoreError>]) -> Vec<(String, String, u64)> {
    events
        .iter()
        .filter_map(|event| match event {
            Ok(ProviderEvent::ToolCallProgress {
                id,
                name,
                args_bytes,
            }) => Some((id.clone(), name.clone(), *args_bytes)),
            _ => None,
        })
        .collect()
}
fn progress(id: &str, bytes: &[u64]) -> Vec<(String, String, u64)> {
    bytes
        .iter()
        .map(|bytes| (id.to_owned(), "write".to_owned(), *bytes))
        .collect()
}
#[tokio::test]
async fn responses_reports_tool_call_progress_per_fragment_before_the_request() {
    let big = "a".repeat(3000);
    let stream = sse(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"fc1","type":"function_call","call_id":"c1","name":"write","arguments":"","namespace":"agentkit"}}),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc1","delta":big}),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc1","delta":"tail"}),
    ]);
    let provider =
        ResponsesProvider::new(Transport::new("responses", vec![(200, stream)])).unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert_eq!(tool_progress(&events), progress("c1", &[0, 3000, 3004]));
}
#[tokio::test]
async fn chat_reports_tool_call_progress_once_named_and_before_the_request() {
    let big = "a".repeat(3000);
    let stream = sse(&[
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"arguments":""}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"write","arguments":""}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":big}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"tail"}}]}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ]) + "data: [DONE]\n\n";
    let provider =
        ChatCompletionsProvider::new(Transport::new("chat-completions", vec![(200, stream)]))
            .unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert_eq!(tool_progress(&events), progress("c1", &[0, 3000, 3004]));
    let last_progress = events
        .iter()
        .rposition(|event| matches!(event, Ok(ProviderEvent::ToolCallProgress { .. })))
        .unwrap();
    let requested = events
        .iter()
        .position(
            |event| matches!(event, Ok(ProviderEvent::ToolRequested(call)) if call.id == "c1"),
        )
        .unwrap();
    assert!(last_progress < requested);
}
fn body_for(effort: Option<ReasoningEffort>, summary: ReasoningSummary) -> Value {
    let identity = ConnectionIdentity {
        provider_id: "p".into(),
        protocol: "responses".into(),
        connection_id: "c".into(),
        generation: 1,
    };
    let mut input = request();
    input.reasoning_effort = effort;
    input.reasoning_summary = summary;
    build_responses_request(&input, &identity, false).unwrap()
}
#[test]
fn responses_request_asks_for_reasoning_summary_by_default() {
    assert_eq!(
        ChatRequest::default().reasoning_summary,
        ReasoningSummary::Auto
    );
    assert_eq!(
        body_for(None, ReasoningSummary::Auto)["reasoning"],
        json!({"summary":"auto"})
    );
    assert_eq!(
        body_for(Some(ReasoningEffort::High), ReasoningSummary::Auto)["reasoning"],
        json!({"effort":"high","summary":"auto"})
    );
    assert_eq!(
        body_for(Some(ReasoningEffort::Low), ReasoningSummary::Detailed)["reasoning"],
        json!({"effort":"low","summary":"detailed"})
    );
    assert_eq!(
        body_for(None, ReasoningSummary::Concise)["reasoning"],
        json!({"summary":"concise"})
    );
    assert_eq!(
        body_for(Some(ReasoningEffort::Medium), ReasoningSummary::Off)["reasoning"],
        json!({"effort":"medium"})
    );
    assert!(
        body_for(None, ReasoningSummary::Off)
            .get("reasoning")
            .is_none()
    );
}
fn responses_transport(provider_id: &str, replies: Vec<(u16, String)>) -> Arc<Transport> {
    let mut transport = Transport::new("responses", replies);
    Arc::get_mut(&mut transport).unwrap().identity.provider_id = provider_id.into();
    transport
}
fn answer_stream() -> String {
    sse(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"message1","type":"message","role":"assistant","content":[]}}),
        terminal(vec![message()]),
    ])
}
const SUMMARY_REJECTION: &str = r#"{"error":{"message":"Your organization must be verified to generate reasoning summaries.","type":"invalid_request_error","param":"reasoning.summary","code":"unsupported_value"}}"#;
#[tokio::test]
async fn responses_retries_once_without_rejected_summary_and_remembers_the_model() {
    let transport = responses_transport(
        "summary-retry",
        vec![
            (400, SUMMARY_REJECTION.into()),
            (200, answer_stream()),
            (200, answer_stream()),
            (200, answer_stream()),
        ],
    );
    let provider = ResponsesProvider::new(transport.clone()).unwrap();
    let mut input = request();
    input.model = "remembered".into();
    input.reasoning_effort = Some(ReasoningEffort::High);
    let events: Vec<_> = provider
        .stream_chat(input.clone(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    let later: Vec<_> = provider
        .stream_chat(input.clone(), CancellationToken::new())
        .collect()
        .await;
    assert!(later.iter().all(Result::is_ok), "{later:?}");
    input.model = "other".into();
    let other: Vec<_> = provider
        .stream_chat(input, CancellationToken::new())
        .collect()
        .await;
    assert!(other.iter().all(Result::is_ok), "{other:?}");
    let sent = transport.sent.lock().unwrap();
    let reasoning: Vec<_> = sent.iter().map(|body| body["reasoning"].clone()).collect();
    assert_eq!(
        reasoning,
        vec![
            json!({"effort":"high","summary":"auto"}),
            json!({"effort":"high"}),
            json!({"effort":"high"}),
            json!({"effort":"high","summary":"auto"}),
        ]
    );
}
#[tokio::test]
async fn responses_summary_retry_is_once_and_only_for_summary_rejections() {
    let unrelated = responses_transport(
        "summary-unrelated",
        vec![
            (
                400,
                r#"{"error":{"message":"Invalid input","code":"invalid_value"}}"#.into(),
            ),
            (200, answer_stream()),
        ],
    );
    let events: Vec<_> = ResponsesProvider::new(unrelated.clone())
        .unwrap()
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(error) if error.code == "provider_error"))
    );
    assert_eq!(unrelated.sent.lock().unwrap().len(), 1);
    let twice = responses_transport(
        "summary-twice",
        vec![
            (400, SUMMARY_REJECTION.into()),
            (400, SUMMARY_REJECTION.into()),
            (200, answer_stream()),
        ],
    );
    let events: Vec<_> = ResponsesProvider::new(twice.clone())
        .unwrap()
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().any(Result::is_err));
    let sent = twice.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert!(sent[1].get("reasoning").is_none());
}
#[tokio::test]
async fn responses_summary_retry_tells_a_rejected_effort_from_a_rejected_reasoning_object() {
    let effort = responses_transport(
        "summary-effort",
        vec![
            (400, r#"{"error":{"message":"Unsupported value: 'reasoning.effort'","param":"reasoning.effort"}}"#.into()),
            (200, answer_stream()),
        ],
    );
    let mut input = request();
    input.reasoning_effort = Some(ReasoningEffort::Minimal);
    let events: Vec<_> = ResponsesProvider::new(effort.clone())
        .unwrap()
        .stream_chat(input, CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().any(Result::is_err));
    assert_eq!(effort.sent.lock().unwrap().len(), 1);
    let object = responses_transport(
        "summary-object",
        vec![
            (400, r#"{"error":{"message":"Unsupported parameter: 'reasoning' is not supported with this model.","param":"reasoning"}}"#.into()),
            (200, answer_stream()),
        ],
    );
    let events: Vec<_> = ResponsesProvider::new(object.clone())
        .unwrap()
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    let sent = object.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert!(sent[1].get("reasoning").is_none());
}
#[tokio::test]
async fn responses_summary_off_sends_no_summary_and_never_retries() {
    let transport = responses_transport(
        "summary-off",
        vec![(400, SUMMARY_REJECTION.into()), (200, answer_stream())],
    );
    let mut input = request();
    input.reasoning_summary = ReasoningSummary::Off;
    let events: Vec<_> = ResponsesProvider::new(transport.clone())
        .unwrap()
        .stream_chat(input, CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().any(Result::is_err));
    let sent = transport.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].get("reasoning").is_none());
}
#[tokio::test]
async fn responses_streams_reasoning_deltas() {
    let stream = sse(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"reason1","type":"reasoning"}}),
        json!({"type":"response.reasoning_text.delta","output_index":0,"item_id":"reason1","content_index":0,"delta":"thin"}),
        json!({"type":"response.reasoning_text.delta","output_index":0,"item_id":"reason1","content_index":0,"delta":"king"}),
        json!({"type":"response.reasoning_text.done","output_index":0,"item_id":"reason1","content_index":0,"text":"thinking"}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"message1","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.output_text.delta","output_index":1,"item_id":"message1","content_index":0,"delta":"answer"}),
        json!({"type":"response.completed","response":{"status":"completed","output":[
            {"id":"reason1","type":"reasoning","encrypted_content":"enc"},
            message()]}}),
    ]);
    let transport = Transport::new("responses", vec![(200, stream)]);
    let provider = ResponsesProvider::new(transport).unwrap();
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
        .stream_chat(input, CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let reasoning: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            Ok(ProviderEvent::Event { kind, data }) if kind == "run.message.reasoning-delta" => {
                Some(data["delta"].as_str().unwrap_or_default().to_owned())
            }
            _ => None,
        })
        .collect();
    // Incremental deltas stream verbatim; the snapshot .done frame adds no
    // duplicate suffix (TypeScript ResponsesReasoning semantics).
    assert_eq!(reasoning, vec!["thin".to_owned(), "king".to_owned()]);
    assert_eq!(saved.states.lock().unwrap().len(), 1);
    assert!(saved.states.lock().unwrap()[0].to_string().contains("enc"));
}
#[tokio::test]
async fn responses_preserves_terminal_only_reasoning_in_completed_event() {
    // A reasoning item that NEVER streamed (no delta frames) but rides the
    // final response object must survive into the public completed event —
    // the frozen `ResponsesReasoning.finish` keeps it, the native lane used
    // to drop it.
    let stream = sse(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"reason1","type":"reasoning"}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"message1","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.output_text.delta","output_index":1,"item_id":"message1","content_index":0,"delta":"answer"}),
        json!({"type":"response.completed","response":{"status":"completed","output":[
            {"id":"reason1","type":"reasoning","encrypted_content":"enc",
             "content":[{"type":"reasoning_text","text":"secret plan"}]},
            message()]}}),
    ]);
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
    let provider =
        ResponsesProvider::new(Transport::new("responses", vec![(200, stream)])).unwrap();
    let events: Vec<_> = provider
        .stream_chat(input, CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    assert!(
        events.iter().any(|event| matches!(event,
            Ok(ProviderEvent::Event { kind, data })
                if kind == "run.message.completed"
                    && data["reasoningContent"] == "secret plan")),
        "terminal-only reasoning must ride the completed event: {events:?}"
    );
}
#[tokio::test]
async fn responses_fails_stream_output_mismatch_when_final_reasoning_conflicts() {
    // The stream said "thin"; the final object says "different" for the
    // same part. The frozen client raises `stream_output_mismatch` — the
    // run must FAIL, not complete on the streamed text.
    let stream = sse(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"reason1","type":"reasoning"}}),
        json!({"type":"response.reasoning_text.delta","output_index":0,"item_id":"reason1","content_index":0,"delta":"thin"}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"message1","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.output_text.delta","output_index":1,"item_id":"message1","content_index":0,"delta":"answer"}),
        json!({"type":"response.completed","response":{"status":"completed","output":[
            {"id":"reason1","type":"reasoning","encrypted_content":"enc",
             "content":[{"type":"reasoning_text","text":"different"}]},
            message()]}}),
    ]);
    let provider =
        ResponsesProvider::new(Transport::new("responses", vec![(200, stream)])).unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(error) if error.code == "stream_output_mismatch")),
        "conflicting final reasoning must fail stream_output_mismatch: {events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(event,
            Ok(ProviderEvent::Event { kind, .. }) if kind == "run.message.completed")),
        "no completion may follow a reasoning conflict: {events:?}"
    );
}
#[tokio::test]
async fn responses_reconciles_streamed_reasoning_with_extended_final_text() {
    // The final part EXTENDS the streamed prefix — legal, and the public
    // reasoningContent is the reconciled final text, not the streamed one.
    let stream = sse(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"reason1","type":"reasoning"}}),
        json!({"type":"response.reasoning_text.delta","output_index":0,"item_id":"reason1","content_index":0,"delta":"thin"}),
        json!({"type":"response.reasoning_text.delta","output_index":0,"item_id":"reason1","content_index":0,"delta":"king"}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"message1","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.output_text.delta","output_index":1,"item_id":"message1","content_index":0,"delta":"answer"}),
        json!({"type":"response.completed","response":{"status":"completed","output":[
            {"id":"reason1","type":"reasoning","encrypted_content":"enc",
             "content":[{"type":"reasoning_text","text":"thinking more"}]},
            message()]}}),
    ]);
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
    let provider =
        ResponsesProvider::new(Transport::new("responses", vec![(200, stream)])).unwrap();
    let events: Vec<_> = provider
        .stream_chat(input, CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    assert!(
        events.iter().any(|event| matches!(event,
            Ok(ProviderEvent::Event { kind, data })
                if kind == "run.message.completed"
                    && data["reasoningContent"] == "thinking more")),
        "reconciled final reasoning must replace the streamed prefix: {events:?}"
    );
}
#[tokio::test]
async fn responses_joins_reasoning_parts_in_first_appearance_order() {
    // The frozen reference joins with Map iteration order (first insertion,
    // updates keep their slot): r2 streamed first keeps the front slot even
    // though r1 sorts before it.
    let stream = sse(&[
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"r1","type":"reasoning"}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"r2","type":"reasoning"}}),
        json!({"type":"response.reasoning_text.delta","output_index":1,"item_id":"r2","content_index":0,"delta":"b"}),
        json!({"type":"response.reasoning_text.delta","output_index":0,"item_id":"r1","content_index":0,"delta":"a"}),
        json!({"type":"response.output_item.added","output_index":2,"item":{"id":"message1","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.output_text.delta","output_index":2,"item_id":"message1","content_index":0,"delta":"answer"}),
        json!({"type":"response.completed","response":{"status":"completed","output":[
            {"id":"r1","type":"reasoning","encrypted_content":"e1","content":[{"type":"reasoning_text","text":"a"}]},
            {"id":"r2","type":"reasoning","encrypted_content":"e2","content":[{"type":"reasoning_text","text":"b"}]},
            message()]}}),
    ]);
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
    let provider =
        ResponsesProvider::new(Transport::new("responses", vec![(200, stream)])).unwrap();
    let events: Vec<_> = provider
        .stream_chat(input, CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    assert!(
        events.iter().any(|event| matches!(event,
            Ok(ProviderEvent::Event { kind, data })
                if kind == "run.message.completed"
                    && data["reasoningContent"] == "ba")),
        "parts join in first-appearance order: {events:?}"
    );
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
        assert!(
            events
                .iter()
                .any(|event| matches!(event,Err(error) if error.code==code))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(ProviderEvent::Completed { .. })))
        );
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
    assert!(
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "reasoning" && item["encrypted_content"] == "opaque")
    );
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
    assert!(
        events
            .iter()
            .any(|event| matches!(event,Err(error) if error.code=="store_error"))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::Completed { .. })))
    );
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
    assert!(
        events
            .iter()
            .any(|event| matches!(event,Err(error) if error.code=="stream_limit_exceeded"))
    );
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
                if let Ok(ProviderEvent::Event { kind, data }) = event {
                    (kind == "run.message.completed").then(|| data["toolCalls"].as_array().unwrap())
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
#[tokio::test]
async fn chat_stream_emits_incremental_reasoning_deltas() {
    let stream = sse(&[
        json!({"choices":[{"delta":{"reasoning_content":"th"}}]}),
        json!({"choices":[{"delta":{"reasoning":"inking","content":"for"}}]}),
        json!({"choices":[{"delta":{"content":"t"},"finish_reason":"stop"}]}),
        json!({"choices":[{"delta":{"reasoning_content":""}}]}),
    ]) + "data: [DONE]\n\n";
    let provider =
        ChatCompletionsProvider::new(Transport::new("chat-completions", vec![(200, stream)]))
            .unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let sequence: Vec<(String, String)> = events
        .iter()
        .filter_map(|event| match event {
            Ok(ProviderEvent::Delta(delta)) => Some(("run.message.delta".into(), delta.clone())),
            Ok(ProviderEvent::Event { kind, data }) if kind == "run.message.reasoning-delta" => {
                Some((
                    kind.clone(),
                    data["delta"].as_str().unwrap_or_default().into(),
                ))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        sequence,
        vec![
            ("run.message.reasoning-delta".to_string(), "th".to_string()),
            ("run.message.delta".to_string(), "for".to_string()),
            (
                "run.message.reasoning-delta".to_string(),
                "inking".to_string()
            ),
            ("run.message.delta".to_string(), "t".to_string()),
        ]
    );
    assert!(events.iter().any(|event|matches!(event,Ok(ProviderEvent::Event {data,..}) if data["content"]=="fort"&&data["reasoningContent"]=="thinking")));
}
#[tokio::test]
async fn chat_stream_surfaces_url_citation_annotations_on_delta_and_completed() {
    // Mirrors the OpenAI-compatible citation lane the mock provider emits:
    // an annotation-carrying chunk with `content: ""` + `annotations[]`,
    // plain content chunks, and a `citations`-shaped chunk (with a duplicate
    // url and a bare-string citation) — ONEMIND-352.
    let stream = sse(&[
        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"","annotations":[
            {"index":0,"type":"url_citation","url_citation":{
                "title":"Example Capital Report",
                "url":"https://example.com/capital-report",
                "start_index":47,"end_index":68}}
        ]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{"content":"Paris."}}]}),
        json!({"choices":[{"index":0,"delta":{"citations":[
            {"url":"https://demo.example/census","title":"Demo Census"},
            "https://plain.example/only",
            {"url":"https://example.com/capital-report"}
        ]}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ]) + "data: [DONE]\n\n";
    let provider =
        ChatCompletionsProvider::new(Transport::new("chat-completions", vec![(200, stream)]))
            .unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    let citation_frames: Vec<&Value> = events
        .iter()
        .filter_map(|event| match event {
            Ok(ProviderEvent::Event { kind, data })
                if kind == "run.message.delta" && data.get("citations").is_some() =>
            {
                Some(data)
            }
            _ => None,
        })
        .collect();
    assert_eq!(citation_frames.len(), 2, "{citation_frames:?}");
    assert_eq!(
        citation_frames[0],
        &json!({"delta":"","citations":[
            {"url":"https://example.com/capital-report","title":"Example Capital Report"}
        ]})
    );
    assert_eq!(
        citation_frames[1],
        &json!({"delta":"","citations":[
            {"url":"https://demo.example/census","title":"Demo Census"},
            {"url":"https://plain.example/only"}
        ]})
    );
    // Text deltas stay plain `Delta` events — the citation frames never
    // replace or swallow them.
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::Delta(text)) if text == "Paris."))
    );
    // The completed event carries every citation the call attested.
    assert!(events.iter().any(|event| matches!(
        event,
        Ok(ProviderEvent::Event { kind, data })
            if kind == "run.message.completed"
                && data["citations"] == json!([
                    {"url":"https://example.com/capital-report","title":"Example Capital Report"},
                    {"url":"https://demo.example/census","title":"Demo Census"},
                    {"url":"https://plain.example/only"}
                ])
    )),
        "{events:?}");
}
#[tokio::test]
async fn chat_stream_without_annotations_never_adds_a_citations_field() {
    // Byte-compatibility of the ordinary lane: no annotations in, no
    // `citations` key out (additive-only contract).
    let stream = sse(&[json!({"choices":[{"delta":{"content":"plain"},"finish_reason":"stop"}]})])
        + "data: [DONE]\n\n";
    let provider =
        ChatCompletionsProvider::new(Transport::new("chat-completions", vec![(200, stream)]))
            .unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    assert!(!events.iter().any(|event| matches!(
        event,
        Ok(ProviderEvent::Event { kind, data })
            if kind == "run.message.delta"
    )));
    assert!(events.iter().all(|event| !matches!(
        event,
        Ok(ProviderEvent::Event { data, .. }) if data.get("citations").is_some()
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Ok(ProviderEvent::Event { kind, data })
            if kind == "run.message.completed" && data["content"] == "plain"
    )));
}
#[tokio::test]
async fn chat_usage_maps_detail_counts_and_omits_absent_or_non_numeric_ones() {
    let reported = sse(&[
        json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14,"prompt_tokens_details":{"cached_tokens":6,"audio_tokens":1},"completion_tokens_details":{"reasoning_tokens":3}}}),
    ]) + "data: [DONE]\n\n";
    let provider =
        ChatCompletionsProvider::new(Transport::new("chat-completions", vec![(200, reported)]))
            .unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    assert!(events.iter().any(|event|matches!(event,Ok(ProviderEvent::Usage(data)) if data["promptTokens"]==10&&data["cachedPromptTokens"]==6&&data["reasoningTokens"]==3)));
    let untracked = sse(&[
        json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2,"prompt_tokens_details":null,"completion_tokens_details":{"cached_tokens":null,"reasoning_tokens":"3"}}}),
    ]) + "data: [DONE]\n\n";
    let provider =
        ChatCompletionsProvider::new(Transport::new("chat-completions", vec![(200, untracked)]))
            .unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    // The event contract is a number or absent, never null: servers send `null`
    // (and sloppy providers strings) for details they do not track.
    assert!(events.iter().any(|event|matches!(event,Ok(ProviderEvent::Usage(data)) if data["promptTokens"]==1&&data.get("cachedPromptTokens").is_none()&&data.get("reasoningTokens").is_none())));
}
struct FailingTransport;
#[async_trait]
impl TrustedTransport for FailingTransport {
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
        Err(CoreError::new(
            "transport_error",
            "ECONNREFUSED 127.0.0.1:1",
        ))
    }
}
#[tokio::test]
async fn chat_transport_failure_before_any_response_is_network_error() {
    let provider = ChatCompletionsProvider::new(Arc::new(FailingTransport)).unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    // The request never produced a response: distinct from a provider that
    // answered with an error status, like the TypeScript client codes it.
    assert!(events.iter().any(|event|matches!(event,Err(error) if error.code=="network_error"&&error.message.contains("ECONNREFUSED"))));
    assert!(!events
        .iter()
        .any(|event| matches!(event, Ok(ProviderEvent::Event { kind, .. }) if kind == "run.message.completed")));
}
#[tokio::test]
async fn chat_response_without_body_is_empty_body() {
    let provider = ChatCompletionsProvider::new(Transport::new(
        "chat-completions",
        vec![(200, String::new())],
    ))
    .unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().any(|event|matches!(event,Err(error) if error.code=="empty_body"&&error.message=="Provider returned empty body")));
    assert!(!events
        .iter()
        .any(|event| matches!(event, Ok(ProviderEvent::Event { kind, .. }) if kind == "run.message.completed")));
}
#[tokio::test]
async fn responses_service_unavailable_is_the_only_retryable_http_error() {
    for (status, error_code, code, retryable) in [
        // Unknown body code falls back to the status map (TS responses.ts).
        (503_u16, "server_error", "service_unavailable", true),
        (429, "server_error", "usage_limit", false),
        // Provider-declared codes classify first (TS classifyResponseError):
        // both subscription-sharing unavailabilities are service_unavailable,
        // while the limit-exceeded sibling stays usage_limit.
        (
            500,
            "subscription_sharing_user_unavailable",
            "service_unavailable",
            true,
        ),
        (
            500,
            "subscription_sharing_usage_unavailable",
            "service_unavailable",
            true,
        ),
        (
            429,
            "subscription_sharing_usage_limit_exceeded",
            "usage_limit",
            false,
        ),
    ] {
        let transport = Transport::new(
            "responses",
            vec![(
                status,
                json!({"error": {"code": error_code, "message": "try later"}}).to_string(),
            )],
        );
        let provider = ResponsesProvider::new(transport).unwrap();
        let events: Vec<_> = provider
            .stream_chat(request(), CancellationToken::new())
            .collect()
            .await;
        assert!(events.iter().any(
            |event| matches!(event,Err(error) if error.code==code&&error.retryable==retryable)
        ));
    }
}
#[tokio::test]
async fn responses_stream_level_failure_shares_the_retryable_taxonomy() {
    // SSE-level failure events classify through the same table as HTTP bodies
    // (TS responses-stream.ts attaches the same diagnostic to both).
    let stream = sse(&[
        json!({"type":"response.failed","response":{"status":"failed","error":{"code":"subscription_sharing_user_unavailable","message":"busy"}}}),
    ]);
    let provider =
        ResponsesProvider::new(Transport::new("responses", vec![(200, stream)])).unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().any(
        |event| matches!(event,Err(error) if error.code=="service_unavailable"&&error.retryable)
    ));
}
#[tokio::test]
async fn responses_usage_maps_detail_counts_and_omits_absent_or_non_numeric_ones() {
    let added = json!({"type":"response.output_item.added","output_index":0,"item":{"id":"message1","type":"message","role":"assistant","content":[]}});
    let delta = json!({"type":"response.output_text.delta","output_index":0,"item_id":"message1","content_index":0,"delta":"answer"});
    let reported = json!({"type":"response.completed","response":{"status":"completed","output":[message()],"usage":{"input_tokens":8,"output_tokens":5,"total_tokens":13,"input_tokens_details":{"cached_tokens":4,"audio_tokens":1},"output_tokens_details":{"reasoning_tokens":2}}}});
    let provider = ResponsesProvider::new(Transport::new(
        "responses",
        vec![(200, sse(&[added.clone(), delta.clone(), reported]))],
    ))
    .unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    assert!(events.iter().any(|event|matches!(event,Ok(ProviderEvent::Usage(data)) if data["promptTokens"]==8&&data["completionTokens"]==5&&data["cachedPromptTokens"]==4&&data["reasoningTokens"]==2)));
    let untracked = json!({"type":"response.completed","response":{"status":"completed","output":[message()],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2,"input_tokens_details":null,"output_tokens_details":{"reasoning_tokens":"2"}}}});
    let provider = ResponsesProvider::new(Transport::new(
        "responses",
        vec![(200, sse(&[added, delta, untracked]))],
    ))
    .unwrap();
    let events: Vec<_> = provider
        .stream_chat(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    // Like the chat lane: the event contract is a number or absent, never null.
    assert!(events.iter().any(|event|matches!(event,Ok(ProviderEvent::Usage(data)) if data["promptTokens"]==1&&data.get("cachedPromptTokens").is_none()&&data.get("reasoningTokens").is_none())));
}
