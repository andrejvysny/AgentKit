//! M0 fault matrix — task admission and history replay.
//!
//! Admission is the host's promise to run a task, so a refused admission must
//! leave nothing behind and a duplicate must not double-book the queue. The
//! gate property asserted here by name is that replaying durable history —
//! reopening the database and running the host again — never admits or starts
//! work. Each profile is the one authored in the shared schedule
//! `contracts/tests/fixtures/conformance/runner-faults-v1.json`; the observed
//! transcript is read back through the store, never from a runner's memory.

mod runner_support;
use runner_support::support;

use agentkit_engine::EngineError;
use agentkit_store::{SqliteStore, SubmitTask};
use serde_json::json;
use support::{Directory, Environment, snapshot};

/// A profile the shared schedule actually carries: a boundary with no authored
/// profile is a failure, never a silently skipped test.
fn vector(name: &str) -> support::Profile {
    let profile = support::profile(name);
    assert!(
        !profile.schedule.is_empty(),
        "profile {} documents no schedule",
        profile.name
    );
    assert!(
        profile.expected.typescript.is_empty(),
        "profile {} is executed by the Rust host only",
        profile.name
    );
    profile
}

fn task(id: &str, kind: &str, scope: &str) -> SubmitTask {
    SubmitTask {
        task_id: id.into(),
        kind: kind.into(),
        scope_id: scope.into(),
        payload: json!({}),
        priority: 0,
        now: 1000,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: false,
    }
}

async fn durable_counts(env: &Environment, task_id: &str) -> serde_json::Value {
    let store = env.host.store();
    json!({
        "attemptRows": store.attempts(task_id).await.unwrap().len(),
        "eventRows": store.events(task_id, None, 100).await.unwrap().len(),
        "publicationRows": support::publication(env, task_id).await.len(),
    })
}

/// A kind this host registered no executor for is refused BEFORE any row
/// lands, so the queue never holds work the host has promised and cannot run.
#[tokio::test]
async fn a_refused_admission_leaves_no_durable_row() {
    let vector = vector("m0_admission_refused");
    let directory = Directory::new();
    let store = SqliteStore::open(directory.0.join("m0-admission-refused.db"), 16).unwrap();
    let env = Environment::new(store, "m0_admission_refused", 1000).await;
    env.submit().await;

    let refused = env
        .host
        .submit(task("ghost", "unregistered", "scope"))
        .await;
    assert!(
        matches!(&refused, Err(EngineError::NoExecutorForKind(kind)) if kind.as_str() == "unregistered"),
        "an unregistered kind must be refused by the admission gate"
    );
    // Zero unauthorized mutation: no task, no attempt, no event, no publication.
    let store = env.host.store();
    assert!(store.get_task("ghost").await.unwrap().is_none());
    assert!(store.attempts("ghost").await.unwrap().is_empty());
    assert!(
        matches!(
            store.events("ghost", None, 100).await,
            Err(agentkit_store::StoreError::NotFound(_))
        ),
        "a refused admission leaves no event stream at all"
    );
    let extra = json!({
        "ghostRow": false,
        "taskRows": store.list_tasks(None).await.unwrap().len(),
    });
    let mut transcript = vec![snapshot(&env, "refused", extra.clone()).await];

    assert!(env.host.run_once().await.unwrap());
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        agentkit_store::TaskStatus::Completed
    );
    transcript.push(snapshot(&env, "settled", extra).await);
    assert_eq!(transcript, vector.expected.rust);
}

/// The identical request is deduplicated to the one task it names; a differing
/// request under the same id is refused without disturbing the stored row.
#[tokio::test]
async fn a_duplicate_request_admits_exactly_one_task() {
    let vector = vector("m0_admission_duplicate");
    let directory = Directory::new();
    let store = SqliteStore::open(directory.0.join("m0-admission-duplicate.db"), 16).unwrap();
    let env = Environment::new(store, "m0_admission_duplicate", 1000).await;
    env.submit().await;
    env.submit().await;

    // A conflicting body under the same id is refused at the store gate.
    let conflict = env
        .host
        .store()
        .submit(task("run", "fault", "another-scope"))
        .await;
    assert!(
        matches!(&conflict, Err(agentkit_store::StoreError::Duplicate(id)) if id.as_str() == "run"),
        "a conflicting duplicate must be refused"
    );

    let store = env.host.store();
    let rows = store.list_tasks(None).await.unwrap();
    assert_eq!(rows.len(), 1, "the queue holds one task, not two");
    assert_eq!(
        rows[0].scope_id, "scope",
        "the refused conflict did not rewrite the admitted row"
    );
    let extra = json!({"taskRows": rows.len(), "conflictRefused": true});
    let mut transcript = vec![snapshot(&env, "deduplicated", extra.clone()).await];

    assert!(env.host.run_once().await.unwrap());
    assert!(
        !env.host.run_once().await.unwrap(),
        "the deduplicated request dispatches once"
    );
    assert_eq!(store.attempts("run").await.unwrap().len(), 1);
    assert_eq!(
        env.witness
            .effects
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    transcript.push(snapshot(&env, "settled", extra).await);
    assert_eq!(transcript, vector.expected.rust);
}

/// Gate property: replaying durable history never admits work. A second host
/// over the same database recovers, polls and runs without producing a single
/// new attempt, event, publication or side effect, and the client-visible
/// projection still agrees with durable state.
#[tokio::test]
async fn history_replay_never_admits_work() {
    let vector = vector("m0_replay_no_admission");
    let directory = Directory::new();
    let path = directory.0.join("m0-replay.db");
    let first = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "m0_replay_no_admission",
        1000,
    )
    .await;
    first.submit().await;
    assert!(first.host.run_once().await.unwrap());
    let before = durable_counts(&first, "run").await;
    let events_before = first
        .host
        .store()
        .events("run", None, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|record| record.event)
        .collect::<Vec<_>>();
    let transcript_first = snapshot(&first, "settled", before.clone()).await;
    drop(first);

    // A real reopen: a fresh host, fresh executors, a fresh witness.
    let reopened = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "m0_replay_no_admission",
        3000,
    )
    .await;
    assert!(
        !reopened.host.run_once().await.unwrap(),
        "replaying history must not claim or start work"
    );
    assert_eq!(
        reopened
            .witness
            .effects
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the replaying host performed no effect"
    );
    let after = durable_counts(&reopened, "run").await;
    assert_eq!(after, before, "replay changed no durable row");
    // The client-visible projection after restart agrees with durable state.
    let replayed = reopened
        .host
        .store()
        .events_after_number("run", Some(0.0), 100)
        .await
        .unwrap()
        .into_iter()
        .map(|record| record.event)
        .collect::<Vec<_>>();
    assert_eq!(
        replayed,
        events_before[1..].to_vec(),
        "a reconnecting cursor serves the durable suffix and nothing else"
    );
    let transcript = vec![
        transcript_first,
        snapshot(&reopened, "replayed", after).await,
    ];
    assert_eq!(transcript, vector.expected.rust);
}
