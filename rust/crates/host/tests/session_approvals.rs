//! Session approvals end-to-end: a remembered approval runs later identical
//! calls until it is revoked, a plan approval escalates the session's
//! execution mode (or is vetoed by the policy), and an approval whose call
//! arguments changed is refused on the record — never silently run.

use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_host::*;
use agentkit_store::{
    AgentInputKind, AgentInputQuery, AgentInputStatus, ClaimInput, CreateAgentInput,
    ExecutionReceiptAdmission, ExecutionReceiptStatus, SettleTask, SqliteStore, TaskStatus,
};
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

const ARGS_A: &str = "{\"path\":\"a.txt\"}";

/// Asks (one mutating call) while `ask` is set; answers otherwise.
struct Scripted {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    ask: Arc<AtomicBool>,
    tool: &'static str,
    /// The verbatim JSON the provider declares; the numeric-spelling tests
    /// use it to keep the model-visible text outside any host re-parse.
    arguments_json: &'static str,
}

#[async_trait]
impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        self.requests.lock().unwrap().push(input);
        let calls = if self.ask.load(Ordering::SeqCst) {
            vec![agentkit_core::ToolCall {
                id: "call-1".into(),
                name: self.tool.into(),
                arguments_json: self.arguments_json.into(),
            }]
        } else {
            vec![]
        };
        Box::pin(futures::stream::iter(vec![Ok(ProviderEvent::Completed {
            content: if calls.is_empty() { "done" } else { "" }.into(),
            finish_reason: if calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            }
            .into(),
            tool_calls: calls,
        })]))
    }
}

struct Mutate {
    definition: ToolDefinition,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Mutate {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"done": true})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
        })
    }
}

struct WriterContributor {
    tool: Arc<dyn Tool>,
}

#[async_trait]
impl ToolContributor for WriterContributor {
    fn namespace(&self) -> &str {
        "coding"
    }
    async fn contribute(&self, _: &ToolContributionContext) -> HostResult<Vec<ContributedTool>> {
        Ok(vec![ContributedTool::DirectWrite(self.tool.clone())])
    }
}

struct AskGuard {
    tool: &'static str,
    requested_mode: Option<&'static str>,
}

#[async_trait]
impl ToolGuard for AskGuard {
    async fn can_execute(&self, context: &ToolGuardContext) -> HostResult<ToolGuardVerdict> {
        if context.tool.name != self.tool {
            return Ok(ToolGuardVerdict::Allowed);
        }
        Ok(ToolGuardVerdict::AwaitingHuman(ApprovalAsk {
            reason: format!("approval_required: {}", self.tool),
            preview: None,
            requested_mode: self.requested_mode.map(str::to_owned),
            uncertainty_generation: None,
        }))
    }
}

/// Covers exactly `write_file` with these args, the ordinary
/// `approval_required:` surface only.
struct KeyPolicy;

#[async_trait]
impl StandingApprovalPolicy for KeyPolicy {
    async fn key_for(
        &self,
        context: &ToolGuardContext,
        reasons: &[String],
    ) -> HostResult<Option<(String, String)>> {
        if context.tool.name != "write_file"
            || !reasons
                .iter()
                .all(|reason| reason.starts_with("approval_required:"))
        {
            return Ok(None);
        }
        let arguments = context.arguments.clone().unwrap_or(Value::Null);
        Ok(Some((args_sha256(&arguments), "write a.txt".into())))
    }
}

struct ModePolicy {
    tool: &'static str,
    granted: &'static str,
    deny_with: Mutex<Option<String>>,
}

#[async_trait]
impl PlanApprovalPolicy for ModePolicy {
    fn tool_names(&self) -> Vec<String> {
        vec![self.tool.into()]
    }
    async fn granted_mode(&self, _: &str) -> HostResult<String> {
        Ok(self.granted.into())
    }
    async fn authorize(&self, _: &PlanGrantIntent<'_>) -> HostResult<PlanAuthorization> {
        Ok(match self.deny_with.lock().unwrap().clone() {
            None => PlanAuthorization::Allowed,
            Some(reason) => PlanAuthorization::Denied(Some(reason)),
        })
    }
}

struct Fixture {
    store: SqliteStore,
    host: AgentHost,
    service: AgentSessionService,
    hits: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    ask: Arc<AtomicBool>,
}

async fn fixture(
    tool: &'static str,
    standing: bool,
    plan: Option<Arc<dyn PlanApprovalPolicy>>,
) -> Fixture {
    fixture_with(tool, ARGS_A, tool, standing, plan).await
}

/// `guarded` is the tool name the ask-everything guard intercepts; pass a
/// non-matching name (e.g. `"other"`) to let the call through the guard
/// chain so only the durable gate's own ask remains.
async fn fixture_with(
    tool: &'static str,
    arguments_json: &'static str,
    guarded: &'static str,
    standing: bool,
    plan: Option<Arc<dyn PlanApprovalPolicy>>,
) -> Fixture {
    let store = SqliteStore::memory(64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let hits = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(vec![]));
    let ask = Arc::new(AtomicBool::new(false));

    let definition: ToolDefinition = serde_json::from_value(json!({
        "name": tool, "version": "1", "effect": "write",
        "capability": "fs.write", "description": "mutate",
        "inputSchema": {"type": "object"},
    }))
    .unwrap();
    let executor: Arc<dyn TaskExecutor> = Arc::new(
        ChatExecutor::new(
            Arc::new(Scripted {
                requests: requests.clone(),
                ask: ask.clone(),
                tool,
                arguments_json,
            }),
            Arc::new(ToolRegistry::new()),
        )
        .with_options(ChatExecutorOptions {
            tool_contributors: vec![Arc::new(WriterContributor {
                tool: Arc::new(Mutate {
                    definition,
                    hits: hits.clone(),
                }),
            })],
            tool_guards: vec![Arc::new(AskGuard {
                tool: guarded,
                requested_mode: if plan.is_some() {
                    Some("default")
                } else {
                    None
                },
            })],
            standing: standing.then(|| Arc::new(KeyPolicy) as Arc<dyn StandingApprovalPolicy>),
            ..Default::default()
        }),
    );
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        runtime.clone(),
        HostOptions::default(),
    )
    .unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime,
        plan_approval: plan,
        wake: Some(host.wake_handle()),
    });
    host.set_session_flush_planner(Arc::new(service.clone()))
        .await;
    service
        .create_session(&CreateSessionInput {
            session_id: Some("s1".into()),
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
    Fixture {
        store,
        host,
        service,
        hits,
        requests,
        ask,
    }
}

impl Fixture {
    /// The ask flag must be set BEFORE the provider is consulted.
    fn set_ask(&self, ask: bool) {
        self.ask.store(ask, Ordering::SeqCst);
    }

    async fn send(&self, text: &str, id: &str) -> String {
        let result = self
            .service
            .send(
                "s1",
                &SendInput {
                    content: json!(text),
                    input_id: Some(id.into()),
                    when_busy: WhenBusy::Reject,
                    turn_id: None,
                    on_settled: None,
                },
            )
            .await
            .unwrap();
        match result {
            AgentSendResult::Turn { turn_id, .. } => turn_id,
            AgentSendResult::Queued { .. } => panic!("an idle session starts the turn"),
        }
    }

    async fn status(&self, turn_id: &str) -> TaskStatus {
        self.store.get_task(turn_id).await.unwrap().unwrap().status
    }

    async fn pending(&self, turn_id: &str) -> Vec<agentkit_store::AgentInputRecord> {
        self.store
            .list_inputs(AgentInputQuery {
                session_id: "s1".into(),
                kinds: Some(vec![AgentInputKind::HumanRequest]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: Some(turn_id.to_owned()),
            })
            .await
            .unwrap()
    }

    /// Crash residue: a run submitted, claimed and settled `failed`, leaving
    /// an admitted-without-result receipt for `call-1` of `tool` under
    /// `arguments_json`. Returns the execution id — the uncertainty
    /// generation an answer must name.
    async fn seed_uncertain(&self, run_id: &str, tool: &str, arguments_json: &str) -> String {
        self.host
            .submit_chat(SubmitChat {
                task_id: run_id.into(),
                chat_id: "s1".into(),
                model: "model".into(),
                content: json!("work"),
                parent_message_id: None,
            })
            .await
            .unwrap();
        let claimed = self
            .store
            .claim(ClaimInput {
                owner_id: "crash".into(),
                now: now_ms(),
                ttl_ms: 600_000,
                kinds: vec![CHAT_TURN_TASK_KIND.into()],
                busy_scopes: vec![],
                max_attempts: 5,
            })
            .await
            .unwrap()
            .expect("the seeded run is queued");
        assert_eq!(claimed.task.task_id, run_id);
        let fence = claimed.lease.fence();
        let execution = agentkit_host::execution_id(run_id, "call-1", None);
        self.store
            .admit_execution(
                fence.clone(),
                ExecutionReceiptAdmission {
                    execution_id: execution.clone(),
                    run_id: run_id.into(),
                    chat_id: "s1".into(),
                    tool_call_id: "call-1".into(),
                    tool_name: tool.into(),
                    args_sha256: args_sha256(
                        &serde_json::from_str::<Value>(arguments_json).unwrap(),
                    ),
                    context_sha256: "0".repeat(64),
                    attempt_id: "1".into(),
                    approval_request_id: None,
                    approval_generation: None,
                    now: now_ms(),
                },
            )
            .await
            .unwrap();
        self.store
            .bind_run(
                run_id,
                "s1",
                json!({
                    "providerId": "test",
                    "protocol": "responses",
                    "connectionId": "c1",
                    "generation": 1,
                    "chatId": "s1",
                    "branchId": "b1",
                    "model": "model",
                }),
                fence.clone(),
                now_ms(),
            )
            .await
            .unwrap();
        self.store
            .settle(SettleTask {
                fence,
                now: now_ms(),
                status: TaskStatus::Failed,
                error: Some("simulated process death".into()),
                events: vec![],
                messages: vec![],
            })
            .await
            .unwrap();
        execution
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn a_remembered_approval_runs_later_calls_until_revoked() {
    let f = fixture("write_file", true, None).await;
    // The provider needs `ask` on BEFORE the stream is consulted; the
    // fixture's Scripted reads the shared flag directly.
    f.set_ask(true);
    let turn = f.send("first", "t1").await;
    f.host.run_once().await.unwrap();
    assert_eq!(f.status(&turn).await, TaskStatus::WaitingApproval);
    let request = f.pending(&turn).await.into_iter().next().unwrap();
    let digest = args_sha256(&serde_json::from_str::<Value>(ARGS_A).unwrap());
    assert_eq!(request.content["standing"]["digest"], digest);

    f.service
        .approve(&request.input_id, "ans-1", None, Some(true))
        .await
        .unwrap();
    f.set_ask(false);
    f.host.run_once().await.unwrap();
    assert_eq!(f.status(&turn).await, TaskStatus::Completed);
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);

    let grants = f.service.list_approval_grants("s1").await.unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0]["digest"], digest);
    assert_eq!(grants[0]["grantId"], format!("grant:{}", request.input_id));
    assert!(grants[0].get("revokedAt").is_none_or(Value::is_null));

    // A later identical call runs under the grant: no park, no new request.
    f.set_ask(true);
    let turn2 = f.send("second", "t2").await;
    f.host.run_once().await.unwrap();
    assert_eq!(f.status(&turn2).await, TaskStatus::Completed);
    assert_eq!(f.hits.load(Ordering::SeqCst), 2);
    assert!(f.pending(&turn2).await.is_empty());

    // Revoked: the same call asks again.
    f.service
        .revoke_approval_grant("s1", grants[0]["grantId"].as_str().unwrap())
        .await
        .unwrap();
    let grants = f.service.list_approval_grants("s1").await.unwrap();
    assert!(grants[0]["revokedAt"].is_string(), "revocation is recorded");
    let turn3 = f.send("third", "t3").await;
    f.host.run_once().await.unwrap();
    assert_eq!(f.status(&turn3).await, TaskStatus::WaitingApproval);
    assert_eq!(f.pending(&turn3).await.len(), 1);
    assert_eq!(
        f.hits.load(Ordering::SeqCst),
        2,
        "the third call has not run"
    );
}

#[tokio::test]
async fn an_approved_plan_escalates_the_sessions_mode() {
    let policy = Arc::new(ModePolicy {
        tool: "submit_plan",
        granted: "default",
        deny_with: Mutex::new(None),
    });
    let f = fixture("submit_plan", false, Some(policy)).await;
    f.set_ask(true);
    let turn = f.send("plan it", "t1").await;
    f.host.run_once().await.unwrap();
    assert_eq!(f.status(&turn).await, TaskStatus::WaitingApproval);
    let request = f.pending(&turn).await.into_iter().next().unwrap();
    assert_eq!(request.content["kind"], "approval");
    assert_eq!(request.content["toolName"], "submit_plan");
    assert_eq!(request.content["requestedMode"], "default");

    f.service
        .approve(&request.input_id, "ans-1", None, None)
        .await
        .unwrap();
    let execution = f
        .service
        .get_execution("s1")
        .await
        .unwrap()
        .expect("the grant is durable");
    assert_eq!(execution["mode"], "default");
    f.set_ask(false);
    f.host.run_once().await.unwrap();
    assert_eq!(f.status(&turn).await, TaskStatus::Completed);
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_plan_vetoed_by_the_policy_is_refused_and_never_runs() {
    let policy = Arc::new(ModePolicy {
        tool: "submit_plan",
        granted: "default",
        deny_with: Mutex::new(Some("role ceiling".into())),
    });
    let f = fixture("submit_plan", false, Some(policy)).await;
    f.set_ask(true);
    let turn = f.send("plan it", "t1").await;
    f.host.run_once().await.unwrap();
    let request = f.pending(&turn).await.into_iter().next().unwrap();
    f.service
        .approve(&request.input_id, "ans-1", None, None)
        .await
        .unwrap();
    assert!(f.service.get_execution("s1").await.unwrap().is_none());
    f.set_ask(false);
    f.host.run_once().await.unwrap();
    assert_eq!(f.status(&turn).await, TaskStatus::Completed);
    assert_eq!(
        f.hits.load(Ordering::SeqCst),
        0,
        "the vetoed plan never ran"
    );
    let last = f.requests.lock().unwrap().last().unwrap().clone();
    let tool = last
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "tool")
        .expect("the veto answers the call");
    let envelope: Value =
        serde_json::from_str(tool.content.as_str().expect("envelope string")).unwrap();
    assert_eq!(envelope["data"]["errorCode"], "approval_denied");
    assert_eq!(
        envelope["data"]["errorMessage"],
        "plan_not_authorized: role ceiling"
    );
}

#[tokio::test]
async fn plan_mode_is_never_a_grantable_destination() {
    let policy = Arc::new(ModePolicy {
        tool: "submit_plan",
        granted: "plan",
        deny_with: Mutex::new(None),
    });
    let f = fixture("submit_plan", false, Some(policy)).await;
    f.set_ask(true);
    let turn = f.send("plan it", "t1").await;
    f.host.run_once().await.unwrap();
    let request = f.pending(&turn).await.into_iter().next().unwrap();
    let error = f
        .service
        .approve(&request.input_id, "ans-1", None, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "invalid_input");
    // Nothing was consumed: the request still awaits a legitimate answer.
    assert_eq!(f.pending(&turn).await.len(), 1);
}

#[tokio::test]
async fn changed_arguments_are_refused_on_the_record_not_run() {
    let f = fixture("write_file", false, None).await;
    f.set_ask(true);
    let turn = f.send("first", "t1").await;
    f.host.run_once().await.unwrap();
    assert_eq!(f.status(&turn).await, TaskStatus::WaitingApproval);

    // A second request for the same call that approves DIFFERENT arguments.
    let changed = json!({"path": "b.txt"});
    f.store
        .create_input(
            CreateAgentInput {
                input_id: "hreq-mismatch".into(),
                session_id: "s1".into(),
                kind: AgentInputKind::HumanRequest,
                content: json!({
                    "kind": "approval",
                    "toolName": "write_file",
                    "argsSha256": args_sha256(&changed),
                    "prompt": "approve the other call?",
                }),
                content_hash: args_sha256(&changed),
                target_task_id: Some(turn.clone()),
                on_settled: None,
                attempt_id: Some("attempt-1".into()),
                tool_call_id: Some("call-1".into()),
                expires_at: None,
            },
            1_700_000_000_000,
        )
        .await
        .unwrap();
    f.service
        .approve("hreq-mismatch", "ans-m", None, None)
        .await
        .unwrap();
    f.set_ask(false);
    f.host.run_once().await.unwrap();
    assert_eq!(f.status(&turn).await, TaskStatus::Completed);
    assert_eq!(
        f.hits.load(Ordering::SeqCst),
        0,
        "a changed call never runs"
    );

    let last = f.requests.lock().unwrap().last().unwrap().clone();
    let tool = last
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "tool" && message.tool_call_id.as_deref() == Some("call-1"))
        .expect("the mismatch answers the call");
    assert_eq!(tool.content["data"]["errorCode"], "approval_mismatch");

    // The real ask is untouched: its answer is still owed.
    assert_eq!(f.pending(&turn).await.len(), 1);
}

#[tokio::test]
async fn an_uncertain_effect_parks_until_a_generation_bound_approval() {
    // Crash residue leaves `call-1` admitted without a result. The next run
    // of the SAME operation is uncertain: it never re-executes unasked, it
    // parks with a request that names the exact unresolved generation, and
    // only that answer opens the store's pending-execution gate.
    let f = fixture_with("write_file", ARGS_A, "other", false, None).await;
    let generation = f.seed_uncertain("run-old", "write_file", ARGS_A).await;

    f.set_ask(true);
    let turn = f.send("mutate it", "t1").await;
    f.host.run_once().await.unwrap();
    assert_eq!(f.status(&turn).await, TaskStatus::WaitingApproval);
    assert_eq!(
        f.hits.load(Ordering::SeqCst),
        0,
        "an uncertain effect is never re-executed without an answer"
    );
    let request = f.pending(&turn).await.into_iter().next().unwrap();
    assert_eq!(request.content["kind"], "approval");
    assert_eq!(request.content["toolName"], "write_file");
    assert_eq!(request.content["uncertaintyGeneration"], generation);
    assert_eq!(
        request.content["argsSha256"],
        args_sha256(&json!({"path": "a.txt"}))
    );

    f.service
        .approve(&request.input_id, "ans-1", None, None)
        .await
        .unwrap();
    f.set_ask(false);
    f.host.run_once().await.unwrap();
    assert_eq!(f.status(&turn).await, TaskStatus::Completed);
    assert_eq!(
        f.hits.load(Ordering::SeqCst),
        1,
        "the approved re-run executed"
    );

    // The re-run's receipt completes; the crashed generation stays on the
    // record as admitted evidence — completion is earned, not overwritten.
    let receipts = f.store.list_executions_by_chat("s1".into()).await.unwrap();
    assert!(
        receipts
            .iter()
            .any(|record| record.execution_id == generation
                && record.status == ExecutionReceiptStatus::Admitted),
        "the uncertain generation remains admitted evidence"
    );
    assert!(
        receipts
            .iter()
            .any(|record| record.run_id == turn
                && record.status == ExecutionReceiptStatus::Completed),
        "the approved re-run completed on the record"
    );
}

#[tokio::test]
async fn a_later_uncertainty_requires_a_new_answer() {
    let f = fixture_with("write_file", ARGS_A, "other", false, None).await;
    let first = f.seed_uncertain("run-old", "write_file", ARGS_A).await;
    f.set_ask(true);
    let turn = f.send("mutate it", "t1").await;
    f.host.run_once().await.unwrap();
    let request = f.pending(&turn).await.into_iter().next().unwrap();
    f.service
        .approve(&request.input_id, "ans-1", None, None)
        .await
        .unwrap();
    f.set_ask(false);
    f.host.run_once().await.unwrap();
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);

    // A second crash leaves the same operation uncertain again, under a NEW
    // generation. The answer that authorized the first re-run is spent and
    // names a different execution: it must not silently open this one.
    let second = f.seed_uncertain("run-old-2", "write_file", ARGS_A).await;
    assert_ne!(first, second);
    f.set_ask(true);
    let turn2 = f.send("mutate again", "t2").await;
    f.host.run_once().await.unwrap();
    assert_eq!(
        f.status(&turn2).await,
        TaskStatus::WaitingApproval,
        "an answered generation cannot authorize a newer uncertainty"
    );
    assert_eq!(f.hits.load(Ordering::SeqCst), 1, "still not executed");
    let request = f.pending(&turn2).await.into_iter().next().unwrap();
    assert_eq!(request.content["uncertaintyGeneration"], second);

    f.service
        .approve(&request.input_id, "ans-2", None, None)
        .await
        .unwrap();
    f.set_ask(false);
    f.host.run_once().await.unwrap();
    assert_eq!(f.status(&turn2).await, TaskStatus::Completed);
    assert_eq!(
        f.hits.load(Ordering::SeqCst),
        2,
        "the new answer opened the new generation"
    );
}

#[tokio::test]
async fn approvals_survive_every_javascript_number_spelling() {
    // One JavaScript-semantic parser at every digest site: what the
    // dispatch hashed must be what the resume re-derives, or an unchanged
    // call is refused as `approval_mismatch` — the `1.0` defect.
    for arguments_json in [
        "{\"timeoutSec\":1.0}",
        "{\"z\":-0}",
        "{\"e\":1e2}",
        "{\"big\":1e400}",
        "{\"tiny\":5e-324}",
        "{\"2\":\"b\",\"1\":\"a\",\"x\":{}}",
        "{\"😀key\":true,\"e\u{0301}\":1.0}",
    ] {
        let f = fixture_with("write_file", arguments_json, "write_file", false, None).await;
        f.set_ask(true);
        let turn = f.send("numeric", "t1").await;
        f.host.run_once().await.unwrap();
        assert_eq!(f.status(&turn).await, TaskStatus::WaitingApproval);
        let request = f.pending(&turn).await.into_iter().next().unwrap();
        f.service
            .approve(&request.input_id, "ans-1", None, None)
            .await
            .unwrap();
        f.set_ask(false);
        f.host.run_once().await.unwrap();
        assert_eq!(
            f.status(&turn).await,
            TaskStatus::Completed,
            "{arguments_json}"
        );
        assert_eq!(
            f.hits.load(Ordering::SeqCst),
            1,
            "{arguments_json} is the operation the human approved, unchanged"
        );
    }
}
