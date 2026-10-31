use super::*;
use agentkit_core::{CoreError, ProviderEvent, ProviderStream};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

struct UsageThenWait;
#[async_trait]
impl ChatProvider for UsageThenWait {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let usage = json!({"callId":"billed","attempt":1,"step":0,"model":"model","totalTokens":9,"source":"stream","finalForCall":true});
        Box::pin(
            futures::stream::iter([Ok::<_, CoreError>(ProviderEvent::Usage(usage))])
                .chain(futures::stream::pending()),
        )
    }
}

#[tokio::test]
async fn timeout_preserves_usage_seen_before_stall() {
    let mut handle = agentkit_core::run_chat(
        RunOptions::default(),
        Arc::new(UsageThenWait),
        Arc::new(ToolRegistry::new()),
    );
    let (usage, _, error) = collect_events(&mut handle, std::time::Duration::from_millis(20)).await;
    handle.result.abort();
    assert_eq!(error, Some("summarizer timed out"));
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["totalTokens"], 9);
}

#[test]
fn empty_and_truncated_responses_are_failures() {
    let result = agentkit_core::RunResult {
        run_id: "r".into(),
        terminal: Terminal::Completed,
        appended_messages: vec![agentkit_core::ChatMessage::text("assistant", "")],
        iterations: 1,
        continuation: None,
        suspended: false,
    };
    assert_eq!(
        summary_text(result.clone(), "stop"),
        Err("summarizer returned no text")
    );
    assert_eq!(summary_text(result, "length"), Err("summary incomplete"));
}

#[tokio::test]
async fn durable_event_failure_does_not_skip_later_usage_metering() {
    let attempted = Arc::new(std::sync::Mutex::new(Vec::new()));
    let metered = Arc::new(std::sync::Mutex::new(Vec::new()));
    let attempts = attempted.clone();
    let records = metered.clone();
    let result = account_all(vec![json!(1), json!(2), json!(3)], move |item| {
        let attempts = attempts.clone();
        let records = records.clone();
        async move {
            attempts.lock().unwrap().push(item.clone());
            let event_result = if item == json!(1) {
                Err(HostError::Execution("first event failed".into()))
            } else if item == json!(2) {
                Err(HostError::Execution("second event failed".into()))
            } else {
                Ok(())
            };
            records.lock().unwrap().push(item);
            event_result
        }
    })
    .await;
    assert_eq!(
        *attempted.lock().unwrap(),
        vec![json!(1), json!(2), json!(3)]
    );
    assert_eq!(*metered.lock().unwrap(), vec![json!(1), json!(2), json!(3)]);
    assert_eq!(result.unwrap_err().to_string(), "first event failed");
}
