//! M0 fault matrix — the effect-receipt commit boundary.
//!
//! An external effect and its receipt cannot be one atomic transaction, so the
//! store admits the effect *before* the body runs and records the result
//! *after*. A receipt left `admitted` without a result is the honest
//! representation of an unknown outcome: it must never be silently re-executed
//! and must never be reported as rolled back. The gate property asserted here
//! by name is that an uncertain effect is never replayed.

#[path = "runner_conformance/scenarios.rs"]
mod scenarios;
#[path = "runner_conformance/support.rs"]
mod support;

use agentkit_host::{args_sha256, execution_id};
use agentkit_store::{
    ArgsHashProvenance, ExecutionReceiptAdmission, ExecutionReceiptStatus, SqliteStore,
};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use support::{Directory, Environment, snapshot};

const CALL: &str = "call-1";
const CHAT: &str = "chat-1";
const TOOL: &str = "write_file";

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
        chat_id: CHAT.into(),
        tool_call_id: CALL.into(),
        tool_name: TOOL.into(),
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

async fn receipt_extra(env: &Environment) -> Value {
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

/// Gate property: an uncertain effect is never replayed. The attempt is dropped
/// after the effect was admitted and before its result was recorded, so on
/// reopen the outcome is unknown. The restarted host records it as uncertain —
/// not completed, not resolved, not rolled back — and refuses to admit the
/// identical effect again, however the operation is re-issued.
#[tokio::test]
async fn an_uncertain_effect_is_never_replayed() {
    let vector = vector("m0_receipt_uncertain");
    let directory = Directory::new();
    let path = directory.0.join("m0-receipt-uncertain.db");
    let env = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "m0_receipt_uncertain",
        1000,
    )
    .await;
    env.submit().await;
    let attempt = scenarios::suspend(&env).await;

    // The dispatch layer admits the second effect under the LIVE attempt lease.
    let lease = support::lease(&env, "run").await;
    let execution = execution_id("run", CALL, None);
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

    // Process death between the external write and its recorded result.
    attempt.abort();
    assert!(
        attempt.await.is_err(),
        "the dropped attempt never settled itself"
    );
    let uncertain = receipt_extra(&env).await;
    assert_eq!(uncertain["receiptStatuses"], json!(["admitted"]));
    assert_eq!(uncertain["resolutionRecorded"], json!(false));
    let transcript_uncertain = snapshot(&env, "uncertain", uncertain).await;
    drop(env);

    let reopened = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "m0_receipt_uncertain",
        2100,
    )
    .await;
    assert!(
        !reopened.host.run_once().await.unwrap(),
        "recovery parks the abandoned attempt instead of redispatching it"
    );
    assert_eq!(
        reopened.witness.effects.load(Ordering::SeqCst),
        0,
        "the restarted host performed no effect"
    );

    // Re-issuing the identical operation under a fresh identity is refused:
    // an unknown outcome is never treated as an absent one.
    let fresh = reopened
        .host
        .store()
        .acquire_lease("run", "attempt-2", "ghost", 2100, 1000)
        .await
        .unwrap()
        .fence();
    assert!(
        !reopened
            .host
            .store()
            .admit_execution(
                fresh,
                admission(&execution_id("run", "call-late", None), "attempt-2", 2100)
            )
            .await
            .unwrap(),
        "an uncertain effect is never replayed"
    );
    let restarted = receipt_extra(&reopened).await;
    assert_eq!(
        restarted["receiptCount"],
        json!(1),
        "no duplicate side effect"
    );
    assert_eq!(restarted["receiptStatuses"], json!(["admitted"]));
    assert_eq!(restarted["resolutionRecorded"], json!(false));
    assert!(
        reopened
            .host
            .store()
            .list_uncertainty_barriers(CHAT.into(), "run".into())
            .await
            .unwrap()
            .is_empty()
    );
    // The effect that DID complete keeps its durable evidence: nothing was
    // silently rolled back.
    assert_eq!(
        reopened
            .host
            .store()
            .events("run", None, 100)
            .await
            .unwrap()
            .iter()
            .filter(|record| record.event["type"] == "fault.committed")
            .count(),
        1
    );
    assert_eq!(
        vec![
            transcript_uncertain,
            snapshot(&reopened, "restarted", restarted).await
        ],
        vector.expected.rust
    );
}

/// An effect whose result was recorded before the crash is durable evidence:
/// completing it again is idempotent, and reopening serves the record instead
/// of executing the effect a second time.
#[tokio::test]
async fn a_committed_effect_is_recorded_once_and_never_re_executed() {
    let vector = vector("m0_receipt_committed");
    let directory = Directory::new();
    let path = directory.0.join("m0-receipt-committed.db");
    let env = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "m0_receipt_committed",
        1000,
    )
    .await;
    env.submit().await;
    let attempt = scenarios::suspend(&env).await;

    let lease = support::lease(&env, "run").await;
    let fence = lease.fence();
    let execution = execution_id("run", CALL, None);
    assert!(
        env.host
            .store()
            .admit_execution(
                fence.clone(),
                admission(&execution, &lease.attempt_id, 1000)
            )
            .await
            .unwrap()
    );
    env.host
        .store()
        .complete_execution(
            fence.clone(),
            execution.clone(),
            json!({"done": true}),
            1000,
        )
        .await
        .unwrap();
    env.host
        .store()
        .complete_execution(fence, execution.clone(), json!({"done": true}), 1000)
        .await
        .unwrap();

    env.witness.release.notify_one();
    attempt.await.unwrap().unwrap();
    let settled = receipt_extra(&env).await;
    assert_eq!(settled["receiptStatuses"], json!(["completed"]));
    let transcript_settled = snapshot(&env, "settled", settled).await;
    drop(env);

    let reopened = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "m0_receipt_committed",
        3000,
    )
    .await;
    assert!(
        !reopened.host.run_once().await.unwrap(),
        "a recorded effect is served, never executed again"
    );
    let receipts = reopened
        .host
        .store()
        .list_executions("run".into())
        .await
        .unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].status, ExecutionReceiptStatus::Completed);
    assert_eq!(receipts[0].result, Some(json!({"done": true})));
    assert_eq!(
        reopened.witness.effects.load(Ordering::SeqCst),
        0,
        "the restarted host performed no effect"
    );
    assert_eq!(
        vec![
            transcript_settled,
            snapshot(&reopened, "restarted", receipt_extra(&reopened).await).await
        ],
        vector.expected.rust
    );
}
