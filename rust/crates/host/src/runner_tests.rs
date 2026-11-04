use super::*;
use agentkit_core::HumanRequestSpec;
use agentkit_store::{AgentInputKind, AnswerHumanRequest, CreateAgentInput, ParkTurn};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

struct Clock(AtomicI64);
impl Runtime for Clock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    fn id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

/// The task the fixture claims is never executed through an executor (the
/// tests drive the context directly), but the kind must be registered for
/// admission and the claim filter to accept it.
struct Silent;
#[async_trait::async_trait]
impl TaskExecutor for Silent {
    async fn execute(&self, _context: ExecutionContext) -> HostResult<()> {
        Ok(())
    }
}

struct Counter(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl TaskExecutor for Counter {
    async fn execute(&self, _context: ExecutionContext) -> HostResult<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn queued_task(id: &str, kind: &str) -> SubmitTask {
    SubmitTask {
        task_id: id.into(),
        kind: kind.into(),
        scope_id: id.into(),
        payload: json!({}),
        priority: 0,
        now: 1_000,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: false,
    }
}

async fn fixture() -> (AgentHost, ExecutionContext, Arc<Clock>) {
    let store = SqliteStore::memory(16).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1_000)));
    store
        .create_chat("chat", None, json!({}), 1_000)
        .await
        .unwrap();
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(
            "chat.turn".to_owned(),
            Arc::new(Silent) as Arc<dyn TaskExecutor>,
        )]),
        clock.clone(),
        HostOptions::default(),
    )
    .unwrap();
    host.submit(SubmitTask {
        task_id: "turn".into(),
        kind: "chat.turn".into(),
        scope_id: "chat".into(),
        payload: json!({"chatId":"chat"}),
        priority: 0,
        now: 1_000,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: true,
    })
    .await
    .unwrap();
    let context = host.claim_context().await.unwrap().unwrap();
    (host, context, clock)
}

/// Recreates the precise park/heartbeat interleaving without sleeps: the
/// store has deleted the lease, but the parking executor has not published
/// its suspension yet. Renewal must skip this transition rather than report
/// a stolen lease, and must not wait on the mutex held by the executor that
/// the same select loop needs to keep polling.
#[tokio::test]
async fn heartbeat_during_committed_park_does_not_lose_the_worker_and_answer_resumes() {
    let (host, context, clock) = fixture().await;
    let request = HumanRequestSpec {
        request_id: "request".into(),
        content: json!({"kind":"approval", "toolName":"write_file", "argsSha256":"a".repeat(64), "prompt":"write"}),
        content_hash: "request-hash".into(),
        expires_at: None,
    };
    let mut suspension = context.suspension.lock().await;
    host.store
        .park_turn(ParkTurn {
            fence: context.fence.clone(),
            assistant_message: None,
            events: vec![],
            now: 1_000,
            request: Some(CreateAgentInput {
                input_id: request.request_id.clone(),
                session_id: "chat".into(),
                kind: AgentInputKind::HumanRequest,
                content: request.content.clone(),
                content_hash: request.content_hash.clone(),
                target_task_id: Some("turn".into()),
                on_settled: None,
                attempt_id: Some(context.attempt_id.clone()),
                tool_call_id: Some("call".into()),
                expires_at: None,
            }),
        })
        .await
        .unwrap();
    assert_eq!(
        host.store.get_task("turn").await.unwrap().unwrap().status,
        TaskStatus::WaitingApproval
    );
    tokio::time::timeout(Duration::from_secs(5), host.heartbeat(&context))
        .await
        .expect("heartbeat must not block the parked executor")
        .expect("a committed park is not lease theft");
    assert!(!context.cancellation.is_cancelled());
    *suspension = Some(request);
    drop(suspension);
    host.heartbeat(&context)
        .await
        .expect("a published suspension no longer owns a lease");

    host.store
        .answer_human_request(AnswerHumanRequest {
            request_id: "request".into(),
            answer_id: "answer".into(),
            response: json!({"decision":"approve"}),
            response_hash: "answer-hash".into(),
            now: 1_001,
            message_id: "answer-message".into(),
            plan_tool_names: vec![],
            policy_denial: None,
            granted_mode: None,
        })
        .await
        .unwrap();
    clock.0.store(1_001, Ordering::SeqCst);
    let resumed = host
        .claim_context()
        .await
        .unwrap()
        .expect("answer resumes the turn");
    assert_ne!(resumed.fence.lease_token, context.fence.lease_token);
    assert_ne!(resumed.attempt_id, context.attempt_id);
    host.heartbeat(&resumed).await.unwrap();
}

/// Coordination with an intentional park must not turn an actually expired
/// or replaced lease into success. Those cases still cancel the old context.
#[tokio::test]
async fn heartbeat_still_refuses_expired_and_replaced_leases() {
    for replace in [false, true] {
        let (host, context, clock) = fixture().await;
        if replace {
            host.store
                .acquire_lease("turn", "other-attempt", "other-worker", 1_000, 30_000)
                .await
                .unwrap();
        } else {
            clock.0.store(31_000, Ordering::SeqCst);
        }
        assert!(matches!(
            host.heartbeat(&context).await,
            Err(HostError::Store(StoreError::LeaseLost(_)))
        ));
        assert!(context.cancellation.is_cancelled());
    }
}

/// Admission is the host's promise to execute: a kind with no registered
/// `TaskExecutor` is refused with the stable `no_executor_for_kind` code
/// BEFORE any row lands — zero tasks, and (for the batch-shaped store
/// admissions the session service drives) zero messages or inputs too. The
/// whole admission is rejected; an admitted-except-the-missing-one split is
/// not the chosen semantics because the store admits a task with its
/// messages atomically, so no partial batch exists to keep.
#[tokio::test]
async fn runner_admission_refuses_unregistered_kind_before_landing_rows() {
    let store = SqliteStore::memory(16).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1_000)));
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(
            "runnable".to_owned(),
            Arc::new(Silent) as Arc<dyn TaskExecutor>,
        )]),
        clock.clone(),
        HostOptions::default(),
    )
    .unwrap();
    let error = match host.submit(queued_task("ghost", "nobody.registered")).await {
        Ok(_) => panic!("a kind without an executor must be refused at admission"),
        Err(error) => error,
    };
    assert!(
        matches!(&error, HostError::NoExecutorForKind(kind) if kind == "nobody.registered"),
        "{error}"
    );
    assert_eq!(
        classify_execution_error(&error).reason,
        "no_executor_for_kind"
    );
    assert!(
        store.get_task("ghost").await.unwrap().is_none(),
        "a refused admission leaves zero rows"
    );
    // The registry check is exposed for host-boundary admission sites, and
    // the executor map is fixed at construction: nothing registers later.
    assert_eq!(host.supported_kinds(), vec!["runnable".to_owned()]);
    assert!(host.admission_gate().supports("runnable"));
    assert!(!host.admission_gate().supports("nobody.registered"));
    // An empty registry admits nothing — not "everything of any kind".
    let empty =
        AgentHost::new(store.clone(), HashMap::new(), clock, HostOptions::default()).unwrap();
    assert!(matches!(
        empty.submit(queued_task("any", "runnable")).await,
        Err(HostError::NoExecutorForKind(kind)) if kind == "runnable"
    ));
    assert!(store.get_task("any").await.unwrap().is_none());
}

/// The claim filter's starvation guarantee: a queued task of a kind this
/// host cannot run is stepped over, never claimed, never failed, never
/// dead-lettered — and never blocks the runnable kinds enqueued behind it.
/// Dead-kind rows are seeded through the store because host admission now
/// refuses them; the queue may still legitimately hold cross-pool work.
#[tokio::test]
async fn runner_claim_steps_over_unrunnable_kinds_without_starving_the_queue() {
    let store = SqliteStore::memory(16).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1_000)));
    store
        .submit(queued_task("dead-first", "nobody.registered"))
        .await
        .unwrap();
    store
        .submit(queued_task("runnable-second", "runnable"))
        .await
        .unwrap();
    let ran = Arc::new(AtomicUsize::new(0));
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(
            "runnable".to_owned(),
            Arc::new(Counter(ran.clone())) as Arc<dyn TaskExecutor>,
        )]),
        clock.clone(),
        HostOptions::default(),
    )
    .unwrap();
    assert!(
        host.run_once().await.unwrap(),
        "the runnable task is claimed even though a dead kind was enqueued first"
    );
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    let dead = store.get_task("dead-first").await.unwrap().unwrap();
    assert_eq!(dead.status, TaskStatus::Queued);
    assert_eq!(dead.attempt_count, 0);
    assert!(dead.dead_lettered_at.is_none());
    assert!(dead.dead_letter_reason.is_none());
    // Nothing runnable remains; and a host with an empty registry must not
    // ask the queue at all — the store reads an EMPTY kinds list as "any".
    assert!(!host.run_once().await.unwrap());
    let empty =
        AgentHost::new(store.clone(), HashMap::new(), clock, HostOptions::default()).unwrap();
    assert!(!empty.run_once().await.unwrap());
    assert_eq!(
        store.get_task("dead-first").await.unwrap().unwrap().status,
        TaskStatus::Queued,
        "the dead-kind task outlives every claim attempt, unchanged"
    );
}

/// Settle plans that always collide with the descriptor fence for one task.
struct ConflictFor(&'static str);
#[async_trait::async_trait]
impl SessionFlushPlanner for ConflictFor {
    async fn settle_plan(&self, settling: &TaskRecord, _completed: bool) -> HostResult<SettlePlan> {
        Ok(SettlePlan {
            // The chat carries no session descriptor, so any expectation
            // fails the store's atomic descriptor check.
            expected_descriptor: (settling.task_id == self.0).then(|| json!({"stale": true})),
            staging: None,
            convert: vec![],
        })
    }
}

/// A settle conflict that outlasts every re-plan is that ONE task's failure:
/// the worker logs it, leaves the lease to expire, and keeps claiming.
#[tokio::test]
async fn worker_survives_an_exhausted_settle_conflict_and_keeps_claiming() {
    let store = SqliteStore::memory(16).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1_000)));
    let ran = Arc::new(AtomicUsize::new(0));
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(
            "runnable".to_owned(),
            Arc::new(Counter(ran.clone())) as Arc<dyn TaskExecutor>,
        )]),
        clock,
        HostOptions {
            // One slot: "good" can only run if the conflicted task freed it.
            concurrency: 1,
            poll_interval_ms: 10,
            ..Default::default()
        },
    )
    .unwrap();
    host.set_session_flush_planner(Arc::new(ConflictFor("bad")))
        .await;
    for id in ["bad", "good"] {
        store.create_chat(id, None, json!({}), 1_000).await.unwrap();
        host.submit(queued_task(id, "runnable")).await.unwrap();
    }
    let worker = host.start_worker();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while store.get_task("good").await.unwrap().unwrap().status != TaskStatus::Completed {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker died on the conflicting task"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(ran.load(Ordering::SeqCst), 2);
    let bad = store.get_task("bad").await.unwrap().unwrap();
    assert_eq!(
        bad.status,
        TaskStatus::Interrupted,
        "the conflicted task is parked resumable, not left leased"
    );
    assert_eq!(bad.error.as_deref(), Some("settle_conflict"));
    worker.shutdown().await.expect("worker exits cleanly");
}
