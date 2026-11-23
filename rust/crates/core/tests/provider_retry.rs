//! Provider retry: which failure codes `RetryProvider` asks again for.
use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use std::sync::{Arc, Mutex};

/// Fails the first request with `code`, answers every later one.
struct FailOnce {
    code: &'static str,
    attempts: Mutex<u32>,
}

#[async_trait]
impl ChatProvider for FailOnce {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let mut attempts = self.attempts.lock().unwrap();
        *attempts += 1;
        let event = if *attempts == 1 {
            Err(CoreError::new(self.code, "request failed"))
        } else {
            Ok(ProviderEvent::Completed {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            })
        };
        Box::pin(futures_util::stream::iter([event]))
    }
}

struct Instant;

#[async_trait]
impl RetryHooks for Instant {
    async fn random(&self, _: &CancellationToken) -> Result<f64, CoreError> {
        Ok(0.5)
    }
    async fn sleep(&self, _: u64, _: &CancellationToken) {}
}

/// Attempts made and warning codes seen for one request failing with `code`.
async fn attempt(code: &'static str) -> (u32, Vec<String>) {
    let inner = Arc::new(FailOnce {
        code,
        attempts: Mutex::new(0),
    });
    let provider = RetryProvider {
        provider: inner.clone(),
        policy: RetryPolicy {
            max_attempts: 3,
            base_delay_ms: 1.0,
            max_delay_ms: 1.0,
        },
        hooks: Arc::new(Instant),
    };
    let mut stream = provider.stream_chat(ChatRequest::default(), CancellationToken::new());
    let mut warnings = vec![];
    while let Some(event) = stream.next().await {
        match event {
            Ok(ProviderEvent::RetryBackoff(ack)) => ack.notify_one(),
            Ok(ProviderEvent::Warning { message, .. }) => warnings.push(message),
            _ => {}
        }
    }
    let attempts = *inner.attempts.lock().unwrap();
    (attempts, warnings)
}

#[tokio::test]
async fn transport_and_transient_failures_are_retried() {
    for code in [
        "network_error",
        "provider_unreachable",
        "empty_body",
        "incomplete_stream",
        "provider_error",
        "429",
        "503",
        "service_unavailable",
    ] {
        let (attempts, warnings) = attempt(code).await;
        assert_eq!(attempts, 2, "{code}");
        assert!(
            warnings
                .iter()
                .any(|message| message.contains(&format!("({code})"))),
            "{code}: {warnings:?}"
        );
    }
}

#[tokio::test]
async fn request_errors_are_not_retried() {
    for code in ["400", "401", "invalid_input"] {
        let (attempts, warnings) = attempt(code).await;
        assert_eq!(attempts, 1, "{code}");
        assert!(warnings.is_empty(), "{code}: {warnings:?}");
    }
}
