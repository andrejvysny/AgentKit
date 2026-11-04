//! Advisor & Watchdog end-to-end: structured findings through the real
//! core run loop (no mocks below the provider), durable persistence,
//! critical escalation as a steer on the reviewed session, watchdog
//! triggers with cooldown/window loop guards, and model/budget separation.

use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolCall,
    ToolContext, ToolDefinition, ToolLimits, ToolResult,
};
use agentkit_host::*;
use agentkit_store::{SqliteStore, SubmitTask};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const VALID_REPORT: &str = r#"{"summary":"two real risks","findings":[
 {"severity":"critical","category":"regression","title":"tests never ran",
  "evidence":[{"file":"src/lib.rs","line":42,"detail":"run_tests exited 0 with no tests collected"}],
  "recommendation":"assert a nonzero test count"},
 {"severity":"warning","category":"implementation_risk","title":"lock held across await",
  "evidence":[{"detail":"the mutex guard spans the network call"}]},
 {"severity":"bogus","category":"regression","title":"unknown severity dropped","evidence":[]},
 {"severity":"info","category":"scope_deviation","title":"","evidence":[]}
]}"#;

/// Scripted advisor provider: answers from a queue (repeating the last),
/// records every request (model, visible tools, message text), and can
/// hold replies on a gate for the saturation test.
struct Oracle {
    answers: Mutex<VecDeque<String>>,
    hold: AtomicBool,
    released: Arc<AtomicBool>,
    gate: Arc<Notify>,
    seen: Mutex<Vec<Seen>>,
}

struct Seen {
    model: String,
    tools: Vec<String>,
    text: String,
}

impl Oracle {
    fn new(answers: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            hold: AtomicBool::new(false),
            released: Arc::new(AtomicBool::new(false)),
            gate: Arc::new(Notify::new()),
            seen: Mutex::new(vec![]),
        })
    }
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().iter().cloned().collect()
    }
}

impl Clone for Seen {
    fn clone(&self) -> Self {
        Seen {
            model: self.model.clone(),
            tools: self.tools.clone(),
            text: self.text.clone(),
        }
    }
}

#[async_trait]
impl ChatProvider for Oracle {
    fn id(&self) -> &str {
        "oracle"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let answer = {
            let mut answers = self.answers.lock().unwrap();
            if answers.len() > 1 {
                answers.pop_front().expect("len checked")
            } else {
                answers
                    .front()
                    .cloned()
                    .unwrap_or_else(|| "{\"summary\":\"empty\",\"findings\":[]}".into())
            }
        };
        self.seen.lock().unwrap().push(Seen {
            model: input.model.clone(),
            tools: input.tools.iter().map(|tool| tool.name.clone()).collect(),
            text: input
                .messages
                .iter()
                .map(|message| message.content.to_string())
                .collect::<Vec<_>>()
                .join("|"),
        });
        if self.hold.load(Ordering::SeqCst) {
            let gate = Arc::clone(&self.gate);
            let released = Arc::clone(&self.released);
            return Box::pin(futures::stream::once(async move {
                // Re-arm the gate: a released flag means later requests
                // answer immediately (no lost-wakeup on `notify_waiters`).
                while !released.load(Ordering::SeqCst) {
                    gate.notified().await;
                }
                Ok(ProviderEvent::Completed {
                    content: answer,
                    finish_reason: "stop".into(),
                    tool_calls: vec![],
                })
            }));
        }
        if answer == "TOOLCALL" {
            return Box::pin(futures::stream::iter([Ok(ProviderEvent::Completed {
                content: String::new(),
                finish_reason: "tool_calls".into(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments_json: "{\"path\":\"src/lib.rs\"}".into(),
                }],
            })]));
        }
        Box::pin(futures::stream::iter([Ok(ProviderEvent::Completed {
            content: answer,
            finish_reason: "stop".into(),
            tool_calls: vec![],
        })]))
    }
}

/// The advisor's read-only lens: a stub read tool plus a WRITE tool that
/// the engine's read-only ceiling must never show the model.
struct ReadFile {
    def: ToolDefinition,
}
#[async_trait]
impl Tool for ReadFile {
    fn definition(&self) -> &ToolDefinition {
        &self.def
    }
    async fn execute(&self, _: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"text": "fn main() {}"})),
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

struct WriteFile {
    def: ToolDefinition,
}
#[async_trait]
impl Tool for WriteFile {
    fn definition(&self) -> &ToolDefinition {
        &self.def
    }
    async fn execute(&self, _: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        unreachable!("the advisor must never be handed a write tool")
    }
}

fn tool_definition(name: &str, effect: &str, capability: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        version: "1".into(),
        effect: effect.into(),
        capability: capability.into(),
        description: String::new(),
        input_schema: json!({"type":"object"}),
        output_schema: None,
        timeout_ms: None,
    }
}

struct Sink(Mutex<Vec<Value>>);
impl AdvisorEventSink for Sink {
    fn emit(&self, event: Value) {
        self.0.lock().unwrap().push(event);
    }
}

fn config() -> AdvisorConfig {
    AdvisorConfig {
        enabled: true,
        model: Some("advisor-model".into()),
        watchdog_model: Some("watchdog-model".into()),
        ..Default::default()
    }
}

struct Harness {
    store: SqliteStore,
    service: AgentSessionService,
    engine: AdvisorEngine,
    oracle: Arc<Oracle>,
    events: Arc<Sink>,
    config: Arc<std::sync::Mutex<AdvisorConfig>>,
}

impl Harness {
    async fn start(answers: Vec<String>, mut config: AdvisorConfig) -> Self {
        let store = SqliteStore::memory(64).unwrap();
        let runtime: SharedRuntime = Arc::new(SystemRuntime);
        let oracle = Oracle::new(answers);
        let events = Arc::new(Sink(Mutex::new(vec![])));
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
                profile_id: "p1".into(),
                profile_digest: None,
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
        // Keep the parent busy so escalation steers queue visibly instead
        // of starting turns nothing will execute here.
        store
            .submit(SubmitTask {
                task_id: "parent-turn".into(),
                kind: CHAT_TURN_TASK_KIND.into(),
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
        config = config.effective();
        let engine = AdvisorEngine::new(
            oracle.clone() as Arc<dyn ChatProvider>,
            store.clone(),
            Arc::new(agentkit_core::SystemRuntime),
            config.clone(),
            Arc::new(StaticToolSource::new(vec![
                Arc::new(ReadFile {
                    def: tool_definition("read_file", "read", "fs.read"),
                }) as Arc<dyn Tool>,
                Arc::new(WriteFile {
                    def: tool_definition("write_file", "write", "fs.write"),
                }) as Arc<dyn Tool>,
            ])),
            Some(events.clone() as Arc<dyn AdvisorEventSink>),
            service.clone(),
        );
        Self {
            store,
            service,
            engine,
            oracle,
            events,
            config: Arc::new(std::sync::Mutex::new(config)),
        }
    }

    async fn review(&self, chat: &str, focus: &str) -> HostResult<AdvisorReport> {
        self.engine
            .review(
                &ReviewRequest {
                    chat_id: chat.into(),
                    focus: focus.into(),
                    evidence_hint: None,
                },
                ReviewOrigin::Explicit,
                CancellationToken::new(),
            )
            .await
    }
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
async fn explicit_review_persists_findings_and_escalates_criticals() {
    let h = Harness::start(vec![VALID_REPORT.into()], config()).await;
    let report = h.review("parent", "the parser rewrite").await.unwrap();

    // Malformed findings are dropped, honest ones kept and capped.
    assert_eq!(report.findings.len(), 2);
    assert_eq!(report.summary, "two real risks");
    assert_eq!(report.findings[0].severity, FindingSeverity::Critical);
    assert_eq!(
        report.findings[0].evidence[0].file.as_deref(),
        Some("src/lib.rs")
    );
    assert_eq!(
        report.findings[1].category,
        FindingCategory::ImplementationRisk
    );
    assert_eq!(report.finding_ids.len(), 2);
    assert!(!report.run_id.is_empty());

    // Durable persistence, per chat, with the advisor's own model recorded.
    let rows = h.store.list_advisor_findings("parent", 50).await.unwrap();
    assert_eq!(rows.len(), 2);
    let critical = rows
        .iter()
        .find(|row| row.severity == "critical")
        .expect("critical row");
    assert_eq!(critical.source, "explicit");
    assert_eq!(critical.model, "advisor-model");
    assert_eq!(critical.detail["category"], json!("regression"));
    assert!(critical.escalated, "criticals are marked escalated");
    assert!(
        !rows
            .iter()
            .find(|row| row.severity == "warning")
            .unwrap()
            .escalated
    );

    // Escalation events: the host event sink saw the structured report.
    let events = h.events.0.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], json!("advisor.finding"));
    assert_eq!(events[0]["chatId"], json!("parent"));
    assert_eq!(events[0]["findingIds"].as_array().unwrap().len(), 2);

    // Criticals also confront the main agent: a durable steer input on the
    // busy parent session.
    let state = h.service.get_state("parent").await.unwrap();
    assert!(
        state
            .pending
            .iter()
            .any(|input| input.content.to_string().contains("ADVISORY (critical")),
        "critical advisory must reach the parent as a steer"
    );

    // Read-only ceiling + independent model: the provider saw only the
    // read tool and the advisor's configured model.
    let seen = h.oracle.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].model, "advisor-model");
    assert_eq!(
        seen[0].tools,
        vec!["read_file".to_owned()],
        "write tools never reach the advisor"
    );
    assert!(seen[0].text.contains("the parser rewrite"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unconfigured_advisor_model_falls_back_to_the_session_model() {
    let unconfigured = AdvisorConfig {
        enabled: true,
        ..Default::default()
    };
    let h = Harness::start(vec![VALID_REPORT.into()], unconfigured).await;
    h.review("parent", "fallback").await.unwrap();
    let seen = h.oracle.seen();
    assert_eq!(seen.len(), 1);
    // Never an empty model on the wire: the reviewed session's own model.
    assert_eq!(seen[0].model, "model");

    // No advisor model and no session model to fall back on: refuse
    // before any provider work instead of sending `model: ""`.
    let error = h.review("not-a-session", "fallback").await.unwrap_err();
    assert!(
        error.to_string().contains("advisor_model_unconfigured"),
        "{error}"
    );
    assert_eq!(h.oracle.seen().len(), 1, "no provider request was made");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unparseable_output_gets_one_repair_then_the_honest_finding() {
    let h = Harness::start(vec!["not json at all".into()], config()).await;
    let report = h.review("parent", "anything").await.unwrap();
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].category, FindingCategory::OutputInvalid);
    assert_eq!(report.findings[0].severity, FindingSeverity::Warning);
    assert!(report.summary.contains("failed validation twice"));
    let seen = h.oracle.seen();
    assert_eq!(seen.len(), 2, "exactly one bounded repair turn");
    assert!(seen[1].text.contains("not a valid report"));
    // The honest failure is persisted too — never a silent pass.
    let rows = h.store.list_advisor_findings("parent", 50).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].severity, "warning");
    assert_eq!(rows[0].detail["category"], json!("output_invalid"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repair_turn_recovers_a_valid_report() {
    let h = Harness::start(vec!["garbage first".into(), VALID_REPORT.into()], config()).await;
    let report = h.review("parent", "second time").await.unwrap();
    assert_eq!(report.findings.len(), 2);
    assert_eq!(h.oracle.seen().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loop_and_kill_switch_guards() {
    // The advisor never reviews advisor traffic.
    let h = Harness::start(vec![VALID_REPORT.into()], config()).await;
    let advisor_chat = advisor_run_chat_id("parent", &ReviewOrigin::Explicit, "t1");
    assert!(is_advisor_chat_id(&advisor_chat));
    let error = h
        .review(&advisor_chat, "nope")
        .await
        .expect_err("loop guard");
    assert!(error.to_string().contains("may not review an advisor run"));

    // Disabled engine: nothing runs, nothing persists.
    let mut off = config();
    off.enabled = false;
    let h2 = Harness::start(vec![VALID_REPORT.into()], off).await;
    let error = h2.review("parent", "x").await.expect_err("kill switch");
    assert!(error.to_string().contains("advisor_disabled"));
    assert!(
        h2.store
            .list_advisor_findings("parent", 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contributor_tracks_the_enabled_flag() {
    let h = Harness::start(vec![VALID_REPORT.into()], config()).await;
    let context = ToolContributionContext {
        chat_id: Some("parent".into()),
        run_id: "r".into(),
        scope_id: None,
        actor: Value::Null,
        actor_id: None,
        bindings: vec![],
        limits: agentkit_core::ToolLimits::default(),
        cancellation: CancellationToken::new(),
        has_primary_binding: false,
        timeout_ms: 30_000,
        durable: None,
    };
    let tools = AdvisorTools::new(h.engine.clone(), h.config.clone());
    let names: Vec<String> = AdvisorContributor::new(tools.clone())
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
    assert_eq!(names, vec!["advisor_review".to_owned()]);

    off_config(&h.config);
    assert!(
        AdvisorContributor::new(tools)
            .contribute(&context)
            .await
            .unwrap()
            .is_empty()
    );
}

fn off_config(config: &Arc<std::sync::Mutex<AdvisorConfig>>) {
    config.lock().unwrap().enabled = false;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watchdog_fires_a_bounded_review_on_repeated_failures() {
    let mut cfg = config();
    cfg.trigger_consecutive_tool_failures = 2;
    let h = Harness::start(vec![VALID_REPORT.into()], cfg.clone()).await;
    let watchdog = Watchdog::new(h.engine.clone(), h.config.clone(), Arc::new(SystemRuntime));

    // Two consecutive failures cross the threshold.
    watchdog.observe_tool_result("parent", "run-1", "write_file", "failed", Some("boom"));
    watchdog.observe_tool_result("parent", "run-1", "write_file", "failed", Some("boom"));
    wait_until(
        || async {
            !h.store
                .list_advisor_findings("parent", 50)
                .await
                .unwrap()
                .is_empty()
        },
        "watchdog review persisted findings",
    )
    .await;
    let rows = h.store.list_advisor_findings("parent", 50).await.unwrap();
    assert!(
        rows.iter()
            .all(|row| row.source == "watchdog" && row.trigger_kind == "repeated_failures")
    );
    assert_eq!(rows.first().unwrap().model, "watchdog-model");
    let seen = h.oracle.seen();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0]
            .text
            .contains("Automatic watchdog review (repeated_failures)")
    );

    // Loop guard: more failures on the same chat hit the per-chat cooldown
    // and the (run, trigger) dedupe — no second review.
    watchdog.observe_tool_result("parent", "run-2", "write_file", "failed", Some("boom"));
    watchdog.observe_tool_result("parent", "run-2", "write_file", "failed", Some("boom"));
    watchdog.observe_tool_result("parent", "run-1", "write_file", "failed", Some("boom"));
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert_eq!(
        h.store
            .list_advisor_findings("parent", 50)
            .await
            .unwrap()
            .len(),
        rows.len()
    );
    assert_eq!(h.oracle.seen().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_escalation_leaves_findings_unescalated() {
    // Reviewing a session that does not exist: the findings still persist
    // (the review ran and was paid for), but the escalation steer is
    // rejected — the rows must not claim a user-visible surfacing that
    // never happened.
    let h = Harness::start(vec![VALID_REPORT.into()], config()).await;
    let report = h.review("ghost-session", "focus").await.unwrap();
    assert_eq!(report.findings.len(), 2);
    let rows = h
        .store
        .list_advisor_findings("ghost-session", 50)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|row| !row.escalated),
        "a rejected steer must not mark anything escalated"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watchdog_never_fires_on_successful_tools_and_resets_on_success() {
    // Core reports tool outcomes as "succeeded" | "failed". Successes must
    // not count toward the failure threshold, and one success must reset
    // the streak — a status comparison once treated every SUCCESS as a
    // failure, so every healthy run paid for a model review.
    let mut cfg = config();
    cfg.trigger_consecutive_tool_failures = 3;
    let h = Harness::start(vec![VALID_REPORT.into()], cfg.clone()).await;
    let watchdog = Watchdog::new(h.engine.clone(), h.config.clone(), Arc::new(SystemRuntime));

    for _ in 0..6 {
        watchdog.observe_tool_result("parent", "run-ok", "read_file", "succeeded", None);
    }
    // Alternating failures and successes never build a consecutive streak;
    // distinct codes keep the repeated-error trigger out of the picture.
    for i in 0..6 {
        let (status, code) = if i % 2 == 0 {
            ("failed", format!("e{i}"))
        } else {
            ("succeeded", format!("e{i}"))
        };
        watchdog.observe_tool_result("parent", "run-mixed", "write_file", status, Some(&code));
    }
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert!(
        h.oracle.seen().is_empty(),
        "healthy and flapping-but-recovering runs must not consult the advisor"
    );
    assert!(
        h.store
            .list_advisor_findings("parent", 50)
            .await
            .unwrap()
            .is_empty()
    );

    // Positive control: a genuine consecutive streak still fires.
    for _ in 0..3 {
        watchdog.observe_tool_result("parent", "run-bad", "write_file", "failed", Some("boom"));
    }
    wait_until(
        || async { !h.oracle.seen().is_empty() },
        "a genuine streak still fires the review",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watchdog_ignores_advisor_traffic() {
    let mut cfg = config();
    cfg.trigger_consecutive_tool_failures = 2;
    let h = Harness::start(vec![VALID_REPORT.into()], cfg.clone()).await;
    let watchdog = Watchdog::new(h.engine.clone(), h.config.clone(), Arc::new(SystemRuntime));
    let advisor_chat = advisor_run_chat_id(
        "parent",
        &ReviewOrigin::Watchdog {
            trigger: "repeated_failures".into(),
        },
        "t1",
    );
    for _ in 0..6 {
        watchdog.observe_tool_result(&advisor_chat, "adv-run", "write_file", "failed", None);
    }
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert!(
        h.oracle.seen().is_empty(),
        "reviews must not review reviews"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watchdog_fires_on_turn_failure_streak() {
    let mut cfg = config();
    cfg.trigger_turn_failures = 2;
    let h = Harness::start(vec![VALID_REPORT.into()], cfg.clone()).await;
    let watchdog = Watchdog::new(h.engine.clone(), h.config.clone(), Arc::new(SystemRuntime));

    // Two failed chat.turn tasks, settled for real through the store.
    for index in 0..2 {
        let task_id = format!("turn-{index}");
        h.store
            .submit(SubmitTask {
                task_id: task_id.clone(),
                kind: CHAT_TURN_TASK_KIND.into(),
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
        let claimed = h
            .store
            .claim(agentkit_store::ClaimInput {
                owner_id: "test".into(),
                now: 1,
                ttl_ms: 60_000,
                kinds: vec![CHAT_TURN_TASK_KIND.into()],
                busy_scopes: vec![],
                max_attempts: 3,
            })
            .await
            .unwrap()
            .expect("claim");
        let settled = h
            .store
            .settle(agentkit_store::SettleTask {
                fence: claimed.lease.fence(),
                now: 2,
                status: agentkit_store::TaskStatus::Failed,
                error: Some("boom".into()),
                events: vec![],
                messages: vec![],
            })
            .await
            .unwrap();
        watchdog.observe_turn_settled(&settled);
    }
    wait_until(
        || async {
            h.store
                .list_advisor_findings("parent", 50)
                .await
                .unwrap()
                .iter()
                .any(|row| row.trigger_kind == "repeated_turn_failures")
        },
        "turn-failure streak triggered a review",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saturated_engine_refuses_concurrent_reviews() {
    let mut cfg = config();
    cfg.max_review_concurrency = 1;
    let h = Harness::start(vec![VALID_REPORT.into()], cfg).await;
    h.oracle.hold.store(true, Ordering::SeqCst);
    let engine = h.engine.clone();
    let first = tokio::spawn(async move {
        engine
            .review(
                &ReviewRequest {
                    chat_id: "parent".into(),
                    focus: "slow review".into(),
                    evidence_hint: None,
                },
                ReviewOrigin::Explicit,
                CancellationToken::new(),
            )
            .await
    });
    wait_until(
        || async {
            let seen = h.oracle.seen();
            !seen.is_empty()
        },
        "first review reached the provider",
    )
    .await;
    // The single permit is held by the in-flight review: a second caller is
    // refused immediately rather than queued (session turns never block).
    let error = h.review("parent", "too many").await.expect_err("saturated");
    assert!(error.to_string().contains("advisor_saturated"));
    h.oracle.released.store(true, Ordering::SeqCst);
    h.oracle.gate.notify_waiters();
    first.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_review_budget_bounds_model_spend() {
    let mut cfg = config();
    cfg.provider_requests = Some(1);
    let h = Harness::start(vec!["TOOLCALL".into()], cfg).await;
    // A provider that keeps calling tools must be cut off by the durable
    // budget after exactly one provider request.
    let outcome = h.review("parent", "endless tools").await;
    let seen = h.oracle.seen();
    assert_eq!(seen.len(), 1, "budget clamps provider requests");
    assert!(outcome.is_err(), "a budget-truncated review fails honestly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_finding_reviews_still_consume_the_window_budget() {
    // The window budget counts STARTED reviews, not persisted findings:
    // an adversarial context that keeps making tools fail while the
    // advisor honestly finds nothing must still be rate-limited, or the
    // session drains provider quota through reviews that write no rows.
    const EMPTY: &str = r#"{"summary":"all clear","findings":[]}"#;
    let mut cfg = config();
    cfg.reviews_per_window = 2;
    // Exactly the effective floor (values below it are replaced by the
    // 180 s default), so the window budget — not the cooldown — is what
    // stops the third review.
    cfg.cooldown_ms = 1_000;
    cfg.window_ms = 60_000;
    cfg.trigger_consecutive_tool_failures = 2;
    let h = Harness::start(vec![EMPTY.into(); 8], cfg.clone()).await;
    let watchdog = Watchdog::new(h.engine.clone(), h.config.clone(), Arc::new(SystemRuntime));

    // The effective config floors the per-(chat, trigger) cooldown at one
    // second, so triggers must be spaced past it to exercise the window
    // budget rather than the cooldown.
    for i in 0..4 {
        watchdog.observe_tool_result(
            "parent",
            &format!("run-{i}"),
            "write_file",
            "failed",
            Some("boom"),
        );
        watchdog.observe_tool_result(
            "parent",
            &format!("run-{i}"),
            "write_file",
            "failed",
            Some("boom"),
        );
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    }
    wait_until(
        || async { h.oracle.seen().len() >= 2 },
        "the first two reviews reach the provider",
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert_eq!(
        h.oracle.seen().len(),
        2,
        "zero-finding reviews consumed the window; later triggers must not pay"
    );
    // The attempts ledger — not findings — is what the budget counted:
    // both reviews recorded a started row despite returning nothing.
    assert_eq!(
        h.store
            .count_advisor_reviews_since(Some("parent"), Some("watchdog"), 0)
            .await
            .unwrap(),
        2
    );
    assert!(
        h.store
            .list_advisor_findings("parent", 50)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_triggers_cannot_overshoot_the_window_budget() {
    // One failing observation pass pushes up to three distinct triggers,
    // each with its own cooldown key. Before the synchronous reservation,
    // all three detached reviews read the same pre-window count and all
    // three paid: the budget now lands count+attempt atomically per chat,
    // so `reviewsPerWindow: 1` means ONE review — not three.
    const EMPTY: &str = r#"{"summary":"all clear","findings":[]}"#;
    let mut cfg = config();
    cfg.reviews_per_window = 1;
    cfg.window_ms = 3_600_000;
    cfg.cooldown_ms = 0;
    cfg.trigger_consecutive_tool_failures = 2;
    cfg.trigger_edit_count = 2;
    cfg.trigger_repeated_error_code = u32::MAX;
    let h = Harness::start(vec![EMPTY.into(); 6], cfg.clone()).await;
    let watchdog = Watchdog::new(h.engine.clone(), h.config.clone(), Arc::new(SystemRuntime));

    // ScopeDeviation crosses on the second declared edit…
    let calls = |path: &str| {
        vec![agentkit_core::ToolCall {
            id: "c".into(),
            name: "edit_file".into(),
            arguments_json: format!(r#"{{"path":"{path}"}}"#),
        }]
    };
    watchdog.observe_tool_calls("parent", "run-burst", &calls("a.rs"));
    watchdog.observe_tool_calls("parent", "run-burst", &calls("b.rs"));
    // …and the test failure after edits stacks RepeatedFailures and
    // PotentialRegression on the same instant.
    for _ in 0..2 {
        watchdog.observe_tool_result("parent", "run-burst", "run_tests", "failed", Some("t"));
    }

    wait_until(
        || async { !h.oracle.seen().is_empty() },
        "the first reserved review reaches the provider",
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        h.oracle.seen().len(),
        1,
        "budget-blocked triggers must not pay for a second review"
    );
    assert_eq!(
        h.store
            .count_advisor_reviews_since(Some("parent"), Some("watchdog"), 0)
            .await
            .unwrap(),
        1,
        "only the reserved attempt row lands; blocked reservations must not record"
    );
}
