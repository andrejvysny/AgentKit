use agentkit_core::{
    CancellationToken, ChatMessage, ChatProvider, ChatRequest, ProviderEvent, Runtime, UsageRecord,
};
use agentkit_testkit::cache_provider::{CacheSimulatingProvider, WireObserver, mock_request_bytes};
use futures::StreamExt;
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

#[derive(Default)]
struct Clock(AtomicI64);
impl Runtime for Clock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    fn id(&self) -> String {
        "test".into()
    }
}
fn request(key: &str, text: &str) -> ChatRequest {
    ChatRequest {
        run_id: key.into(),
        model: "model".into(),
        messages: vec![ChatMessage::text("user", text)],
        ..Default::default()
    }
}
async fn usage(provider: &CacheSimulatingProvider, request: ChatRequest) -> UsageRecord {
    let events: Vec<_> = provider
        .stream_chat(request, CancellationToken::new())
        .collect()
        .await;
    let data = events
        .into_iter()
        .find_map(|event| match event.unwrap() {
            ProviderEvent::Usage(data) => Some(data),
            _ => None,
        })
        .unwrap();
    serde_json::from_value(data).unwrap()
}
fn provider(clock: Arc<Clock>, minimum: u64) -> CacheSimulatingProvider {
    CacheSimulatingProvider::new(clock, 100, minimum, |request| request.run_id.clone())
}
#[tokio::test]
async fn hits_and_key_isolation() {
    let clock = Arc::new(Clock::default());
    let provider = provider(clock, 1);
    let first = usage(&provider, request("a", &"x".repeat(100))).await;
    let hit = usage(&provider, request("a", &"x".repeat(100))).await;
    let isolated = usage(&provider, request("b", &"x".repeat(100))).await;
    assert_eq!(first.cache_read, Some(0));
    assert_eq!(hit.cache_read, first.input_uncached);
    assert_eq!(isolated.cache_read, Some(0));
}
#[tokio::test]
async fn changed_prefix_and_expiry() {
    let clock = Arc::new(Clock::default());
    let original = request("a", &"x".repeat(300));
    let total = (mock_request_bytes(&original).len() / 4) as u64;
    let provider = provider(clock.clone(), total);
    usage(&provider, original.clone()).await;
    assert_eq!(
        usage(&provider, request("a", &"y".repeat(300)))
            .await
            .cache_read,
        Some(0)
    );
    usage(&provider, original.clone()).await;
    clock.0.store(99, Ordering::SeqCst);
    assert_eq!(
        usage(&provider, original.clone()).await.cache_read,
        Some(total)
    );
    clock.0.store(199, Ordering::SeqCst);
    assert_eq!(usage(&provider, original).await.cache_read, Some(0));
}
#[tokio::test]
async fn minimum_is_inclusive_and_prefix_count_is_floored() {
    let clock = Arc::new(Clock::default());
    let input = request("a", &"x".repeat(40));
    let total = (mock_request_bytes(&input).len() / 4) as u64;
    let exact = provider(clock.clone(), total);
    usage(&exact, input.clone()).await;
    assert_eq!(usage(&exact, input.clone()).await.cache_read, Some(total));
    let above = provider(clock, total + 1);
    usage(&above, input.clone()).await;
    assert_eq!(usage(&above, input).await.cache_read, Some(0));
}
#[test]
fn observer_preserves_actual_bytes_and_tool_arguments() {
    let observer = WireObserver::default();
    let bytes = br#"{ "tool": {"arguments":"{\\\"x\\\":1}"} }"#;
    observer.observe(bytes);
    assert_eq!(observer.snapshot(), vec![bytes.to_vec()]);
    let mut input = request("a", "x");
    input.messages[0].tool_calls.push(agentkit_core::ToolCall {
        id: "id".into(),
        name: "tool".into(),
        arguments_json: "{\"nested\": [1, 2]}".into(),
    });
    let serialized = mock_request_bytes(&input);
    assert!(
        String::from_utf8(serialized)
            .unwrap()
            .contains(r#"{\"nested\": [1, 2]}"#)
    );
}

#[tokio::test]
async fn partial_hit_reports_exact_common_prefix() {
    let clock = Arc::new(Clock::default());
    let provider = provider(clock, 1);
    let first = request("a", &format!("{}x", "stable".repeat(30)));
    let second = request("a", &format!("{}y", "stable".repeat(30)));
    let left = mock_request_bytes(&first);
    let right = mock_request_bytes(&second);
    let common = left.iter().zip(&right).take_while(|(a, b)| a == b).count();
    usage(&provider, first).await;
    let result = usage(&provider, second).await;
    assert_eq!(result.cache_read, Some((common / 4) as u64));
    assert_eq!(
        result.input_uncached,
        Some((right.len() / 4 - common / 4) as u64)
    );
}

#[tokio::test]
async fn retains_distinct_warm_prefixes_for_each_key() {
    let clock = Arc::new(Clock::default());
    let first = request("a", &"x".repeat(100));
    let mut second = first.clone();
    second.model = "another-model".into();
    let total = (mock_request_bytes(&first).len() / 4) as u64;
    let provider = provider(clock, total);
    usage(&provider, first.clone()).await;
    assert_eq!(usage(&provider, second).await.cache_read, Some(0));
    assert_eq!(usage(&provider, first).await.cache_read, Some(total));
}

#[tokio::test]
async fn provider_usage_survives_real_run_chat() {
    let clock = Arc::new(Clock::default());
    let provider = Arc::new(provider(clock.clone(), 1));
    let options = agentkit_core::RunOptions {
        request: request("a", &"x".repeat(100)),
        runtime: clock,
        ..Default::default()
    };
    let handle = agentkit_core::run_chat(
        options,
        provider,
        Arc::new(agentkit_core::ToolRegistry::new()),
    );
    let events: Vec<_> = handle
        .events
        .map(|event| serde_json::to_value(event).unwrap())
        .collect()
        .await;
    assert_eq!(
        handle.result.await.unwrap().terminal,
        agentkit_core::Terminal::Completed
    );
    let event = events
        .iter()
        .find(|event| event["type"] == "run.usage")
        .expect("valid usage event");
    let data = &event["data"];
    assert_eq!(data["output"], 0);
    assert_eq!(data["reasoning"], 0);
    assert_eq!(data["cache_write_short"], 0);
    assert_eq!(data["cache_write_long"], 0);
    assert_eq!(data["completionTokens"], 0);
    assert_eq!(data["totalTokens"], data["promptTokens"]);
}

#[tokio::test]
async fn rollback_clears_all_warm_prefixes_even_older_entries() {
    let clock = Arc::new(Clock::default());
    let first = request("a", &"x".repeat(100));
    let mut second = first.clone();
    second.model = "different".into();
    let total = (mock_request_bytes(&first).len() / 4) as u64;
    let provider = provider(clock.clone(), total);
    usage(&provider, first.clone()).await;
    clock.0.store(50, Ordering::SeqCst);
    usage(&provider, second).await;
    clock.0.store(25, Ordering::SeqCst);
    assert_eq!(usage(&provider, first).await.cache_read, Some(0));
}
