use super::*;
use agentkit_core::{DispatchFence, DispatchGuard, Tool, ToolLimits, ToolRegistry};
use agentkit_store::{ClaimInput, SqliteStore, SubmitTask};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct Body {
    definition: ToolDefinition,
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Body {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"ran": true})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
            model_content: None,
        })
    }
}

/// Passes the dispatch check but cancels the run on the way out: the
/// cancellation lands exactly between the guard chain and admission.
#[derive(Debug)]
struct CancelsOnCheck(CancellationToken);

#[async_trait]
impl DispatchGuard for CancelsOnCheck {
    async fn check(&self) -> Result<(), CoreError> {
        self.0.cancel();
        Ok(())
    }
    fn fence(&self) -> DispatchFence {
        DispatchFence {
            task_id: "run".into(),
            attempt_id: "attempt".into(),
            fencing_token: 1,
        }
    }
}

fn definition(name: &str, effect: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        version: "1".into(),
        effect: effect.into(),
        capability: "test".into(),
        description: String::new(),
        input_schema: json!({"type": "object"}),
        output_schema: None,
        timeout_ms: None,
    }
}

async fn claimed_context() -> (SqliteStore, ExecutionContext) {
    let store = SqliteStore::memory(8).unwrap();
    // Wall-clock time so `SystemRuntime` never sees the lease as expired.
    let now = crate::Runtime::now_ms(&crate::SystemRuntime);
    store
        .create_chat("chat", None, json!({}), now)
        .await
        .unwrap();
    store
        .submit(SubmitTask {
            task_id: "run".into(),
            kind: "chat.turn".into(),
            scope_id: "chat".into(),
            payload: json!({"chatId": "chat"}),
            priority: 0,
            now,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .unwrap();
    let claimed = store
        .claim_with_concurrency(
            ClaimInput {
                owner_id: "worker".into(),
                now,
                ttl_ms: 1_000_000_000,
                kinds: vec![],
                busy_scopes: vec![],
                max_attempts: 3,
            },
            1,
        )
        .await
        .unwrap()
        .expect("the queued turn claims");
    let context = ExecutionContext {
        task: claimed.task,
        attempt_id: claimed.lease.attempt_id.clone(),
        cancellation: CancellationToken::new(),
        interruption: CancellationToken::new(),
        suspension: Arc::new(Mutex::new(None)),
        store: store.clone(),
        fence: claimed.lease.fence(),
        runtime: Arc::new(crate::SystemRuntime),
        event_gate: Arc::new(Mutex::new(())),
    };
    (store, context)
}

fn durable(
    context: &ExecutionContext,
    is_mutating: Option<Arc<MutatingPolicy>>,
) -> Arc<DurableExecution> {
    Arc::new(DurableExecution {
        context: context.clone(),
        chat_id: "chat".into(),
        is_mutating,
        approvals: Arc::new(Default::default()),
        standing: None,
    })
}

fn contribution(durable: &Arc<DurableExecution>) -> ToolContributionContext {
    ToolContributionContext {
        chat_id: Some("chat".into()),
        run_id: "run".into(),
        scope_id: None,
        actor: Value::Null,
        actor_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: CancellationToken::new(),
        has_primary_binding: false,
        timeout_ms: 1_000,
        durable: Some(durable.clone()),
    }
}

fn tool_context(context: &ExecutionContext, call_id: &str) -> ToolContext {
    ToolContext {
        run_id: "run".into(),
        tool_call_id: Some(call_id.into()),
        attempt_id: Some(context.attempt_id.clone()),
        chat_id: Some("chat".into()),
        scope_id: None,
        user_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: context.cancellation.clone(),
        dispatch_guard: None,
        metadata: Default::default(),
        suspension: None,
    }
}

fn wrapped(tool: Arc<dyn Tool>, durable: &Arc<DurableExecution>) -> agentkit_core::ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(tool).unwrap();
    wrap_static_writes(&registry, durable.clone(), &contribution(durable))
}

#[tokio::test]
async fn cancellation_before_admission_leaves_no_admitted_receipt() {
    let (store, context) = claimed_context().await;
    let durable = durable(&context, None);
    let runs = Arc::new(AtomicUsize::new(0));
    let registry = wrapped(
        Arc::new(Body {
            definition: definition("mutate", "write"),
            runs: runs.clone(),
        }),
        &durable,
    );
    let mut call = tool_context(&context, "call-1");
    call.dispatch_guard = Some(Arc::new(CancelsOnCheck(context.cancellation.clone())));
    let error = registry
        .get("mutate")
        .unwrap()
        .execute(call, json!({}))
        .await
        .expect_err("cancelled before admission");
    assert_eq!(error.code, "cancelled");
    assert_eq!(runs.load(Ordering::SeqCst), 0, "the body never ran");
    assert!(
        store
            .list_executions("run".into())
            .await
            .unwrap()
            .is_empty(),
        "no Admitted receipt may outlive a cancel that preceded admission"
    );
}

#[tokio::test]
async fn admitted_calls_run_their_body_and_complete_the_receipt() {
    let (store, context) = claimed_context().await;
    let durable = durable(&context, None);
    let runs = Arc::new(AtomicUsize::new(0));
    let registry = wrapped(
        Arc::new(Body {
            definition: definition("mutate", "write"),
            runs: runs.clone(),
        }),
        &durable,
    );
    let result = registry
        .get("mutate")
        .unwrap()
        .execute(tool_context(&context, "call-1"), json!({}))
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let receipts = store.list_executions("run".into()).await.unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        receipts[0].status,
        agentkit_store::ExecutionReceiptStatus::Completed
    );
}

#[tokio::test]
async fn static_wrapper_follows_the_mutating_policy_not_effect_alone() {
    let (store, context) = claimed_context().await;
    let policy: Arc<MutatingPolicy> = Arc::new(|name: &str| name == "flagged_read");
    let durable = durable(&context, Some(policy));
    let flagged_runs = Arc::new(AtomicUsize::new(0));
    let plain_runs = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(Body {
            definition: definition("flagged_read", "read"),
            runs: flagged_runs.clone(),
        }))
        .unwrap();
    registry
        .register(Arc::new(Body {
            definition: definition("plain_read", "read"),
            runs: plain_runs.clone(),
        }))
        .unwrap();
    let wrapped = wrap_static_writes(&registry, durable.clone(), &contribution(&durable));
    for (name, call) in [("flagged_read", "c1"), ("plain_read", "c2")] {
        wrapped
            .get(name)
            .unwrap()
            .execute(tool_context(&context, call), json!({}))
            .await
            .unwrap();
    }
    let receipts = store.list_executions("run".into()).await.unwrap();
    assert_eq!(
        receipts.len(),
        1,
        "only the policy-flagged read is receipted"
    );
    assert_eq!(receipts[0].tool_name, "flagged_read");
    assert_eq!(flagged_runs.load(Ordering::SeqCst), 1);
    assert_eq!(plain_runs.load(Ordering::SeqCst), 1);
}

// -- AGENTKIT-73: the static fast path must have an audited resolve route.
//    A crash leaves an admitted receipt; until an operator resolves it the
//    identical effect is refused on every retry (never auto-replayed), and
//    once resolved the retry proceeds.

fn retry_contribution(durable: &Arc<DurableExecution>) -> ToolContributionContext {
    ToolContributionContext {
        run_id: "run2".into(),
        ..contribution(durable)
    }
}

/// A crashed attempt: the effect is admitted under `run`'s lease, its result
/// never recorded, and the run settles as dead — process death between the
/// external write and the durable result.
async fn crash_with_uncertain_effect(
    store: &SqliteStore,
    context: &ExecutionContext,
    input: &Value,
) -> String {
    let now = crate::Runtime::now_ms(&crate::SystemRuntime);
    let execution = execution_id("run", "call-1", None);
    store
        .admit_execution(
            context.fence.clone(),
            agentkit_store::ExecutionReceiptAdmission {
                execution_id: execution.clone(),
                run_id: "run".into(),
                chat_id: "chat".into(),
                tool_call_id: "call-1".into(),
                tool_name: "mutate".into(),
                args_sha256: args_sha256(input),
                ts_args_sha256: Some(ts_args_sha256(input)),
                args_hash_provenance: agentkit_store::ArgsHashProvenance::Canonical,
                context_sha256: "0".repeat(64),
                attempt_id: context.attempt_id.clone(),
                approval_request_id: None,
                approval_generation: None,
                now,
            },
        )
        .await
        .unwrap();
    store
        .bind_run(
            "run",
            "chat",
            json!({
                "providerId": "test", "protocol": "responses",
                "connectionId": "c1", "generation": 1,
                "chatId": "chat", "branchId": "b1", "model": "model",
            }),
            context.fence.clone(),
            now,
        )
        .await
        .unwrap();
    store
        .settle(agentkit_store::SettleTask {
            fence: context.fence.clone(),
            now,
            status: agentkit_store::TaskStatus::Failed,
            error: Some("simulated process death".into()),
            events: vec![],
            messages: vec![],
        })
        .await
        .unwrap();
    execution
}

/// The retry pass: a fresh run of the same chat, as the host re-asks after a
/// crash. Returns its claimed execution context.
async fn retry_context(store: &SqliteStore) -> ExecutionContext {
    let now = crate::Runtime::now_ms(&crate::SystemRuntime);
    store
        .submit(SubmitTask {
            task_id: "run2".into(),
            kind: "chat.turn".into(),
            scope_id: "chat".into(),
            payload: json!({"chatId": "chat"}),
            priority: 0,
            now,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .unwrap();
    let claimed = store
        .claim_with_concurrency(
            ClaimInput {
                owner_id: "worker".into(),
                now,
                ttl_ms: 1_000_000_000,
                kinds: vec![],
                busy_scopes: vec![],
                max_attempts: 3,
            },
            1,
        )
        .await
        .unwrap()
        .expect("the retry run claims");
    ExecutionContext {
        task: claimed.task,
        attempt_id: claimed.lease.attempt_id.clone(),
        cancellation: CancellationToken::new(),
        interruption: CancellationToken::new(),
        suspension: Arc::new(Mutex::new(None)),
        store: store.clone(),
        fence: claimed.lease.fence(),
        runtime: Arc::new(crate::SystemRuntime),
        event_gate: Arc::new(Mutex::new(())),
    }
}

async fn mutate_registry(
    store: &SqliteStore,
    runs: Arc<AtomicUsize>,
) -> (
    Arc<DurableExecution>,
    ExecutionContext,
    agentkit_core::ToolRegistry,
) {
    let context = retry_context(store).await;
    let durable = durable(&context, None);
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(Body {
            definition: definition("mutate", "write"),
            runs,
        }))
        .unwrap();
    let wrapped = wrap_static_writes(&registry, durable.clone(), &retry_contribution(&durable));
    (durable, context, wrapped)
}

#[tokio::test]
async fn static_fast_path_uncertain_effect_is_never_replayed_without_resolution() {
    let (store, context) = claimed_context().await;
    let input = json!({"path": "a.txt"});
    let execution = crash_with_uncertain_effect(&store, &context, &input).await;

    let runs = Arc::new(AtomicUsize::new(0));
    let (_durable, retry, registry) = mutate_registry(&store, runs.clone()).await;
    for attempt in 0..2 {
        let refusal = registry
            .get("mutate")
            .unwrap()
            .execute(tool_context(&retry, "call-2"), input.clone())
            .await
            .unwrap();
        assert!(
            !refusal.ok,
            "attempt {attempt}: an unresolved uncertain effect must not proceed"
        );
        let data = refusal.data.expect("the refusal carries model data");
        assert_eq!(data["errorCode"], json!("tool_result_uncertain"));
        assert_eq!(
            data["approvalGeneration"],
            json!(execution),
            "the refusal names the exact generation an operator must resolve"
        );
    }
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "an uncertain effect is never replayed"
    );
    let receipts = store.list_executions_by_chat("chat".into()).await.unwrap();
    assert_eq!(receipts.len(), 1, "the retries admitted nothing new");
    assert_eq!(
        receipts[0].status,
        agentkit_store::ExecutionReceiptStatus::Admitted
    );
}

#[tokio::test]
async fn static_fast_path_uncertain_receipt_resolves_then_the_retry_proceeds() {
    let (store, context) = claimed_context().await;
    let input = json!({"path": "a.txt"});
    let execution = crash_with_uncertain_effect(&store, &context, &input).await;

    let runs = Arc::new(AtomicUsize::new(0));
    let (_durable, retry, registry) = mutate_registry(&store, runs.clone()).await;
    let refusal = registry
        .get("mutate")
        .unwrap()
        .execute(tool_context(&retry, "call-2"), input.clone())
        .await
        .unwrap();
    assert!(!refusal.ok);

    // The session surface is not the static path's resolve route: this chat
    // was never created as a session, and resolution must not pretend it was.
    let service = crate::AgentSessionService::new(crate::AgentSessionServiceDeps {
        store: store.clone(),
        runtime: Arc::new(crate::SystemRuntime),
        plan_approval: None,
        wake: None,
    });
    let dead_end = service
        .resolve_uncertain_execution(
            "chat",
            &execution,
            agentkit_store::ResolutionOutcome::NotStarted,
            None,
            None,
            None,
        )
        .await
        .expect_err("a sessionless chat is not resolvable through the session surface");
    assert_eq!(dead_end.code, "session_not_found");

    // The host-owned resolve route: explicit, audited, and the only thing
    // that ever unblocks the effect.
    let now = crate::Runtime::now_ms(&crate::SystemRuntime);
    let record = agentkit_sessions::resolve_uncertain_execution(
        &store,
        "chat",
        &execution,
        agentkit_store::ResolutionOutcome::NotStarted,
        None,
        Some("verified: the file is absent on the target volume".into()),
        Some("operator@site".into()),
        now,
    )
    .await
    .expect("the operator closes the uncertain receipt");
    assert_eq!(
        record.status,
        agentkit_store::ExecutionReceiptStatus::Resolved
    );
    let resolution = record
        .resolution
        .expect("resolution is audited inside the receipt");
    assert_eq!(
        resolution.note.as_deref(),
        Some("verified: the file is absent on the target volume")
    );
    assert_eq!(resolution.resolved_by.as_deref(), Some("operator@site"));
    assert_eq!(resolution.resolved_at, now);

    // Resolved: the retry proceeds and the body finally runs — once.
    let result = registry
        .get("mutate")
        .unwrap()
        .execute(tool_context(&retry, "call-2"), input)
        .await
        .unwrap();
    assert!(result.ok);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let receipts = store.list_executions("run2".into()).await.unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        receipts[0].status,
        agentkit_store::ExecutionReceiptStatus::Completed
    );
}
