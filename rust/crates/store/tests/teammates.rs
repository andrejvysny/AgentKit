//! AGENTKIT-88 acceptance cells for durable teammate admission, mailbox
//! ordering, and run ownership on the existing task engine: competing
//! admits under one idempotency key create ONE logical task; archived
//! agents refuse admission before anything lands; one agent never runs
//! two state-mutating attempts at once while different agents never
//! cross lanes; a child wait releases the pool slot without losing
//! ownership; stale workers are fenced; duplicate terminals are
//! idempotent; crash before/after dequeue is proven at process level;
//! retries are new attempts on the same logical task; the mailbox is a
//! durable per-agent FIFO; and the schema-15 migration keeps every row.

mod support;

use agentkit_store::*;
use serde_json::json;
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use support::{event, finish, task};

/// A claim whose lease outlives every assertion in the happy paths.
fn claim_at(now: i64) -> ClaimInput {
    ClaimInput {
        owner_id: "worker".into(),
        now,
        ttl_ms: 10_000,
        kinds: vec![],
        busy_scopes: vec![],
        max_attempts: 3,
    }
}

/// A claim meant to EXPIRE: for fencing and reconciliation cells.
fn claim_short(now: i64) -> ClaimInput {
    ClaimInput {
        owner_id: "worker".into(),
        now,
        ttl_ms: 100,
        kinds: vec![],
        busy_scopes: vec![],
        max_attempts: 3,
    }
}

fn definition(name: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.into(),
        purpose: format!("purpose of {name}"),
        instructions: format!("instructions of {name}"),
        permission_mode: "ask".into(),
        allowed_tools: vec!["read_file".into()],
        ..Default::default()
    }
}

async fn create_agent(store: &SqliteStore, agent_id: &str) -> AgentRecord {
    store
        .create_agent(CreateAgent {
            agent_id: Some(agent_id.into()),
            scope: "team".into(),
            owner_id: "owner".into(),
            definition: definition(agent_id),
            now: 1_000,
        })
        .await
        .unwrap()
}

fn promote(now: i64) -> PromoteMailbox {
    PromoteMailbox {
        kind: "teammate.mailbox".into(),
        priority: 0,
        now,
    }
}

fn admit(agent_id: &str, key: &str, marker: &str) -> AdmitTeammateTask {
    AdmitTeammateTask {
        task_id: None,
        agent_id: agent_id.into(),
        admission_key: key.into(),
        kind: "teammate.turn".into(),
        payload: json!({"marker": marker}),
        priority: 0,
        now: 1_000,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        run_profile: None,
    }
}

async fn admit_ok(store: &SqliteStore, agent_id: &str, key: &str) -> AdmittedTask {
    store
        .admit_teammate_task(admit(agent_id, key, key))
        .await
        .unwrap()
}

fn child(id: &str, parent: &str, priority: i32, now: i64) -> SubmitTask {
    SubmitTask {
        task_id: id.into(),
        kind: "teammate.child".into(),
        scope_id: "workspace".into(),
        payload: json!({}),
        priority,
        now,
        available_at: None,
        depends_on: vec![],
        parent_task_id: Some(parent.into()),
        exclusive_scope: false,
    }
}

// ---------------------------------------------------------------------------
// Idempotent admission.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn competing_admits_under_one_key_create_one_logical_task() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let first = admit_ok(&store, "alice", "key-1").await;
    assert!(!first.replayed);
    // The competing duplicate — same key, same facts — returns the SAME
    // logical task and creates nothing.
    let second = admit_ok(&store, "alice", "key-1").await;
    assert!(second.replayed);
    assert_eq!(first.task, second.task);
    assert_eq!(
        store.list_agent_tasks("alice".into()).await.unwrap().len(),
        1
    );
    // The ledger names it; the binding is complete.
    let by_key = store
        .task_by_admission_key("key-1".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_key, first.task);
    assert_eq!(by_key.agent_id.as_deref(), Some("alice"));
    assert_eq!(by_key.agent_revision, Some(1));
    assert_eq!(by_key.admission_key.as_deref(), Some("key-1"));
    assert_eq!(by_key.scope_id, teammate_scope("alice"));
}

#[tokio::test]
async fn a_key_never_admits_different_work() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    admit_ok(&store, "alice", "key-1").await;
    // Same key, different payload: refused, nothing new lands.
    let error = store
        .admit_teammate_task(admit("alice", "key-1", "different"))
        .await;
    assert!(
        matches!(error, Err(StoreError::InvalidInput(_))),
        "{error:?}"
    );
    assert_eq!(
        store.list_agent_tasks("alice".into()).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn archive_before_admission_refuses_and_commits_nothing() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let before = admit_ok(&store, "alice", "key-1").await;
    store.archive_agent("alice".into(), 2_000).await.unwrap();
    // NEW admission is refused, atomically, leaving no trace.
    let error = store
        .admit_teammate_task(admit("alice", "key-2", "m2"))
        .await;
    match error {
        Err(StoreError::InvalidTransition(reason)) => {
            assert!(reason.contains("agent_archived"), "{reason}")
        }
        other => panic!("archived agent must refuse admission, got {other:?}"),
    }
    assert!(
        store
            .task_by_admission_key("key-2".into())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.list_agent_tasks("alice".into()).await.unwrap().len(),
        1
    );
    // The task admitted BEFORE the archive keeps its identity and stays
    // readable; a replay of its OWN key returns the same task — replay
    // is not a new admission.
    let replay = admit_ok(&store, "alice", "key-1").await;
    assert!(replay.replayed);
    assert_eq!(replay.task, before.task);
}

#[tokio::test]
async fn unknown_and_deleted_agents_refuse_admission() {
    let store = SqliteStore::memory(4).unwrap();
    let error = store
        .admit_teammate_task(admit("nobody", "key-1", "m"))
        .await;
    assert!(matches!(error, Err(StoreError::NotFound(_))), "{error:?}");
    create_agent(&store, "ghost").await;
    store.delete_agent("ghost".into(), 2_000).await.unwrap();
    let error = store
        .admit_teammate_task(admit("ghost", "key-1", "m"))
        .await;
    assert!(matches!(error, Err(StoreError::NotFound(_))), "{error:?}");
}

#[tokio::test]
async fn admission_pins_the_immutable_definition_revision() {
    let store = SqliteStore::memory(4).unwrap();
    let mut current = create_agent(&store, "alice").await;
    let before = admit_ok(&store, "alice", "key-1").await;
    assert_eq!(before.task.agent_revision, Some(1));
    let mut edited = definition("alice");
    edited.purpose = "revised purpose".into();
    current = store
        .update_agent(UpdateAgent {
            agent_id: "alice".into(),
            expected_revision: current.current_revision,
            definition: edited,
            source: AgentRevisionSource::Edit,
            now: 2_000,
        })
        .await
        .unwrap();
    assert_eq!(current.current_revision, 2);
    let after = admit_ok(&store, "alice", "key-2").await;
    assert_eq!(after.task.agent_revision, Some(2));
    // The earlier task keeps the revision it was admitted under — the
    // ledger resolves it; the row never drifts.
    let same = store.get_task(&before.task.task_id).await.unwrap().unwrap();
    assert_eq!(same.agent_revision, Some(1));
}

#[tokio::test]
async fn pinned_task_id_admits_once_and_collides_honestly() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    create_agent(&store, "bob").await;
    let mut input = admit("alice", "key-1", "m");
    input.task_id = Some("logical-1".into());
    let first = store.admit_teammate_task(input.clone()).await.unwrap();
    assert!(!first.replayed);
    assert_eq!(first.task.task_id, "logical-1");
    // The same admission (key + agent) replays the pinned task.
    let replay = store.admit_teammate_task(input).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.task.task_id, "logical-1");
    // A DIFFERENT admission trying to squat the id is a collision.
    let mut squatter = admit("alice", "key-2", "m");
    squatter.task_id = Some("logical-1".into());
    assert!(matches!(
        store.admit_teammate_task(squatter).await,
        Err(StoreError::Duplicate(_))
    ));
    // Even under the same id, a different agent cannot replay it.
    let mut cross = admit("bob", "key-3", "m");
    cross.task_id = Some("logical-1".into());
    assert!(matches!(
        store.admit_teammate_task(cross).await,
        Err(StoreError::Duplicate(_))
    ));
}

// ---------------------------------------------------------------------------
// Lane ownership: one active writer per agent; agents don't cross.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_active_writer_per_agent_across_scopes() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let first = admit_ok(&store, "alice", "key-1").await;
    let second = admit_ok(&store, "alice", "key-2").await;
    let claimed = store.claim(claim_at(1_100)).await.unwrap().unwrap();
    assert_eq!(claimed.task.task_id, first.task.task_id);
    // While alice has a running attempt, nothing else of alice claims —
    // not even with an idle pool.
    assert!(
        store
            .claim_with_concurrency(claim_at(1_110), 4)
            .await
            .unwrap()
            .is_none()
    );
    finish(&store, claimed.lease, TaskStatus::Completed, 1_200).await;
    let next = store.claim(claim_at(1_300)).await.unwrap().unwrap();
    assert_eq!(next.task.task_id, second.task.task_id);
    assert_eq!(next.task.attempt_count, 1);
}

#[tokio::test]
async fn concurrent_agents_do_not_cross_lanes() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    create_agent(&store, "bob").await;
    let alice = admit_ok(&store, "alice", "a1").await;
    let bob = admit_ok(&store, "bob", "b1").await;
    let first = store
        .claim_with_concurrency(claim_at(1_100), 2)
        .await
        .unwrap()
        .unwrap();
    let second = store
        .claim_with_concurrency(claim_at(1_110), 2)
        .await
        .unwrap()
        .unwrap();
    // Both agents run at once (shared bounded pool), each exactly once,
    // each owning its own lane.
    let mut ids = vec![first.task.task_id.clone(), second.task.task_id.clone()];
    ids.sort();
    let mut expected = vec![alice.task.task_id.clone(), bob.task.task_id.clone()];
    expected.sort();
    assert_eq!(ids, expected);
    assert_ne!(first.task.agent_id, second.task.agent_id);
    // A third task for alice waits behind alice's own writer, not bob's.
    let alice_again = admit_ok(&store, "alice", "a2").await;
    assert!(store.claim(claim_at(1_120)).await.unwrap().is_none());
    finish(&store, first.lease, TaskStatus::Completed, 1_200).await;
    let advanced = store.claim(claim_at(1_210)).await.unwrap().unwrap();
    assert_eq!(advanced.task.task_id, alice_again.task.task_id);
    finish(&store, second.lease, TaskStatus::Completed, 1_220).await;
}

#[tokio::test]
async fn cancel_while_queued_settles_now_and_never_claims() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let admitted = admit_ok(&store, "alice", "key-1").await;
    let cancelled = store
        .cancel_task(&admitted.task.task_id, 1_100)
        .await
        .unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert_eq!(cancelled.finished_at, Some(1_100));
    assert!(store.claim(claim_at(1_200)).await.unwrap().is_none());
    // The lane frees with the terminal: a fresh admission runs.
    let next = admit_ok(&store, "alice", "key-2").await;
    let claimed = store.claim(claim_at(1_300)).await.unwrap().unwrap();
    assert_eq!(claimed.task.task_id, next.task.task_id);
}

#[tokio::test]
async fn priority_order_is_deterministic_and_aging_lifts_the_waiting_task() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "a1").await;
    create_agent(&store, "a2").await;
    create_agent(&store, "a3").await;
    let mut low = admit("a1", "low", "low");
    low.priority = 1;
    let mut high = admit("a2", "high", "high");
    high.priority = 5;
    let mut mid = admit("a3", "mid", "mid");
    mid.priority = 3;
    for input in [low.clone(), high.clone(), mid.clone()] {
        store.admit_teammate_task(input).await.unwrap();
    }
    // Plain priority order, deterministic.
    let order = ["high", "mid", "low"];
    for name in order {
        let claimed = store.claim(claim_at(2_000)).await.unwrap().unwrap();
        assert_eq!(claimed.task.payload["marker"], name);
        finish(&store, claimed.lease, TaskStatus::Completed, 2_001).await;
    }
    // Starvation protection: with aging, the queued-low-lifts.
    let aged = SqliteStore::memory(4).unwrap().with_task_aging(TaskAging {
        interval_ms: 1_000,
        bonus: 10,
        max_bonus: 100,
    });
    for agent in ["a1", "a2", "a3"] {
        create_agent(&aged, agent).await;
    }
    let mut low = admit("a1", "low", "low");
    low.priority = 1;
    let mut high = admit("a2", "high", "high");
    high.priority = 5;
    low.now = 1_000;
    high.now = 15_500;
    aged.admit_teammate_task(low).await.unwrap();
    aged.admit_teammate_task(high).await.unwrap();
    // Far in the future: 15 intervals earned ⇒ low overtakes high.
    let claimed = aged.claim(claim_at(16_000)).await.unwrap().unwrap();
    assert_eq!(claimed.task.payload["marker"], "low");
}

// ---------------------------------------------------------------------------
// Waiting releases capacity without losing ownership.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn waiting_child_releases_the_slot_without_losing_ownership() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let parent = admit_ok(&store, "alice", "parent").await;
    // One worker in the pool.
    let claimed = store
        .claim_with_concurrency(claim_at(1_100), 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.task.task_id, parent.task.task_id);
    // A child, submitted under the running parent.
    let child = store
        .submit(child("child-1", &parent.task.task_id, 10, 1_150))
        .await
        .unwrap();
    // The single slot is taken: nothing claims.
    assert!(
        store
            .claim_with_concurrency(claim_at(1_160), 1)
            .await
            .unwrap()
            .is_none()
    );
    // A sibling queued behind the lane.
    let sibling = admit_ok(&store, "alice", "sibling").await;
    // The parent parks on the unfinished child: attempt ends, lease and
    // pool slot release, the lane stays owned.
    assert!(
        store
            .wait_for_children(claimed.lease.fence(), 1_200)
            .await
            .unwrap()
    );
    let parked = store.get_task(&parent.task.task_id).await.unwrap().unwrap();
    assert_eq!(parked.status, TaskStatus::WaitingChild);
    // The freed slot lets the CHILD run on the single worker…
    let running = store
        .claim_with_concurrency(claim_at(1_300), 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(running.task.task_id, child.task_id);
    // …while alice's lane is STILL owned by the parked parent: her next
    // queued task does not claim even with a wider pool.
    assert!(
        store
            .claim_with_concurrency(claim_at(1_310), 2)
            .await
            .unwrap()
            .is_none()
    );
    finish(&store, running.lease, TaskStatus::Completed, 1_400).await;
    // The wait resolves through the queue — a FRESH attempt, same task.
    let resumed = store
        .continue_after_children(parent.task.task_id.clone(), 1_500)
        .await
        .unwrap();
    assert_eq!(resumed.status, TaskStatus::Queued);
    let next = store.claim(claim_at(1_600)).await.unwrap().unwrap();
    assert_eq!(next.task.task_id, parent.task.task_id);
    assert_eq!(next.task.attempt_count, 2);
    // The sibling waits its turn behind the resumed parent.
    finish(&store, next.lease, TaskStatus::Completed, 1_700).await;
    let last = store.claim(claim_at(1_800)).await.unwrap().unwrap();
    assert_eq!(last.task.task_id, sibling.task.task_id);
}

#[tokio::test]
async fn waiting_child_park_fences_stale_leases() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let parent = admit_ok(&store, "alice", "parent").await;
    let claimed = store.claim(claim_at(1_100)).await.unwrap().unwrap();
    store
        .submit(child("child-1", &parent.task.task_id, 0, 1_150))
        .await
        .unwrap();
    let stale = LeaseFence {
        task_id: parent.task.task_id.clone(),
        lease_token: "stale".into(),
        fencing_token: 999,
    };
    assert!(matches!(
        store.wait_for_children(stale, 1_200).await,
        Err(StoreError::LeaseLost(_))
    ));
    // The live lease still parks afterwards.
    assert!(
        store
            .wait_for_children(claimed.lease.fence(), 1_210)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn wait_for_children_is_a_noop_when_children_are_done() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let parent = admit_ok(&store, "alice", "parent").await;
    let claimed = store.claim(claim_at(1_100)).await.unwrap().unwrap();
    // No children at all: the park answers false and commits nothing.
    assert!(
        !store
            .wait_for_children(claimed.lease.fence(), 1_150)
            .await
            .unwrap()
    );
    // A terminal child does not hold the parent.
    store
        .submit(child("settled-child", &parent.task.task_id, 0, 1_160))
        .await
        .unwrap();
    let lease = store
        .claim(claim_at(1_170))
        .await
        .unwrap()
        .expect("child claim")
        .lease;
    store
        .settle(SettleTask {
            fence: lease.fence(),
            now: 1_180,
            status: TaskStatus::Completed,
            error: None,
            events: vec![],
            messages: vec![],
        })
        .await
        .unwrap();
    assert!(
        !store
            .wait_for_children(claimed.lease.fence(), 1_190)
            .await
            .unwrap()
    );
    finish(&store, claimed.lease, TaskStatus::Completed, 1_210).await;
}

// ---------------------------------------------------------------------------
// Fencing, duplicate terminals, retries.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stale_worker_is_fenced_from_late_results() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let admitted = admit_ok(&store, "alice", "key-1").await;
    let stale = store.claim(claim_short(1_100)).await.unwrap().unwrap();
    // The lease expires; recovery requeues the abandoned attempt.
    let recovered = store
        .recover(5_000, 3, 0, RecoveryMode::Automatic)
        .await
        .unwrap();
    assert_eq!(recovered[0].task_id, admitted.task.task_id);
    // The stale generation can neither settle nor park nor mark.
    assert!(matches!(
        store
            .settle(SettleTask {
                fence: stale.lease.fence(),
                now: 5_100,
                status: TaskStatus::Completed,
                error: None,
                events: vec![],
                messages: vec![],
            })
            .await,
        Err(StoreError::LeaseLost(_))
    ));
    assert!(matches!(
        store.wait_for_children(stale.lease.fence(), 5_100).await,
        Err(StoreError::LeaseLost(_))
    ));
    assert!(matches!(
        store
            .mark_uncertain(stale.lease.fence(), 5_100, "late")
            .await,
        Err(StoreError::LeaseLost(_))
    ));
    // The fresh attempt lands.
    let fresh = store.claim(claim_at(5_200)).await.unwrap().unwrap();
    assert_eq!(fresh.task.attempt_count, 2);
    finish(&store, fresh.lease, TaskStatus::Completed, 5_300).await;
    let done = store
        .get_task(&admitted.task.task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.status, TaskStatus::Completed);
    // One logical task the whole time.
    assert_eq!(
        store.list_agent_tasks("alice".into()).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn duplicate_terminal_events_are_idempotent_and_fenced() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let admitted = admit_ok(&store, "alice", "key-1").await;
    let claimed = store.claim(claim_at(1_100)).await.unwrap().unwrap();
    store
        .settle(SettleTask {
            fence: claimed.lease.fence(),
            now: 1_200,
            status: TaskStatus::Completed,
            error: None,
            events: vec![event(0, "terminal")],
            messages: vec![],
        })
        .await
        .unwrap();
    // A duplicate terminal event from the same (now dead) generation is
    // fenced: nothing changes, nothing appends, the attempt stays ended.
    let duplicate = store
        .settle(SettleTask {
            fence: claimed.lease.fence(),
            now: 1_300,
            status: TaskStatus::Completed,
            error: None,
            events: vec![event(0, "terminal")],
            messages: vec![],
        })
        .await;
    assert!(
        matches!(duplicate, Err(StoreError::LeaseLost(_))),
        "{duplicate:?}"
    );
    let settled_events: Vec<_> = store
        .outbox()
        .await
        .unwrap()
        .into_iter()
        .filter(|record| {
            record.payload["type"] == "task.settled"
                && record.payload["taskId"] == admitted.task.task_id.as_str()
        })
        .collect();
    assert_eq!(
        settled_events.len(),
        1,
        "exactly one durable terminal event"
    );
    let attempts = store.attempts(&admitted.task.task_id).await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].status, "completed");
    assert_eq!(
        store
            .events(&admitted.task.task_id, None, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn retry_is_a_new_attempt_on_the_same_logical_task() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let admitted = admit_ok(&store, "alice", "key-1").await;
    let claimed = store.claim(claim_at(1_100)).await.unwrap().unwrap();
    store
        .retry_task(RetryTask {
            fence: claimed.lease.fence(),
            now: 1_200,
            available_at: 1_300,
            max_attempts: 3,
            error: "transient".into(),
        })
        .await
        .unwrap();
    let next = store.claim(claim_at(1_300)).await.unwrap().unwrap();
    assert_eq!(next.task.task_id, admitted.task.task_id);
    assert_eq!(next.task.attempt_count, 2);
    assert_eq!(
        store.list_agent_tasks("alice".into()).await.unwrap().len(),
        1
    );
    let attempts = store.attempts(&admitted.task.task_id).await.unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].status, "failed");
    assert_eq!(attempts[0].attempt_number, 1);
    assert_eq!(attempts[1].attempt_number, 2);
}

// ---------------------------------------------------------------------------
// Uncertainty and reconciliation.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn uncertain_parks_the_lane_and_resolves_only_explicitly() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let admitted = admit_ok(&store, "alice", "key-1").await;
    let claimed = store.claim(claim_at(1_100)).await.unwrap().unwrap();
    let parked = store
        .mark_uncertain(claimed.lease.fence(), 1_150, "outcome_unknown")
        .await
        .unwrap();
    assert_eq!(parked.status, TaskStatus::Uncertain);
    // Uncertain is unclaimable and lane-owning.
    assert!(store.claim(claim_at(1_200)).await.unwrap().is_none());
    let later = admit_ok(&store, "alice", "key-2").await;
    assert!(store.claim(claim_at(1_300)).await.unwrap().is_none());
    // Illegal resolutions are refused…
    assert!(matches!(
        store
            .resolve_uncertain(
                admitted.task.task_id.clone(),
                TaskStatus::Running,
                None,
                1_400
            )
            .await,
        Err(StoreError::InvalidInput(_))
    ));
    // …a legal one sends it back through the queue as a new attempt.
    let requeued = store
        .resolve_uncertain(
            admitted.task.task_id.clone(),
            TaskStatus::Queued,
            None,
            1_400,
        )
        .await
        .unwrap();
    assert_eq!(requeued.status, TaskStatus::Queued);
    // Resolving a task that is no longer uncertain is refused by the
    // machine, not pretended.
    assert!(matches!(
        store
            .resolve_uncertain(
                admitted.task.task_id.clone(),
                TaskStatus::Failed,
                None,
                1_450
            )
            .await,
        Err(StoreError::InvalidTransition(_))
    ));
    let next = store.claim(claim_at(1_500)).await.unwrap().unwrap();
    assert_eq!(next.task.task_id, admitted.task.task_id);
    assert_eq!(next.task.attempt_count, 2);
    let _ = later;
}

#[tokio::test]
async fn reconcile_classifies_orphans_and_lands_queued_cancels() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    create_agent(&store, "bob").await;
    create_agent(&store, "carol").await;
    // Orphan: claimed, lease expired at reconcile time.
    let orphan = admit_ok(&store, "alice", "orphan").await;
    let stale = store.claim(claim_short(1_100)).await.unwrap().unwrap();
    // Live: bob has a lease that has NOT expired.
    let live = admit_ok(&store, "bob", "live").await;
    let live_claim = store.claim(claim_at(1_110)).await.unwrap().unwrap();
    // Cancel requested on a RUNNING task whose worker then died: the
    // durable flag was written, nobody was left to observe it.
    let pending_cancel = admit_ok(&store, "carol", "pending-cancel").await;
    let cancel_claim = store.claim(claim_short(1_115)).await.unwrap().unwrap();
    assert_eq!(cancel_claim.task.task_id, pending_cancel.task.task_id);
    store
        .cancel_task(&pending_cancel.task.task_id, 1_120)
        .await
        .unwrap();
    let changed = store
        .reconcile_agent_runs("alice".into(), 5_000)
        .await
        .unwrap();
    let ids: Vec<&str> = changed.iter().map(|t| t.task_id.as_str()).collect();
    assert_eq!(ids, [orphan.task.task_id.as_str()]);
    let carol_changed = store
        .reconcile_agent_runs("carol".into(), 5_000)
        .await
        .unwrap();
    let carol_ids: Vec<&str> = carol_changed.iter().map(|t| t.task_id.as_str()).collect();
    assert_eq!(carol_ids, [pending_cancel.task.task_id.as_str()]);
    // Cancellation intent is not evidence of a settled outcome: the dead
    // attempt may have mutated state, so the orphan is UNCERTAIN with the
    // request still recorded.
    let cancelled = store
        .get_task(&pending_cancel.task.task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.status, TaskStatus::Uncertain);
    assert!(cancelled.cancel_requested);
    assert_eq!(
        cancelled.error.as_deref(),
        Some("cancel_requested_outcome_unknown")
    );
    let orphan_now = store.get_task(&orphan.task.task_id).await.unwrap().unwrap();
    assert_eq!(orphan_now.status, TaskStatus::Uncertain);
    assert_eq!(
        orphan_now.error.as_deref(),
        Some("lease_expired_unresolved")
    );
    let attempts = store.attempts(&orphan.task.task_id).await.unwrap();
    assert_eq!(attempts[0].status, "abandoned");
    // Bob's live attempt is untouched (another worker still attests it).
    let live_now = store.get_task(&live.task.task_id).await.unwrap().unwrap();
    assert_eq!(live_now.status, TaskStatus::Running);
    let other = store
        .reconcile_agent_runs("bob".into(), 5_000)
        .await
        .unwrap();
    assert!(other.is_empty());
    finish(&store, live_claim.lease, TaskStatus::Completed, 5_100).await;
    let _ = stale;
}

#[tokio::test]
async fn reconcile_leaves_parked_waits_and_live_leases_alone() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let parent = admit_ok(&store, "alice", "parent").await;
    let claimed = store.claim(claim_at(1_100)).await.unwrap().unwrap();
    store
        .submit(child("child", &parent.task.task_id, 0, 1_150))
        .await
        .unwrap();
    assert!(
        store
            .wait_for_children(claimed.lease.fence(), 1_200)
            .await
            .unwrap()
    );
    // The child now holds a LIVE lease; both waits are legitimate.
    let child_claim = store.claim(claim_at(1_250)).await.unwrap().unwrap();
    assert!(
        store
            .reconcile_agent_runs("alice".into(), 1_300)
            .await
            .unwrap()
            .is_empty()
    );
    let parked = store.get_task(&parent.task.task_id).await.unwrap().unwrap();
    assert_eq!(parked.status, TaskStatus::WaitingChild);
    finish(&store, child_claim.lease, TaskStatus::Completed, 1_400).await;
}

// ---------------------------------------------------------------------------
// Mailbox: durable, ordered, per agent.
// ---------------------------------------------------------------------------

fn mailbox(id: &str, text: &str, now: i64) -> CreateAgentMailbox {
    CreateAgentMailbox {
        input_id: id.into(),
        kind: AgentInputKind::FollowUp,
        content: json!({"text": text}),
        content_hash: format!("hash-{id}"),
        expires_at: None,
        now,
    }
}

#[tokio::test]
async fn mailbox_orders_per_agent_and_promotes_head_first() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    create_agent(&store, "bob").await;
    let first = store
        .enqueue_agent_mailbox("alice".into(), mailbox("i1", "one", 1_000))
        .await
        .unwrap();
    assert!(first.1);
    // Idempotent replay returns the same row without a second enqueue.
    let replay = store
        .enqueue_agent_mailbox("alice".into(), mailbox("i1", "one", 1_001))
        .await
        .unwrap();
    assert!(!replay.1);
    assert_eq!(replay.0, first.0);
    store
        .enqueue_agent_mailbox("alice".into(), mailbox("i2", "two", 1_002))
        .await
        .unwrap();
    store
        .enqueue_agent_mailbox("alice".into(), mailbox("i3", "three", 1_003))
        .await
        .unwrap();
    // Bob's mailbox is a distinct queue: same input id under a different
    // agent is a conflict, never a shared row.
    assert!(matches!(
        store
            .enqueue_agent_mailbox("bob".into(), mailbox("i1", "one", 1_004))
            .await,
        Err(StoreError::Duplicate(_))
    ));
    let (bob_input, _) = store
        .enqueue_agent_mailbox("bob".into(), mailbox("b1", "bob one", 1_005))
        .await
        .unwrap();
    // Strict durable FIFO for alice; bob's order is independent.
    let pending = store
        .list_agent_mailbox("alice".into(), Some(vec![AgentInputStatus::Pending]))
        .await
        .unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|r| r.input_id.as_str())
            .collect::<Vec<_>>(),
        ["i1", "i2", "i3"]
    );
    assert!(pending[0].order < pending[1].order && pending[1].order < pending[2].order);
    assert_eq!(pending[0].agent_id.as_deref(), Some("alice"));
    // Promotion is head-first and CAS-guarded.
    let promoted = store
        .promote_next_agent_mailbox("alice".into(), promote(1_100))
        .await
        .unwrap()
        .unwrap();
    let (promoted, promoted_task) = promoted;
    assert_eq!(promoted.input_id, "i1");
    assert_eq!(promoted.status, AgentInputStatus::Converted);
    assert_eq!(
        promoted.applied_task_id.as_deref(),
        Some(promoted_task.task_id.as_str())
    );
    let next = store
        .promote_next_agent_mailbox("alice".into(), promote(1_101))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.0.input_id, "i2");
    // Cancellation removes a pending input from the queue…
    store
        .cancel_agent_mailbox("i3".into(), 1_102)
        .await
        .unwrap();
    assert!(
        store
            .list_agent_mailbox("alice".into(), Some(vec![AgentInputStatus::Pending]))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .promote_next_agent_mailbox("alice".into(), promote(1_103))
            .await
            .unwrap()
            .is_none()
    );
    // Double-cancel is refused by the machine, not pretended.
    assert!(matches!(
        store.cancel_agent_mailbox("i3".into(), 1_104).await,
        Err(StoreError::InvalidTransition(_))
    ));
    // Bob's head is untouched by alice's drains.
    let bob_head = store
        .promote_next_agent_mailbox("bob".into(), promote(1_105))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bob_head.0.input_id, bob_input.input_id);
    // A human request is not mailbox material.
    let mut request = mailbox("i9", "approve?", 1_200);
    request.kind = AgentInputKind::HumanRequest;
    assert!(matches!(
        store.enqueue_agent_mailbox("alice".into(), request).await,
        Err(StoreError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn mailbox_rows_survive_session_input_deletes() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    store
        .enqueue_agent_mailbox("alice".into(), mailbox("i1", "one", 1_000))
        .await
        .unwrap();
    // Deleting a session's inputs (even one named like the agent) never
    // reaches the agent mailbox.
    assert_eq!(
        store.delete_session_inputs("alice".into()).await.unwrap(),
        0
    );
    assert_eq!(
        store
            .list_agent_mailbox("alice".into(), None)
            .await
            .unwrap()
            .len(),
        1
    );
}

// ---------------------------------------------------------------------------
// Reconnect / history load never creates.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reconnect_and_history_load_never_create_tasks() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let admitted = admit_ok(&store, "alice", "key-1").await;
    for _ in 0..3 {
        // A reconnecting client loads: ledger lookup, task read, agent
        // history, mailbox. All pure reads.
        assert!(
            store
                .task_by_admission_key("key-1".into())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .get_task(&admitted.task.task_id)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store.list_agent_tasks("alice".into()).await.unwrap().len(),
            1
        );
        assert!(
            store
                .list_agent_mailbox("alice".into(), None)
                .await
                .unwrap()
                .is_empty()
        );
    }
    // Unknown keys answer absence — they do not conjure tasks either.
    assert!(
        store
            .task_by_admission_key("nope".into())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.list_agent_tasks("alice".into()).await.unwrap().len(),
        1
    );
}

// ---------------------------------------------------------------------------
// Display-state projection (derived, never stored).
// ---------------------------------------------------------------------------

fn bare(task_id: &str, status: TaskStatus) -> TaskRecord {
    TaskRecord {
        task_id: task_id.into(),
        kind: "teammate.turn".into(),
        scope_id: teammate_scope("alice"),
        status,
        payload: json!({}),
        priority: 0,
        enqueued_at: 1,
        available_at: 1,
        started_at: None,
        finished_at: None,
        depends_on: vec![],
        parent_task_id: None,
        attempt_count: 0,
        poison_count: 0,
        dead_lettered_at: None,
        dead_letter_reason: None,
        error: None,
        progress: None,
        cancel_requested: false,
        runtime_state: None,
        retry_pending: false,
        interrupt_requested: false,
        agent_id: Some("alice".into()),
        agent_revision: Some(1),
        admission_key: None,
        run_profile: None,
    }
}

#[test]
fn display_state_is_the_derived_projection() {
    use AgentDisplayState::*;
    assert_eq!(derive_agent_display_state(&[]), Idle);
    assert_eq!(
        derive_agent_display_state(&[bare("t", TaskStatus::Completed)]),
        Idle
    );
    assert_eq!(
        derive_agent_display_state(&[bare("t", TaskStatus::Queued)]),
        Queued
    );
    assert_eq!(
        derive_agent_display_state(&[bare("t", TaskStatus::Running)]),
        Working
    );
    // A child wait is still the run in flight…
    assert_eq!(
        derive_agent_display_state(&[bare("t", TaskStatus::WaitingChild)]),
        Working
    );
    // …an approval wait asks the human…
    assert_eq!(
        derive_agent_display_state(&[bare("t", TaskStatus::WaitingApproval)]),
        NeedsYou
    );
    // …an undecidable outcome reads as an error, not silence…
    assert_eq!(
        derive_agent_display_state(&[bare("t", TaskStatus::Uncertain)]),
        Error
    );
    assert_eq!(
        derive_agent_display_state(&[bare("t", TaskStatus::Interrupted)]),
        Interrupted
    );
    // Precedence: work in flight outranks queued work; a parked human
    // request outranks a parked child wait; dead-lettered rows project
    // away.
    assert_eq!(
        derive_agent_display_state(&[
            bare("a", TaskStatus::Queued),
            bare("b", TaskStatus::Running),
        ]),
        Working
    );
    assert_eq!(
        derive_agent_display_state(&[
            bare("a", TaskStatus::WaitingChild),
            bare("b", TaskStatus::WaitingApproval),
        ]),
        Working
    );
    let mut dead = bare("x", TaskStatus::Queued);
    dead.dead_lettered_at = Some(2);
    assert_eq!(derive_agent_display_state(&[dead]), Idle);
}

// ---------------------------------------------------------------------------
// Process-level crash evidence: before and after dequeue.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn crash_worker_teammate() {
    let Ok(path) = std::env::var("AGENTKIT_TEAMMATE_CRASH_PATH") else {
        return;
    };
    let phase = std::env::var("AGENTKIT_TEAMMATE_CRASH_PHASE").unwrap_or_default();
    let store = SqliteStore::open(path, 2).unwrap();
    store
        .create_agent(CreateAgent {
            agent_id: Some("alice".into()),
            scope: "team".into(),
            owner_id: "owner".into(),
            definition: definition("alice"),
            now: 1_000,
        })
        .await
        .unwrap();
    store
        .admit_teammate_task(admit("alice", "key-1", "m"))
        .await
        .unwrap();
    if phase == "after" {
        store
            .claim_with_concurrency(claim_short(1_100), 1)
            .await
            .unwrap()
            .expect("claim");
    }
    std::fs::write(
        std::env::var("AGENTKIT_TEAMMATE_READY_PATH").unwrap(),
        b"ready",
    )
    .unwrap();
    loop {
        std::thread::park();
    }
}

fn kill_after_ready(directory: &std::path::Path, phase: &str) -> std::path::PathBuf {
    let path = directory.join(format!("teammate-{phase}.db"));
    let ready = directory.join(format!("ready-{phase}"));
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_worker_teammate", "--nocapture"])
        .env("AGENTKIT_TEAMMATE_CRASH_PATH", &path)
        .env("AGENTKIT_TEAMMATE_CRASH_PHASE", phase)
        .env("AGENTKIT_TEAMMATE_READY_PATH", &ready)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ready.exists(), "crash worker readiness");
    child.kill().unwrap();
    child.wait().unwrap();
    path
}

#[tokio::test]
async fn crash_before_dequeue_reopens_with_the_admission_intact() {
    let directory = tempfile::tempdir().unwrap();
    let path = kill_after_ready(directory.path(), "before");
    let store = SqliteStore::open(&path, 2).unwrap();
    // The admission is durable: the same key replays the same logical
    // task; nothing was dequeued, so the task waits in the queue.
    let replay = store
        .admit_teammate_task(admit("alice", "key-1", "m"))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.task.status, TaskStatus::Queued);
    let tasks = store.list_agent_tasks("alice".into()).await.unwrap();
    assert_eq!(tasks.len(), 1);
    // And it claims normally after the restart.
    let claimed = store.claim(claim_at(2_000)).await.unwrap().unwrap();
    assert_eq!(claimed.task.task_id, replay.task.task_id);
}

#[tokio::test]
async fn crash_after_dequeue_reconciles_to_uncertain_then_retries_same_task() {
    let directory = tempfile::tempdir().unwrap();
    let path = kill_after_ready(directory.path(), "after");
    let store = SqliteStore::open(&path, 2).unwrap();
    let tasks = store.list_agent_tasks("alice".into()).await.unwrap();
    assert_eq!(tasks.len(), 1);
    // Crash AFTER dequeue: the task is `Running` with its lease, and it
    // must NOT be claimable a second time.
    assert_eq!(tasks[0].status, TaskStatus::Running);
    assert!(store.claim(claim_at(5_000)).await.unwrap().is_none());
    // Reconciling before restarting admission classifies the orphan —
    // it is NOT silently re-run.
    let changed = store
        .reconcile_agent_runs("alice".into(), 5_000)
        .await
        .unwrap();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].status, TaskStatus::Uncertain);
    let attempts = store.attempts(&changed[0].task_id).await.unwrap();
    assert_eq!(attempts[0].status, "abandoned");
    assert_eq!(
        attempts[0].error.as_deref(),
        Some("lease_expired_unresolved")
    );
    // Evidence resolves it; the retry is a NEW ATTEMPT on the SAME task.
    store
        .resolve_uncertain(changed[0].task_id.clone(), TaskStatus::Queued, None, 5_100)
        .await
        .unwrap();
    let next = store.claim(claim_at(5_200)).await.unwrap().unwrap();
    assert_eq!(next.task.task_id, changed[0].task_id);
    assert_eq!(next.task.attempt_count, 2);
    finish(&store, next.lease, TaskStatus::Completed, 5_300).await;
    assert_eq!(
        store.list_agent_tasks("alice".into()).await.unwrap().len(),
        1
    );
}

// ---------------------------------------------------------------------------
// Migration evidence: v14 → v15 keeps every row and lands the ledger.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_version_fourteen_database_upgrades_and_keeps_admissions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v14.db");
    let store = SqliteStore::open(&path, 4).unwrap();
    create_agent(&store, "alice").await;
    let admitted = admit_ok(&store, "alice", "key-1").await;
    let plain = store.submit(task("plain", "chat-1")).await.unwrap();
    store
        .enqueue_agent_mailbox("alice".into(), mailbox("i1", "one", 1_000))
        .await
        .unwrap();
    drop(store);
    // Downgrade: drop the v15 tables/indexes and rebuild `agent_inputs`
    // exactly as version 14 defined it.
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "DROP INDEX tasks_agent; DROP INDEX agent_inputs_agent; DROP TABLE agent_admissions;
         DROP TABLE agent_inputs;
         CREATE TABLE agent_inputs(input_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, kind TEXT NOT NULL, status TEXT NOT NULL, target_task_id TEXT, record TEXT NOT NULL);
         CREATE INDEX agent_inputs_session ON agent_inputs(session_id,status);
         CREATE INDEX agent_inputs_target ON agent_inputs(target_task_id);",
    )
    .unwrap();
    db.pragma_update(None, "user_version", 14).unwrap();
    drop(db);
    // Reopen: the v15 migration lands the ledger and the mailbox column,
    // and every pre-existing row survives untouched.
    let reopened = SqliteStore::open(&path, 4).unwrap();
    let kept = reopened
        .get_task(&admitted.task.task_id)
        .await
        .unwrap()
        .expect("admitted task survives the upgrade");
    assert_eq!(kept, admitted.task);
    assert_eq!(kept.agent_id.as_deref(), Some("alice"));
    assert_eq!(kept.agent_revision, Some(1));
    assert_eq!(
        reopened.get_task("plain").await.unwrap().unwrap().task_id,
        plain.task_id
    );
    // The freshly landed ledger does its job on the upgraded store.
    let fresh_admission = reopened
        .admit_teammate_task(admit("alice", "key-2", "key-2"))
        .await
        .unwrap();
    assert!(!fresh_admission.replayed);
    let again = reopened
        .admit_teammate_task(admit("alice", "key-2", "key-2"))
        .await
        .unwrap();
    assert!(again.replayed);
    assert_eq!(again.task, fresh_admission.task);
    // The mailbox table is ready for the upgraded store.
    let (fresh, created) = reopened
        .enqueue_agent_mailbox("alice".into(), mailbox("i2", "two", 2_000))
        .await
        .unwrap();
    assert!(created);
    assert_eq!(fresh.agent_id.as_deref(), Some("alice"));
    let db = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = db
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

#[tokio::test]
async fn admitted_records_stay_byte_compatible_when_unbound() {
    // A non-admitted task serializes exactly like a pre-88 row: the new
    // binding fields are absent, not null-stamped.
    let store = SqliteStore::memory(4).unwrap();
    store.submit(task("plain", "chat-1")).await.unwrap();
    let claimed = store.claim(claim_at(1_100)).await.unwrap().unwrap();
    let record = claimed.task;
    let text = serde_json::to_string(&record).unwrap();
    assert!(!text.contains("agentId"), "{text}");
    assert!(!text.contains("agentRevision"), "{text}");
    assert!(!text.contains("admissionKey"), "{text}");
}

// ---------------------------------------------------------------------------
// Hardening: cancel/uncertain, bounded reads, deletion scope, child joins,
// atomic mailbox conversion, authority separation.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_with_lost_lease_is_uncertain_and_keeps_the_lane_blocked() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let first = admit_ok(&store, "alice", "first").await;
    let second = admit_ok(&store, "alice", "second").await;
    let claimed = store.claim(claim_short(1_100)).await.unwrap().unwrap();
    assert_eq!(claimed.task.task_id, first.task.task_id);
    store.cancel_task(&first.task.task_id, 1_120).await.unwrap();
    let changed = store
        .reconcile_agent_runs("alice".into(), 5_000)
        .await
        .unwrap();
    assert_eq!(changed.len(), 1);
    let task = store.get_task(&first.task.task_id).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Uncertain);
    assert!(task.cancel_requested);
    assert_eq!(
        task.error.as_deref(),
        Some("cancel_requested_outcome_unknown")
    );
    // The lane is still owned: the second task of the same agent waits.
    assert!(store.claim(claim_at(5_100)).await.unwrap().is_none());
    assert_eq!(
        store
            .get_task(&second.task.task_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Queued
    );
}

#[tokio::test]
async fn lease_recovery_of_a_cancelled_agent_task_is_uncertain() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let admitted = admit_ok(&store, "alice", "k").await;
    store.claim(claim_short(1_100)).await.unwrap().unwrap();
    store
        .cancel_task(&admitted.task.task_id, 1_120)
        .await
        .unwrap();
    store
        .recover(5_000, 3, 10, RecoveryMode::Automatic)
        .await
        .unwrap();
    let task = store
        .get_task(&admitted.task.task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status, TaskStatus::Uncertain);
    assert!(task.cancel_requested);
}

#[tokio::test]
async fn reconcile_ignores_terminal_rows_and_pages_are_newest_first() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    for index in 0..5 {
        let admitted = admit_ok(&store, "alice", &format!("k{index}")).await;
        let claimed = store.claim(claim_at(1_100 + index)).await.unwrap().unwrap();
        assert_eq!(claimed.task.task_id, admitted.task.task_id);
        finish(&store, claimed.lease, TaskStatus::Completed, 1_200 + index).await;
    }
    let live = admit_ok(&store, "alice", "live").await;
    assert!(
        store
            .reconcile_agent_runs("alice".into(), 9_000)
            .await
            .unwrap()
            .is_empty()
    );
    let unfinished = store
        .list_agent_unfinished_tasks("alice".into())
        .await
        .unwrap();
    assert_eq!(unfinished.len(), 1);
    assert_eq!(unfinished[0].task_id, live.task.task_id);
    let all = store.list_agent_tasks("alice".into()).await.unwrap();
    assert_eq!(all.len(), 6);
    let first = store
        .list_agent_tasks_page("alice".into(), None, 4)
        .await
        .unwrap();
    let ids = |tasks: &[TaskRecord]| tasks.iter().map(|t| t.task_id.clone()).collect::<Vec<_>>();
    let newest_first: Vec<_> = all.iter().rev().map(|t| t.task_id.clone()).collect();
    assert_eq!(ids(&first.tasks), newest_first[..4]);
    let cursor = first.next_before.expect("full page yields a cursor");
    let second = store
        .list_agent_tasks_page("alice".into(), Some(cursor), 4)
        .await
        .unwrap();
    assert_eq!(ids(&second.tasks), newest_first[4..]);
    assert_eq!(second.next_before, None, "short page ends the cursor");
    // limit 0 clamps to 1.
    let one = store
        .list_agent_tasks_page("alice".into(), None, 0)
        .await
        .unwrap();
    assert_eq!(one.tasks.len(), 1);
}

#[tokio::test]
async fn delete_is_blocked_by_the_agents_own_unfinished_work_only() {
    // Queued.
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    create_agent(&store, "bob").await;
    let queued = admit_ok(&store, "alice", "q").await;
    let blocked = store.delete_agent("alice".into(), 2_000).await.unwrap_err();
    assert!(
        matches!(&blocked, StoreError::InvalidTransition(m)
            if m == "agent_delete_blocked_active_work: 1 active task(s), 0 unresolved execution receipt(s)"),
        "{blocked}"
    );
    let preview = store
        .preview_agent_deletion("alice".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((preview.active_tasks, preview.deletable), (1, false));
    // WaitingApproval.
    let claimed = store.claim(claim_at(1_100)).await.unwrap().unwrap();
    assert_eq!(claimed.task.task_id, queued.task.task_id);
    store
        .park_turn(ParkTurn {
            fence: claimed.lease.fence(),
            assistant_message: None,
            events: vec![],
            request: None,
            now: 1_150,
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .get_task(&queued.task.task_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::WaitingApproval
    );
    assert!(store.delete_agent("alice".into(), 2_000).await.is_err());
    // Another agent's unresolved receipt does not block alice; bob's own
    // running task with a receipt blocks bob only.
    let bob_task = admit_ok(&store, "bob", "b").await;
    let bob_claim = store.claim(claim_at(1_200)).await.unwrap().unwrap();
    assert_eq!(bob_claim.task.task_id, bob_task.task.task_id);
    store
        .admit_execution(
            bob_claim.lease.fence(),
            ExecutionReceiptAdmission {
                execution_id: "exec-b".into(),
                run_id: bob_task.task.task_id.clone(),
                chat_id: "chat".into(),
                tool_call_id: "call-1".into(),
                tool_name: "write_file".into(),
                args_sha256: "0".repeat(64),
                ts_args_sha256: None,
                args_hash_provenance: ArgsHashProvenance::Canonical,
                context_sha256: "0".repeat(64),
                attempt_id: "attempt-1".into(),
                approval_request_id: None,
                approval_generation: None,
                now: 1_201,
            },
        )
        .await
        .unwrap();
    let bob_preview = store
        .preview_agent_deletion("bob".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bob_preview.unresolved_executions, 1);
    // A third agent with no work deletes despite the global receipt.
    create_agent(&store, "carol").await;
    let carol = store
        .preview_agent_deletion("carol".into())
        .await
        .unwrap()
        .unwrap();
    assert!(carol.deletable);
    assert_eq!(carol.unresolved_executions, 0);
    store.delete_agent("carol".into(), 2_000).await.unwrap();
}

#[tokio::test]
async fn delete_is_blocked_by_an_own_uncertain_task() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let admitted = admit_ok(&store, "alice", "k").await;
    let claimed = store.claim(claim_at(1_100)).await.unwrap().unwrap();
    store
        .mark_uncertain(claimed.lease.fence(), 1_150, "outcome_unknown")
        .await
        .unwrap();
    let preview = store
        .preview_agent_deletion("alice".into())
        .await
        .unwrap()
        .unwrap();
    assert!(!preview.deletable);
    assert!(store.delete_agent("alice".into(), 2_000).await.is_err());
    store
        .resolve_uncertain(admitted.task.task_id, TaskStatus::Failed, None, 1_200)
        .await
        .unwrap();
    assert!(store.delete_agent("alice".into(), 2_000).await.is_ok());
}

#[tokio::test]
async fn continue_after_children_refuses_while_a_child_is_unsettled() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    let parent = admit_ok(&store, "alice", "parent").await;
    let claimed = store
        .claim_with_concurrency(claim_at(1_100), 1)
        .await
        .unwrap()
        .unwrap();
    store
        .submit(child("child-1", &parent.task.task_id, 10, 1_150))
        .await
        .unwrap();
    assert!(
        store
            .wait_for_children(claimed.lease.fence(), 1_200)
            .await
            .unwrap()
    );
    let refused = store
        .continue_after_children(parent.task.task_id.clone(), 1_300)
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, StoreError::InvalidTransition(m) if m == "children_unsettled"),
        "{refused}"
    );
    let running = store
        .claim_with_concurrency(claim_at(1_310), 1)
        .await
        .unwrap()
        .unwrap();
    finish(&store, running.lease, TaskStatus::Completed, 1_400).await;
    assert_eq!(
        store
            .continue_after_children(parent.task.task_id, 1_500)
            .await
            .unwrap()
            .status,
        TaskStatus::Queued
    );
}

#[tokio::test]
async fn same_agent_child_admission_is_refused_unless_the_parent_is_terminal() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    create_agent(&store, "bob").await;
    let parent = admit_ok(&store, "alice", "parent").await;
    let mut under_parent = admit("alice", "kid", "kid");
    under_parent.parent_task_id = Some(parent.task.task_id.clone());
    let refused = store
        .admit_teammate_task(under_parent.clone())
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, StoreError::InvalidInput(m) if m == "self_delegation_unsupported"),
        "{refused}"
    );
    // A different agent may be the child.
    let mut other = admit("bob", "bob-kid", "bob-kid");
    other.parent_task_id = Some(parent.task.task_id.clone());
    store.admit_teammate_task(other).await.unwrap();
    // Once the parent is terminal it is a continuation link: admitted.
    let claimed = store.claim(claim_at(1_100)).await.unwrap().unwrap();
    assert_eq!(claimed.task.task_id, parent.task.task_id);
    finish(&store, claimed.lease, TaskStatus::Completed, 1_200).await;
    let admitted = store.admit_teammate_task(under_parent).await.unwrap();
    assert_eq!(
        admitted.task.parent_task_id.as_deref(),
        Some(parent.task.task_id.as_str())
    );
}

#[tokio::test]
async fn mailbox_promotion_creates_one_task_and_is_replay_safe() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    store
        .enqueue_agent_mailbox("alice".into(), mailbox("i1", "one", 1_000))
        .await
        .unwrap();
    let (input, task) = store
        .promote_next_agent_mailbox("alice".into(), promote(1_100))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(input.status, AgentInputStatus::Converted);
    assert_eq!(
        input.applied_task_id.as_deref(),
        Some(task.task_id.as_str())
    );
    assert_eq!(task.admission_key.as_deref(), Some("mailbox:i1"));
    assert_eq!(task.agent_id.as_deref(), Some("alice"));
    assert_eq!(task.kind, "teammate.mailbox");
    assert_eq!(
        task.payload,
        json!({"mailboxInputId": "i1", "inputKind": "follow_up", "content": {"text": "one"}})
    );
    // Promoting again finds nothing and creates nothing.
    assert!(
        store
            .promote_next_agent_mailbox("alice".into(), promote(1_101))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.list_agent_tasks("alice".into()).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn refused_mailbox_promotion_leaves_the_input_pending() {
    let store = SqliteStore::memory(4).unwrap();
    create_agent(&store, "alice").await;
    store
        .enqueue_agent_mailbox("alice".into(), mailbox("i1", "one", 1_000))
        .await
        .unwrap();
    store.archive_agent("alice".into(), 1_050).await.unwrap();
    let error = store
        .promote_next_agent_mailbox("alice".into(), promote(1_100))
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::InvalidTransition(_)), "{error}");
    let pending = store
        .list_agent_mailbox("alice".into(), Some(vec![AgentInputStatus::Pending]))
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        store
            .list_agent_tasks("alice".into())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn duplicate_and_import_clear_resource_grants() {
    let store = SqliteStore::memory(4).unwrap();
    let mut def = definition("src");
    def.resources = vec!["resource://src/repo".into()];
    store
        .create_agent(CreateAgent {
            agent_id: Some("src".into()),
            scope: "team".into(),
            owner_id: "owner".into(),
            definition: def,
            now: 1_000,
        })
        .await
        .unwrap();
    let copy = store
        .duplicate_agent(DuplicateAgent {
            agent_id: "src".into(),
            new_agent_id: Some("copy".into()),
            owner_id: None,
            now: 2_000,
        })
        .await
        .unwrap();
    assert!(copy.definition.resources.is_empty());
    let envelope = store.export_agents("team".into()).await.unwrap();
    let target = SqliteStore::memory(4).unwrap();
    target.import_agents(envelope, "team".into()).await.unwrap();
    let imported = target.get_agent("src".into()).await.unwrap().unwrap();
    assert!(imported.definition.resources.is_empty());
    let revisions = target.list_agent_revisions("src".into()).await.unwrap();
    assert!(revisions.iter().all(|r| r.snapshot.resources.is_empty()));
}
