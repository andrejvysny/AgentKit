//! Token budgets (delegation spec "Budgets", DEL-06) end to end: a scripted
//! provider reports `run.usage`, the pre-request token gate stops a child at
//! its own or its tree's budget, and the numbers are recomputed from durable
//! events so a restart, a sibling or a resume sees the same remaining
//! budget. Every child turn runs through the host's own worker.

use agentkit_agents::{
    AdvisorConfig, AdvisorEngine, Brief, BudgetExhausted, OutputFormat, ResultEnvelope,
    ReviewOrigin, ReviewRequest, SUMMARY_CAP_BYTES, SpawnRequest, SpawnedChild, StaticToolSource,
    SubagentConfig, SubagentRoleGuard, SubagentRoleLimits, SubagentService, SubagentTokenGate,
};
use agentkit_chat::{
    AgentHost, ChatExecutor, ChatExecutorOptions, ChatResult, HostOptions, UsageAuthorizer,
    WorkerHandle,
};
use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolCall,
    ToolContext, ToolDefinition, ToolLimits, ToolRegistry, ToolResult,
};
use agentkit_engine::{RetryBackoff, SharedRuntime, SystemRuntime, TaskExecutor};
use agentkit_governance::{ToolContributionContext, ToolGuard, ToolGuardContext, ToolGuardVerdict};
use agentkit_sessions::{
    AgentSessionService, AgentSessionServiceDeps, CHAT_TURN_TASK_KIND, CreateSessionInput,
};
use agentkit_store::{SqliteStore, SubmitTask};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

/// One provider response: its text, whether it asks for a tool round (which
/// makes the model loop send another request), and the usage it reports.
#[derive(Clone)]
struct Step {
    text: &'static str,
    tool: bool,
    usage: Value,
}

fn reply(text: &'static str, tool: bool, usage: Value) -> Step {
    Step { text, tool, usage }
}

// Hard caps (R13): the gate admits a request only when settled spend +
// outstanding reservations + its own estimate fit under BOTH caps. A real
// child turn estimates ~16.5k tokens (role prompt, tool definitions and the
// 1,024-token default output reserve), so the end-to-end caps below sit well
// above that floor.

/// Scripted per child: the child is recognized by a marker in its prompt
/// (the brief objective). Unscripted requests answer "done" without usage.
#[derive(Default)]
struct Scripted {
    steps: Mutex<HashMap<&'static str, VecDeque<Step>>>,
    failing: Mutex<HashSet<&'static str>>,
    requests: Mutex<Vec<&'static str>>,
    calls: AtomicU64,
}

impl Scripted {
    fn script(&self, marker: &'static str, steps: Vec<Step>) {
        self.steps.lock().unwrap().insert(marker, steps.into());
    }

    fn fail(&self, marker: &'static str, on: bool) {
        let mut failing = self.failing.lock().unwrap();
        if on {
            failing.insert(marker);
        } else {
            failing.remove(marker);
        }
    }

    fn requests_of(&self, marker: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| **seen == marker)
            .count()
    }
}

#[async_trait]
impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "token-budget-test"
    }

    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let text = input
            .messages
            .iter()
            .map(|message| message.content.to_string())
            .collect::<Vec<_>>()
            .join("|");
        let marker = self
            .steps
            .lock()
            .unwrap()
            .keys()
            .find(|marker| text.contains(**marker))
            .copied();
        let step = match marker {
            Some(marker) => {
                self.requests.lock().unwrap().push(marker);
                if self.failing.lock().unwrap().contains(marker) {
                    return Box::pin(futures::stream::iter([Err(CoreError::new(
                        "provider_error",
                        "the provider said no",
                    ))]));
                }
                self.steps
                    .lock()
                    .unwrap()
                    .get_mut(marker)
                    .and_then(VecDeque::pop_front)
            }
            None => None,
        };
        let step = step.unwrap_or_else(|| reply("done", false, Value::Null));
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let mut events = vec![];
        if !step.usage.is_null() {
            let mut usage = step.usage.clone();
            usage["callId"] = json!(format!("usage-{call}"));
            usage["attempt"] = json!(1);
            usage["step"] = json!(0);
            usage["model"] = json!("model");
            usage["source"] = json!("stream");
            usage["finalForCall"] = json!(true);
            events.push(Ok(ProviderEvent::Usage(usage)));
        }
        events.push(Ok(ProviderEvent::Completed {
            content: step.text.into(),
            finish_reason: if step.tool { "tool_calls" } else { "stop" }.into(),
            tool_calls: if step.tool {
                vec![ToolCall {
                    id: format!("probe-{call}"),
                    name: "probe".into(),
                    arguments_json: "{}".into(),
                }]
            } else {
                vec![]
            },
        }));
        Box::pin(futures::stream::iter(events))
    }
}

/// The tool a "tool round" step calls, so a turn sends a further request.
struct Probe {
    definition: ToolDefinition,
}

#[async_trait]
impl Tool for Probe {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(&self, _: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"text": "probed"})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: ToolLimits::default(),
            model_data: None,
            summary: None,
            status: None,
            model_content: None,
        })
    }
}

/// The executor is built before the subagent service exists (the service
/// needs the host's wake handle), so the gate is late-bound exactly as the
/// runtime's subagent lane does it.
struct LateGate(OnceLock<Arc<SubagentTokenGate>>);

#[async_trait]
impl UsageAuthorizer for LateGate {
    async fn authorize(&self, task_id: &str, usage: &Value) -> ChatResult<()> {
        match self.0.get() {
            Some(gate) => gate.authorize(task_id, usage).await,
            None => Ok(()),
        }
    }
}

struct Rig {
    store: SqliteStore,
    session: AgentSessionService<SqliteStore>,
    subagents: SubagentService,
    provider: Arc<Scripted>,
    worker: WorkerHandle,
}

fn config(tree_tokens: Option<u64>) -> SubagentConfig {
    SubagentConfig {
        enabled: true,
        limits: SubagentRoleLimits {
            await_poll_ms: 20,
            max_await_ms: 20_000,
            lease_ttl_ms: 60_000,
            max_tree_tokens: tree_tokens,
            ..Default::default()
        },
        ..Default::default()
    }
}

impl Rig {
    /// `fresh` creates the parent session and its live stand-in turn; a
    /// restarted rig finds both in the reopened store.
    async fn start(store: SqliteStore, tree_tokens: Option<u64>, fresh: bool) -> Self {
        let runtime: SharedRuntime = Arc::new(SystemRuntime);
        let provider = Arc::new(Scripted::default());
        let gate = Arc::new(LateGate(OnceLock::new()));
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(Probe {
                definition: ToolDefinition {
                    name: "probe".into(),
                    version: "1".into(),
                    effect: "read".into(),
                    capability: "fs.read".into(),
                    description: "a read that lets a turn continue".into(),
                    input_schema: json!({"type": "object"}),
                    output_schema: None,
                    timeout_ms: None,
                    examples: vec![],
                },
            }))
            .unwrap();
        let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(
            ChatExecutor::new(
                Arc::clone(&provider) as Arc<dyn ChatProvider>,
                Arc::new(registry),
            )
            .with_options(ChatExecutorOptions {
                usage: Some(Arc::clone(&gate) as Arc<dyn UsageAuthorizer>),
                ..Default::default()
            }),
        );
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
        let session = service.clone();
        host.set_session_flush_planner(Arc::new(service.clone()))
            .await;
        if fresh {
            // The `general` role writes, so the parent needs a workspace
            // (authority 4.4: `workspace_required`).
            let workspace = std::env::temp_dir().join(format!(
                "agentkit-budget-ws-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ));
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
                    workspace: Some(json!({
                        "root": workspace.to_string_lossy(),
                        "trusted": true
                    })),
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
        }
        host.recover().await.unwrap();
        let worker = host.start_worker();
        let subagents = SubagentService::new(
            store.clone(),
            service,
            runtime,
            config(tree_tokens).effective(),
        );
        assert!(gate.0.set(subagents.token_gate(None)).is_ok());
        Self {
            store,
            session,
            subagents,
            provider,
            worker,
        }
    }

    async fn memory(tree_tokens: Option<u64>) -> Self {
        Self::start(SqliteStore::memory(64).unwrap(), tree_tokens, true).await
    }

    /// A `general` child whose prompt carries `marker`; `max_tokens` is its
    /// own budget (the default applies when `None`).
    async fn spawn(&self, key: &str, marker: &str, max_tokens: Option<u64>) -> SpawnedChild {
        let mut brief = Brief::new(
            "general",
            "child task",
            marker,
            OutputFormat::Summary,
            "the task is done",
        );
        brief.budget.max_tokens = max_tokens;
        self.subagents
            .spawn(&SpawnRequest {
                parent_chat_id: "parent".into(),
                parent_task_id: "parent-turn".into(),
                spawn_key: key.into(),
                brief,
                exclusive_workspace: false,
            })
            .await
            .expect("spawn")
    }

    /// Waits for `child` to be terminal and returns its result envelope.
    async fn settle(&self, child: &SpawnedChild) -> ResultEnvelope {
        let ids = [child.child_chat_id.clone()];
        let outcome = self
            .subagents
            .await_children(
                "parent",
                Some(&ids[..]),
                Some(20_000),
                &CancellationToken::new(),
            )
            .await
            .expect("await");
        assert!(outcome.settled, "child never settled: {outcome:?}");
        outcome
            .results
            .into_iter()
            .next()
            .and_then(|result| result.envelope)
            .expect("settled envelope")
    }

    /// What the runtime's settled observer does once each of the child's
    /// turns is terminal: persists totals and emits the durable event.
    async fn observe(&self, child_chat_id: &str) {
        for task in self
            .store
            .list_tasks(Some(child_chat_id.to_owned()))
            .await
            .unwrap()
            .into_iter()
            .filter(|task| task.kind == CHAT_TURN_TASK_KIND && task.status.is_terminal())
        {
            self.subagents.observe_settled(&task).await.unwrap();
        }
    }

    /// A queued turn of `chat_id` no worker will claim: a live turn whose
    /// requests the test authorizes by hand.
    async fn live_turn(&self, chat_id: &str, task_id: &str) {
        let now = self.subagents.now_ms();
        self.store
            .submit(SubmitTask {
                task_id: task_id.into(),
                kind: CHAT_TURN_TASK_KIND.into(),
                scope_id: chat_id.into(),
                payload: json!({}),
                priority: 0,
                now,
                available_at: Some(now + 3_600_000),
                depends_on: vec![],
                parent_task_id: None,
                exclusive_scope: false,
            })
            .await
            .unwrap();
    }

    /// One request authorization as the executor makes it: the request's own
    /// run id differs from the turn task id it carries.
    async fn authorize(
        &self,
        chat_id: &str,
        turn: &str,
        prompt: u64,
        output: u64,
    ) -> ChatResult<()> {
        self.subagents
            .token_gate(None)
            .authorize(
                "request-run",
                &json!({"chatId": chat_id, "taskId": turn, "runId": "request-run",
                    "estimatedPromptTokens": prompt, "estimatedOutputTokens": output}),
            )
            .await
    }

    /// The gate's verdict for the next request of `chat_id`, as the
    /// executor would ask it.
    async fn gate(&self, chat_id: &str) -> ChatResult<()> {
        self.subagents
            .token_gate(None)
            .authorize(
                "probe-task",
                &json!({"chatId": chat_id, "estimatedPromptTokens": 0}),
            )
            .await
    }

    /// Durable `subagent.budget_exhausted` events on `task_id`'s stream.
    async fn exhaustion_events(&self, task_id: &str) -> Vec<Value> {
        self.store
            .events(task_id, None, 1_000)
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.event)
            .filter(|event| event["type"] == "subagent.budget_exhausted")
            .collect()
    }
}

fn exhausted(budget: &str, used: u64, limit: u64) -> BudgetExhausted {
    BudgetExhausted {
        budget: budget.into(),
        used,
        limit,
    }
}

/// `used` out of "... budget spent (<used> of <limit>)".
fn spent_used(message: &str) -> u64 {
    message
        .split("budget spent (")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|used| used.parse().ok())
        .unwrap_or_else(|| panic!("no spend figure in: {message}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_stops_at_its_budget_and_returns_partial_text() {
    let rig = Rig::memory(None).await;
    // The first request alone spends 60000 of the child's 50000 tokens and asks
    // for a tool round; the gate must refuse the second request.
    rig.provider.script(
        "CHILD-A",
        vec![
            reply("partial findings", true, json!({"output": 60_000})),
            reply("never sent", false, Value::Null),
        ],
    );
    let child = rig.spawn("a", "CHILD-A", Some(50_000)).await;
    let envelope = rig.settle(&child).await;

    assert_eq!(envelope.status, "budget_exhausted", "{envelope:?}");
    assert_eq!(
        envelope.budget_exhausted,
        Some(exhausted("child", 60_000, 50_000))
    );
    assert!(
        envelope.summary.starts_with("partial findings"),
        "the partial text leads the summary: {}",
        envelope.summary
    );
    assert!(
        envelope
            .summary
            .contains("child budget spent 60000 of 50000 tokens"),
        "the exhaustion note follows it: {}",
        envelope.summary
    );
    assert_eq!(
        rig.provider.requests_of("CHILD-A"),
        1,
        "no provider request may follow the exhausted gate"
    );

    // While the turn ran the gate wrote chat metadata only: appending to the
    // turn's stream mid-turn would collide with its own event sequence.
    let turn = child.turn_task_id.clone().expect("child turn");
    assert!(
        rig.exhaustion_events(&turn).await.is_empty(),
        "no event is appended before the turn settles"
    );
    // The settled observer emits the durable event once, however often it
    // observes: child id, budget, numbers.
    rig.observe(&child.child_chat_id).await;
    rig.observe(&child.child_chat_id).await;
    let events = rig.exhaustion_events(&turn).await;
    assert_eq!(events.len(), 1, "idempotent per child: {events:?}");
    assert_eq!(
        events[0]["data"],
        json!({"childId": child.child_chat_id, "budget": "child", "used": 60000, "limit": 50000})
    );
    // The chat record the envelope is rebuilt from.
    let chat = rig
        .store
        .get_chat(&child.child_chat_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        chat.metadata["agentkit.subagent"]["budgetExhausted"]["budget"],
        "child"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tree_budget_stops_the_second_sibling() {
    let rig = Rig::memory(Some(50_000)).await;
    rig.provider.script(
        "SIBLING-A",
        vec![reply("a is done", false, json!({"output": 50_000}))],
    );
    rig.provider.script("SIBLING-B", vec![]);

    let first = rig.spawn("a", "SIBLING-A", None).await;
    let first = rig.settle(&first).await;
    assert_eq!(first.status, "completed", "{first:?}");
    assert_eq!(first.summary, "a is done");

    // Each sibling is far inside its own default budget; the tree's is spent.
    let second = rig.spawn("b", "SIBLING-B", None).await;
    let second_envelope = rig.settle(&second).await;
    assert_eq!(
        second_envelope.status, "budget_exhausted",
        "{second_envelope:?}"
    );
    assert_eq!(
        second_envelope.budget_exhausted,
        Some(exhausted("tree", 50_000, 50_000))
    );
    assert!(
        second_envelope
            .summary
            .contains("tree budget spent 50000 of 50000 tokens"),
        "{}",
        second_envelope.summary
    );
    assert_eq!(
        rig.provider.requests_of("SIBLING-B"),
        0,
        "the second sibling never reached the provider"
    );

    // The parent chat is not a child: the gate passes it through.
    rig.gate("parent").await.expect("non-children pass through");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_mid_run_keeps_the_remaining_budget_identical() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("budgets.db");
    {
        let first = Rig::start(SqliteStore::open(&path, 64).unwrap(), Some(50_000), true).await;
        first.provider.script(
            "BEFORE-RESTART",
            vec![reply("spent", false, json!({"output": 30_000}))],
        );
        let child = first.spawn("before", "BEFORE-RESTART", None).await;
        assert_eq!(first.settle(&child).await.status, "completed");
        // Process death: nothing of the 30000 tokens lives anywhere but the
        // database.
        first.worker.interrupt_active().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let second = Rig::start(SqliteStore::open(&path, 64).unwrap(), Some(50_000), false).await;
    second.provider.script(
        "AFTER-RESTART",
        vec![reply("spent", false, json!({"output": 20_000}))],
    );
    second.provider.script("LAST", vec![]);
    // 30000 (before the restart) + 20000 = exactly the tree budget.
    let middle = second.spawn("after", "AFTER-RESTART", None).await;
    let middle = second.settle(&middle).await;
    assert_eq!(
        middle.status, "completed",
        "20000 fits the 20000 that remained: {middle:?}"
    );

    let last = second.spawn("last", "LAST", None).await;
    let last = second.settle(&last).await;
    assert_eq!(last.status, "budget_exhausted", "{last:?}");
    assert_eq!(
        last.budget_exhausted,
        Some(exhausted("tree", 50_000, 50_000)),
        "the pre-restart spend counts toward the same total"
    );
    assert_eq!(second.provider.requests_of("LAST"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_reads_are_weighted_a_tenth() {
    let rig = Rig::memory(None).await;
    // Request 1: 200000 cache reads (20000) + 10000 output = 30000 of 50000.
    // At full weight it would be 210000 and the second would be refused.
    // Request 2: 300000 cache reads (+30000) = 60000; the third is refused.
    rig.provider.script(
        "CHILD-CACHE",
        vec![
            reply(
                "first",
                true,
                json!({"cache_read": 200_000, "output": 10_000}),
            ),
            reply("second", true, json!({"cache_read": 300_000})),
            reply("never sent", false, Value::Null),
        ],
    );
    let child = rig.spawn("cache", "CHILD-CACHE", Some(50_000)).await;
    let envelope = rig.settle(&child).await;

    assert_eq!(envelope.status, "budget_exhausted", "{envelope:?}");
    assert_eq!(
        envelope.budget_exhausted,
        Some(exhausted("child", 60_000, 50_000))
    );
    assert_eq!(
        rig.provider.requests_of("CHILD-CACHE"),
        2,
        "the first request left room for the second's estimate, so it ran"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_child_is_charged_to_the_same_budget() {
    let rig = Rig::memory(None).await;
    rig.provider.script("RESUME-A", vec![]);
    rig.provider.fail("RESUME-A", true);
    let child = rig.spawn("resume-a", "RESUME-A", Some(200_000)).await;
    assert_eq!(rig.settle(&child).await.status, "failed");
    // The failed turn had reported 20000 tokens before it died.
    let turn = child.turn_task_id.clone().expect("child turn");
    rig.store
        .append_system_event(
            &turn,
            "run.usage",
            json!({"callId": "seed", "attempt": 1, "step": 0, "model": "model",
                "source": "stream", "finalForCall": true, "output": 20_000}),
            rig.subagents.now_ms(),
        )
        .await
        .unwrap();

    // 20000 of 200000 spent: the resume is admitted, and its turn spends 190000 more.
    rig.provider.fail("RESUME-A", false);
    rig.provider.script(
        "RESUME-A",
        vec![reply("resumed", false, json!({"output": 190_000}))],
    );
    let resumed = rig
        .subagents
        .resume(&child.child_chat_id, "r1")
        .await
        .expect("resume within budget");
    assert_ne!(resumed.turn_task_id, child.turn_task_id);
    assert_eq!(rig.settle(&child).await.status, "completed");

    // Both turns count: at least 20000 + 190000 against the one child budget.
    let error = rig.gate(&child.child_chat_id).await.expect_err("spent");
    let message = error.to_string();
    assert!(message.contains("subagent_budget_exhausted"), "{message}");
    assert!(message.contains("child token budget spent"), "{message}");
    assert!(spent_used(&message) >= 210_000, "{message}");

    // A failed child that already spent its budget cannot be resumed at all.
    rig.provider.script("RESUME-B", vec![]);
    rig.provider.fail("RESUME-B", true);
    let doomed = rig.spawn("resume-b", "RESUME-B", Some(200_000)).await;
    assert_eq!(rig.settle(&doomed).await.status, "failed");
    rig.store
        .append_system_event(
            doomed.turn_task_id.as_deref().expect("child turn"),
            "run.usage",
            json!({"callId": "seed", "attempt": 1, "step": 0, "model": "model",
                "source": "stream", "finalForCall": true, "output": 200_000}),
            rig.subagents.now_ms(),
        )
        .await
        .unwrap();
    rig.provider.fail("RESUME-B", false);
    let refusal = rig
        .subagents
        .resume(&doomed.child_chat_id, "r2")
        .await
        .expect_err("a spent budget refuses the resume");
    assert!(
        refusal.to_string().contains("subagent_budget_exhausted"),
        "{refusal}"
    );
}

/// The tool-guard context of `tool` in `namespace` for chat `chat_id`.
fn guard_context(chat_id: &str, namespace: &str, tool: &str) -> ToolGuardContext {
    ToolGuardContext {
        contribution: ToolContributionContext {
            chat_id: Some(chat_id.into()),
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
        },
        namespace: namespace.into(),
        tool: ToolDefinition {
            name: tool.into(),
            version: "1.0.0".into(),
            effect: "read".into(),
            capability: "test".into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            timeout_ms: None,
            examples: vec![],
        },
        arguments: None,
    }
}

/// An advisor model that answers an empty report and reports `output` tokens.
struct Reviewer {
    output: u64,
}

#[async_trait]
impl ChatProvider for Reviewer {
    fn id(&self) -> &str {
        "reviewer"
    }

    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let usage = json!({"callId": "review-usage", "attempt": 1, "step": 0, "model": "model",
            "source": "stream", "finalForCall": true, "output": self.output});
        Box::pin(futures::stream::iter([
            Ok(ProviderEvent::Usage(usage)),
            Ok(ProviderEvent::Completed {
                content: r#"{"summary":"nothing found","findings":[]}"#.into(),
                finish_reason: "stop".into(),
                tool_calls: vec![],
            }),
        ]))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_cannot_summon_the_advisor() {
    let rig = Rig::memory(None).await;
    rig.provider.script(
        "CHILD-ADVISOR",
        vec![reply("done", false, json!({"output": 5}))],
    );
    let child = rig.spawn("adv", "CHILD-ADVISOR", None).await;
    rig.settle(&child).await;

    // The `general` role admits every tool, yet never the advisor.
    let guard = SubagentRoleGuard::new(rig.subagents.clone());
    let review = guard_context(&child.child_chat_id, "advisor", "advisor_review");
    assert!(!guard.is_visible(&review).await.unwrap());
    match guard.can_execute(&review).await.unwrap() {
        ToolGuardVerdict::Refused(reason) => {
            assert!(
                reason.contains("advisor_not_available_to_children"),
                "{reason}"
            )
        }
        other => panic!(
            "a child ran the advisor: {:?}",
            matches!(other, ToolGuardVerdict::Allowed)
        ),
    }
    // The parent session keeps it, and children keep their other tools.
    assert!(
        guard
            .is_visible(&guard_context("parent", "advisor", "advisor_review"))
            .await
            .unwrap()
    );
    let read = guard_context(&child.child_chat_id, "coding", "read_file");
    assert!(guard.is_visible(&read).await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn advisor_reviews_of_a_child_are_charged_to_its_budget() {
    let rig = Rig::memory(None).await;
    rig.provider.script(
        "CHILD-REVIEWED",
        vec![reply("done", false, json!({"output": 5}))],
    );
    let child = rig.spawn("reviewed", "CHILD-REVIEWED", Some(50_000)).await;
    rig.settle(&child).await;
    rig.gate(&child.child_chat_id)
        .await
        .expect("5 of 50000 tokens spent");

    // A review (watchdog or explicit) of the child's session spends 60000
    // tokens on the advisor's own model.
    let engine = AdvisorEngine::new(
        Arc::new(Reviewer { output: 60_000 }),
        rig.store.clone(),
        Arc::new(agentkit_core::SystemRuntime),
        AdvisorConfig {
            enabled: true,
            model: Some("advisor-model".into()),
            ..Default::default()
        }
        .effective(),
        Arc::new(StaticToolSource::new(vec![])),
        None,
        rig.session.clone(),
    );
    engine
        .review(
            &ReviewRequest {
                chat_id: child.child_chat_id.clone(),
                focus: "the child's work".into(),
                evidence_hint: None,
            },
            ReviewOrigin::Explicit,
            CancellationToken::new(),
        )
        .await
        .expect("review");

    let message = rig
        .gate(&child.child_chat_id)
        .await
        .expect_err("the review spent the child's budget")
        .to_string();
    assert!(message.contains("child token budget spent"), "{message}");
    assert_eq!(spent_used(&message), 60_005, "{message}");
    // A review of the parent (not a child) charges nothing.
    rig.gate("parent").await.expect("parent is not metered");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reservation_counts_against_siblings_and_is_keyed_by_the_turn() {
    let rig = Rig::memory(Some(1_000_000)).await;
    for marker in ["RESERVE-A", "RESERVE-B"] {
        rig.provider
            .script(marker, vec![reply("done", false, json!({"output": 10}))]);
    }
    // A writer child holds the workspace lease until it settles (4.4).
    let first = rig.spawn("a", "RESERVE-A", None).await;
    rig.settle(&first).await;
    let second = rig.spawn("b", "RESERVE-B", None).await;
    rig.settle(&second).await;
    rig.subagents.configure(config(Some(2_000)));

    // Each child's next turn is live. A's request (500 prompt + 1000 output
    // reserve) is admitted: 20 + 1500 fits the 2000 tree cap.
    rig.live_turn(&first.child_chat_id, "turn-a").await;
    rig.live_turn(&second.child_chat_id, "turn-b").await;
    rig.authorize(&first.child_chat_id, "turn-a", 500, 1_000)
        .await
        .expect("fits the tree");
    let chat = rig
        .store
        .get_chat(&first.child_chat_id)
        .await
        .unwrap()
        .unwrap();
    let ledger = &chat.metadata["agentkit.subagent"]["tokenLedger"];
    assert_eq!(ledger["turn-a"]["reserved"], 1_500, "{ledger}");
    assert!(
        ledger.get("request-run").is_none(),
        "the ledger is keyed by the turn task, never the request run id: {ledger}"
    );

    // B's request (100 + 500) does not fit beside A's in-flight reservation:
    // 1520 + 600 > 2000. The hard cap refuses it before dispatch.
    let message = rig
        .authorize(&second.child_chat_id, "turn-b", 100, 500)
        .await
        .expect_err("A's reservation leaves no room for B")
        .to_string();
    assert!(
        message.contains("tree token budget cannot fit the request"),
        "{message}"
    );
    assert!(message.contains("1520 used + 600 requested"), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_cap_of_one_token_refuses_the_first_request_before_dispatch() {
    let rig = Rig::memory(None).await;
    rig.provider.script(
        "CAP-ONE",
        vec![reply("never sent", false, json!({"output": 500}))],
    );
    let child = rig.spawn("cap-one", "CAP-ONE", Some(1)).await;
    let envelope = rig.settle(&child).await;

    assert_eq!(envelope.status, "budget_exhausted", "{envelope:?}");
    assert_eq!(
        rig.provider.requests_of("CAP-ONE"),
        0,
        "the provider must never be called past a hard cap"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_larger_than_the_tree_remainder_is_refused_not_reserved() {
    let rig = Rig::memory(Some(1_000_000)).await;
    rig.provider
        .script("TREE-A", vec![reply("done", false, json!({"output": 10}))]);
    let first = rig.spawn("a", "TREE-A", None).await;
    rig.settle(&first).await;
    rig.subagents.configure(config(Some(1_000)));
    rig.live_turn(&first.child_chat_id, "turn-a").await;

    // 10 of 1000 spent: 990 remain, the request needs 1500.
    let message = rig
        .authorize(&first.child_chat_id, "turn-a", 500, 1_000)
        .await
        .expect_err("1500 does not fit 990")
        .to_string();
    assert!(message.contains("subagent_budget_exhausted"), "{message}");
    let chat = rig
        .store
        .get_chat(&first.child_chat_id)
        .await
        .unwrap()
        .unwrap();
    let ledger = &chat.metadata["agentkit.subagent"]["tokenLedger"];
    assert_eq!(ledger["turn-a"]["reserved"], 0, "{ledger}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_siblings_racing_for_the_last_remainder_admit_at_most_one() {
    let rig = Rig::memory(Some(1_000_000)).await;
    for marker in ["RACE-A", "RACE-B"] {
        rig.provider
            .script(marker, vec![reply("done", false, json!({"output": 10}))]);
    }
    let first = rig.spawn("a", "RACE-A", None).await;
    rig.settle(&first).await;
    let second = rig.spawn("b", "RACE-B", None).await;
    rig.settle(&second).await;
    rig.subagents.configure(config(Some(2_000)));
    rig.live_turn(&first.child_chat_id, "turn-a").await;
    rig.live_turn(&second.child_chat_id, "turn-b").await;

    // 20 spent; each request needs 1100, so only one fits under 2000.
    let (a, b) = tokio::join!(
        rig.authorize(&first.child_chat_id, "turn-a", 100, 1_000),
        rig.authorize(&second.child_chat_id, "turn-b", 100, 1_000),
    );
    assert_eq!(
        usize::from(a.is_ok()) + usize::from(b.is_ok()),
        1,
        "exactly one sibling may take the last remainder: {a:?} / {b:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn totals_resume_from_the_last_event_and_survive_a_missing_ledger() {
    let rig = Rig::memory(None).await;
    rig.provider.script(
        "CHILD-TOTALS",
        vec![reply("done", false, json!({"output": 30_000}))],
    );
    let child = rig.spawn("totals", "CHILD-TOTALS", Some(50_000)).await;
    rig.settle(&child).await;
    rig.observe(&child.child_chat_id).await;
    let turn = child.turn_task_id.clone().expect("turn");

    // Settling stored running totals through the last usage event.
    let chat = rig
        .store
        .get_chat(&child.child_chat_id)
        .await
        .unwrap()
        .unwrap();
    let entry = chat.metadata["agentkit.subagent"]["tokenLedger"][turn.as_str()].clone();
    assert_eq!(entry["tenths"], 300_000, "{entry}");
    assert_eq!(entry["calls"], 1, "{entry}");
    assert!(entry["seq"].is_u64(), "{entry}");

    // A usage event past the stored seq is picked up incrementally...
    rig.store
        .append_system_event(
            &turn,
            "run.usage",
            json!({"callId": "late", "attempt": 1, "step": 0, "model": "model",
                "source": "stream", "finalForCall": true, "output": 40_000}),
            rig.subagents.now_ms(),
        )
        .await
        .unwrap();
    let message = rig
        .gate(&child.child_chat_id)
        .await
        .expect_err("70000 of 50000")
        .to_string();
    assert_eq!(spent_used(&message), 70_000, "{message}");

    // ...and a restart that lost the totals rescans everything once.
    rig.store
        .mutate_chat(&child.child_chat_id, rig.subagents.now_ms(), |chat| {
            chat.metadata["agentkit.subagent"]["tokenLedger"] = Value::Null;
            Ok(())
        })
        .await
        .unwrap();
    let message = rig
        .gate(&child.child_chat_id)
        .await
        .expect_err("still spent")
        .to_string();
    assert_eq!(spent_used(&message), 70_000, "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_comes_from_the_recorded_exhaustion_not_error_text() {
    let rig = Rig::memory(None).await;
    // A child that fails without ever tripping the gate is just failed,
    // whatever its error text says.
    rig.provider.script("FORGED", vec![]);
    rig.provider.fail("FORGED", true);
    let forged = rig.spawn("forged", "FORGED", None).await;
    let envelope = rig.settle(&forged).await;
    assert_eq!(envelope.status, "failed", "{envelope:?}");
    assert_eq!(envelope.budget_exhausted, None);

    // Marking the turn's exhaustion in metadata, as the gate does, is what
    // makes it `budget_exhausted`.
    let turn = forged.turn_task_id.clone().expect("turn");
    rig.store
        .mutate_chat(&forged.child_chat_id, rig.subagents.now_ms(), move |chat| {
            chat.metadata["agentkit.subagent"]["budgetExhausted"] = json!({
                "event": "subagent.budget_exhausted", "childId": "x", "budget": "tree",
                "used": 9, "limit": 5, "taskId": turn, "emitted": false});
            Ok(())
        })
        .await
        .unwrap();
    let view = rig
        .subagents
        .list("parent")
        .await
        .unwrap()
        .into_iter()
        .find(|view| view.child_chat_id == forged.child_chat_id)
        .expect("view");
    let envelope = rig
        .subagents
        .build_envelope(&view)
        .await
        .unwrap()
        .expect("envelope");
    assert_eq!(envelope.status, "budget_exhausted", "{envelope:?}");
    assert_eq!(envelope.budget_exhausted, Some(exhausted("tree", 9, 5)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_partial_answer_and_its_note_stay_inside_the_summary_cap() {
    let rig = Rig::memory(None).await;
    let long: &'static str = Box::leak("word ".repeat(4_000).into_boxed_str());
    rig.provider.script(
        "LONG-PARTIAL",
        vec![
            reply(long, true, json!({"output": 60_000})),
            reply("never sent", false, Value::Null),
        ],
    );
    let child = rig.spawn("long", "LONG-PARTIAL", Some(50_000)).await;
    let envelope = rig.settle(&child).await;

    assert_eq!(envelope.status, "budget_exhausted", "{envelope:?}");
    assert!(
        envelope.summary.len() <= SUMMARY_CAP_BYTES,
        "summary is {} bytes",
        envelope.summary.len()
    );
    assert!(
        envelope
            .summary
            .ends_with("the child stopped before finishing]"),
        "the note survives the cut: {}",
        &envelope.summary[envelope.summary.len().saturating_sub(120)..]
    );
    assert!(envelope.truncated, "{envelope:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawning_onto_the_native_lane_is_refused() {
    let rig = Rig::memory(None).await;
    let mut config = rig.subagents.config();
    config.roles.get_mut("general").expect("role").provider_id = Some("native".into());
    rig.subagents.configure(config);
    let brief = Brief::new(
        "general",
        "child task",
        "NATIVE",
        OutputFormat::Summary,
        "done",
    );
    let refusal = rig
        .subagents
        .spawn(&SpawnRequest {
            parent_chat_id: "parent".into(),
            parent_task_id: "parent-turn".into(),
            spawn_key: "native".into(),
            brief,
            exclusive_workspace: false,
        })
        .await
        .expect_err("the native lane is not metered");
    assert!(
        refusal.to_string().contains("native_lane_unsupported"),
        "{refusal}"
    );
}

fn brief_with_tokens(max_tokens: Option<u64>) -> Brief {
    let mut brief = Brief::new(
        "general",
        "child task",
        "do the thing",
        OutputFormat::Summary,
        "the task is done",
    );
    brief.budget.max_tokens = max_tokens;
    brief
}

#[test]
fn an_explicit_child_token_budget_below_the_floor_is_refused_with_a_teaching_error() {
    for tokens in [1, 2_000, 4_000, 31_999] {
        let error = brief_with_tokens(Some(tokens))
            .check_token_floor()
            .expect_err("below the floor");
        let text = error.to_string();
        assert!(text.starts_with("invalid_brief:"), "{text}");
        assert!(text.contains("32000"), "names the minimum: {text}");
        assert!(text.contains(&tokens.to_string()), "names the ask: {text}");
        assert!(
            text.contains("Omit `budget`"),
            "suggests the default: {text}"
        );
    }
}

#[test]
fn a_child_token_budget_at_or_above_the_floor_or_unset_is_accepted() {
    for tokens in [None, Some(32_000), Some(50_000), Some(1_000_000)] {
        brief_with_tokens(tokens)
            .check_token_floor()
            .expect("accepted");
    }
}
