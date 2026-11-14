//! Deterministic cache simulation and raw transport-body observation for tests.

use agentkit_core::{
    CancellationToken, ChatProvider, ChatRequest, ProviderEvent, ProviderStream, Runtime,
    UsageRecord,
};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

type CacheKey = dyn Fn(&ChatRequest) -> String + Send + Sync;
struct Entry {
    bytes: Vec<u8>,
    at_ms: i64,
}

#[derive(Default)]
struct CacheState {
    last_now: Option<i64>,
    entries: HashMap<String, Vec<Entry>>,
}
impl CacheState {
    fn expire(&mut self, now: i64, lifetime_ms: u64) {
        if self.last_now.is_some_and(|last| now < last) {
            self.entries.clear();
        }
        self.last_now = Some(now);
        self.entries.retain(|_, entries| {
            entries.retain(|entry| {
                now.checked_sub(entry.at_ms)
                    .and_then(|age| u64::try_from(age).ok())
                    .is_some_and(|age| age < lifetime_ms)
            });
            !entries.is_empty()
        });
    }
}

/// Mock cache based on serialized generic requests, not provider wire formats.
/// Token estimates are byte counts divided by four, rounded down. Expiry is
/// sliding and inclusive at the lifetime boundary. Each key retains distinct
/// unexpired requests. Minimum applies to stored requests and matched prefixes.
pub struct CacheSimulatingProvider {
    clock: Arc<dyn Runtime>,
    lifetime_ms: u64,
    minimum_tokens: u64,
    key: Box<CacheKey>,
    state: Mutex<CacheState>,
}
impl CacheSimulatingProvider {
    pub fn new(
        clock: Arc<dyn Runtime>,
        lifetime_ms: u64,
        minimum_tokens: u64,
        key: impl Fn(&ChatRequest) -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            clock,
            lifetime_ms,
            minimum_tokens,
            key: Box::new(key),
            state: Mutex::new(CacheState::default()),
        }
    }
    fn usage(&self, request: &ChatRequest) -> UsageRecord {
        let key = (self.key)(request);
        let bytes = mock_request_bytes(request);
        let total = tokens(bytes.len());
        let mut state = self.state.lock().expect("cache mutex poisoned");
        let now = self.clock.now_ms();
        state.expire(now, self.lifetime_ms);
        let warm = state.entries.entry(key).or_default();
        let matched = warm
            .iter()
            .map(|entry| {
                tokens(
                    entry
                        .bytes
                        .iter()
                        .zip(&bytes)
                        .take_while(|(a, b)| a == b)
                        .count(),
                )
            })
            .max()
            .unwrap_or(0);
        let read = if matched >= self.minimum_tokens {
            matched
        } else {
            0
        };
        if total >= self.minimum_tokens {
            if let Some(entry) = warm.iter_mut().find(|entry| entry.bytes == bytes) {
                entry.at_ms = now;
            } else {
                warm.push(Entry { bytes, at_ms: now });
            }
        }
        UsageRecord {
            input_uncached: Some(total - read),
            cache_read: Some(read),
            cache_write_short: Some(0),
            cache_write_long: Some(0),
            output: Some(0),
            reasoning: Some(0),
            provider_raw: json!({"source": "cache_simulator"}),
        }
    }
}
impl ChatProvider for CacheSimulatingProvider {
    fn id(&self) -> &str {
        "cache-simulator"
    }
    fn stream_chat(&self, input: ChatRequest, cancellation: CancellationToken) -> ProviderStream {
        if cancellation.is_cancelled() {
            return Box::pin(futures::stream::empty());
        }
        let record = self.usage(&input);
        let total = record.input_uncached.unwrap_or(0) + record.cache_read.unwrap_or(0);
        let mut usage = serde_json::to_value(&record).expect("usage serializes");
        usage["callId"] = self.clock.id().into();
        usage["attempt"] = 1.into();
        usage["step"] = 0.into();
        usage["model"] = input.model.into();
        usage["source"] = "response".into();
        usage["finalForCall"] = true.into();
        usage["promptTokens"] = total.into();
        usage["completionTokens"] = 0.into();
        usage["totalTokens"] = total.into();
        usage["cachedPromptTokens"] = record.cache_read.unwrap_or(0).into();
        Box::pin(futures::stream::iter([
            Ok(ProviderEvent::Usage(usage)),
            Ok(ProviderEvent::Completed {
                content: String::new(),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            }),
        ]))
    }
}
fn tokens(bytes: usize) -> u64 {
    u64::try_from(bytes / 4).unwrap_or(u64::MAX)
}

/// Stable mock-lane encoding; deliberately distinct from provider wire bodies.
/// Runtime-only state, run ID and cache key are excluded.
pub fn mock_request_bytes(request: &ChatRequest) -> Vec<u8> {
    serde_json::to_vec(&json!([
        request.model,
        request.tools,
        request.temperature,
        request.reasoning_effort,
        request.reasoning_summary,
        request.max_output_tokens,
        request.selected_content,
        request.messages,
        request.continuation,
        request.continuation_scope,
        request.continuation_required
    ]))
    .expect("chat request fields serialize")
}

/// Records exactly the bytes passed by a transport fixture or HTTP server.
#[derive(Clone, Default)]
pub struct WireObserver {
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
}
impl WireObserver {
    pub fn observe(&self, body: &[u8]) {
        self.bodies
            .lock()
            .expect("wire observer mutex poisoned")
            .push(body.to_vec());
    }
    pub fn snapshot(&self) -> Vec<Vec<u8>> {
        self.bodies
            .lock()
            .expect("wire observer mutex poisoned")
            .clone()
    }
}
