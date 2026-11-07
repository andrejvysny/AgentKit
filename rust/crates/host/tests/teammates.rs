//! AGENTKIT-88 host acceptance cells: the teammate service boundary on
//! the existing host runner — idempotent competing admission, the
//! archive-rejects-admission gate, reconcile-before-admission, the
//! derived display state, the mailbox, and a single-worker parent wait
//! that releases executor capacity while keeping run ownership.

use agentkit_host::*;
use agentkit_store::{
    AgentDefinition, AgentInputKind, AgentInputStatus, ClaimInput, CreateAgent, CreateAgentMailbox,
    SettleTask, SqliteStore, SubmitTask, TaskStatus,
};
use async_trait::async_trait;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};

/// A clock the test drives explicitly; lease expiry is always the
/// test's choice, never a race.
struct FixedClock(AtomicI64);

impl FixedClock {
    fn new(now: i64) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(now)))
    }
    fn set(&self, now: i64) {
        self.0.store(now, Ordering::SeqCst);
    }
}

impl Runtime for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    fn id(&self) -> String {
        // Event uniqueness is per (task, event id): every emit is a
        // distinct occurrence, exactly like the host's runtime.
        uuid::Uuid::new_v4().to_string()
    }
}

struct Fixture {
    service: TeammateService,
    store: SqliteStore,
    clock: Arc<FixedClock>,
}

fn new_fixture() -> Fixture {
    let store = SqliteStore::memory(8).unwrap();
    let clock = FixedClock::new(1_000);
    let service = TeammateService::with_runtime(store.clone(), clock.clone());
    Fixture {
        service,
        store,
        clock,
    }
}

impl Fixture {
    async fn agent(&self, agent_id: &str) {
        self.service
            .store()
            .create_agent(CreateAgent {
                agent_id: Some(agent_id.into()),
                scope: "team".into(),
                owner_id: "owner".into(),
                definition: AgentDefinition {
                    name: agent_id.into(),
                    purpose: "work".into(),
                    instructions: "do it".into(),
                    permission_mode: "ask".into(),
                    allowed_tools: vec!["read_file".into()],
                    ..Default::default()
                },
                now: self.clock.0.load(Ordering::SeqCst),
            })
            .await
            .unwrap();
    }

    fn admission(&self, agent_id: &str, key: &str) -> TeammateAdmission {
        TeammateAdmission {
            task_id: None,
            agent_id: agent_id.into(),
            admission_key: key.into(),
            kind: "teammate.turn".into(),
            payload: json!({"key": key}),
            priority: 0,
            depends_on: vec![],
            parent_task_id: None,
            run_profile: None,
        }
    }

    fn claim(&self, now: i64, ttl_ms: i64) -> ClaimInput {
        ClaimInput {
            owner_id: "worker".into(),
            now,
            ttl_ms,
            kinds: vec![],
            busy_scopes: vec![],
            max_attempts: 3,
        }
    }
}

// ---------------------------------------------------------------------------
// Admission.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn competing_admits_under_one_key_create_one_logical_task() {
    let fixture = new_fixture();
    fixture.agent("alice").await;
    let first = fixture
        .service
        .admit(fixture.admission("alice", "key-1"))
        .await
        .unwrap();
    assert!(!first.replayed);
    let duplicate = fixture
        .service
        .admit(fixture.admission("alice", "key-1"))
        .await
        .unwrap();
    assert!(duplicate.replayed);
    assert_eq!(first.task.task_id, duplicate.task.task_id);
    assert_eq!(fixture.service.tasks("alice").await.unwrap().len(), 1);
    assert_eq!(duplicate.task.agent_revision, Some(1));
}

#[tokio::test]
async fn admission_validates_its_shape() {
    let fixture = new_fixture();
    fixture.agent("alice").await;
    let mut no_key = fixture.admission("alice", "x");
    no_key.admission_key = "  ".into();
    assert!(fixture.service.admit(no_key).await.is_err());
    let mut no_kind = fixture.admission("alice", "key-1");
    no_kind.kind = "".into();
    assert!(fixture.service.admit(no_kind).await.is_err());
    assert!(fixture.service.tasks("alice").await.unwrap().is_empty());
}

#[tokio::test]
async fn archive_during_admission_is_rejected_before_dispatch() {
    let fixture = new_fixture();
    fixture.agent("alice").await;
    let admitted = fixture
        .service
        .admit(fixture.admission("alice", "key-1"))
        .await
        .unwrap();
    fixture
        .service
        .store()
        .archive_agent("alice".into(), 2_000)
        .await
        .unwrap();
    let error = fixture
        .service
        .admit(fixture.admission("alice", "key-2"))
        .await
        .expect_err("archived agent must refuse new admission");
    assert!(error.to_string().contains("agent_archived"), "{error}");
    // Nothing landed; the pre-archive task keeps its claimable place.
    assert_eq!(fixture.service.tasks("alice").await.unwrap().len(), 1);
    let claimed = fixture
        .store
        .claim(fixture.claim(2_100, 10_000))
        .await
        .unwrap()
        .expect("the admitted task still runs");
    assert_eq!(claimed.task.task_id, admitted.task.task_id);
}

#[tokio::test]
async fn unknown_agent_refuses_admission() {
    let fixture = new_fixture();
    let error = fixture
        .service
        .admit(fixture.admission("nobody", "key-1"))
        .await
        .expect_err("unknown agent is absence");
    assert!(matches!(
        error,
        HostError::Store(agentkit_store::StoreError::NotFound(_))
    ));
}

// ---------------------------------------------------------------------------
// Reconcile before admission.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admission_reconciles_orphans_before_admitting_new_work() {
    let fixture = new_fixture();
    fixture.agent("alice").await;
    let orphan = fixture
        .service
        .admit(fixture.admission("alice", "orphan"))
        .await
        .unwrap();
    // The worker claims and then dies: lease expires at 1_600.
    let claimed = fixture
        .store
        .claim(fixture.claim(1_100, 500))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.task.task_id, orphan.task.task_id);
    fixture.clock.set(5_000);
    // Admission of NEW work first reconciles the unfinished run…
    let next = fixture
        .service
        .admit(fixture.admission("alice", "next"))
        .await
        .unwrap();
    assert!(!next.replayed);
    let after = fixture
        .store
        .get_task(&orphan.task.task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.status, TaskStatus::Uncertain);
    // …the uncertain run keeps the lane: nothing claims while unresolved.
    assert!(
        fixture
            .store
            .claim(fixture.claim(5_100, 10_000))
            .await
            .unwrap()
            .is_none()
    );
    // Evidence resolves it; the retry is a fresh attempt on the SAME task.
    fixture
        .service
        .resolve_uncertain(&orphan.task.task_id, TaskStatus::Queued, None)
        .await
        .unwrap();
    let retry = fixture
        .store
        .claim(fixture.claim(5_200, 10_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retry.task.task_id, orphan.task.task_id);
    assert_eq!(retry.task.attempt_count, 2);
    // Explicit reconciliation stays idempotent once everything is settled.
    let changed = fixture.service.reconcile("alice").await.unwrap();
    assert!(changed.is_empty());
}

// ---------------------------------------------------------------------------
// Display state: a derived projection of the task rows.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn display_state_follows_the_task_projection() {
    use agentkit_store::AgentDisplayState::*;
    let fixture = new_fixture();
    fixture.agent("alice").await;
    assert_eq!(fixture.service.display_state("alice").await.unwrap(), Idle);
    let queued = fixture
        .service
        .admit(fixture.admission("alice", "queued"))
        .await
        .unwrap();
    assert_eq!(
        fixture.service.display_state("alice").await.unwrap(),
        Queued
    );
    let running = fixture
        .store
        .claim(fixture.claim(1_100, 10_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(running.task.task_id, queued.task.task_id);
    assert_eq!(
        fixture.service.display_state("alice").await.unwrap(),
        Working
    );
    // A human park reads as needs-you.
    fixture
        .store
        .transition_task(
            &running.task.task_id,
            vec![TaskStatus::Running],
            TaskStatus::WaitingApproval,
            Default::default(),
            Some(running.lease.lease_token.clone()),
            1_200,
        )
        .await
        .unwrap();
    assert_eq!(
        fixture.service.display_state("alice").await.unwrap(),
        NeedsYou
    );
    // A second admitted run, claimed, then left uncertain: error.
    let other = fixture
        .service
        .admit(fixture.admission("alice", "other"))
        .await;
    // (the waiting approval keeps alice's lane, so first release it)
    fixture
        .store
        .transition_task(
            &running.task.task_id,
            vec![TaskStatus::WaitingApproval],
            TaskStatus::Completed,
            Default::default(),
            Some(running.lease.lease_token.clone()),
            1_300,
        )
        .await
        .unwrap();
    let other = other.unwrap();
    let second = fixture
        .store
        .claim(fixture.claim(1_400, 10_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.task.task_id, other.task.task_id);
    fixture
        .service
        .mark_uncertain(second.lease.fence(), "lost")
        .await
        .unwrap();
    assert_eq!(fixture.service.display_state("alice").await.unwrap(), Error);
    // Resolve terminal: back to idle.
    fixture
        .service
        .resolve_uncertain(&other.task.task_id, TaskStatus::Failed, Some("lost".into()))
        .await
        .unwrap();
    assert_eq!(fixture.service.display_state("alice").await.unwrap(), Idle);
}

// ---------------------------------------------------------------------------
// Runner: single worker, parent wait releases capacity.
// ---------------------------------------------------------------------------

struct ParentExecutor;

#[async_trait]
impl TaskExecutor for ParentExecutor {
    async fn execute(&self, context: ExecutionContext) -> HostResult<()> {
        if context.task.attempt_count == 1 {
            context
                .spawn_child(SubmitTask {
                    task_id: format!("{}-child", context.task.task_id),
                    kind: "teammate.child".into(),
                    scope_id: "workspace".into(),
                    payload: json!({}),
                    priority: 0,
                    now: 0,
                    available_at: None,
                    depends_on: vec![],
                    parent_task_id: None,
                    exclusive_scope: false,
                })
                .await?;
            context.emit("run.parent.spawned", json!({})).await?;
            assert!(
                context.park_on_children().await?,
                "the unfinished child must hold the parent"
            );
            return Ok(());
        }
        assert!(
            !context.park_on_children().await?,
            "the settled child must release the parent"
        );
        context.emit("run.parent.resumed", json!({})).await?;
        Ok(())
    }
}

struct ChildExecutor;

#[async_trait]
impl TaskExecutor for ChildExecutor {
    async fn execute(&self, context: ExecutionContext) -> HostResult<()> {
        context.emit("run.child.done", json!({})).await?;
        Ok(())
    }
}

#[tokio::test]
async fn single_worker_parent_wait_releases_capacity_then_resumes_same_task() {
    let store = SqliteStore::memory(8).unwrap();
    let clock = FixedClock::new(1_000);
    let service = TeammateService::with_runtime(store.clone(), clock.clone());
    service
        .store()
        .create_agent(CreateAgent {
            agent_id: Some("alice".into()),
            scope: "team".into(),
            owner_id: "owner".into(),
            definition: AgentDefinition {
                name: "alice".into(),
                purpose: "work".into(),
                instructions: "do it".into(),
                permission_mode: "ask".into(),
                allowed_tools: vec![],
                ..Default::default()
            },
            now: 1_000,
        })
        .await
        .unwrap();
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([
            (
                "teammate.turn".into(),
                Arc::new(ParentExecutor) as Arc<dyn TaskExecutor>,
            ),
            (
                "teammate.child".into(),
                Arc::new(ChildExecutor) as Arc<dyn TaskExecutor>,
            ),
        ]),
        clock.clone(),
        HostOptions {
            concurrency: 1,
            recovery_sweep_interval_ms: 0,
            ..HostOptions::default()
        },
    )
    .unwrap();
    let parent = service
        .admit(TeammateAdmission {
            task_id: Some("parent".into()),
            agent_id: "alice".into(),
            admission_key: "parent-key".into(),
            kind: "teammate.turn".into(),
            payload: json!({"deep": true}),
            priority: 0,
            depends_on: vec![],
            parent_task_id: None,
            run_profile: None,
        })
        .await
        .unwrap();
    // Worker pass 1: the parent runs, spawns its child, and parks.
    assert!(host.run_once().await.unwrap());
    let parked = store.get_task("parent").await.unwrap().unwrap();
    assert_eq!(parked.status, TaskStatus::WaitingChild);
    // Worker pass 2 runs the CHILD although the pool has exactly one
    // slot and the parent still owns alice's lane — waiting released
    // capacity without losing ownership.
    assert!(host.run_once().await.unwrap());
    let child = store.get_task("parent-child").await.unwrap().unwrap();
    assert_eq!(child.status, TaskStatus::Completed);
    assert_eq!(
        store.get_task("parent").await.unwrap().unwrap().status,
        TaskStatus::WaitingChild
    );
    // The wait resolves through the queue and the SAME logical task
    // finishes on a fresh attempt.
    service.continue_after_children("parent").await.unwrap();
    assert!(host.run_once().await.unwrap());
    let done = store.get_task("parent").await.unwrap().unwrap();
    assert_eq!(
        done.status,
        TaskStatus::Completed,
        "error: {:?}",
        done.error
    );
    assert_eq!(done.attempt_count, 2);
    // One logical parent task; the events prove the wait was durable.
    assert_eq!(service.tasks("alice").await.unwrap().len(), 1);
    let events: Vec<String> = store
        .events("parent", None, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|record| record.event["type"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(events, ["run.parent.spawned", "run.parent.resumed"]);
    let outbox = store.outbox().await.unwrap();
    assert!(outbox.iter().any(|record| {
        record.payload["type"] == "task.waiting_child" && record.payload["taskId"] == "parent"
    }));
    let _ = parent;
}

#[tokio::test]
async fn the_runner_never_gives_one_agent_two_active_writers() {
    let store = SqliteStore::memory(8).unwrap();
    let clock = FixedClock::new(1_000);
    let service = TeammateService::with_runtime(store.clone(), clock.clone());
    service
        .store()
        .create_agent(CreateAgent {
            agent_id: Some("alice".into()),
            scope: "team".into(),
            owner_id: "owner".into(),
            definition: AgentDefinition {
                name: "alice".into(),
                purpose: "work".into(),
                instructions: "do it".into(),
                permission_mode: "ask".into(),
                allowed_tools: vec![],
                ..Default::default()
            },
            now: 1_000,
        })
        .await
        .unwrap();
    // A no-op executor; the point is the CLAIM filter, not the work.
    struct Quiet;
    #[async_trait]
    impl TaskExecutor for Quiet {
        async fn execute(&self, _context: ExecutionContext) -> HostResult<()> {
            Ok(())
        }
    }
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(
            "teammate.turn".into(),
            Arc::new(Quiet) as Arc<dyn TaskExecutor>,
        )]),
        clock.clone(),
        HostOptions {
            concurrency: 4,
            recovery_sweep_interval_ms: 0,
            ..HostOptions::default()
        },
    )
    .unwrap();
    for key in ["k1", "k2", "k3"] {
        service
            .admit(TeammateAdmission {
                task_id: None,
                agent_id: "alice".into(),
                admission_key: key.into(),
                kind: "teammate.turn".into(),
                payload: json!({}),
                priority: 0,
                depends_on: vec![],
                parent_task_id: None,
                run_profile: None,
            })
            .await
            .unwrap();
    }
    // Even four workers serialize alice's lane: exactly one run per
    // pass, in durable admission order.
    let mut finished = Vec::new();
    for _ in 0..3 {
        assert!(host.run_once().await.unwrap());
        let running: Vec<_> = store
            .list_tasks(None)
            .await
            .unwrap()
            .into_iter()
            .filter(|task| task.status == TaskStatus::Running)
            .collect();
        assert!(running.is_empty(), "a pass ended with a live writer");
        finished.push(
            store
                .list_tasks(None)
                .await
                .unwrap()
                .iter()
                .filter(|task| task.status == TaskStatus::Completed)
                .count(),
        );
    }
    assert_eq!(finished, vec![1, 2, 3]);
}

// ---------------------------------------------------------------------------
// Cancel, mailbox, reconnect.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_while_queued_settles_through_the_service() {
    let fixture = new_fixture();
    fixture.agent("alice").await;
    let admitted = fixture
        .service
        .admit(fixture.admission("alice", "key-1"))
        .await
        .unwrap();
    let cancelled = fixture
        .service
        .cancel(&admitted.task.task_id)
        .await
        .unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert!(
        fixture
            .store
            .claim(fixture.claim(2_000, 10_000))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture.service.display_state("alice").await.unwrap(),
        agentkit_store::AgentDisplayState::Idle
    );
}

#[tokio::test]
async fn mailbox_is_a_durable_fifo_through_the_service() {
    let fixture = new_fixture();
    fixture.agent("alice").await;
    let mailbox = |id: &str| CreateAgentMailbox {
        input_id: id.into(),
        kind: AgentInputKind::FollowUp,
        content: json!({"text": id}),
        content_hash: format!("hash-{id}"),
        expires_at: None,
        now: 1_000,
    };
    fixture
        .service
        .enqueue_input("alice", mailbox("m2"))
        .await
        .unwrap();
    fixture
        .service
        .enqueue_input("alice", mailbox("m1"))
        .await
        .unwrap();
    fixture
        .service
        .enqueue_input("alice", mailbox("m2"))
        .await
        .unwrap(); // idempotent replay
    let pending = fixture.service.pending_inputs("alice").await.unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|r| r.input_id.as_str())
            .collect::<Vec<_>>(),
        ["m2", "m1"],
        "durable insertion order (m2 was enqueued first), not the ids"
    );
    let head = fixture
        .service
        .promote_next_input("alice", "teammate.mailbox", 0)
        .await
        .unwrap()
        .unwrap();
    let (head, _task) = head;
    assert_eq!(head.input_id, "m2");
    assert_eq!(head.status, AgentInputStatus::Converted);
    fixture.service.cancel_input("m1").await.unwrap();
    assert!(
        fixture
            .service
            .pending_inputs("alice")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture
            .service
            .promote_next_input("alice", "teammate.mailbox", 0)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn reconnect_and_history_load_create_nothing() {
    let fixture = new_fixture();
    fixture.agent("alice").await;
    let admitted = fixture
        .service
        .admit(fixture.admission("alice", "key-1"))
        .await
        .unwrap();
    for _ in 0..4 {
        assert_eq!(
            fixture
                .service
                .task(&admitted.task.task_id)
                .await
                .unwrap()
                .unwrap()
                .task_id,
            admitted.task.task_id
        );
        assert_eq!(fixture.service.tasks("alice").await.unwrap().len(), 1);
        assert!(
            fixture
                .service
                .pending_inputs("alice")
                .await
                .unwrap()
                .is_empty()
        );
        let _ = fixture.service.display_state("alice").await.unwrap();
    }
    // Re-admitting under the same key after a "reconnect" replays.
    let again = fixture
        .service
        .admit(fixture.admission("alice", "key-1"))
        .await
        .unwrap();
    assert!(again.replayed);
    assert_eq!(fixture.service.tasks("alice").await.unwrap().len(), 1);
}

#[tokio::test]
async fn duplicate_terminal_settlement_cannot_restate_finished_work() {
    let fixture = new_fixture();
    fixture.agent("alice").await;
    let admitted = fixture
        .service
        .admit(fixture.admission("alice", "key-1"))
        .await
        .unwrap();
    let claimed = fixture
        .store
        .claim(fixture.claim(1_100, 10_000))
        .await
        .unwrap()
        .unwrap();
    for now in [1_200, 1_300] {
        let result = fixture
            .store
            .settle(SettleTask {
                fence: claimed.lease.fence(),
                now,
                status: TaskStatus::Completed,
                error: None,
                events: vec![],
                messages: vec![],
            })
            .await;
        match now {
            1_200 => assert!(result.is_ok()),
            _ => assert!(matches!(
                result,
                Err(agentkit_store::StoreError::LeaseLost(_))
            )),
        }
    }
    assert_eq!(
        fixture.service.display_state("alice").await.unwrap(),
        agentkit_store::AgentDisplayState::Idle
    );
    let attempts = fixture
        .store
        .attempts(&admitted.task.task_id)
        .await
        .unwrap();
    assert_eq!(attempts.len(), 1);
}
