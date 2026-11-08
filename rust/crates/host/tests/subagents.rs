//! Subagent coordination end-to-end over the durable engine: spawn,
//! replay, await, message, cancel, resume, limits, role ceilings,
//! permission inheritance, workspace write leases and the orphan sweep —
//! with no mocks below the store: the host's own worker executes every
//! child turn.

use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, ToolCall, ToolDefinition,
    ToolLimits, ToolRegistry,
};
use agentkit_host::*;
use agentkit_store::{RegisterSubagent, SqliteStore, SubmitTask, TaskStatus};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const ANSWER: &str = "the child answer";

/// Answers every request with `ANSWER`; a gate lets tests hold turns
/// mid-flight, and a fail flag makes the provider reject requests until
/// flipped (for the failure/resume path).
struct Child {
    gate: Arc<Notify>,
    hold: AtomicBool,
    released: Arc<AtomicBool>,
    fail: AtomicBool,
    requests: Mutex<Vec<String>>,
}

#[async_trait]
impl ChatProvider for Child {
    fn id(&self) -> &str {
        "child-test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let text = input
            .messages
            .iter()
            .map(|message| message.content.to_string())
            .collect::<Vec<_>>()
            .join("|");
        self.requests.lock().unwrap().push(text);
        if self.fail.load(Ordering::SeqCst) {
            return Box::pin(futures::stream::iter([Err(CoreError::new(
                "provider_error",
                "the provider said no",
            ))]));
        }
        if self.hold.load(Ordering::SeqCst) {
            let gate = Arc::clone(&self.gate);
            let released = Arc::clone(&self.released);
            return Box::pin(
                futures::stream::once(async move {
                    while !released.load(Ordering::SeqCst) {
                        gate.notified().await;
                    }
                    Ok(ProviderEvent::Completed {
                        content: ANSWER.into(),
                        finish_reason: "stop".into(),
                        tool_calls: vec![],
                    })
                })
                .chain(futures::stream::iter([])),
            );
        }
        Box::pin(futures::stream::iter([Ok(ProviderEvent::Completed {
            content: ANSWER.into(),
            finish_reason: "stop".into(),
            tool_calls: vec![],
        })]))
    }
}

struct Harness {
    store: SqliteStore,
    service: AgentSessionService,
    subagents: SubagentService,
    child: Arc<Child>,
    workspace: String,
}

fn config() -> SubagentConfig {
    SubagentConfig {
        enabled: true,
        limits: SubagentRoleLimits {
            max_depth: 2,
            max_concurrency: 8,
            max_per_parent: 4,
            await_poll_ms: 20,
            max_await_ms: 15_000,
            lease_ttl_ms: 60_000,
            cancel_orphans: true,
            max_tree_provider_requests: None,
        },
        roles: Default::default(), // effective() preloads the five built-ins
        delegation_guidance: None,
    }
}

impl Harness {
    async fn start() -> Self {
        Self::start_with(config(), None, None).await
    }

    async fn start_with(
        mut config: SubagentConfig,
        max_concurrency: Option<i64>,
        max_per_parent: Option<i64>,
    ) -> Self {
        if let Some(value) = max_concurrency {
            config.limits.max_concurrency = value;
        }
        if let Some(value) = max_per_parent {
            config.limits.max_per_parent = value;
        }
        let workspace = std::env::temp_dir().join(format!("agentkit-subagent-ws-{}", nonce()));
        std::fs::create_dir_all(&workspace).unwrap();
        Self::build(config, workspace.to_string_lossy().into_owned()).await
    }

    async fn build(config: SubagentConfig, workspace: String) -> Self {
        let store = SqliteStore::memory(64).unwrap();
        let runtime: SharedRuntime = Arc::new(SystemRuntime);
        let child = Arc::new(Child {
            gate: Arc::new(Notify::new()),
            hold: AtomicBool::new(false),
            released: Arc::new(AtomicBool::new(false)),
            fail: AtomicBool::new(false),
            requests: Mutex::new(vec![]),
        });
        let executor: Arc<dyn TaskExecutor> = Arc::new(ChatExecutor::new(
            Arc::clone(&child) as Arc<dyn ChatProvider>,
            Arc::new(ToolRegistry::new()),
        ));
        let host = AgentHost::new(
            store.clone(),
            HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
            runtime.clone(),
            HostOptions {
                // Failing turns must exhaust attempts inside the test budget.
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
                workspace: Some(json!({"root": workspace, "trusted": true})),
                context: None,
                skills: None,
                turn_kind: None,
                execution_mode: Some("read_write".into()),
                metadata: None,
            })
            .await
            .unwrap();
        // A live (never-claimed) task stands in for the parent's running
        // turn: spawn requires the caller's own non-terminal task.
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
        let subagents =
            SubagentService::new(store.clone(), service.clone(), runtime, config.effective());
        Self {
            store,
            service,
            subagents,
            child,
            workspace,
        }
    }

    fn spawn_request(&self, key: &str, role: &str, prompt: &str) -> SpawnRequest {
        SpawnRequest {
            parent_chat_id: "parent".into(),
            parent_task_id: "parent-turn".into(),
            spawn_key: key.into(),
            role: role.into(),
            prompt: prompt.into(),
            title: None,
            model: None,
            exclusive_workspace: false,
            max_iterations: None,
            max_provider_requests: None,
            overall_timeout_ms: None,
        }
    }

    fn hold(&self, on: bool) {
        self.child.hold.store(on, Ordering::SeqCst);
    }

    fn release(&self) {
        self.child.released.store(true, Ordering::SeqCst);
        self.child.gate.notify_waiters();
    }

    async fn spawn(&self, key: &str, role: &str, prompt: &str) -> SpawnedChild {
        self.subagents
            .spawn(&self.spawn_request(key, role, prompt))
            .await
            .expect("spawn")
    }

    async fn await_all(&self) -> AwaitOutcome {
        self.subagents
            .await_children("parent", None, Some(10_000), &CancellationToken::new())
            .await
            .expect("await")
    }

    async fn status(&self, child_id: &str) -> SubagentView {
        self.subagents.status(child_id).await.expect("status")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_await_returns_durable_child_answer() {
    let h = Harness::start().await;
    let spawned = h.spawn("k1", "reviewer", "audit the module").await;
    assert_eq!(spawned.role, "reviewer");
    assert_eq!(spawned.depth, 1);

    // The child's prompt carries the role briefing plus the assignment.
    wait_until(
        || async {
            h.child
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|text| text.contains("audit the module"))
        },
        "child prompt reached the provider",
    )
    .await;

    let outcome = h.await_all().await;
    assert!(outcome.settled, "settled: {outcome:?}");
    assert!(!outcome.timed_out);
    assert_eq!(outcome.results.len(), 1);
    assert_eq!(outcome.results[0].status, "completed");
    assert_eq!(outcome.results[0].result.as_deref(), Some(ANSWER));

    let view = h.status(&spawned.child_chat_id).await;
    assert_eq!(view.status, "completed");
    assert_eq!(view.parent_chat_id, "parent");
    assert_eq!(view.turn_task_id, spawned.turn_task_id);
    assert_eq!(view.task_status.as_deref(), Some("Completed"));
    let list = h.subagents.list("parent").await.unwrap();
    assert_eq!(list.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_replays_idempotently_per_spawn_key() {
    let h = Harness::start().await;
    let first = h.spawn("same", "reviewer", "one").await;
    // Retry of the same logical tool call (attempt replay) re-admits the
    // SAME child — never a second execution.
    let replay = h.spawn("same", "reviewer", "one").await;
    assert_eq!(first.child_chat_id, replay.child_chat_id);
    let other = h.spawn("other", "reviewer", "two").await;
    assert_ne!(first.child_chat_id, other.child_chat_id);
    assert_eq!(h.subagents.list("parent").await.unwrap().len(), 2);
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_subagents_spawn_nothing_and_contribute_no_tools() {
    let mut disabled = config();
    disabled.enabled = false;
    let h = Harness::build(disabled, "/tmp/unused".into()).await;
    let error = h
        .subagents
        .spawn(&h.spawn_request("k", "reviewer", "x"))
        .await
        .expect_err("disabled");
    assert!(error.to_string().contains("subagents_disabled"));

    let context = contribution_context(Some("parent".into()));
    assert!(
        SubagentContributor::new(h.subagents.clone())
            .contribute(&context)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enabled_contributor_offers_the_seven_coordination_tools() {
    let h = Harness::start().await;
    let context = contribution_context(Some("parent".into()));
    let names: Vec<String> = SubagentContributor::new(h.subagents.clone())
        .contribute(&context)
        .await
        .unwrap()
        .into_iter()
        .map(|contributed| match contributed {
            ContributedTool::Read(tool) => tool.definition().name.clone(),
            ContributedTool::DirectWrite(tool) => tool.definition().name.clone(),
            ContributedTool::Write(_) => "governed".into(),
        })
        .collect();
    for expected in SUBAGENT_TOOL_NAMES {
        assert!(names.contains(&expected.to_owned()), "missing {expected}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn depth_and_nesting_permissions_are_enforced() {
    // Depth 0: no delegation at all.
    let mut none = config();
    none.limits.max_depth = 0;
    let h = Harness::build(none, "/tmp/unused".into()).await;
    let error = h
        .subagents
        .spawn(&h.spawn_request("k", "reviewer", "x"))
        .await
        .expect_err("depth 0");
    assert!(error.to_string().contains("subagent_depth_exceeded"));

    // A read-only reviewer child may not delegate further.
    let h = Harness::start().await;
    h.hold(true);
    let child = h.spawn("k", "reviewer", "hang").await;
    let child_turn = h
        .status(&child.child_chat_id)
        .await
        .turn_task_id
        .expect("turn");
    let error = h
        .subagents
        .spawn(&SpawnRequest {
            parent_chat_id: child.child_chat_id.clone(),
            parent_task_id: child_turn,
            ..h.spawn_request("grand", "reviewer", "nope")
        })
        .await
        .expect_err("reviewer may not nest");
    assert!(error.to_string().contains("may not spawn children"));
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrency_and_parent_budgets_are_enforced() {
    let h = Harness::start_with(config(), Some(2), Some(2)).await;
    h.hold(true);
    h.spawn("a", "researcher", "one").await;
    h.spawn("b", "researcher", "two").await;
    let error = h
        .subagents
        .spawn(&h.spawn_request("c", "researcher", "three"))
        .await
        .expect_err("global cap");
    assert!(error.to_string().contains("subagent_concurrency_limit"));

    // A parent at its own per-parent cap reports that limit.
    let h2 = Harness::start_with(config(), Some(50), Some(1)).await;
    h2.hold(true);
    h2.spawn("a", "researcher", "one").await;
    let error = h2
        .subagents
        .spawn(&h2.spawn_request("b", "researcher", "two"))
        .await
        .expect_err("parent cap");
    assert!(error.to_string().contains("subagent_parent_limit"));
    h.hold(false);
    h.release();
    h.await_all().await;
    h2.hold(false);
    h2.release();
    h2.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_spawns_cannot_outrun_the_parent_cap() {
    // Parallel tool calls spawn concurrently; the per-parent cap must hold
    // even when every spawn's count check runs before any child row is
    // visible. Pre-serialization this created more active children than
    // `max_per_parent` allows.
    let h = Harness::start_with(config(), Some(50), Some(2)).await;
    h.hold(true);
    let requests: Vec<SpawnRequest> = ["a", "b", "c", "d"]
        .iter()
        .map(|key| h.spawn_request(key, "researcher", "work"))
        .collect();
    let results =
        futures::future::join_all(requests.iter().map(|request| h.subagents.spawn(request))).await;
    let admitted = results.iter().filter(|result| result.is_ok()).count();
    let refused: Vec<String> = results
        .iter()
        .filter_map(|result| result.as_ref().err().map(|error| error.to_string()))
        .collect();
    assert_eq!(admitted, 2, "exactly the cap's worth of children admitted");
    assert_eq!(refused.len(), 2);
    assert!(
        refused
            .iter()
            .all(|error| error.contains("subagent_parent_limit")),
        "overshoot must be refused by the parent cap: {refused:?}"
    );
    let children = h.subagents.list("parent").await.unwrap();
    assert_eq!(
        children
            .iter()
            .filter(|view| !matches!(view.status.as_str(), "completed" | "failed" | "canceled"))
            .count(),
        2,
        "durable rows must agree with the cap"
    );
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_exclusive_spawn_on_a_held_root_is_refused() {
    // Admission — not the runtime guard — must enforce exclusivity between
    // children: a second exclusive spawn used to silently overwrite the
    // first holder's lease, leaving the FIRST writer denied its own
    // workspace while both believed they held it.
    let h = Harness::start().await;
    h.hold(true);
    let first = h
        .subagents
        .spawn(&SpawnRequest {
            exclusive_workspace: true,
            ..h.spawn_request("k1", "implementer", "write code")
        })
        .await
        .expect("first exclusive spawn");
    wait_until(
        || async {
            h.subagents
                .leases()
                .holder(&h.workspace, h.subagents.now_ms())
                .as_deref()
                == Some(first.child_chat_id.as_str())
        },
        "first lease registered",
    )
    .await;
    let error = h
        .subagents
        .spawn(&SpawnRequest {
            exclusive_workspace: true,
            ..h.spawn_request("k2", "implementer", "write too")
        })
        .await
        .expect_err("second exclusive child must not preempt the holder");
    assert!(
        error.to_string().contains("workspace_busy"),
        "refusal names the conflict: {error}"
    );
    assert!(
        h.subagents
            .leases()
            .is_held_by(&h.workspace, &first.child_chat_id, h.subagents.now_ms()),
        "the refusal must leave the holder's lease intact"
    );
    // ...and it must leave no row either: the refused child was never
    // admitted, so a ghost `queued` sibling is not just noise — it counts
    // toward the parent cap and could later be re-driven unleased.
    let roster = h.subagents.list("parent").await.unwrap();
    assert_eq!(
        roster.len(),
        1,
        "a workspace_busy refusal must not leave a ghost child row: {roster:?}"
    );
    h.subagents.cancel(&first.child_chat_id).await.unwrap();
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_settles_the_child_and_frees_its_lease() {
    let h = Harness::start().await;
    h.hold(true);
    let child = h
        .subagents
        .spawn(&SpawnRequest {
            exclusive_workspace: true,
            ..h.spawn_request("k", "implementer", "write code")
        })
        .await
        .expect("implementer spawn");
    wait_until(
        || async {
            h.subagents
                .leases()
                .holder(&h.workspace, h.subagents.now_ms())
                .as_deref()
                == Some(child.child_chat_id.as_str())
        },
        "exclusive lease registered",
    )
    .await;

    let view = h.subagents.cancel(&child.child_chat_id).await.unwrap();
    // Cancel is cooperative: the request is durable immediately, the task
    // settles as the worker reaches its next boundary.
    let _ = view;
    // The worker may land the cancel at once (the held provider stream is
    // abortable), so "still held" is racy. The invariant is the ordering:
    // the lease is never free while the child is not yet terminal — a
    // replacement child must not start writing beside a mid-tool one.
    let holder = h
        .subagents
        .leases()
        .holder(&h.workspace, h.subagents.now_ms());
    if holder.is_none() {
        assert_eq!(
            h.status(&child.child_chat_id).await.status,
            "cancelled",
            "the lease was released before the cancelled child settled"
        );
    } else {
        assert_eq!(holder.as_deref(), Some(child.child_chat_id.as_str()));
    }
    h.hold(false);
    h.release();
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "cancelled" },
        "worker lands the cancel",
    )
    .await;
    assert!(
        h.subagents
            .leases()
            .holder(&h.workspace, h.subagents.now_ms())
            .is_none(),
        "the terminal projection releases the workspace lease"
    );
    let view = h.status(&child.child_chat_id).await;
    assert_eq!(view.status, "cancelled");
    assert!(view.error.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn message_steers_live_turns_and_queues_behind_them() {
    let h = Harness::start().await;
    h.hold(true);
    let child = h.spawn("k", "reviewer", "hang").await;
    wait_until(
        || async { !h.child.requests.lock().unwrap().is_empty() },
        "child turn started",
    )
    .await;

    // A steer while the turn is live is durably queued TARGETED at it: the
    // run reads steers at its next boundary and settlement arbitrates the
    // rest. It is never promoted blind.
    let receipt = h
        .subagents
        .message(
            &child.child_chat_id,
            Value::String("focus on the parser".into()),
            "steer",
            "m1",
        )
        .await
        .unwrap();
    assert_eq!(
        receipt.delivered, "queued",
        "no blind promotion under a live turn"
    );

    let receipt = h
        .subagents
        .message(
            &child.child_chat_id,
            Value::String("also check tests".into()),
            "follow_up",
            "m2",
        )
        .await
        .unwrap();
    assert_eq!(
        receipt.delivered, "queued",
        "follow-up queues behind the turn"
    );
    assert_eq!(h.status(&child.child_chat_id).await.queued_inputs, 2);

    h.hold(false);
    h.release();
    // Settlement promotes the queued inputs: the child actually RUNS both.
    wait_until(
        || async {
            let view = h.status(&child.child_chat_id).await;
            let seen = h.child.requests.lock().unwrap();
            view.status == "completed"
                && view.queued_inputs == 0
                && seen.iter().any(|text| text.contains("focus on the parser"))
                && seen.iter().any(|text| text.contains("also check tests"))
        },
        "queued inputs ran as follow-up turns",
    )
    .await;

    // A settled child is NOT revivable by message. The row is frozen at
    // `completed`; a follow-up that dispatched behind the freeze would run
    // a hidden turn — uncounted by the concurrency cap, invisible to
    // `await`, and unleased for writer roles. Revival is `resume`'s domain
    // (failed/cancelled/interrupted turns); new work spawns a new child.
    let requests_before = h.child.requests.lock().unwrap().len();
    let error = h
        .subagents
        .message(
            &child.child_chat_id,
            Value::String("one more pass".into()),
            "follow_up",
            "m3",
        )
        .await
        .expect_err("a completed child must not accept follow-ups");
    assert!(
        error.to_string().contains("subagent_terminal"),
        "honest terminal rejection, got: {error}"
    );
    assert_eq!(
        h.child.requests.lock().unwrap().len(),
        requests_before,
        "the rejected message must never reach the provider"
    );
    // Late awaits are a no-op: the child is already terminal.
    let outcome = h.await_all().await;
    assert!(outcome.settled && outcome.results.is_empty());
    let view = h.status(&child.child_chat_id).await;
    assert_eq!(view.status, "completed");
    assert_eq!(view.result.as_deref(), Some(ANSWER));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn message_refuses_terminal_children_in_both_modes() {
    let h = Harness::start().await;
    h.hold(true);
    let child = h.spawn("k", "reviewer", "hang").await;
    wait_until(
        || async { !h.child.requests.lock().unwrap().is_empty() },
        "child turn started",
    )
    .await;
    h.subagents.cancel(&child.child_chat_id).await.unwrap();
    h.hold(false);
    h.release();
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "cancelled" },
        "worker lands the cancel",
    )
    .await;
    let requests_before = h.child.requests.lock().unwrap().len();
    for mode in ["queue", "steer"] {
        let error = h
            .subagents
            .message(
                &child.child_chat_id,
                Value::String("still waiting".into()),
                mode,
                &format!("t-{mode}"),
            )
            .await
            .expect_err("a cancelled child must not accept messages");
        assert!(
            error.to_string().contains("subagent_terminal"),
            "{mode} must be rejected on a cancelled child, got: {error}"
        );
    }
    assert_eq!(
        h.child.requests.lock().unwrap().len(),
        requests_before,
        "no provider traffic may run behind a terminal row"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_re_dispatches_a_failed_assignment() {
    let h = Harness::start().await;
    h.child.fail.store(true, Ordering::SeqCst);
    let child = h.spawn("k", "reviewer", "doomed").await;
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "failed" },
        "child failed after retries",
    )
    .await;

    h.child.fail.store(false, Ordering::SeqCst);
    let resumed = h
        .subagents
        .resume(&child.child_chat_id, "r1")
        .await
        .unwrap();
    assert_ne!(
        resumed.turn_task_id, child.turn_task_id,
        "resume dispatches a fresh turn"
    );
    // The reactivated row is RUNNING the moment resume returns: a parent
    // that polls right after resuming must see a live child, not the stale
    // `failed` status the row carried before re-dispatch.
    assert_eq!(resumed.status, "running", "resume must flip the row live");
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "completed" },
        "resumed child completed",
    )
    .await;
    let view = h.status(&child.child_chat_id).await;
    assert_eq!(view.result.as_deref(), Some(ANSWER));
    // The resume prompt tells the child what happened.
    assert!(
        h.child
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|text| text.contains("Resume your previous assignment"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_refuses_live_turns_and_unknown_children() {
    let h = Harness::start().await;
    h.hold(true);
    let child = h.spawn("k", "reviewer", "hang").await;
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "running" },
        "child running",
    )
    .await;
    let error = h
        .subagents
        .resume(&child.child_chat_id, "r")
        .await
        .expect_err("live turn");
    assert!(
        error
            .to_string()
            .contains("only interrupted, failed or cancelled")
    );

    // Unknown children are refused outright.
    let error = h
        .subagents
        .resume("sub_missing", "r")
        .await
        .expect_err("unknown child");
    assert!(error.to_string().contains("subagent_not_found"));
    assert!(matches!(
        h.subagents.status("sub_missing").await,
        Err(HostError::NotFound(message)) if message == "subagent_not_found: sub_missing"
    ));
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parent_settlement_reaps_live_children() {
    let h = Harness::start().await;
    h.hold(true);
    let child = h.spawn("k", "researcher", "orphan me").await;
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "running" },
        "child running",
    )
    .await;

    // The parent turn settles (here: cancelled — any terminal state has the
    // same effect; the same sweep runs at startup after a crash).
    h.store
        .cancel_task("parent-turn", h.subagents.now_ms())
        .await
        .unwrap();
    let reaped = h.subagents.reap_orphans().await.unwrap();
    assert_eq!(reaped, 1);
    // Re-running the sweep (startup + observer overlap) is idempotent.
    assert_eq!(h.subagents.reap_orphans().await.unwrap(), 0);
    // The cancel is recorded while the wedged worker still holds the turn.
    let mid = h.status(&child.child_chat_id).await;
    assert_eq!(mid.status, "running");
    assert!(
        mid.error
            .as_deref()
            .is_some_and(|error| error.contains("parent turn settled"))
    );
    // Cooperative: the claim is cancelled and the worker lands it.
    h.hold(false);
    h.release();
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "cancelled" },
        "reaped child settles",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reaped_exclusive_orphan_keeps_its_lease_until_the_turn_settles() {
    let h = Harness::start().await;
    h.hold(true);
    let child = h
        .subagents
        .spawn(&SpawnRequest {
            exclusive_workspace: true,
            ..h.spawn_request("k", "implementer", "write code")
        })
        .await
        .expect("implementer spawn");
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "running" },
        "child running",
    )
    .await;
    h.store
        .cancel_task("parent-turn", h.subagents.now_ms())
        .await
        .unwrap();
    assert_eq!(h.subagents.reap_orphans().await.unwrap(), 1);
    // Cooperative cancel: the wedged child may still be mid-tool, so a
    // competing exclusive writer must stay refused until it settles.
    assert_eq!(
        h.subagents
            .leases()
            .holder(&h.workspace, h.subagents.now_ms())
            .as_deref(),
        Some(child.child_chat_id.as_str()),
        "the orphan reaper released the lease of a still-running child"
    );
    h.hold(false);
    h.release();
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "cancelled" },
        "reaped child settles",
    )
    .await;
    assert!(
        h.subagents
            .leases()
            .holder(&h.workspace, h.subagents.now_ms())
            .is_none(),
        "the settled orphan's lease is released by the terminal projection"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orphan_reaper_cancels_the_live_turn_behind_a_stale_pointer() {
    let h = Harness::start().await;
    h.hold(true);
    let child = h.spawn("k", "researcher", "orphan me").await;
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "running" },
        "child running",
    )
    .await;
    // Crash shape: the registry still names an already-settled turn (a
    // follow-up promotion, or admission before `set_subagent_turn`), while
    // the child session runs another live turn.
    h.store
        .cancel_task("parent-turn", h.subagents.now_ms())
        .await
        .unwrap();
    h.store
        .set_subagent_turn(&child.child_chat_id, "parent-turn")
        .await
        .unwrap();
    assert_eq!(h.subagents.reap_orphans().await.unwrap(), 1);
    h.hold(false);
    h.release();
    wait_until(
        || async {
            let status = h.status(&child.child_chat_id).await.status;
            status == "cancelled" || status == "completed"
        },
        "child settles",
    )
    .await;
    assert_eq!(
        h.status(&child.child_chat_id).await.status,
        "cancelled",
        "the reaper must cancel the live turn, not only the stale pointer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exclusive_child_locks_workspace_writes_for_everyone_else() {
    let h = Harness::start().await;
    h.hold(true);
    let child = h
        .subagents
        .spawn(&SpawnRequest {
            exclusive_workspace: true,
            ..h.spawn_request("k", "implementer", "write code")
        })
        .await
        .expect("implementer spawn");
    wait_until(
        || async {
            h.subagents
                .leases()
                .holder(&h.workspace, h.subagents.now_ms())
                .is_some()
        },
        "lease up",
    )
    .await;

    let guard = SubagentRoleGuard::new(h.subagents.clone());
    let holder_call = guard_context(child.child_chat_id.clone(), "coding", "write_file", "write");
    let foreign_call = guard_context("parent".into(), "coding", "write_file", "write");
    match guard.can_execute(&foreign_call).await.unwrap() {
        ToolGuardVerdict::Refused(reason) => {
            assert!(reason.contains("workspace_locked_by_subagent"));
            assert!(reason.contains(&child.child_chat_id));
        }
        other => {
            let _ = other;
            panic!("lease must block foreign writes");
        }
    }
    // The holder itself is unaffected.
    assert!(matches!(
        guard.can_execute(&holder_call).await.unwrap(),
        ToolGuardVerdict::Allowed
    ));
    // Reads are never lease-blocked.
    let read = guard_context("parent".into(), "coding", "read_file", "read");
    assert!(matches!(
        guard.can_execute(&read).await.unwrap(),
        ToolGuardVerdict::Allowed
    ));
    h.hold(false);
    h.release();
    h.await_all().await;
    // Terminal settle frees the lease for the parent.
    let verdict = guard.can_execute(&foreign_call).await.unwrap();
    assert!(matches!(verdict, ToolGuardVerdict::Allowed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn role_ceilings_hide_and_refuse_forbidden_tools() {
    let h = Harness::start().await;
    let child = h.spawn("k", "reviewer", "review only").await;

    let guard = SubagentRoleGuard::new(h.subagents.clone());
    let write = guard_context(child.child_chat_id.clone(), "coding", "write_file", "write");
    assert!(
        !guard.is_visible(&write).await.unwrap(),
        "hidden from the model"
    );
    match guard.can_execute(&write).await.unwrap() {
        ToolGuardVerdict::Refused(reason) => assert!(reason.contains("role_read_only")),
        other => {
            let _ = other;
            panic!("read-only child must be refused");
        }
    }
    let spawn_tool = guard_context(
        child.child_chat_id.clone(),
        "subagent",
        "subagent_spawn",
        "write",
    );
    match guard.can_execute(&spawn_tool).await.unwrap() {
        ToolGuardVerdict::Refused(reason) => assert!(reason.contains("role_read_only")),
        other => {
            let _ = other;
            panic!("reviewer may not delegate");
        }
    }
    // Non-subagent sessions are pass-through.
    let parent_call = guard_context("parent".into(), "coding", "write_file", "write");
    assert!(guard.is_visible(&parent_call).await.unwrap());
    h.await_all().await;
}

/// Reconfigure one role in place, the way a hot config edit would.
fn edit_role(h: &Harness, role: &str, edit: impl FnOnce(&mut RoleSpec)) {
    let mut config = h.subagents.config();
    edit(config.roles.get_mut(role).expect("role"));
    h.subagents.configure(config);
}

async fn allowed(
    guard: &SubagentRoleGuard,
    chat: &str,
    ns: &str,
    tool: &str,
    effect: &str,
) -> bool {
    matches!(
        guard
            .can_execute(&guard_context(chat.to_owned(), ns, tool, effect))
            .await
            .unwrap(),
        ToolGuardVerdict::Allowed
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn widening_a_role_never_widens_a_live_child() {
    let h = Harness::start().await;
    let old = h.spawn("old", "reviewer", "review only").await;
    let guard = SubagentRoleGuard::new(h.subagents.clone());
    assert!(!allowed(&guard, &old.child_chat_id, "coding", "write_file", "write").await);

    edit_role(&h, "reviewer", |role| {
        role.read_only = false;
        role.can_spawn = true;
        role.allowed_tools = vec!["*".into()];
    });
    assert!(
        !allowed(&guard, &old.child_chat_id, "coding", "write_file", "write").await,
        "live child keeps its spawn-time ceiling"
    );
    assert!(
        !allowed(
            &guard,
            &old.child_chat_id,
            "subagent",
            "subagent_spawn",
            "write"
        )
        .await,
        "live child may not gain delegation"
    );
    // A child spawned AFTER the edit gets the new authority.
    let fresh = h.spawn("new", "reviewer", "now allowed").await;
    assert!(
        allowed(
            &guard,
            &fresh.child_chat_id,
            "coding",
            "write_file",
            "write"
        )
        .await
    );
    let view = h.status(&fresh.child_chat_id).await;
    assert_eq!(view.role_digest.as_ref().map(String::len), Some(64));
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restricting_or_removing_a_role_applies_to_live_children() {
    let h = Harness::start().await;
    let child = h.spawn("k", "reviewer", "review only").await;
    let guard = SubagentRoleGuard::new(h.subagents.clone());
    assert!(
        allowed(
            &guard,
            &child.child_chat_id,
            "coding",
            "code_search",
            "read"
        )
        .await
    );

    edit_role(&h, "reviewer", |role| {
        role.allowed_tools
            .retain(|tool| tool != "coding:code_search");
    });
    assert!(
        !allowed(
            &guard,
            &child.child_chat_id,
            "coding",
            "code_search",
            "read"
        )
        .await,
        "removed tool is gone immediately"
    );
    assert!(allowed(&guard, &child.child_chat_id, "coding", "read_file", "read").await);

    edit_role(&h, "reviewer", |role| role.allowed_tools.clear());
    assert!(!allowed(&guard, &child.child_chat_id, "coding", "read_file", "read").await);
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unconfigured_custom_role_and_legacy_children_fail_closed_or_follow_config() {
    let mut cfg = config();
    cfg.roles = agentkit_host::default_role_specs();
    cfg.roles.insert(
        "custom".into(),
        RoleSpec {
            id: "custom".into(),
            allowed_tools: vec!["coding:read_file".into()],
            ..Default::default()
        },
    );
    let h = Harness::start_with(cfg, None, None).await;
    let custom = h.spawn("c", "custom", "custom work").await;
    let guard = SubagentRoleGuard::new(h.subagents.clone());
    assert!(allowed(&guard, &custom.child_chat_id, "coding", "read_file", "read").await);
    let mut config = h.subagents.config();
    config.roles.remove("custom");
    h.subagents.configure(config);
    assert!(
        !allowed(&guard, &custom.child_chat_id, "coding", "read_file", "read").await,
        "a deleted role denies every tool"
    );

    // Legacy child: no snapshot in its metadata, so it follows the config.
    h.service
        .create_session(&CreateSessionInput {
            session_id: Some("legacy".into()),
            title: None,
            profile_id: "coding".into(),
            profile_digest: None,
            reasoning_effort: None,
            content_selection: None,
            provider_id: None,
            model: Some("model".into()),
            workspace: None,
            context: None,
            skills: None,
            turn_kind: None,
            execution_mode: None,
            metadata: Some(json!({"agentkit.subagent": {"role": "reviewer"}})),
        })
        .await
        .unwrap();
    h.store
        .register_subagent_if_under(
            RegisterSubagent {
                child_chat_id: "legacy".into(),
                parent_chat_id: "parent".into(),
                parent_task_id: Some("parent-turn".into()),
                role: "reviewer".into(),
                depth: 1,
                prompt: "p".into(),
                model: None,
                turn_task_id: None,
                now: 0,
            },
            8,
            8,
        )
        .await
        .unwrap();
    assert!(allowed(&guard, "legacy", "coding", "read_file", "read").await);
    edit_role(&h, "reviewer", |role| role.allowed_tools = vec!["*".into()]);
    assert!(
        allowed(&guard, "legacy", "coding", "write_file", "write").await,
        "legacy children keep tracking the live role"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permission_inheritance_pins_read_only_and_copies_parent_mode() {
    let h = Harness::start().await;
    let reader = h.spawn("r", "reviewer", "read the code").await;
    let writer = h.spawn("w", "implementer", "change the code").await;

    // Read-only roles are pinned to read_only no matter what the parent has.
    let mode = h
        .service
        .get_execution(&reader.child_chat_id)
        .await
        .unwrap()
        .and_then(|state| state.get("mode").and_then(Value::as_str).map(str::to_owned));
    assert_eq!(mode.as_deref(), Some("read_only"));

    // Writing roles inherit the parent's granted mode — never exceed it.
    let mode = h
        .service
        .get_execution(&writer.child_chat_id)
        .await
        .unwrap()
        .and_then(|state| state.get("mode").and_then(Value::as_str).map(str::to_owned));
    assert_eq!(mode.as_deref(), Some("read_write"));
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writer_children_require_a_parent_workspace() {
    let h = Harness::start().await;
    // A workspace-less parent cannot authorize writing children.
    h.service
        .create_session(&CreateSessionInput {
            session_id: Some("parent2".into()),
            title: None,
            profile_id: "coding".into(),
            profile_digest: None,
            reasoning_effort: None,
            content_selection: None,
            provider_id: None,
            model: Some("model".into()),
            workspace: None,
            context: None,
            skills: None,
            turn_kind: None,
            execution_mode: None,
            metadata: None,
        })
        .await
        .unwrap();
    h.store
        .submit(SubmitTask {
            task_id: "parent2-turn".into(),
            kind: "parent.hold".into(),
            scope_id: "parent2".into(),
            payload: json!({}),
            priority: 0,
            now: 0,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .unwrap();
    let error = h
        .subagents
        .spawn(&SpawnRequest {
            parent_chat_id: "parent2".into(),
            parent_task_id: "parent2-turn".into(),
            ..h.spawn_request("k", "implementer", "write")
        })
        .await
        .expect_err("no workspace for a writing role");
    assert!(error.to_string().contains("workspace_required"));
    // Read-only children are fine without a workspace.
    let spawned = h
        .subagents
        .spawn(&SpawnRequest {
            parent_chat_id: "parent2".into(),
            parent_task_id: "parent2-turn".into(),
            ..h.spawn_request("r", "reviewer", "review")
        })
        .await
        .expect("read-only child needs no workspace");
    assert_eq!(spawned.status, "running");
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_role_empty_prompt_and_missing_parent_are_refused() {
    let h = Harness::start().await;
    let error = h
        .subagents
        .spawn(&h.spawn_request("k", "wizard", "magic"))
        .await
        .expect_err("unknown role");
    assert!(error.to_string().contains("unknown_subagent_role"));
    let error = h
        .subagents
        .spawn(&h.spawn_request("k", "reviewer", "   "))
        .await
        .expect_err("empty prompt");
    assert!(error.to_string().contains("prompt must not be empty"));
    // Spawning requires the caller's own live task.
    let error = h
        .subagents
        .spawn(&SpawnRequest {
            parent_task_id: "nonexistent".into(),
            ..h.spawn_request("k", "reviewer", "x")
        })
        .await
        .expect_err("no such parent task");
    assert!(error.to_string().contains("caller's own live turn task"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_recovery_cancels_children_of_settled_parents() {
    let store = SqliteStore::memory(64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: runtime.clone(),
        plan_approval: None,
        wake: None,
    });
    service
        .create_session(&CreateSessionInput {
            session_id: Some("parent".into()),
            title: None,
            profile_id: "coding".into(),
            profile_digest: None,
            reasoning_effort: None,
            content_selection: None,
            provider_id: None,
            model: None,
            workspace: None,
            context: None,
            skills: None,
            turn_kind: None,
            execution_mode: None,
            metadata: None,
        })
        .await
        .unwrap();
    // A previous life left a queued child whose parent already settled.
    store
        .submit(SubmitTask {
            task_id: "dead-parent".into(),
            kind: "parent.hold".into(),
            scope_id: "parent".into(),
            payload: json!({}),
            priority: 0,
            now: 0,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .unwrap();
    store.cancel_task("dead-parent", 1).await.unwrap();
    store
        .register_subagent(agentkit_store::RegisterSubagent {
            child_chat_id: "sub_orphan".into(),
            parent_chat_id: "parent".into(),
            parent_task_id: Some("dead-parent".into()),
            role: "general".into(),
            depth: 1,
            prompt: "left behind".into(),
            model: None,
            now: 0,
            turn_task_id: None,
        })
        .await
        .unwrap();

    let subagents = SubagentService::new(store.clone(), service, runtime, config().effective());
    assert_eq!(subagents.reap_orphans().await.unwrap(), 1);
    assert_eq!(
        store
            .get_subagent("sub_orphan")
            .await
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enabled_delegation_guidance_reaches_every_session_turn() {
    // A plain Chat turn's provider request must carry the delegation
    // guidance while subagents are enabled — this is what makes
    // delegation AUTONOMOUS rather than tool-discovery luck.
    let store = SqliteStore::memory(64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let child = Arc::new(Child {
        gate: Arc::new(Notify::new()),
        hold: AtomicBool::new(false),
        released: Arc::new(AtomicBool::new(true)),
        fail: AtomicBool::new(false),
        requests: Mutex::new(vec![]),
    });
    let subagents = SubagentService::new(
        store.clone(),
        AgentSessionService::new(AgentSessionServiceDeps {
            store: store.clone(),
            runtime: runtime.clone(),
            plan_approval: None,
            wake: None,
        }),
        runtime.clone(),
        config().effective(),
    );
    let options = ChatExecutorOptions {
        actor: json!({"kind": "user"}),
        delegation_guidance: Some(Arc::new(subagents) as Arc<dyn DelegationGuidance>),
        ..Default::default()
    };
    let executor: Arc<dyn TaskExecutor> = Arc::new(ChatExecutor::with_options(
        ChatExecutor::new(
            Arc::clone(&child) as Arc<dyn ChatProvider>,
            Arc::new(ToolRegistry::new()),
        ),
        options,
    ));
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        runtime.clone(),
        HostOptions::default(),
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
    service
        .create_session(&CreateSessionInput {
            session_id: Some("guided".into()),
            title: None,
            profile_id: "chat".into(),
            profile_digest: None,
            reasoning_effort: None,
            content_selection: None,
            provider_id: None,
            model: Some("model".into()),
            workspace: None,
            context: None,
            skills: None,
            turn_kind: None,
            execution_mode: None,
            metadata: None,
        })
        .await
        .unwrap();
    host.recover().await.unwrap();
    host.start_worker();
    service
        .follow_up(
            "guided",
            &FollowUpInput { frozen_payload: None,
                content: Value::String("hello".into()),
                input_id: Some("g1".into()),
            },
        )
        .await
        .unwrap();
    wait_until(
        || async {
            child
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|text| text.contains("subagent_spawn"))
        },
        "delegation guidance reached the provider",
    )
    .await;
}

fn tool_definition(name: &str, effect: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        version: "1.0.0".into(),
        effect: effect.into(),
        capability: "test".into(),
        description: String::new(),
        input_schema: json!({"type": "object"}),
        output_schema: None,
        timeout_ms: None,
    }
}

fn contribution_context(chat_id: Option<String>) -> ToolContributionContext {
    ToolContributionContext {
        chat_id,
        run_id: "r".into(),
        scope_id: None,
        actor: Value::Null,
        actor_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: CancellationToken::new(),
        has_primary_binding: false,
        timeout_ms: 30_000,
        durable: None,
    }
}

fn guard_context(chat_id: String, namespace: &str, tool: &str, effect: &str) -> ToolGuardContext {
    ToolGuardContext {
        contribution: contribution_context(Some(chat_id)),
        namespace: namespace.into(),
        tool: tool_definition(tool, effect),
        arguments: None,
    }
}

// ── Worker-pool starvation guard ───────────────────────────────────────────

/// Scripts whole parent turns: a parent holding a `DELEGATE-*` user message
/// calls `subagent_spawn`, then `subagent_await` with the child id taken
/// from the spawn result, then answers. Child turns answer immediately.
#[derive(Default)]
struct Delegating {
    calls: Mutex<HashMap<String, usize>>,
}

fn one(event: ProviderEvent) -> ProviderStream {
    Box::pin(futures::stream::iter([Ok(event)]))
}

fn tool_call(name: &str, arguments: Value) -> ProviderStream {
    one(ProviderEvent::Completed {
        content: "".into(),
        finish_reason: "tool_calls".into(),
        tool_calls: vec![ToolCall {
            id: format!("call-{name}"),
            name: name.to_owned(),
            arguments_json: arguments.to_string(),
        }],
    })
}

fn extract_child_id(text: &str) -> Option<String> {
    // The tool result reaches the next provider request as JSON-encoded
    // message bodies (possibly re-encoded again inside the envelope), so
    // quote escaping stacks. Child ids are hex — dropping backslashes is
    // safe here.
    let flat = text.replace('\\', "");
    let rest = flat.split_once("\"childChatId\":\"")?.1;
    Some(rest.split('"').next()?.to_owned())
}

#[async_trait]
impl ChatProvider for Delegating {
    fn id(&self) -> &str {
        "delegating-test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let text = input
            .messages
            .iter()
            .map(|message| message.content.to_string())
            .collect::<Vec<_>>()
            .join("|");
        if text.contains("delegated task") {
            return one(ProviderEvent::Completed {
                content: "child finished".into(),
                finish_reason: "stop".into(),
                tool_calls: vec![],
            });
        }
        let key = if text.contains("DELEGATE-ALPHA") {
            "alpha"
        } else {
            "beta"
        };
        let turn = {
            let mut calls = self.calls.lock().unwrap();
            let entry = calls.entry(key.to_owned()).or_insert(0);
            *entry += 1;
            *entry
        };
        match turn {
            1 => tool_call(
                "subagent_spawn",
                json!({"role": "general", "prompt": "CHILD-WORK", "spawnKey": format!("k-{key}")}),
            ),
            2 => {
                let Some(child_id) = extract_child_id(&text) else {
                    return one(ProviderEvent::Completed {
                        content: "SPAWN-RESULT-MISSING".into(),
                        finish_reason: "stop".into(),
                        tool_calls: vec![],
                    });
                };
                tool_call("subagent_await", json!({"childIds": [child_id]}))
            }
            _ => one(ProviderEvent::Completed {
                content: "parent done".into(),
                finish_reason: "stop".into(),
                tool_calls: vec![],
            }),
        }
    }
}

/// Two parents delegating from inside their turns through the REAL worker.
/// Parent turns occupy worker slots while `subagent_await` polls, so a pool
/// sized only for the parents starves the children forever: this test fails
/// (children never complete, awaits time out) if the pool cannot host
/// parents plus in-flight children.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_pool_sized_for_delegation_does_not_starve_children() {
    let store = SqliteStore::memory(64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let provider = Arc::new(Delegating::default());

    let config = SubagentConfig {
        enabled: true,
        limits: SubagentRoleLimits {
            max_await_ms: 60_000,
            await_poll_ms: 50,
            ..Default::default()
        },
        ..Default::default()
    }
    .effective();
    let child_session = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: runtime.clone(),
        plan_approval: None,
        wake: None,
    }).with_profiles(Arc::new(ProfileRegistry::new(&[json!({"id":"coding","tools":["subagent_spawn","subagent_await","subagent_list","subagent_message","subagent_terminate"]})]).unwrap()));
    let subagents = SubagentService::new(
        store.clone(),
        child_session,
        runtime.clone(),
        config.clone(),
    );
    let executor: Arc<dyn TaskExecutor> = Arc::new(
        ChatExecutor::new(
            Arc::clone(&provider) as Arc<dyn ChatProvider>,
            Arc::new(ToolRegistry::new()),
        )
        .with_options(ChatExecutorOptions {
            tool_contributors: vec![
                Arc::new(SubagentContributor::new(subagents)) as Arc<dyn ToolContributor>
            ],
            ..Default::default()
        }),
    );
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        runtime.clone(),
        HostOptions {
            // Two parents + at least one in-flight child must fit.
            concurrency: 3,
            retry_backoff: RetryBackoff {
                base_ms: 1,
                max_ms: 2,
                jitter_ratio: 0.0,
            },
            ..Default::default()
        },
    )
    .unwrap();
    let session = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: runtime.clone(),
        plan_approval: None,
        wake: Some(host.wake_handle()),
    }).with_profiles(Arc::new(ProfileRegistry::new(&[json!({"id":"coding","tools":["subagent_spawn","subagent_await","subagent_list","subagent_message","subagent_terminate"]})]).unwrap()));
    host.set_session_flush_planner(Arc::new(session.clone()))
        .await;

    let workspace = std::env::temp_dir().join(format!("agentkit-deleg-ws-{}", nonce()));
    std::fs::create_dir_all(&workspace).unwrap();
    for name in ["alpha", "beta"] {
        session
            .create_session(&CreateSessionInput {
                session_id: Some(name.into()),
                title: None,
                profile_id: "coding".into(),
                profile_digest: None,
                reasoning_effort: None,
                content_selection: None,
                provider_id: None,
                model: Some("model".into()),
                workspace: Some(json!({
                    "root": workspace.to_string_lossy(),
                    "trusted": true,
                })),
                context: None,
                skills: None,
                turn_kind: None,
                execution_mode: Some("read_only".into()),
                metadata: None,
            })
            .await
            .unwrap();
    }

    host.start_worker();
    for (name, key) in [("alpha", "a1"), ("beta", "b1")] {
        session
            .follow_up(
                name,
                &FollowUpInput { frozen_payload: None,
                    content: Value::String(format!("DELEGATE-{}", name.to_uppercase())),
                    input_id: Some(key.into()),
                },
            )
            .await
            .unwrap();
    }

    let probe = || async {
        let alpha_tasks = store.list_tasks(Some("alpha".into())).await.unwrap();
        let beta_tasks = store.list_tasks(Some("beta".into())).await.unwrap();
        let alpha_children = store.list_subagents("alpha").await.unwrap();
        let beta_children = store.list_subagents("beta").await.unwrap();
        (
            !alpha_tasks.is_empty()
                && alpha_tasks
                    .iter()
                    .all(|t| t.status == TaskStatus::Completed)
                && !beta_tasks.is_empty()
                && beta_tasks.iter().all(|t| t.status == TaskStatus::Completed)
                && alpha_children.len() == 1
                && beta_children.len() == 1
                && alpha_children[0].status == "completed"
                && beta_children[0].status == "completed",
            (alpha_tasks, beta_tasks, alpha_children, beta_children),
        )
    };
    let started = std::time::Instant::now();
    loop {
        let (done, (alpha_tasks, beta_tasks, alpha_children, beta_children)) = probe().await;
        if done {
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(12),
            "delegation stalled: alpha_tasks={} beta_tasks={} ac={:?} bc={:?} calls={:?}",
            alpha_tasks
                .iter()
                .map(|t| format!("{}:{:?}", t.task_id, t.status))
                .collect::<Vec<_>>()
                .join(","),
            beta_tasks
                .iter()
                .map(|t| format!("{}:{:?}", t.task_id, t.status))
                .collect::<Vec<_>>()
                .join(","),
            alpha_children
                .iter()
                .map(|r| (
                    r.child_chat_id.clone(),
                    r.status.clone(),
                    r.turn_task_id.clone()
                ))
                .collect::<Vec<_>>(),
            beta_children
                .iter()
                .map(|r| (
                    r.child_chat_id.clone(),
                    r.status.clone(),
                    r.turn_task_id.clone()
                ))
                .collect::<Vec<_>>(),
            *provider.calls.lock().unwrap(),
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let calls = provider.calls.lock().unwrap();
    assert_eq!(calls.get("alpha"), Some(&3));
    assert_eq!(calls.get("beta"), Some(&3));
}

// ── Budget clamps & namespace-collision guard ──────────────────────────────

/// Model-authored budget fields cannot outrun the role ceiling or the
/// absolute caps: a prompt-injected parent must never mint 4-billion-
/// iteration children, and a host-set role bound is a ceiling the request
/// cannot lift.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_authored_budgets_are_clamped_by_role_and_absolute_ceilings() {
    let mut config = SubagentConfig {
        enabled: true,
        ..Default::default()
    }
    .effective();
    config
        .roles
        .get_mut("researcher")
        .expect("builtin researcher")
        .max_iterations = Some(3);
    let h = Harness::start_with(config, None, None).await;

    // Absolute ceilings: no role bound set → the built-in caps clamp.
    let mut request = h.spawn_request("big", "general", "read everything");
    request.max_iterations = Some(4_000_000_000);
    request.max_provider_requests = Some(u64::MAX);
    request.overall_timeout_ms = Some(u64::MAX);
    let child = h.subagents.spawn(&request).await.unwrap();
    let chat = h
        .store
        .get_chat(&child.child_chat_id)
        .await
        .unwrap()
        .expect("child chat");
    let limits = &chat.metadata["agentkit.subagent"]["limits"];
    assert_eq!(limits["maxToolIterations"], json!(64));
    assert_eq!(limits["maxProviderRequests"], json!(200));
    assert_eq!(limits["overallTimeoutMs"], json!(30 * 60 * 1_000));

    // Role ceiling: a host-set bound outranks a larger request.
    let mut request = h.spawn_request("tight", "researcher", "survey");
    request.max_iterations = Some(50);
    let child = h.subagents.spawn(&request).await.unwrap();
    let chat = h
        .store
        .get_chat(&child.child_chat_id)
        .await
        .unwrap()
        .expect("child chat");
    assert_eq!(
        chat.metadata["agentkit.subagent"]["limits"]["maxToolIterations"],
        json!(3)
    );
    h.await_all().await;
}

/// A bare entry in a role's allow-list names a NAME, not a namespace: a
/// same-named tool contributed by another pack with a write effect must be
/// refused — effect-bearing tools require their qualified name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bare_allow_entries_never_admit_same_named_write_tools() {
    let h = Harness::start().await;
    let child = h.spawn("c", "researcher", "survey the tree").await;
    let guard = SubagentRoleGuard::new(h.subagents.clone());

    // The researcher list bare-names `lsp_diagnostics`; the honest
    // read-effect tool is admitted.
    let benign = guard_context(
        child.child_chat_id.clone(),
        "lsp",
        "lsp_diagnostics",
        "read",
    );
    assert!(guard.is_visible(&benign).await.unwrap());

    // A rogue contributor exposing the SAME bare name from another
    // namespace with a write effect inherits nothing from the name.
    let rogue = guard_context(
        child.child_chat_id.clone(),
        "mcp",
        "lsp_diagnostics",
        "write",
    );
    assert!(
        !guard.is_visible(&rogue).await.unwrap(),
        "a write-effect name collision must stay hidden"
    );
    match guard.can_execute(&rogue).await.unwrap() {
        ToolGuardVerdict::Refused(reason) => assert!(reason.contains("role_read_only")),
        other => {
            let _ = other;
            panic!("bare-name collision must be refused");
        }
    }
    h.await_all().await;
}

/// Replaying a spawn the store already holds is not a new admission: it
/// must succeed even with the caps saturated — otherwise a tool-call retry
/// arriving after the roster filled up could never deliver the child's
/// result (the replay errors behind a full roster forever).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_replay_delivers_behind_a_saturated_roster() {
    let h = Harness::start_with(config(), Some(1), None).await;
    h.hold(true);
    let first = h.spawn("k1", "reviewer", "one").await;
    // A NEW spawn hits the global concurrency cap.
    let error = h
        .subagents
        .spawn(&h.spawn_request("k2", "reviewer", "two"))
        .await
        .expect_err("the single slot is taken");
    assert!(error.to_string().contains("subagent_concurrency_limit"));
    // The REPLAY of the admitted spawn sails past the caps: same child,
    // no second execution, no second row.
    let replay = h.spawn("k1", "reviewer", "one").await;
    assert_eq!(first.child_chat_id, replay.child_chat_id);
    assert_eq!(h.subagents.list("parent").await.unwrap().len(), 1);
    h.hold(false);
    h.release();
    h.await_all().await;
}

/// Crash-window adoption: a child session created but not yet registered
/// (crash between `create_session` and the registry row) must be adopted
/// by the replayed spawn — not wedge forever on `input_conflict` — while a
/// colliding chat that is NOT this spawn's stays a hard error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_adopts_a_child_chat_left_by_a_half_finished_create() {
    // Learn the deterministic child id for (parent, parent-turn, "wedge").
    let probe = Harness::start().await;
    let child_id = probe.spawn("wedge", "reviewer", "one").await.child_chat_id;
    probe.await_all().await;

    let h = Harness::start().await;
    h.service
        .create_session(&CreateSessionInput {
            session_id: Some(child_id.clone()),
            title: None,
            profile_id: "coding".into(),
            profile_digest: None,
            reasoning_effort: None,
            content_selection: None,
            provider_id: None,
            model: Some("model".into()),
            workspace: None,
            context: None,
            skills: None,
            turn_kind: None,
            execution_mode: Some("read_only".into()),
            metadata: Some(json!({"agentkit.subagent": {
                "role": "reviewer",
                "parentChatId": "parent",
                "parentTaskId": "parent-turn",
                "depth": 1,
                "spawnKey": "wedge",
                "exclusive": false,
                "limits": {},
            }})),
        })
        .await
        .unwrap();
    // No registry row exists — the replay must adopt the chat and run it.
    let spawned = h
        .subagents
        .spawn(&h.spawn_request("wedge", "reviewer", "one"))
        .await
        .expect("the wedged spawn is adopted, not conflicted");
    assert_eq!(spawned.child_chat_id, child_id);
    h.await_all().await;
    assert_eq!(
        h.subagents.status(&child_id).await.unwrap().status,
        "completed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_refuses_to_adopt_a_foreign_child_chat() {
    let probe = Harness::start().await;
    let child_id = probe.spawn("wedge", "reviewer", "one").await.child_chat_id;
    probe.await_all().await;

    let h = Harness::start().await;
    // Same id (same hash inputs would collide), different spawn identity.
    h.service
        .create_session(&CreateSessionInput {
            session_id: Some(child_id.clone()),
            title: None,
            profile_id: "coding".into(),
            profile_digest: None,
            reasoning_effort: None,
            content_selection: None,
            provider_id: None,
            model: None,
            workspace: None,
            context: None,
            skills: None,
            turn_kind: None,
            execution_mode: Some("read_only".into()),
            metadata: Some(json!({"agentkit.subagent": {
                "role": "reviewer",
                "parentChatId": "parent",
                "parentTaskId": "parent-turn",
                "depth": 1,
                "spawnKey": "somebody-elses-key",
                "exclusive": false,
                "limits": {},
            }})),
        })
        .await
        .unwrap();
    let error = h
        .subagents
        .spawn(&h.spawn_request("wedge", "reviewer", "one"))
        .await
        .expect_err("a foreign chat at the child id is never adopted");
    assert!(error.to_string().contains("already in use"), "{error}");
}

/// Lease sustain (both halves): a running exclusive child refreshes its
/// lease (an observation must not let the TTL lapse under a live writer),
/// and once a FOREIGN holder legitimately owns the root, the child's
/// writes are dead letter — the next observation cancels it honestly
/// instead of letting it grind against the guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn running_exclusive_child_sustains_its_lease_or_is_cancelled() {
    let h = Harness::start().await;
    let mut request = h.spawn_request("x", "implementer", "change the code");
    request.exclusive_workspace = true;
    h.hold(true);
    let child = h.subagents.spawn(&request).await.unwrap();

    // Sustain: observing the RUNNING child refreshes the lease — the same
    // holder may always re-admit.
    h.subagents.status(&child.child_chat_id).await.unwrap();
    assert!(
        h.subagents
            .leases()
            .try_acquire(
                Some(h.workspace.as_str()),
                true,
                &child.child_chat_id,
                h.subagents.now_ms(),
                0,
            )
            .is_ok(),
        "the live holder's own refresh must succeed"
    );

    // A foreign holder takes the root (the TTL lapsed and a reaper swept
    // it — modelled by acquiring through the exposed registry).
    assert!(
        h.subagents
            .leases()
            .try_acquire(
                Some(h.workspace.as_str()),
                true,
                "intruder",
                h.subagents.now_ms(),
                60_000,
            )
            .is_ok()
    );
    let view = h.subagents.status(&child.child_chat_id).await.unwrap();
    let reason = view.error.clone().unwrap_or_default();
    assert_eq!(view.status, "cancelled");
    assert!(
        reason.contains("lease_lost: the workspace root is held by child intruder"),
        "{reason:?}"
    );
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hot_disable_keeps_live_children_inside_their_role_ceiling() {
    // Hot-disabling delegation must stop NEW spawns only. A child already
    // mid-turn keeps its role ceiling: the tool guard deliberately does
    // not consult `enabled`, or flipping the switch off would silently
    // widen every live read-only child to the full tool set.
    let h = Harness::start().await;
    h.hold(true);
    let child = h.spawn("k", "reviewer", "review while disabled").await;
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "running" },
        "child running",
    )
    .await;

    let mut disabled = config();
    disabled.enabled = false;
    h.subagents.configure(disabled);

    let guard = SubagentRoleGuard::new(h.subagents.clone());
    let write = guard_context(child.child_chat_id.clone(), "coding", "write_file", "write");
    match guard.can_execute(&write).await.unwrap() {
        ToolGuardVerdict::Refused(reason) => assert!(
            reason.contains("role_read_only"),
            "a disabled service must not relax a live child's ceiling: {reason}"
        ),
        other => {
            let _ = other;
            panic!("live read-only child must stay refused while disabled");
        }
    }
    // New admission is still refused.
    let error = h
        .subagents
        .spawn(&h.spawn_request("late", "reviewer", "too late"))
        .await
        .expect_err("disabled");
    assert!(error.to_string().contains("subagents_disabled"));
    h.subagents.cancel(&child.child_chat_id).await.unwrap();
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lease_keys_canonicalize_root_spellings() {
    // `/ws`, `/ws/` and `/ws/.` all name one directory. The coding lane
    // confines writes against canonicalized roots, so keying leases on
    // raw strings let two differently-spelled sessions each believe they
    // exclusively held the same workspace.
    let h = Harness::start().await;
    let registry = h.subagents.leases();
    let now = h.subagents.now_ms();
    registry
        .try_acquire(
            Some(&format!("{}/", h.workspace)),
            true,
            "sub_a",
            now,
            60_000,
        )
        .expect("first writer admits");
    assert_eq!(
        registry.holder(&h.workspace, now).as_deref(),
        Some("sub_a"),
        "the plain spelling must see the slashed holder"
    );
    assert_eq!(
        registry
            .holder(&format!("{}/.", h.workspace), now)
            .as_deref(),
        Some("sub_a"),
        "a dot-suffixed alias must see the same holder"
    );
    let error = registry
        .try_acquire(Some(&h.workspace), true, "sub_b", now, 60_000)
        .expect_err("alias spelling must not preempt");
    assert_eq!(error, "sub_a");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replayed_queued_row_is_re_admitted_against_the_held_lease() {
    // A crash between registering a child and dispatching its first turn
    // leaves a QUEUED row with no owner. Replaying that spawn must re-run
    // the SAME lease admission as a fresh create: dispatching it unleased
    // beside the current exclusive holder would give two writers to one
    // workspace, and leaving the row QUEUED would let recovery try again.
    let h = Harness::start().await;
    h.hold(true);
    let holder = h
        .subagents
        .spawn(&SpawnRequest {
            exclusive_workspace: true,
            ..h.spawn_request("k1", "implementer", "hold the root")
        })
        .await
        .expect("first exclusive spawn");
    wait_until(
        || async {
            h.subagents
                .leases()
                .holder(&h.workspace, h.subagents.now_ms())
                .as_deref()
                == Some(holder.child_chat_id.as_str())
        },
        "holder lease registered",
    )
    .await;

    // Forge the crash residue directly: a QUEUED, turn-less row for the
    // identity the replayed spawn call will derive.
    let orphan = child_identity("parent", "parent-turn", "k2");
    h.store
        .register_subagent(RegisterSubagent {
            child_chat_id: orphan.clone(),
            parent_chat_id: "parent".into(),
            parent_task_id: Some("parent-turn".into()),
            role: "implementer".into(),
            depth: 1,
            prompt: "late writer".into(),
            model: None,
            turn_task_id: None,
            now: h.subagents.now_ms(),
        })
        .await
        .unwrap();

    let error = h
        .subagents
        .spawn(&SpawnRequest {
            exclusive_workspace: true,
            ..h.spawn_request("k2", "implementer", "late writer")
        })
        .await
        .expect_err("re-drive must respect the held lease");
    assert!(
        error.to_string().contains("workspace_busy"),
        "refusal names the conflict: {error}"
    );
    let row = h.subagents.status(&orphan).await.unwrap();
    assert_eq!(
        row.status, "cancelled",
        "the refused re-drive must not linger QUEUED for recovery to re-dispatch"
    );
    h.subagents.cancel(&holder.child_chat_id).await.unwrap();
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn role_provider_and_model_overrides_reach_the_child_session() {
    // Per-role model selection is only real if it lands in the child's
    // durable descriptor: the executor binds the session descriptor at
    // turn time, so a silently dropped override would run the role on
    // the parent's (possibly far more expensive) model binding.
    let mut cfg = config();
    cfg.roles = BTreeMap::from([(
        "auditor".to_owned(),
        RoleSpec {
            id: "auditor".into(),
            description: "reads with a cheap model".into(),
            prompt: "audit only".into(),
            allowed_tools: vec!["coding:read_file".into()],
            read_only: true,
            can_spawn: false,
            model: Some("role-model".into()),
            provider_id: Some("role-provider".into()),
            max_iterations: None,
            capability: None,
        },
    )]);
    let workspace = std::env::temp_dir().join(format!("agentkit-subagent-ws-{}", nonce()));
    std::fs::create_dir_all(&workspace).unwrap();
    let h = Harness::build(cfg, workspace.to_string_lossy().into_owned()).await;

    let child = h.spawn("k", "auditor", "audit the tree").await;
    let chat = h
        .store
        .get_chat(&child.child_chat_id)
        .await
        .unwrap()
        .expect("child chat");
    let descriptor = read_session_descriptor(&chat.metadata).expect("child descriptor");
    assert_eq!(descriptor["providerId"], "role-provider");
    assert_eq!(descriptor["model"], "role-model");
    // The row carries the effective model too, so `subagent_list` shows
    // what is really running.
    assert_eq!(
        h.status(&child.child_chat_id).await.model.as_deref(),
        Some("role-model")
    );
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_descriptor_carries_the_parent_workspace() {
    // Workspace-bound lanes (coding, LSP) decide by the descriptor
    // workspace, so a child must name exactly its parent's.
    let h = Harness::start().await;
    let child = h.spawn("k", "implementer", "write").await;
    let chat = h
        .store
        .get_chat(&child.child_chat_id)
        .await
        .unwrap()
        .expect("child chat");
    let descriptor = read_session_descriptor(&chat.metadata).expect("child descriptor");
    assert_eq!(descriptor["workspace"]["root"], json!(h.workspace));
    h.await_all().await;
}

fn pinned_model_harness_config() -> SubagentConfig {
    let mut cfg = config();
    cfg.roles = BTreeMap::from([(
        "auditor".to_owned(),
        RoleSpec {
            id: "auditor".into(),
            description: "pinned".into(),
            prompt: "audit only".into(),
            allowed_tools: vec!["coding:read_file".into()],
            read_only: true,
            can_spawn: false,
            model: Some("role-model".into()),
            provider_id: None,
            max_iterations: None,
            capability: None,
        },
    )]);
    cfg
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_cannot_override_a_role_pinned_model() {
    let workspace = std::env::temp_dir().join(format!("agentkit-subagent-ws-{}", nonce()));
    std::fs::create_dir_all(&workspace).unwrap();
    let h = Harness::build(
        pinned_model_harness_config(),
        workspace.to_string_lossy().into_owned(),
    )
    .await;
    let error = h
        .subagents
        .spawn(&SpawnRequest {
            model: Some("expensive-model".into()),
            ..h.spawn_request("k1", "auditor", "audit")
        })
        .await
        .expect_err("pinned role model");
    assert!(error.to_string().contains("model_not_allowed"), "{error}");
    assert!(h.subagents.list("parent").await.unwrap().is_empty());
    // The pinned model itself (and no model at all) stay allowed.
    h.subagents
        .spawn(&SpawnRequest {
            model: Some("role-model".into()),
            ..h.spawn_request("k2", "auditor", "audit")
        })
        .await
        .expect("equal model");
    h.spawn("k3", "auditor", "audit").await;
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_of_a_cancelled_child_obeys_the_fan_out_caps() {
    let h = Harness::start_with(config(), Some(2), Some(8)).await;
    h.hold(true);
    let a = h.spawn("a", "researcher", "one").await;
    let b = h.spawn("b", "researcher", "two").await;
    h.subagents.cancel(&a.child_chat_id).await.unwrap();
    h.subagents.cancel(&b.child_chat_id).await.unwrap();
    h.hold(false);
    h.release();
    for id in [&a.child_chat_id, &b.child_chat_id] {
        wait_until(
            || async { h.status(id).await.status == "cancelled" },
            "wave one cancelled",
        )
        .await;
    }
    // Second wave fills the cap again; the gate must hold the new children.
    h.child.released.store(false, Ordering::SeqCst);
    h.hold(true);
    h.spawn("c", "researcher", "three").await;
    h.spawn("d", "researcher", "four").await;
    let error = h
        .subagents
        .resume(&a.child_chat_id, "r1")
        .await
        .expect_err("resume past the concurrency cap");
    assert!(
        error.to_string().contains("subagent_concurrency_limit"),
        "{error}"
    );
    assert_eq!(h.status(&a.child_chat_id).await.status, "cancelled");
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_rebinds_the_row_to_the_callers_live_parent_task() {
    let h = Harness::start().await;
    h.child.fail.store(true, Ordering::SeqCst);
    let child = h.spawn("k", "reviewer", "doomed").await;
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "failed" },
        "child failed",
    )
    .await;
    h.child.fail.store(false, Ordering::SeqCst);
    // A caller whose task is not live in this chat is refused.
    let error = h
        .subagents
        .resume_for_parent(&child.child_chat_id, "r0", Some("no-such-task"))
        .await
        .expect_err("foreign parent task");
    assert!(error.to_string().contains("live turn task"), "{error}");
    h.store
        .submit(SubmitTask {
            task_id: "parent-turn-2".into(),
            kind: "parent.hold".into(),
            scope_id: "parent".into(),
            payload: json!({}),
            priority: 0,
            now: h.subagents.now_ms(),
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .unwrap();
    h.subagents
        .resume_for_parent(&child.child_chat_id, "r1", Some("parent-turn-2"))
        .await
        .unwrap();
    let row = h.store.get_subagent(&child.child_chat_id).await.unwrap();
    assert_eq!(
        row.unwrap().parent_task_id.as_deref(),
        Some("parent-turn-2")
    );
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_of_a_completed_exclusive_child_leaves_the_workspace_unlocked() {
    let h = Harness::start().await;
    let child = h
        .subagents
        .spawn(&SpawnRequest {
            exclusive_workspace: true,
            ..h.spawn_request("k", "implementer", "write code")
        })
        .await
        .expect("implementer spawn");
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "completed" },
        "child completed",
    )
    .await;
    let error = h
        .subagents
        .resume(&child.child_chat_id, "r")
        .await
        .expect_err("completed turns do not resume");
    assert!(
        error
            .to_string()
            .contains("only interrupted, failed or cancelled"),
        "{error}"
    );
    assert!(
        h.subagents
            .leases()
            .holder(&h.workspace, h.subagents.now_ms())
            .is_none(),
        "a refused resume must not leave the parent's writes locked out"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_rebuilds_exclusive_leases_from_durable_rows() {
    let h = Harness::start().await;
    h.hold(true);
    let child = h
        .subagents
        .spawn(&SpawnRequest {
            exclusive_workspace: true,
            ..h.spawn_request("k", "implementer", "write code")
        })
        .await
        .expect("implementer spawn");
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "running" },
        "child running",
    )
    .await;
    // A restarted process: same durable store, empty lease table.
    let restarted = SubagentService::new(
        h.store.clone(),
        h.service.clone(),
        Arc::new(SystemRuntime),
        config().effective(),
    );
    assert!(
        restarted
            .leases()
            .holder(&h.workspace, restarted.now_ms())
            .is_none()
    );
    restarted.reap_orphans().await.unwrap();
    assert_eq!(
        restarted
            .leases()
            .holder(&h.workspace, restarted.now_ms())
            .as_deref(),
        Some(child.child_chat_id.as_str()),
        "the live exclusive writer holds its root again after restart"
    );
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_tool_without_a_call_identity_requires_an_explicit_key() {
    let h = Harness::start().await;
    let tools = SubagentContributor::new(h.subagents.clone())
        .contribute(&contribution_context(Some("parent".into())))
        .await
        .unwrap();
    let spawn = tools
        .into_iter()
        .find_map(|contributed| match contributed {
            ContributedTool::Read(tool) if tool.definition().name == "subagent_spawn" => Some(tool),
            _ => None,
        })
        .expect("spawn tool");
    let context = agentkit_core::ToolContext {
        run_id: "parent-turn".into(),
        tool_call_id: None,
        attempt_id: None,
        chat_id: Some("parent".into()),
        scope_id: Some("parent".into()),
        user_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: CancellationToken::new(),
        dispatch_guard: None,
        metadata: Default::default(),
        suspension: None,
    };
    let refused = spawn
        .execute(
            context.clone(),
            json!({"role": "reviewer", "prompt": "audit"}),
        )
        .await
        .unwrap();
    assert!(!refused.ok);
    assert!(
        refused
            .data
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("spawn_key_required")),
        "{refused:?}"
    );
    assert!(h.subagents.list("parent").await.unwrap().is_empty());
    let accepted = spawn
        .execute(
            context,
            json!({"role": "reviewer", "prompt": "audit", "spawnKey": "explicit"}),
        )
        .await
        .unwrap();
    assert!(accepted.ok, "{accepted:?}");
    h.await_all().await;
}

#[test]
fn subagent_mutators_are_flagged_for_receipts_but_reads_are_not() {
    for name in [
        "subagent_spawn",
        "subagent_message",
        "subagent_cancel",
        "subagent_resume",
    ] {
        assert!(is_mutating_subagent_tool(name), "{name}");
    }
    for name in ["subagent_list", "subagent_status", "subagent_await"] {
        assert!(!is_mutating_subagent_tool(name), "{name}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_resumes_revive_the_child_exactly_once() {
    let h = Harness::start().await;
    h.child.fail.store(true, Ordering::SeqCst);
    let child = h
        .subagents
        .spawn(&SpawnRequest {
            exclusive_workspace: true,
            ..h.spawn_request("k", "implementer", "doomed")
        })
        .await
        .expect("spawn");
    wait_until(
        || async { h.status(&child.child_chat_id).await.status == "failed" },
        "child failed",
    )
    .await;
    h.hold(true);
    h.child.fail.store(false, Ordering::SeqCst);
    let (a, b) = tokio::join!(
        h.subagents.resume(&child.child_chat_id, "ra"),
        h.subagents.resume(&child.child_chat_id, "rb"),
    );
    assert_eq!(
        usize::from(a.is_ok()) + usize::from(b.is_ok()),
        1,
        "exactly one resume wins: {a:?} / {b:?}"
    );
    // The loser's guard must not have dropped the winner's lease.
    assert!(h.subagents.leases().is_held_by(
        &h.workspace,
        &child.child_chat_id,
        h.subagents.now_ms()
    ));
    h.hold(false);
    h.release();
    h.await_all().await;
}

fn tree_config(limit: u64) -> SubagentConfig {
    let mut config = config();
    config.limits.max_tree_provider_requests = Some(limit);
    config
}

async fn recorded_reservation(h: &Harness, child_chat_id: &str) -> u64 {
    let chat = h.store.get_chat(child_chat_id).await.unwrap().unwrap();
    chat.metadata["agentkit.subagent"]["limits"]["maxProviderRequests"]
        .as_u64()
        .expect("reservation recorded")
}

fn with_requests(h: &Harness, key: &str, requests: u64) -> SpawnRequest {
    SpawnRequest {
        max_provider_requests: Some(requests),
        ..h.spawn_request(key, "researcher", "work")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_spawns_cannot_over_reserve_the_tree() {
    let h = Harness::start_with(tree_config(100), None, None).await;
    h.hold(true);
    // Each request wants the whole budget: clamping leaves nothing for the loser.
    let (a, b) = (with_requests(&h, "a", 100), with_requests(&h, "b", 100));
    let (first, second) = tokio::join!(h.subagents.spawn(&a), h.subagents.spawn(&b));
    let (ok, err) = match (first, second) {
        (Ok(ok), Err(err)) | (Err(err), Ok(ok)) => (ok, err),
        other => panic!("exactly one spawn must win: {other:?}"),
    };
    assert!(
        err.to_string().contains("subagent_budget_exhausted"),
        "{err}"
    );
    assert_eq!(recorded_reservation(&h, &ok.child_chat_id).await, 100);
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_partial_requests_sum_to_the_limit() {
    let h = Harness::start_with(tree_config(100), None, None).await;
    h.hold(true);
    let (a, b) = (with_requests(&h, "a", 60), with_requests(&h, "b", 60));
    let (first, second) = tokio::join!(h.subagents.spawn(&a), h.subagents.spawn(&b));
    let mut reserved = vec![
        recorded_reservation(&h, &first.unwrap().child_chat_id).await,
        recorded_reservation(&h, &second.unwrap().child_chat_id).await,
    ];
    reserved.sort_unstable();
    assert_eq!(reserved, vec![40, 60]);
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reservation_is_clamped_to_what_the_tree_has_left() {
    let h = Harness::start_with(tree_config(100), None, None).await;
    h.hold(true);
    let first = h
        .subagents
        .spawn(&with_requests(&h, "a", 60))
        .await
        .unwrap();
    let second = h
        .subagents
        .spawn(&with_requests(&h, "b", 60))
        .await
        .unwrap();
    assert_eq!(recorded_reservation(&h, &first.child_chat_id).await, 60);
    assert_eq!(recorded_reservation(&h, &second.child_chat_id).await, 40);
    let error = h
        .subagents
        .spawn(&with_requests(&h, "c", 1))
        .await
        .expect_err("tree fully reserved");
    assert!(error.to_string().contains("subagent_budget_exhausted"));
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settled_child_returns_its_unused_reservation() {
    let h = Harness::start_with(tree_config(100), None, None).await;
    let child = h
        .subagents
        .spawn(&with_requests(&h, "a", 60))
        .await
        .unwrap();
    h.await_all().await;
    let used = h.child.requests.lock().unwrap().len() as u64;
    assert!(used >= 1);
    let next = h
        .subagents
        .spawn(&with_requests(&h, "b", 100))
        .await
        .unwrap();
    assert_eq!(
        recorded_reservation(&h, &next.child_chat_id).await,
        100 - used,
        "only the settled child's actual requests stay charged"
    );
    assert_eq!(recorded_reservation(&h, &child.child_chat_id).await, 60);
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grandchildren_count_against_the_same_root() {
    let h = Harness::start_with(tree_config(100), None, None).await;
    h.hold(true);
    let child = h
        .subagents
        .spawn(&with_requests(&h, "a", 60))
        .await
        .unwrap();
    let child_turn = h.status(&child.child_chat_id).await.turn_task_id.unwrap();
    let grand = h
        .subagents
        .spawn(&SpawnRequest {
            parent_chat_id: child.child_chat_id.clone(),
            parent_task_id: child_turn,
            ..with_requests(&h, "g", 80)
        })
        .await
        .unwrap();
    assert_eq!(recorded_reservation(&h, &grand.child_chat_id).await, 40);
    let chat = h
        .store
        .get_chat(&grand.child_chat_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(chat.metadata["agentkit.subagent"]["rootChatId"], "parent");
    let error = h
        .subagents
        .spawn(&with_requests(&h, "late", 1))
        .await
        .expect_err("root tree exhausted by descendants");
    assert!(error.to_string().contains("subagent_budget_exhausted"));
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_that_would_exceed_the_tree_budget_is_refused() {
    let h = Harness::start_with(tree_config(100), None, None).await;
    h.child.fail.store(true, Ordering::SeqCst);
    let failed = h
        .subagents
        .spawn(&with_requests(&h, "a", 60))
        .await
        .unwrap();
    wait_until(
        || async { h.status(&failed.child_chat_id).await.status == "failed" },
        "child failed",
    )
    .await;
    h.child.fail.store(false, Ordering::SeqCst);
    h.hold(true);
    h.subagents
        .spawn(&with_requests(&h, "b", 60))
        .await
        .unwrap();
    let error = h
        .subagents
        .resume(&failed.child_chat_id, "r1")
        .await
        .expect_err("60 + 60 exceeds 100 once the failed child is active again");
    assert!(error.to_string().contains("subagent_budget_exhausted"));
    assert_eq!(h.status(&failed.child_chat_id).await.status, "failed");
    h.hold(false);
    h.release();
    h.await_all().await;
}

/// A resumed child's earlier turns stay charged: reservation-only charging
/// would let a 50-request child spend 50, resume, and spend 50 more while
/// the tree counted 50.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_keeps_the_earlier_turns_spend_charged() {
    let h = Harness::start_with(tree_config(100), None, None).await;
    h.child.fail.store(true, Ordering::SeqCst);
    let failed = h
        .subagents
        .spawn(&with_requests(&h, "a", 50))
        .await
        .unwrap();
    wait_until(
        || async { h.status(&failed.child_chat_id).await.status == "failed" },
        "child failed",
    )
    .await;
    h.child.fail.store(false, Ordering::SeqCst);
    h.hold(true);
    let other = h
        .subagents
        .spawn(&with_requests(&h, "b", 50))
        .await
        .unwrap();
    assert_eq!(recorded_reservation(&h, &other.child_chat_id).await, 50);
    let error = h
        .subagents
        .resume(&failed.child_chat_id, "r1")
        .await
        .expect_err("the failed turn's spend plus a fresh 50 exceeds 100");
    assert!(error.to_string().contains("subagent_budget_exhausted"));
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarted_service_still_enforces_the_tree_budget() {
    let h = Harness::start_with(tree_config(100), None, None).await;
    h.hold(true);
    h.subagents
        .spawn(&with_requests(&h, "a", 70))
        .await
        .unwrap();
    let restarted = SubagentService::new(
        h.store.clone(),
        h.service.clone(),
        Arc::new(SystemRuntime),
        tree_config(100).effective(),
    );
    let next = restarted.spawn(&with_requests(&h, "b", 70)).await.unwrap();
    assert_eq!(recorded_reservation(&h, &next.child_chat_id).await, 30);
    h.hold(false);
    h.release();
    h.await_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unset_tree_budget_leaves_reservations_untouched() {
    let h = Harness::start_with(config(), Some(50), Some(50)).await;
    h.hold(true);
    for index in 0..4 {
        let child = h
            .subagents
            .spawn(&with_requests(&h, &format!("k{index}"), 150))
            .await
            .unwrap();
        assert_eq!(recorded_reservation(&h, &child.child_chat_id).await, 150);
    }
    h.hold(false);
    h.release();
    h.await_all().await;
}
