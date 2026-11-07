//! M0 fault matrix — a corrupted durable record.
//!
//! Receipts are the only proof of what an external effect did. An unreadable
//! record must therefore be a hard recovery error — never read as "no effect
//! happened", which would let the identical mutation run a second time. The
//! database is corrupted on disk with the store closed and reopened, so the
//! assertion is against the durable file, not a mocked read.

#[path = "runner_conformance/scenarios.rs"]
mod scenarios;
#[path = "runner_conformance/support.rs"]
mod support;

use agentkit_host::{args_sha256, execution_id};
use agentkit_store::{ArgsHashProvenance, ExecutionReceiptAdmission, SqliteStore};
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

/// An unreadable receipt blocks its own run: the read fails, a fresh admission
/// for the identical effect fails with it, and no effect is executed for the
/// store's inability to read its own evidence.
#[tokio::test]
async fn an_unreadable_record_is_an_error_never_an_absence() {
    let vector = vector("m0_record_corrupted");
    let directory = Directory::new();
    let path = directory.0.join("m0-corrupt-record.db");
    let env = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "m0_record_corrupted",
        1000,
    )
    .await;
    env.submit().await;
    assert!(env.host.run_once().await.unwrap());
    let lease = env
        .host
        .store()
        .acquire_lease("run", "attempt-2", "dispatcher", 1000, 1000)
        .await
        .unwrap();
    assert!(
        env.host
            .store()
            .admit_execution(
                lease.fence(),
                admission(&execution_id("run", "call-1", None), "attempt-2", 1000)
            )
            .await
            .unwrap()
    );
    let receipts = env
        .host
        .store()
        .list_executions("run".into())
        .await
        .unwrap();
    assert_eq!(
        receipts.len(),
        1,
        "the receipt is durable before the corruption"
    );
    drop(env);

    // Corruption at rest: the record's own bytes no longer describe a receipt.
    let database = rusqlite::Connection::open(&path).unwrap();
    let truncated = json!({"executionId": "truncated"}).to_string();
    let changed = database
        .execute(
            "UPDATE tool_executions SET record = ?1 WHERE run_id = 'run'",
            rusqlite::params![truncated],
        )
        .unwrap();
    assert_eq!(changed, 1, "the seeded receipt row was corrupted");
    drop(database);

    let reopened = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "m0_record_corrupted",
        3000,
    )
    .await;
    let read = reopened.host.store().list_executions("run".into()).await;
    assert!(
        read.is_err(),
        "an unreadable record is a hard error, not an absence"
    );
    let fresh = reopened
        .host
        .store()
        .acquire_lease("run", "attempt-3", "ghost", 3000, 1000)
        .await
        .unwrap()
        .fence();
    let admitted = reopened
        .host
        .store()
        .admit_execution(
            fresh,
            admission(&execution_id("run", "call-late", None), "attempt-3", 3000),
        )
        .await;
    assert!(
        admitted.is_err(),
        "a corrupt record blocks a fresh admission"
    );
    assert_eq!(
        reopened.witness.effects.load(Ordering::SeqCst),
        0,
        "unreadable evidence executes nothing"
    );
    let extra = json!({"readFailed": true, "admitFailed": true});
    assert_eq!(
        vec![snapshot(&reopened, "corrupted", extra).await],
        vector.expected.rust
    );
}
