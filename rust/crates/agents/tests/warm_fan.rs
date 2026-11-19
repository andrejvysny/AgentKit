//! Warm-then-fan and child cache keys (DEL-12) over the durable engine and
//! the testkit's cache-simulating provider: the first child of a role runs
//! at once, its siblings wait for its first provider response (or the
//! hold deadline, or its failure, or a restart) and then read the prefix
//! it cached under the key they share.

use agentkit_agents::*;
use agentkit_chat::*;
use agentkit_core::{
    CacheCapability, CacheHint, CacheTtl, ChatProvider, ChatRequest, ProviderEvent, ProviderStream,
    ToolRegistry, UsageRecord, child_cache_key, child_cache_seed,
};
use agentkit_engine::{RetryBackoff, SharedRuntime, SystemRuntime, TaskExecutor};
use agentkit_sessions::*;
use agentkit_store::{SqliteStore, SubmitTask};
use agentkit_testkit::cache_provider::CacheSimulatingProvider;
use futures::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

/// The first child's objective; the provider stalls or fails it on demand.
const LEAD: &str = "task alpha";

struct Seen {
    text: String,
    key: Option<String>,
    ttls: Vec<CacheTtl>,
    cache_read: Option<u64>,
}

/// A cache simulator behind the chat executor: records every request, and can
/// stall or fail the first child's requests.
struct Warm {
    sim: CacheSimulatingProvider,
    capability: CacheCapability,
    stall_lead: Arc<AtomicBool>,
    fail_lead: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Warm {
    fn new(capability: CacheCapability) -> Self {
        let clock: SharedRuntime = Arc::new(SystemRuntime);
        let sim = CacheSimulatingProvider::new(clock, 300_000, 1, |request| {
            request
                .cache
                .as_ref()
                .and_then(|cache| cache.cache_key.clone())
                .unwrap_or_else(|| request.run_id.clone())
        });
        Self {
            sim,
            capability,
            stall_lead: Arc::new(AtomicBool::new(false)),
            fail_lead: Arc::new(AtomicBool::new(false)),
            seen: Arc::new(Mutex::new(vec![])),
        }
    }

    fn open_gate(&self) {
        self.stall_lead.store(false, Ordering::SeqCst);
    }

    fn requests(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn reads_of(&self, objective: &str) -> Vec<Option<u64>> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.text.contains(objective))
            .map(|seen| seen.cache_read)
            .collect()
    }
}

#[async_trait::async_trait]
impl ChatProvider for Warm {
    fn id(&self) -> &str {
        "warm-test"
    }

    fn cache_capability(&self) -> CacheCapability {
        self.capability.clone()
    }

    fn stream_chat(&self, input: ChatRequest, cancel: CancellationToken) -> ProviderStream {
        let text = input
            .messages
            .iter()
            .map(|message| message.content.to_string())
            .collect::<Vec<_>>()
            .join("|");
        let is_lead = text.contains(LEAD);
        let ttls = input
            .cache
            .as_ref()
            .map(|cache| cache.breakpoints.iter().map(|point| point.ttl).collect())
            .unwrap_or_default();
        let index = {
            let mut seen = self.seen.lock().unwrap();
            seen.push(Seen {
                text,
                key: input
                    .cache
                    .as_ref()
                    .and_then(|cache| cache.cache_key.clone()),
                ttls,
                cache_read: None,
            });
            seen.len() - 1
        };
        let seen = Arc::clone(&self.seen);
        let inner = self.sim.stream_chat(input, cancel).map(move |event| {
            match event {
                Ok(ProviderEvent::Usage(data)) => {
                    if let Ok(record) = serde_json::from_value::<UsageRecord>(data.clone()) {
                        seen.lock().unwrap()[index].cache_read = record.cache_read;
                    }
                    Ok(ProviderEvent::Usage(data))
                }
                // The simulator answers with nothing, which the executor
                // re-asks once; a real child answers, so one request is one
                // child turn.
                Ok(ProviderEvent::Completed {
                    content,
                    tool_calls,
                    finish_reason,
                }) if content.is_empty() => Ok(ProviderEvent::Completed {
                    content: "done".into(),
                    tool_calls,
                    finish_reason,
                }),
                other => other,
            }
        });
        if !is_lead {
            return Box::pin(inner);
        }
        // The first child's request waits while stalled, then fails or
        // answers depending on the flags at that moment.
        let stall = Arc::clone(&self.stall_lead);
        let fail = Arc::clone(&self.fail_lead);
        Box::pin(
            futures::stream::once(async move {
                while stall.load(Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                let next: ProviderStream = if fail.load(Ordering::SeqCst) {
                    Box::pin(futures::stream::iter([Err(agentkit_core::CoreError::new(
                        "provider_error",
                        "the provider said no",
                    ))]))
                } else {
                    Box::pin(inner)
                };
                next
            })
            .flatten(),
        )
    }
}

struct Fan {
    store: SqliteStore,
    service: AgentSessionService<SqliteStore>,
    subagents: SubagentService,
    provider: Arc<Warm>,
    _outputs: tempfile::TempDir,
}

fn config(warm_hold_ms: u64) -> SubagentConfig {
    SubagentConfig {
        enabled: true,
        limits: SubagentRoleLimits {
            await_poll_ms: 20,
            max_await_ms: 15_000,
            warm_hold_ms,
            ..Default::default()
        },
        ..Default::default()
    }
    .effective()
}

impl Fan {
    async fn start(capability: CacheCapability, warm_hold_ms: u64) -> Self {
        let outputs = tempfile::tempdir().unwrap();
        let store = SqliteStore::memory(64)
            .unwrap()
            .with_output_root(outputs.path().join("outputs"));
        let runtime: SharedRuntime = Arc::new(SystemRuntime);
        let provider = Arc::new(Warm::new(capability));
        let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(AgentExecutor::new(
            Arc::clone(&provider) as Arc<dyn ChatProvider>,
            Arc::new(ToolRegistry::new()),
        ));
        let host = AgentHost::new(
            store.clone(),
            HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
            runtime.clone(),
            HostOptions {
                retry_backoff: RetryBackoff {
                    base_ms: 1,
                    max_ms: 2,
                    jitter_ratio: 0.0,
                },
                ..Default::default()
            },
        )
        .unwrap();
        let service = AgentSessionService::new(AgentSessionServiceDeps {
            store: store.clone(),
            runtime: runtime.clone(),
            plan_approval: None,
            wake: Some(host.wake_handle()),
        });
        host.set_session_flush_planner(Arc::new(service.clone()))
            .await;
        let workspace = std::env::temp_dir().join(format!("agentkit-warm-fan-{}", nonce()));
        std::fs::create_dir_all(&workspace).unwrap();
        service
            .create_session(&CreateSessionInput {
                session_id: Some("parent".into()),
                title: None,
                profile_id: "coding".into(),
                profile_digest: None,
                reasoning_effort: None,
                content_selection: None,
                provider_id: None,
                model: Some("model".into()),
                workspace: Some(json!({"root": workspace.to_string_lossy(), "trusted": true})),
                context: None,
                skills: None,
                turn_kind: None,
                execution_mode: Some("read_write".into()),
                metadata: None,
            })
            .await
            .unwrap();
        // A live (never-claimed) task stands in for the parent's turn.
        store
            .submit(SubmitTask {
                task_id: "parent-turn".into(),
                kind: "parent.hold".into(),
                scope_id: "parent".into(),
                payload: json!({}),
                priority: 0,
                now: runtime.now_ms(),
                available_at: None,
                depends_on: vec![],
                parent_task_id: None,
                exclusive_scope: false,
            })
            .await
            .unwrap();
        host.recover().await.unwrap();
        host.start_worker();
        let subagents = SubagentService::new(
            store.clone(),
            service.clone(),
            runtime,
            config(warm_hold_ms),
        );
        Self {
            store,
            service,
            subagents,
            provider,
            _outputs: outputs,
        }
    }

    fn request(&self, key: &str, role: &str, objective: &str) -> SpawnRequest {
        SpawnRequest {
            parent_chat_id: "parent".into(),
            parent_task_id: "parent-turn".into(),
            spawn_key: key.into(),
            brief: Brief::new(
                role,
                "child task",
                objective,
                OutputFormat::Summary,
                "the task is done",
            ),
            exclusive_workspace: false,
        }
    }

    async fn spawn(&self, key: &str, role: &str, objective: &str) -> SpawnedChild {
        self.subagents
            .spawn(&self.request(key, role, objective))
            .await
            .expect("spawn")
    }

    async fn settle(&self) {
        let outcome = self
            .subagents
            .await_children("parent", None, Some(10_000), &CancellationToken::new())
            .await
            .expect("await");
        assert!(outcome.settled, "{outcome:?}");
    }

    async fn events_of(&self, child_chat_id: &str) -> Vec<Value> {
        let view = self.subagents.status(child_chat_id).await.unwrap();
        let Some(turn) = view.turn_task_id else {
            return vec![];
        };
        self.store
            .events(&turn, None, 1000)
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.event)
            .collect()
    }

    async fn child_meta(&self, child_chat_id: &str) -> Value {
        let chat = self.store.get_chat(child_chat_id).await.unwrap().unwrap();
        chat.metadata["agentkit.subagent"].clone()
    }

    async fn seed_of(&self, child_chat_id: &str) -> String {
        self.child_meta(child_chat_id).await["cacheKeySeed"]
            .as_str()
            .expect("cacheKeySeed")
            .to_owned()
    }

    async fn effective_tools_of(&self, child_chat_id: &str) -> Vec<String> {
        self.child_meta(child_chat_id).await["roleSnapshot"]["effectiveTools"]
            .as_array()
            .expect("effectiveTools")
            .iter()
            .map(|tool| tool.as_str().unwrap().to_owned())
            .collect()
    }

    /// A restarted process over the same durable store.
    fn restarted(&self) -> SubagentService {
        SubagentService::new(
            self.store.clone(),
            self.service.clone(),
            Arc::new(SystemRuntime),
            config(60_000),
        )
    }

    /// The `subagent.released` event of the child's own first turn, if any.
    async fn released(&self, child_chat_id: &str) -> Option<Value> {
        self.events_of(child_chat_id)
            .await
            .into_iter()
            .find(|event| event["type"] == "subagent.released")
    }
}

fn nonce() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

async fn wait_until<F, Fut>(mut probe: F, what: &str)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !probe().await {
        assert!(std::time::Instant::now() < deadline, "{what} never held");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn implicit() -> CacheCapability {
    CacheCapability::ImplicitPrefix {
        hint: Some(CacheHint::PromptCacheKey),
    }
}

#[test]
fn the_default_hold_is_three_seconds() {
    assert_eq!(SubagentRoleLimits::default().warm_hold_ms, 3_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn siblings_wait_for_the_first_child_then_read_its_cache() {
    let f = Fan::start(implicit(), 60_000).await;
    f.provider.stall_lead.store(true, Ordering::SeqCst);
    let first = f.spawn("k1", "explorer", LEAD).await;
    let held: Vec<SpawnedChild> = vec![
        f.spawn("k2", "explorer", "task beta").await,
        f.spawn("k3", "explorer", "task gamma").await,
        f.spawn("k4", "explorer", "task delta").await,
    ];
    wait_until(|| async { f.provider.requests() >= 1 }, "lead request").await;
    for child in &held {
        assert_eq!(child.status, "queued", "{child:?}");
        assert!(child.turn_task_id.is_none(), "{child:?}");
    }
    // The stalled first child has not produced a response: nobody else ran.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(f.provider.requests(), 1, "siblings must stay held");

    f.provider.open_gate();
    f.settle().await;

    assert_eq!(f.provider.reads_of(LEAD), vec![Some(0)]);
    for objective in ["task beta", "task gamma", "task delta"] {
        let reads = f.provider.reads_of(objective);
        assert!(
            reads.iter().all(|read| read.is_some_and(|read| read > 0)),
            "{objective} should read the first child's cache on its first request: {reads:?}"
        );
    }
    // One shared key: root chat + role + effective tools + revision, not the
    // child's own id.
    let seed = f.seed_of(&first.child_chat_id).await;
    for child in &held {
        assert_eq!(f.seed_of(&child.child_chat_id).await, seed);
    }
    let tools = f.effective_tools_of(&first.child_chat_id).await;
    assert_eq!(seed, child_cache_seed("parent", "explorer", &tools));
    assert_ne!(seed, child_cache_seed("parent", "explorer", &[]));
    let expected = child_cache_key(&seed, 1);
    {
        let seen = f.provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        assert!(
            seen.iter()
                .all(|request| request.key.as_deref() == Some(expected.as_str())),
            "every sibling carries the shared prompt_cache_key"
        );
    }
    // Each held child's own task reports its hold; the first child has none.
    assert!(f.released(&first.child_chat_id).await.is_none());
    for child in &held {
        let event = f
            .released(&child.child_chat_id)
            .await
            .unwrap_or_else(|| panic!("no subagent.released for {}", child.child_chat_id));
        assert_eq!(event["data"]["childId"], json!(child.child_chat_id));
        assert!(event["data"]["heldMs"].as_i64().unwrap() >= 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hold_ends_at_the_deadline_when_the_first_child_stalls() {
    let f = Fan::start(implicit(), 600).await;
    f.provider.stall_lead.store(true, Ordering::SeqCst);
    f.spawn("k1", "explorer", LEAD).await;
    let sibling = f.spawn("k2", "explorer", "task beta").await;
    assert_eq!(sibling.status, "queued");
    assert!(sibling.turn_task_id.is_none());
    // The first child never answers; the sibling starts anyway.
    wait_until(
        || async { f.provider.reads_of("task beta").len() == 1 },
        "sibling released at the deadline",
    )
    .await;
    assert_eq!(
        f.provider.reads_of(LEAD).len(),
        1,
        "the first child is still stalled"
    );
    f.provider.open_gate();
    f.settle().await;
    let event = f
        .released(&sibling.child_chat_id)
        .await
        .expect("subagent.released");
    assert!(
        event["data"]["heldMs"].as_i64().unwrap() >= 400,
        "held about the deadline: {event}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_first_child_releases_its_siblings_at_once() {
    let f = Fan::start(implicit(), 60_000).await;
    f.provider.stall_lead.store(true, Ordering::SeqCst);
    f.spawn("k1", "explorer", LEAD).await;
    let sibling = f.spawn("k2", "explorer", "task beta").await;
    assert_eq!(sibling.status, "queued");
    // The first child's provider now refuses; it fails before it warms.
    f.provider.fail_lead.store(true, Ordering::SeqCst);
    f.provider.open_gate();
    wait_until(
        || async { f.provider.reads_of("task beta").len() == 1 },
        "sibling released after the first child failed",
    )
    .await;
    f.settle().await;
    let event = f
        .released(&sibling.child_chat_id)
        .await
        .expect("subagent.released");
    assert!(
        event["data"]["heldMs"].as_i64().unwrap() < 30_000,
        "released on failure, not at the 60 s deadline: {event}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn children_of_different_roles_are_not_held() {
    let f = Fan::start(implicit(), 60_000).await;
    f.provider.stall_lead.store(true, Ordering::SeqCst);
    f.spawn("k1", "explorer", LEAD).await;
    let other = f.spawn("k2", "reviewer", "task beta").await;
    wait_until(
        || async { f.provider.reads_of("task beta").len() == 1 },
        "reviewer runs beside the stalled explorer",
    )
    .await;
    let view = f.subagents.status(&other.child_chat_id).await.unwrap();
    assert!(view.turn_task_id.is_some(), "{view:?}");
    f.provider.open_gate();
    f.settle().await;
    assert!(f.released(&other.child_chat_id).await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_releases_held_children() {
    let f = Fan::start(implicit(), 60_000).await;
    f.provider.stall_lead.store(true, Ordering::SeqCst);
    f.spawn("k1", "explorer", LEAD).await;
    let sibling = f.spawn("k2", "explorer", "task beta").await;
    assert!(sibling.turn_task_id.is_none());
    // A restarted process: same durable store, nothing remembered.
    let restarted = SubagentService::new(
        f.store.clone(),
        f.service.clone(),
        Arc::new(SystemRuntime),
        config(60_000),
    );
    restarted.reap_orphans().await.unwrap();
    wait_until(
        || async { f.provider.reads_of("task beta").len() == 1 },
        "held child released by startup recovery",
    )
    .await;
    f.provider.open_gate();
    f.settle().await;
    assert!(f.released(&sibling.child_chat_id).await.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zero_hold_dispatches_every_sibling_at_once() {
    let f = Fan::start(implicit(), 0).await;
    f.provider.stall_lead.store(true, Ordering::SeqCst);
    f.spawn("k1", "explorer", LEAD).await;
    let sibling = f.spawn("k2", "explorer", "task beta").await;
    wait_until(
        || async { f.provider.reads_of("task beta").len() == 1 },
        "unheld sibling",
    )
    .await;
    f.provider.open_gate();
    f.settle().await;
    assert!(f.released(&sibling.child_chat_id).await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_requests_use_the_five_minute_class_only() {
    let explicit = CacheCapability::ExplicitBreakpoints {
        max_breakpoints: 4,
        ttls: vec![CacheTtl::FiveMinutes, CacheTtl::OneHour],
        min_tokens: 0,
    };
    let f = Fan::start(explicit, 0).await;
    f.spawn("k1", "explorer", LEAD).await;
    f.settle().await;
    let seen = f.provider.seen.lock().unwrap();
    assert!(!seen.is_empty());
    for request in seen.iter() {
        assert!(!request.ttls.is_empty(), "breakpoints were planned");
        assert!(
            request.ttls.iter().all(|ttl| *ttl == CacheTtl::FiveMinutes),
            "{:?}",
            request.ttls
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_to_a_held_child_is_refused() {
    let f = Fan::start(implicit(), 60_000).await;
    f.provider.stall_lead.store(true, Ordering::SeqCst);
    f.spawn("k1", "explorer", LEAD).await;
    let sibling = f.spawn("k2", "explorer", "task beta").await;
    for (mode, key) in [("follow_up", "m1"), ("steer", "m2")] {
        let error = f
            .subagents
            .message(&sibling.child_chat_id, json!("hello"), mode, key)
            .await
            .expect_err("a held child takes no message");
        assert!(matches!(error, AgentsError::ChildHeld), "{error:?}");
        assert!(error.to_string().starts_with("child_held"), "{error}");
    }
    let view = f.subagents.status(&sibling.child_chat_id).await.unwrap();
    assert!(view.turn_task_id.is_none(), "{view:?}");
    f.provider.open_gate();
    f.settle().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_held_child_is_never_released() {
    let f = Fan::start(implicit(), 60_000).await;
    f.provider.stall_lead.store(true, Ordering::SeqCst);
    f.spawn("k1", "explorer", LEAD).await;
    let sibling = f.spawn("k2", "explorer", "task beta").await;
    f.subagents.cancel(&sibling.child_chat_id).await.unwrap();
    // The first child warms: every release trigger fires, plus a restart.
    f.provider.open_gate();
    f.settle().await;
    f.restarted().reap_orphans().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let view = f.subagents.status(&sibling.child_chat_id).await.unwrap();
    assert_eq!(view.status, "cancelled", "{view:?}");
    assert!(view.turn_task_id.is_none(), "{view:?}");
    assert!(f.provider.reads_of("task beta").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn released_is_emitted_once_and_marked_durably() {
    let f = Fan::start(implicit(), 60_000).await;
    f.provider.stall_lead.store(true, Ordering::SeqCst);
    f.spawn("k1", "explorer", LEAD).await;
    let sibling = f.spawn("k2", "explorer", "task beta").await;
    f.provider.open_gate();
    f.settle().await;
    let released = f
        .events_of(&sibling.child_chat_id)
        .await
        .into_iter()
        .filter(|event| event["type"] == "subagent.released")
        .count();
    assert_eq!(released, 1);
    let meta = f.child_meta(&sibling.child_chat_id).await;
    assert_eq!(meta["warmHold"]["releasedEmitted"], json!(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_release_is_refused_when_the_tree_token_budget_is_spent() {
    let f = Fan::start(implicit(), 60_000).await;
    f.provider.stall_lead.store(true, Ordering::SeqCst);
    let first = f.spawn("k1", "explorer", LEAD).await;
    let sibling = f.spawn("k2", "explorer", "task beta").await;
    // The tree has spent 2M tokens (in tenths) of its default 1M.
    f.store
        .mutate_chat(&first.child_chat_id, 1, |chat| {
            chat.metadata["agentkit.subagent"]["advisorTenths"] = json!(20_000_000u64);
            Ok(())
        })
        .await
        .unwrap();
    f.restarted().reap_orphans().await.unwrap();
    let view = f.subagents.status(&sibling.child_chat_id).await.unwrap();
    assert_eq!(view.status, "cancelled", "{view:?}");
    assert!(view.turn_task_id.is_none(), "{view:?}");
    assert!(
        view.error
            .as_deref()
            .is_some_and(|error| error.contains("subagent_budget_exhausted")),
        "{view:?}"
    );
    assert!(f.provider.reads_of("task beta").is_empty());
    f.provider.open_gate();
    f.settle().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_sibling_waits_its_own_window() {
    let f = Fan::start(implicit(), 800).await;
    f.provider.stall_lead.store(true, Ordering::SeqCst);
    f.spawn("k1", "explorer", LEAD).await;
    let early = f.spawn("k2", "explorer", "task beta").await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let late = f.spawn("k3", "explorer", "task gamma").await;
    wait_until(
        || async { f.provider.reads_of("task beta").len() == 1 },
        "early sibling released at its own deadline",
    )
    .await;
    // The late sibling's window started 500 ms later: still held.
    assert!(f.provider.reads_of("task gamma").is_empty());
    wait_until(
        || async { f.provider.reads_of("task gamma").len() == 1 },
        "late sibling released at its own deadline",
    )
    .await;
    f.provider.open_gate();
    f.settle().await;
    for child in [&early, &late] {
        let event = f.released(&child.child_chat_id).await.expect("released");
        assert!(event["data"]["heldMs"].as_i64().unwrap() >= 700, "{event}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hold_registered_while_another_releases_still_releases() {
    let f = Fan::start(implicit(), 600).await;
    f.provider.stall_lead.store(true, Ordering::SeqCst);
    f.spawn("k1", "explorer", LEAD).await;
    f.spawn("k2", "explorer", "task beta").await;
    wait_until(
        || async { f.provider.reads_of("task beta").len() == 1 },
        "first sibling released",
    )
    .await;
    // The lead is still stalled: only this sibling's own deadline can end
    // its hold, after the waker has just swept a roster with nothing held.
    f.spawn("k3", "explorer", "task gamma").await;
    wait_until(
        || async { f.provider.reads_of("task gamma").len() == 1 },
        "late hold released at its deadline",
    )
    .await;
    f.provider.open_gate();
    f.settle().await;
}
