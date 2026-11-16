//! Parked wait (delegation spec, DEL-07 / DEL-08): a parent that awaits
//! children parks durably in `WaitingChild`, holds no engine slot, and wakes
//! on the first of: awaited children settled, the durable deadline, an owner
//! message, with cancellation cascading as before. Every scenario runs the
//! parent and its children through the real host worker.

use agentkit_agents::*;
use agentkit_chat::*;
use agentkit_core::{
    ChatProvider, ChatRequest, ProviderEvent, ProviderStream, ToolCall, ToolRegistry,
};
use agentkit_engine::{RecoveryMode, RetryBackoff, SharedRuntime, SystemRuntime, TaskExecutor};
use agentkit_governance::ToolContributor;
use agentkit_sessions::*;
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

/// Parent turns are scripted from the request text alone (no call counter),
/// so a resumed turn in a restarted process continues instead of starting
/// over: no await result yet -> spawn three children; children spawned ->
/// await them; await result present -> final answer. Children answer when
/// their per-index hold is released.
struct Scripted {
    holds: Arc<[AtomicBool; 3]>,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    spawn_turns: AtomicUsize,
    final_turns: AtomicUsize,
    parent_texts: Mutex<Vec<String>>,
}

impl Scripted {
    fn new(held: [bool; 3]) -> Arc<Self> {
        Arc::new(Self {
            holds: Arc::new(held.map(AtomicBool::new)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            spawn_turns: AtomicUsize::new(0),
            final_turns: AtomicUsize::new(0),
            parent_texts: Mutex::new(vec![]),
        })
    }

    fn release(&self, index: usize) {
        self.holds[index].store(false, Ordering::SeqCst);
    }

    fn release_all(&self) {
        for index in 0..3 {
            self.release(index);
        }
    }

    fn last_parent_text(&self) -> String {
        self.parent_texts
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

fn one(event: ProviderEvent) -> ProviderStream {
    Box::pin(futures::stream::iter([Ok(event)]))
}

fn tool_calls(calls: Vec<ToolCall>) -> ProviderStream {
    one(ProviderEvent::Completed {
        content: "".into(),
        finish_reason: "tool_calls".into(),
        tool_calls: calls,
    })
}

fn child_ids(text: &str) -> Vec<String> {
    // Tool results reach the provider JSON-encoded, possibly re-encoded:
    // dropping backslashes is safe because child ids are hex.
    let flat = text.replace('\\', "");
    let mut ids: Vec<String> = vec![];
    for part in flat.split("\"childChatId\":\"").skip(1) {
        if let Some(id) = part.split('"').next()
            && !ids.iter().any(|known| known == id)
        {
            ids.push(id.to_owned());
        }
    }
    ids
}

#[async_trait]
impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "parked-wait-test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let text = input
            .messages
            .iter()
            .map(|message| message.content.to_string())
            .collect::<Vec<_>>()
            .join("|");
        if text.contains("delegated task") {
            let index = (0usize..3)
                .find(|index| text.contains(&format!("CHILD-WORK-{index}")))
                .unwrap_or(0);
            let holds = Arc::clone(&self.holds);
            let in_flight = Arc::clone(&self.in_flight);
            let peak = Arc::clone(&self.peak);
            return Box::pin(futures::stream::once(async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                while holds[index].load(Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(ProviderEvent::Completed {
                    content: "child finished".into(),
                    finish_reason: "stop".into(),
                    tool_calls: vec![],
                })
            }));
        }
        self.parent_texts.lock().unwrap().push(text.clone());
        let flat = text.replace('\\', "");
        if flat.contains("\"wake\":\"") {
            self.final_turns.fetch_add(1, Ordering::SeqCst);
            return one(ProviderEvent::Completed {
                content: "parent done".into(),
                finish_reason: "stop".into(),
                tool_calls: vec![],
            });
        }
        let ids = child_ids(&text);
        if ids.is_empty() {
            self.spawn_turns.fetch_add(1, Ordering::SeqCst);
            return tool_calls(
                (0..3)
                    .map(|index| ToolCall {
                        id: format!("call-spawn-{index}"),
                        name: "subagent_spawn".into(),
                        arguments_json: json!({
                            "role": "general",
                            "title": format!("child {index}"),
                            "objective": format!("CHILD-WORK-{index}"),
                            "output_format": "summary",
                            "scope": {"stop_when": "done"},
                        })
                        .to_string(),
                    })
                    .collect(),
            );
        }
        let mut arguments = json!({ "child_ids": ids });
        if text.contains("PARK-ANY") {
            arguments["mode"] = json!("any");
        }
        if text.contains("PARK-TIMEOUT") {
            arguments["timeout"] = json!(300);
        }
        tool_calls(vec![ToolCall {
            id: "call-await".into(),
            name: "subagent_await".into(),
            arguments_json: arguments.to_string(),
        }])
    }
}

struct Rig {
    store: SqliteStore,
    session: AgentSessionService<SqliteStore>,
    subagents: SubagentService,
    worker: WorkerHandle,
}

const PROFILE_TOOLS: [&str; 5] = [
    "subagent_spawn",
    "subagent_await",
    "subagent_list",
    "subagent_message",
    "subagent_terminate",
];

async fn rig(store: SqliteStore, pool: usize, provider: Arc<Scripted>) -> Rig {
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let config = SubagentConfig {
        enabled: true,
        limits: SubagentRoleLimits {
            max_await_ms: 60_000,
            await_poll_ms: 20,
            warm_hold_ms: 0,
            ..Default::default()
        },
        ..Default::default()
    }
    .effective();
    let profiles =
        Arc::new(ProfileRegistry::new(&[json!({"id": "coding", "tools": PROFILE_TOOLS})]).unwrap());
    let child_session = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: runtime.clone(),
        plan_approval: None,
        wake: None,
    })
    .with_profiles(profiles.clone());
    let subagents = SubagentService::new(store.clone(), child_session, runtime.clone(), config);
    let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(
        ChatExecutor::new(
            Arc::clone(&provider) as Arc<dyn ChatProvider>,
            Arc::new(ToolRegistry::new()),
        )
        .with_options(ChatExecutorOptions {
            tool_contributors: vec![
                Arc::new(SubagentContributor::new(subagents.clone())) as Arc<dyn ToolContributor>
            ],
            ..Default::default()
        }),
    );
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        runtime.clone(),
        HostOptions {
            concurrency: pool,
            lease_ttl_ms: 1_000,
            recovery: RecoveryMode::Automatic,
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
        runtime,
        plan_approval: None,
        wake: Some(host.wake_handle()),
    })
    .with_profiles(profiles);
    host.set_session_flush_planner(Arc::new(session.clone()))
        .await;
    host.recover().await.unwrap();
    let worker = host.start_worker();
    Rig {
        store,
        session,
        subagents,
        worker,
    }
}

impl Rig {
    async fn create_parent(&self, workspace: &str) {
        self.session
            .create_session(&CreateSessionInput {
                session_id: Some("p".into()),
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
                execution_mode: Some("read_only".into()),
                metadata: None,
            })
            .await
            .unwrap();
    }

    async fn start_parent(&self, marker: &str) {
        self.session
            .follow_up(
                "p",
                &FollowUpInput {
                    frozen_payload: None,
                    content: Value::String(marker.to_owned()),
                    input_id: Some("in-1".into()),
                },
            )
            .await
            .unwrap();
    }

    async fn parent_turn(&self) -> agentkit_store::TaskRecord {
        wait_until(
            || async {
                !self
                    .store
                    .list_tasks(Some("p".into()))
                    .await
                    .unwrap()
                    .is_empty()
            },
            "parent turn admitted",
        )
        .await;
        self.store
            .list_tasks(Some("p".into()))
            .await
            .unwrap()
            .remove(0)
    }

    async fn parent_status(&self, turn: &str) -> TaskStatus {
        self.store.get_task(turn).await.unwrap().unwrap().status
    }

    async fn wait_parked(&self, turn: &str) {
        wait_until(
            || async { self.parent_status(turn).await == TaskStatus::WaitingChild },
            "parent parked in WaitingChild",
        )
        .await;
    }

    async fn wait_completed(&self, turn: &str) {
        wait_until(
            || async { self.parent_status(turn).await == TaskStatus::Completed },
            "parent completed",
        )
        .await;
    }

    async fn wake_reason(&self, turn: &str) -> Option<String> {
        self.store
            .events(turn, None, 500)
            .await
            .unwrap()
            .into_iter()
            .find(|record| record.event["type"] == "parent.woken")
            .and_then(|record| record.event["data"]["reason"].as_str().map(str::to_owned))
    }

    async fn child_statuses(&self) -> Vec<String> {
        self.store
            .list_subagents("p")
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.status)
            .collect()
    }
}

async fn wait_until<F, Fut>(mut probe: F, what: &str)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !probe().await {
        assert!(std::time::Instant::now() < deadline, "{what} never held");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn workspace() -> String {
    let path = std::env::temp_dir().join(format!(
        "agentkit-parked-ws-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path.to_string_lossy().into_owned()
}

/// DEL-07: with two slots and three children, the parked parent holds no
/// slot — both slots run children at once (a polling parent would leave one)
/// — and all three finish and reach the parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parked_parent_frees_its_slot_so_pool_two_runs_children_in_parallel() {
    let provider = Scripted::new([true, true, true]);
    let rig = rig(SqliteStore::memory(64).unwrap(), 2, provider.clone()).await;
    rig.create_parent(&workspace()).await;
    rig.start_parent("PARK-ALL").await;
    let turn = rig.parent_turn().await.task_id;

    rig.wait_parked(&turn).await;
    wait_until(
        || async { provider.in_flight.load(Ordering::SeqCst) == 2 },
        "two children in flight beside the parked parent",
    )
    .await;
    // The pool has two slots and the parked parent holds neither: the third
    // child queues, nothing exceeds the pool, the parent stays parked.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(provider.peak.load(Ordering::SeqCst), 2);
    assert_eq!(rig.parent_status(&turn).await, TaskStatus::WaitingChild);

    provider.release_all();
    rig.wait_completed(&turn).await;
    assert_eq!(rig.child_statuses().await, vec!["completed"; 3]);
    assert_eq!(rig.wake_reason(&turn).await.as_deref(), Some("all_settled"));
    let text = provider.last_parent_text().replace('\\', "");
    assert!(text.contains("child finished"), "{text}");
    assert_eq!(provider.spawn_turns.load(Ordering::SeqCst), 1);
    assert_eq!(provider.final_turns.load(Ordering::SeqCst), 1);
}

/// DEL-08: mode `any` wakes on the first settled child while the others
/// still run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn any_mode_wakes_on_the_first_settled_child() {
    let provider = Scripted::new([true, true, true]);
    let rig = rig(SqliteStore::memory(64).unwrap(), 4, provider.clone()).await;
    rig.create_parent(&workspace()).await;
    rig.start_parent("PARK-ANY").await;
    let turn = rig.parent_turn().await.task_id;
    rig.wait_parked(&turn).await;
    provider.release(1);
    rig.wait_completed(&turn).await;
    assert_eq!(
        rig.wake_reason(&turn).await.as_deref(),
        Some("first_settled")
    );
    let settled = rig
        .child_statuses()
        .await
        .into_iter()
        .filter(|status| status == "completed")
        .count();
    assert_eq!(settled, 1, "only the first child had settled");
    provider.release_all();
}

/// DEL-08: the durable deadline wakes the parent with the children still
/// running, and the result reports their current status.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_deadline_wakes_the_parent_with_partial_results() {
    let provider = Scripted::new([true, true, true]);
    let rig = rig(SqliteStore::memory(64).unwrap(), 4, provider.clone()).await;
    rig.create_parent(&workspace()).await;
    rig.start_parent("PARK-TIMEOUT").await;
    let turn = rig.parent_turn().await.task_id;
    rig.wait_completed(&turn).await;
    assert_eq!(rig.wake_reason(&turn).await.as_deref(), Some("timeout"));
    let text = provider.last_parent_text().replace('\\', "");
    assert!(text.contains("\"timedOut\":true"), "{text}");
    assert!(
        rig.child_statuses()
            .await
            .iter()
            .all(|status| status != "completed")
    );
    provider.release_all();
}

/// DEL-08: an owner message in the parent chat wakes the parked parent; the
/// message itself stays queued for the resumed turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owner_message_wakes_the_parked_parent() {
    let provider = Scripted::new([true, true, true]);
    let rig = rig(SqliteStore::memory(64).unwrap(), 4, provider.clone()).await;
    rig.create_parent(&workspace()).await;
    rig.start_parent("PARK-ALL").await;
    let turn = rig.parent_turn().await.task_id;
    rig.wait_parked(&turn).await;
    rig.session
        .steer(
            "p",
            &SteerInput {
                content: Value::String("owner here".into()),
                input_id: Some("owner-1".into()),
                turn_id: None,
                on_settled: None,
                frozen_payload: None,
            },
        )
        .await
        .unwrap();
    wait_until(
        || async { rig.wake_reason(&turn).await.is_some() },
        "owner message woke the parent",
    )
    .await;
    assert_eq!(
        rig.wake_reason(&turn).await.as_deref(),
        Some("owner_message")
    );
    provider.release_all();
}

/// DEL-08: cancelling the parked parent settles it without a resume and the
/// cancel cascade still reaches its children.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_the_parked_parent_cascades_to_its_children() {
    let provider = Scripted::new([true, true, true]);
    let rig = rig(SqliteStore::memory(64).unwrap(), 4, provider.clone()).await;
    rig.create_parent(&workspace()).await;
    rig.start_parent("PARK-ALL").await;
    let turn = rig.parent_turn().await.task_id;
    rig.wait_parked(&turn).await;
    rig.session.cancel("p", &turn).await.unwrap();
    assert_eq!(rig.parent_status(&turn).await, TaskStatus::Cancelled);
    wait_until(
        || async { rig.child_statuses().await == vec!["cancelled"; 3] },
        "children cancelled by the cascade",
    )
    .await;
    assert!(rig.wake_reason(&turn).await.is_none());
    assert_eq!(provider.final_turns.load(Ordering::SeqCst), 0);
    provider.release_all();
}

/// DEL-07: a restart in the middle of the wait leaves a waiting parent that
/// is re-evaluated, never re-run: the new process spawns nothing, the
/// children finish, and the same parent turn resumes to its final answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_mid_wait_resumes_the_parked_parent_without_rerunning_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("parked.db");
    let workspace = workspace();
    let turn = {
        let provider = Scripted::new([true, true, true]);
        let first = rig(SqliteStore::open(&path, 64).unwrap(), 3, provider.clone()).await;
        first.create_parent(&workspace).await;
        first.start_parent("PARK-RESTART").await;
        let turn = first.parent_turn().await.task_id;
        first.wait_parked(&turn).await;
        wait_until(
            || async { provider.in_flight.load(Ordering::SeqCst) == 3 },
            "three children in flight",
        )
        .await;
        // Process death: runs are dropped with their leases kept.
        first.worker.interrupt_active().await;
        turn
    };
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;

    let reopened = SqliteStore::open(&path, 64).unwrap();
    assert_eq!(
        reopened.get_task(&turn).await.unwrap().unwrap().status,
        TaskStatus::WaitingChild,
        "the parked parent survives the restart as a waiting task"
    );
    let provider = Scripted::new([false, false, false]);
    let second = rig(reopened, 3, provider.clone()).await;
    // The composition root's startup sweep; it also starts the waker.
    second.subagents.reap_orphans().await.unwrap();
    second.wait_completed(&turn).await;
    assert_eq!(
        second.wake_reason(&turn).await.as_deref(),
        Some("all_settled")
    );
    assert_eq!(
        provider.spawn_turns.load(Ordering::SeqCst),
        0,
        "the parked parent was never re-run from the start"
    );
    assert_eq!(provider.final_turns.load(Ordering::SeqCst), 1);
}

/// A queued follow-up is for after the turn: it does not wake the wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follow_up_does_not_wake_the_parked_parent() {
    let provider = Scripted::new([true, true, true]);
    let rig = rig(SqliteStore::memory(64).unwrap(), 4, provider.clone()).await;
    rig.create_parent(&workspace()).await;
    rig.start_parent("PARK-ALL").await;
    let turn = rig.parent_turn().await.task_id;
    rig.wait_parked(&turn).await;
    rig.session
        .follow_up(
            "p",
            &FollowUpInput {
                frozen_payload: None,
                content: Value::String("later".into()),
                input_id: Some("follow-1".into()),
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(rig.parent_status(&turn).await, TaskStatus::WaitingChild);
    assert!(rig.wake_reason(&turn).await.is_none());
    provider.release_all();
}

/// The park request is minted by the host: reserved id, the deadline as its
/// expiry, and the waker (not a human) is the only way out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_park_request_carries_the_reserved_id_and_the_deadline_expiry() {
    let provider = Scripted::new([true, true, true]);
    let rig = rig(SqliteStore::memory(64).unwrap(), 4, provider.clone()).await;
    rig.create_parent(&workspace()).await;
    rig.start_parent("PARK-ALL").await;
    let turn = rig.parent_turn().await.task_id;
    rig.wait_parked(&turn).await;
    let requests = rig
        .store
        .list_inputs(agentkit_store::AgentInputQuery {
            session_id: "p".into(),
            kinds: Some(vec![agentkit_store::AgentInputKind::HumanRequest]),
            statuses: Some(vec![agentkit_store::AgentInputStatus::Pending]),
            target_task_id: Some(turn.clone()),
        })
        .await
        .unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].input_id.starts_with("childwait:"));
    assert!(
        requests[0].expires_at.is_some(),
        "the deadline is the expiry"
    );
    assert_eq!(requests[0].content["awaited"].as_array().unwrap().len(), 3);
    provider.release_all();
}

/// A parked wait whose awaited child has no registry row is unsettled, even
/// in `any` mode, until the timeout; the timeout reports the row `missing`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_child_row_is_unsettled_until_the_timeout() {
    let store = SqliteStore::memory(64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let session = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: runtime.clone(),
        plan_approval: None,
        wake: None,
    });
    let service = SubagentService::new(
        store.clone(),
        session,
        runtime,
        SubagentConfig {
            enabled: true,
            ..Default::default()
        }
        .effective(),
    );
    let far = park_ghost_wait(&store, "ghost-far", 4_000_000_000_000).await;
    assert_eq!(service.wake_parked_parents().await.unwrap(), 0);
    assert_eq!(
        store.get_task(&far).await.unwrap().unwrap().status,
        TaskStatus::WaitingChild
    );
    let due = park_ghost_wait(&store, "ghost-due", 1).await;
    assert_eq!(service.wake_parked_parents().await.unwrap(), 1);
    assert_eq!(
        store.get_task(&due).await.unwrap().unwrap().status,
        TaskStatus::Queued
    );
    let tool = store
        .messages("ghost-due", false)
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.role == "tool")
        .expect("the wake leaves the await result");
    assert!(
        tool.content
            .as_str()
            .unwrap()
            .contains("\"status\":\"missing\""),
        "{:?}",
        tool.content
    );
}

/// A running turn of `chat` parked on a child wait for one absent child.
async fn park_ghost_wait(store: &SqliteStore, chat: &str, deadline_ms: i64) -> String {
    use agentkit_store::{
        AgentInputKind, ClaimInput, CreateAgentInput, MessageInput, ParkTurn, SubmitTask,
    };
    store
        .create_chat(chat, None, json!({}), 1000)
        .await
        .unwrap();
    let task_id = format!("turn-{chat}");
    let message = |id: &str, run: Option<&str>, role: &str| MessageInput {
        id: Some(format!("{task_id}-{id}")),
        chat_id: chat.into(),
        run_id: run.map(str::to_owned),
        role: role.into(),
        content: json!({"role": role, "content": "x"}),
        ..MessageInput::default()
    };
    store
        .submit_turn(
            SubmitTask {
                task_id: task_id.clone(),
                kind: CHAT_TURN_TASK_KIND.to_owned(),
                scope_id: chat.into(),
                payload: json!({"chatId": chat}),
                priority: 0,
                now: 1000,
                available_at: Some(1000),
                depends_on: vec![],
                parent_task_id: None,
                exclusive_scope: false,
            },
            vec![
                message("user", None, "user"),
                message("assistant", Some(&task_id), "assistant"),
            ],
        )
        .await
        .unwrap();
    let claimed = store
        .claim_with_concurrency(
            ClaimInput {
                owner_id: "worker".into(),
                now: 1500,
                ttl_ms: 1_000_000,
                kinds: vec![],
                busy_scopes: vec![],
                max_attempts: 3,
            },
            1,
        )
        .await
        .unwrap()
        .expect("the seeded turn is claimable");
    assert_eq!(claimed.task.task_id, task_id);
    store
        .park_turn(ParkTurn {
            fence: claimed.lease.fence(),
            assistant_message: None,
            events: vec![],
            request: Some(CreateAgentInput {
                input_id: format!("childwait:{task_id}"),
                session_id: chat.into(),
                kind: AgentInputKind::HumanRequest,
                content: json!({
                    "kind": agentkit_store::CHILD_WAIT_KIND,
                    "awaited": ["ghost"], "mode": "any", "deadlineMs": deadline_ms,
                }),
                content_hash: format!("hash-{task_id}"),
                target_task_id: Some(task_id.clone()),
                on_settled: None,
                attempt_id: Some("attempt-1".into()),
                tool_call_id: Some("call-1".into()),
                expires_at: None,
            }),
            now: 1600,
        })
        .await
        .unwrap();
    task_id
}
