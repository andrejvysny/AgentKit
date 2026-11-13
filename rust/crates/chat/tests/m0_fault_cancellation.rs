//! M0 fault matrix — cancellation settlement.
//!
//! Cancellation must settle truthfully: work that was owned and never started
//! is settled as never started; work whose outcome is unknown stays recorded
//! as unknown. Nothing may be reported as rolled back, because nothing in the
//! host can prove a rollback happened. The gate property asserted here by name
//! is that cancellation settles truthfully and never assumes a rollback.

mod runner_support;
use runner_support::{scenarios, support};

use agentkit_governance::{args_sha256, execution_id};
use agentkit_store::{
    ArgsHashProvenance, ExecutionReceiptAdmission, ExecutionReceiptStatus, SqliteStore, TaskStatus,
};
use serde_json::json;
use std::sync::atomic::Ordering;
use support::{Directory, Environment, snapshot};

fn vector(name: &str) -> support::Profile {
    let profile = support::profile(name);
    assert!(!profile.schedule.is_empty(), "{name} documents no schedule");
    assert!(
        profile.expected.typescript.is_empty(),
        "profile {name} is executed by the Rust host only"
    );
    profile
}

fn admission(execution: &str, attempt_id: &str, now: i64) -> ExecutionReceiptAdmission {
    ExecutionReceiptAdmission {
        execution_id: execution.into(),
        run_id: "run".into(),
        chat_id: "chat-1".into(),
        tool_call_id: "call-1".into(),
        tool_name: "write_file".into(),
        args_sha256: args_sha256(&json!({"path": "a.txt"})),
        ts_args_sha256: None,
        args_hash_provenance: ArgsHashProvenance::Canonical,
        context_sha256: "0".repeat(64),
        attempt_id: attempt_id.into(),
        approval_request_id: None,
        approval_generation: None,
        now,
    }
}

async fn receipt_extra(env: &Environment) -> serde_json::Value {
    let receipts = env
        .host
        .store()
        .list_executions("run".into())
        .await
        .unwrap();
    json!({
        "receiptCount": receipts.len(),
        "receiptStatuses": receipts
            .iter()
            .map(|record| format!("{:?}", record.status).to_lowercase())
            .collect::<Vec<_>>(),
        "resolutionRecorded": receipts.iter().any(|record| record.resolution.is_some()),
    })
}

/// Work that never started is settled as never started: no attempt row, no
/// effect, no event and no publication, so there is nothing to unwind.
#[tokio::test]
async fn a_queued_task_cancels_without_an_attempt_or_effect() {
    let vector = vector("m0_cancel_queued");
    let directory = Directory::new();
    let store = SqliteStore::open(directory.0.join("m0-cancel-queued.db"), 16).unwrap();
    let env = Environment::new(store, "m0_cancel_queued", 1000).await;
    env.submit().await;

    env.host.cancel("run").await.unwrap();
    assert!(
        !env.host.run_once().await.unwrap(),
        "a cancelled task is never dispatched"
    );
    let store = env.host.store();
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Cancelled
    );
    assert!(store.attempts("run").await.unwrap().is_empty());
    assert!(store.events("run", None, 100).await.unwrap().is_empty());
    assert!(support::publication(&env, "run").await.is_empty());
    assert_eq!(env.witness.effects.load(Ordering::SeqCst), 0);
    let extra = json!({"attemptRows": 0, "publicationRows": 0});
    assert_eq!(
        vec![snapshot(&env, "cancelled", extra).await],
        vector.expected.rust
    );
}

/// Gate property: cancellation settles truthfully. The effect whose evidence
/// was committed keeps it; the effect that was admitted without ever recording
/// a result stays recorded as uncertain. Neither is silently marked rolled
/// back, and the restarted host does not replay either one.
///
/// `failureReason` stays null on a cancel, matching the frozen oracle's
/// `cancel_after_commit` checkpoint on both implementations: the status already
/// says the run was cancelled, and stamping an execution-error reason here would
/// report a user's stop as a host failure.
#[tokio::test]
async fn cancellation_settles_truthfully_and_never_assumes_rollback() {
    let vector = vector("m0_cancel_uncertain_effect");
    let directory = Directory::new();
    let path = directory.0.join("m0-cancel-uncertain.db");
    let env = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "m0_cancel_uncertain_effect",
        1000,
    )
    .await;
    env.submit().await;
    let attempt = scenarios::suspend(&env).await;

    // A second effect is dispatched under the live lease: it started, and its
    // outcome will remain unknown when the run is cancelled.
    let lease = support::lease(&env, "run").await;
    let execution = execution_id("run", "call-1", None);
    assert!(
        env.host
            .store()
            .admit_execution(
                lease.fence(),
                admission(&execution, &lease.attempt_id, 1000)
            )
            .await
            .unwrap()
    );

    env.host.cancel("run").await.unwrap();
    env.witness.release.notify_one();
    attempt.await.unwrap().unwrap();

    let store = env.host.store();
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Cancelled
    );
    // (a) no unauthorized mutation: nothing beyond the one admitted receipt.
    let receipts = store.list_executions("run".into()).await.unwrap();
    assert_eq!(receipts.len(), 1);
    // (b) no duplicate side effect.
    assert_eq!(env.witness.effects.load(Ordering::SeqCst), 1);
    // (c) the unknown outcome is recorded as uncertain, never as rolled back.
    assert_eq!(receipts[0].status, ExecutionReceiptStatus::Admitted);
    assert!(receipts[0].result.is_none());
    assert!(
        receipts[0].resolution.is_none(),
        "a cancellation records no rollback"
    );
    // The committed effect keeps its durable evidence.
    assert_eq!(
        store
            .events("run", None, 100)
            .await
            .unwrap()
            .iter()
            .filter(|record| record.event["type"] == "fault.committed")
            .count(),
        1,
        "a committed effect is never un-reported"
    );
    let settled = receipt_extra(&env).await;
    assert_eq!(settled["resolutionRecorded"], json!(false));
    let transcript_settled = snapshot(&env, "settled", settled).await;
    drop(env);

    let reopened = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "m0_cancel_uncertain_effect",
        3000,
    )
    .await;
    assert!(
        !reopened.host.run_once().await.unwrap(),
        "a cancelled run does not resume on restart"
    );
    assert_eq!(
        reopened.witness.effects.load(Ordering::SeqCst),
        0,
        "the restarted host performed no effect"
    );
    let fresh = reopened
        .host
        .store()
        .acquire_lease("run", "attempt-2", "ghost", 3000, 1000)
        .await
        .unwrap()
        .fence();
    assert!(
        !reopened
            .host
            .store()
            .admit_execution(
                fresh,
                admission(&execution_id("run", "call-late", None), "attempt-2", 3000)
            )
            .await
            .unwrap(),
        "the uncertain effect is not replayed after cancellation"
    );
    assert_eq!(
        vec![
            transcript_settled,
            snapshot(&reopened, "restarted", receipt_extra(&reopened).await).await
        ],
        vector.expected.rust
    );
}
