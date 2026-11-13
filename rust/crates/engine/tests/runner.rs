use agentkit_core::HumanRequestSpec;
use agentkit_engine::{
    Runtime, SessionFlushPlanner, SubmitTask, TaskExecutor, TaskRecord, TaskStatus,
    classify_execution_error,
};
use agentkit_store::{
    AgentInputKind, AnswerHumanRequest, CreateAgentInput, ParkTurn, SessionSettle, SqliteStore,
    StoreError,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

type AgentHost = agentkit_engine::AgentHost<SqliteStore>;
type ExecutionContext = agentkit_engine::ExecutionContext<SqliteStore>;
type EngineError = agentkit_engine::EngineError<StoreError>;
type EngineResult<T> = Result<T, EngineError>;
type HostOptions = agentkit_engine::HostOptions<SqliteStore>;

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
impl TaskExecutor<SqliteStore> for Silent {
    async fn execute(&self, _context: ExecutionContext) -> EngineResult<()> {
        Ok(())
    }
}

struct Counter(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl TaskExecutor<SqliteStore> for Counter {
    async fn execute(&self, _context: ExecutionContext) -> EngineResult<()> {
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

/// The parking executor takes the suspension guard, commits a durable park
/// (which deletes the lease) and keeps the guard through several heartbeat
/// ticks before publishing the suspension: the exact park/heartbeat
/// interleaving, recreated without relying on scheduler luck.
struct Parker {
    runs: AtomicUsize,
    seen: std::sync::Mutex<Vec<(String, String)>>,
    cancelled: std::sync::Mutex<Vec<bool>>,
}
#[async_trait::async_trait]
impl TaskExecutor<SqliteStore> for Parker {
    async fn execute(&self, context: ExecutionContext) -> EngineResult<()> {
        self.seen.lock().unwrap().push((
            context.fence.lease_token.clone(),
            context.attempt_id.clone(),
        ));
        if self.runs.fetch_add(1, Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(60)).await;
            return Ok(());
        }
        let request = HumanRequestSpec {
            request_id: "request".into(),
            content: json!({"kind":"approval", "toolName":"write_file", "argsSha256":"a".repeat(64), "prompt":"write"}),
            content_hash: "request-hash".into(),
            expires_at: None,
        };
        let mut suspension = context.suspension.lock().await;
        context
            .store
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
            .await?;
        // Several heartbeat ticks fire while the guard is held and the lease
        // is already gone; none may report theft or wait on the guard.
        tokio::time::sleep(Duration::from_millis(100)).await;
        *suspension = Some(request);
        drop(suspension);
        // A published suspension no longer owns a lease either.
        tokio::time::sleep(Duration::from_millis(60)).await;
        self.cancelled
            .lock()
            .unwrap()
            .push(context.cancellation.is_cancelled());
        Ok(())
    }
}

fn turn_task() -> SubmitTask {
    SubmitTask {
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
    }
}

fn short_lease() -> HostOptions {
    // A 30 ms TTL heartbeats every 10 ms; the fake clock never moves on its
    // own, so expiry is decided only by what the test does to it.
    HostOptions {
        lease_ttl_ms: 30,
        ..Default::default()
    }
}

#[tokio::test]
async fn heartbeat_during_committed_park_does_not_lose_the_worker_and_answer_resumes() {
    let store = SqliteStore::memory(16).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1_000)));
    store
        .create_chat("chat", None, json!({}), 1_000)
        .await
        .unwrap();
    let parker = Arc::new(Parker {
        runs: AtomicUsize::new(0),
        seen: Default::default(),
        cancelled: Default::default(),
    });
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(
            "chat.turn".to_owned(),
            parker.clone() as Arc<dyn TaskExecutor<SqliteStore>>,
        )]),
        clock.clone(),
        short_lease(),
    )
    .unwrap();
    host.submit(turn_task()).await.unwrap();
    assert!(
        host.run_once()
            .await
            .expect("a committed park is not lease theft"),
        "the turn is claimed"
    );
    assert_eq!(
        store.get_task("turn").await.unwrap().unwrap().status,
        TaskStatus::WaitingApproval
    );
    assert_eq!(*parker.cancelled.lock().unwrap(), vec![false]);

    store
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
    assert!(host.run_once().await.unwrap(), "answer resumes the turn");
    let seen = parker.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_ne!(seen[0].0, seen[1].0);
    assert_ne!(seen[0].1, seen[1].1);
}

/// Runs until its lease is taken away, keeping the cancellation token so the
/// test can observe that the old context was cancelled.
struct Stall {
    replace: bool,
    clock: Arc<Clock>,
    token: std::sync::Mutex<Option<tokio_util::sync::CancellationToken>>,
}
#[async_trait::async_trait]
impl TaskExecutor<SqliteStore> for Stall {
    async fn execute(&self, context: ExecutionContext) -> EngineResult<()> {
        *self.token.lock().unwrap() = Some(context.cancellation.clone());
        if self.replace {
            context
                .store
                .acquire_lease("turn", "other-attempt", "other-worker", 1_000, 30_000)
                .await?;
        } else {
            self.clock.0.store(31_000, Ordering::SeqCst);
        }
        context.cancellation.cancelled().await;
        Ok(())
    }
}

/// Coordination with an intentional park must not turn an actually expired
/// or replaced lease into success. Those cases still cancel the old context.
#[tokio::test]
async fn heartbeat_still_refuses_expired_and_replaced_leases() {
    for replace in [false, true] {
        let store = SqliteStore::memory(16).unwrap();
        let clock = Arc::new(Clock(AtomicI64::new(1_000)));
        store
            .create_chat("chat", None, json!({}), 1_000)
            .await
            .unwrap();
        let stall = Arc::new(Stall {
            replace,
            clock: clock.clone(),
            token: Default::default(),
        });
        let host = AgentHost::new(
            store.clone(),
            HashMap::from([(
                "chat.turn".to_owned(),
                stall.clone() as Arc<dyn TaskExecutor<SqliteStore>>,
            )]),
            clock.clone(),
            short_lease(),
        )
        .unwrap();
        host.submit(turn_task()).await.unwrap();
        assert!(matches!(
            host.run_once().await,
            Err(EngineError::Store(StoreError::LeaseLost(_)))
        ));
        assert!(stall.token.lock().unwrap().as_ref().unwrap().is_cancelled());
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
            Arc::new(Silent) as Arc<dyn TaskExecutor<SqliteStore>>,
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
        matches!(&error, EngineError::NoExecutorForKind(kind) if kind == "nobody.registered"),
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
        Err(EngineError::NoExecutorForKind(kind)) if kind == "runnable"
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
            Arc::new(Counter(ran.clone())) as Arc<dyn TaskExecutor<SqliteStore>>,
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
impl SessionFlushPlanner<SqliteStore> for ConflictFor {
    async fn settle_plan(
        &self,
        settling: &TaskRecord,
        _completed: bool,
    ) -> EngineResult<SessionSettle> {
        Ok(SessionSettle {
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
            Arc::new(Counter(ran.clone())) as Arc<dyn TaskExecutor<SqliteStore>>,
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
