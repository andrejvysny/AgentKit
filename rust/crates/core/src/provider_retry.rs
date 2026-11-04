use crate::{
    CancellationToken, ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream,
};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;

#[derive(Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay_ms: f64,
    pub max_delay_ms: f64,
}
impl RetryPolicy {
    fn attempts(&self) -> u32 {
        if !(1..=6).contains(&self.max_attempts)
            || !self.base_delay_ms.is_finite()
            || self.base_delay_ms < 0.0
            || !self.max_delay_ms.is_finite()
            || self.max_delay_ms < 0.0
        {
            1
        } else {
            self.max_attempts
        }
    }
    fn delay(&self, attempt: u32, random: f64) -> u64 {
        let unit = if random.is_finite() {
            random.clamp(0.0, 1.0)
        } else {
            0.5
        };
        (self
            .max_delay_ms
            .min(self.base_delay_ms * 2f64.powi(attempt as i32 - 1))
            * (0.8 + 0.4 * unit))
            .round()
            .max(0.0) as u64
    }
}
#[async_trait]
pub trait RetryHooks: Send + Sync {
    async fn random(&self, cancellation: &CancellationToken) -> Result<f64, CoreError>;
    async fn sleep(&self, ms: u64, cancellation: &CancellationToken);
}
pub struct RetryProvider {
    pub provider: Arc<dyn ChatProvider>,
    pub policy: RetryPolicy,
    pub hooks: Arc<dyn RetryHooks>,
}
fn retryable(code: &str) -> bool {
    matches!(
        code,
        "network_error"
            | "empty_body"
            | "incomplete_stream"
            | "provider_error"
            | "408"
            | "425"
            | "429"
            | "500"
            | "502"
            | "503"
            | "504"
            | "529"
            | "service_unavailable"
            | "usage_limit"
    )
}
fn visible(event: &ProviderEvent) -> bool {
    matches!(
        event,
        ProviderEvent::Delta(_) | ProviderEvent::Completed { .. } | ProviderEvent::ToolRequested(_)
    ) || matches!(event, ProviderEvent::Event { kind, .. } if kind == "run.message.completed" || kind == "run.message.reasoning-delta")
}
impl ChatProvider for RetryProvider {
    fn id(&self) -> &str {
        self.provider.id()
    }
    fn tracks_transport_requests(&self) -> bool {
        self.provider.tracks_transport_requests()
    }
    fn stream_chat(&self, request: ChatRequest, cancellation: CancellationToken) -> ProviderStream {
        retry_stream(
            self.provider.clone(),
            self.policy.clone(),
            self.hooks.clone(),
            request,
            cancellation,
        )
    }
}
fn retry_stream(
    provider: Arc<dyn ChatProvider>,
    policy: RetryPolicy,
    hooks: Arc<dyn RetryHooks>,
    request: ChatRequest,
    cancellation: CancellationToken,
) -> ProviderStream {
    Box::pin(async_stream::stream! {
        let attempts = policy.attempts();
        for attempt in 1..=attempts {
            if attempt > 1 {
                if let Err(error) = reserve_retry(&provider, &request).await { yield Err(error); return; }
            }
            yield Ok(ProviderEvent::RequestStarted);
            let stream = provider.stream_chat(request.clone(), cancellation.child_token());
            let mut events = attempt_stream(stream, cancellation.clone(), attempt, attempts);
            let mut failure = None;
            while let Some(item) = events.next().await {
                match item {
                    AttemptItem::Event(event) => yield event,
                    AttemptItem::Retry(error) => { failure = Some(error); break; }
                }
            }
            let Some(error) = failure else { return };
            let ack = Arc::new(tokio::sync::Notify::new());
            yield Ok(ProviderEvent::RetryBackoff(ack.clone()));
            tokio::select! { _ = cancellation.cancelled() => return, _ = ack.notified() => {} }
            let random = match hooks.random(&cancellation).await { Ok(value) => value, Err(error) => { yield Err(error); return; } };
            if cancellation.is_cancelled() { return; }
            let delay = policy.delay(attempt, random);
            yield Ok(retry_warning(&error, delay, attempt, attempts));
            hooks.sleep(delay, &cancellation).await;
            if cancellation.is_cancelled() { return; }
        }
    })
}
async fn reserve_retry(
    provider: &Arc<dyn ChatProvider>,
    request: &ChatRequest,
) -> Result<(), CoreError> {
    if let Some(budget) = &request.budget {
        budget.consume("retries").await?;
        if !provider.tracks_transport_requests() {
            budget.consume("providerRequests").await?;
        }
    }
    Ok(())
}
enum AttemptItem {
    Event(Result<ProviderEvent, CoreError>),
    Retry(CoreError),
}
fn attempt_stream(
    mut stream: ProviderStream,
    cancellation: CancellationToken,
    attempt: u32,
    attempts: u32,
) -> Pin<Box<dyn Stream<Item = AttemptItem> + Send>> {
    Box::pin(async_stream::stream! {
        let mut has_visible = false;
        let mut failure = None;
        loop {
            let event = tokio::select! { _ = cancellation.cancelled() => return, event = stream.next() => event };
            let Some(event) = event else {
                if let Some(error) = failure { yield AttemptItem::Retry(error); }
                return;
            };
            if failure.is_some() {
                if let Ok(event @ ProviderEvent::Usage(_)) = event { yield AttemptItem::Event(Ok(event)); }
                continue;
            }
            match event {
                Err(error) if !has_visible && attempt < attempts && retryable(&error.code) => failure = Some(error),
                Err(error) => { yield AttemptItem::Event(Err(error)); return; }
                Ok(ProviderEvent::Started) if attempt > 1 => {},
                Ok(event) => { has_visible |= visible(&event); yield AttemptItem::Event(Ok(event)); }
            }
        }
    })
}
fn retry_warning(error: &CoreError, delay: u64, attempt: u32, attempts: u32) -> ProviderEvent {
    let seconds = ((delay as f64 / 100.0).round() / 10.0).to_string();
    ProviderEvent::Warning {
        code: "provider_retry".into(),
        message: format!(
            "Provider request failed ({}); retrying in {}s (attempt {} of {}).",
            error.code,
            seconds,
            attempt + 1,
            attempts
        ),
    }
}
