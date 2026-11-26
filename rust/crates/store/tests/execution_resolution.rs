mod support;
use agentkit_store::*;
use serde_json::json;
use support::*;

fn admission(execution_id: &str) -> ExecutionReceiptAdmission {
    ExecutionReceiptAdmission {
        execution_id: execution_id.into(),
        run_id: "run".into(),
        chat_id: "chat".into(),
        tool_call_id: "call".into(),
        tool_name: "write".into(),
        args_sha256: "a".repeat(64),
        ts_args_sha256: None,
        args_hash_provenance: ArgsHashProvenance::Canonical,
        context_sha256: "b".repeat(64),
        attempt_id: "attempt".into(),
        approval_request_id: None,
        approval_generation: None,
        now: 1002,
    }
}

fn resolution(outcome: ResolutionOutcome, result: Option<serde_json::Value>) -> ResolveExecution {
    ResolveExecution {
        outcome,
        result,
        note: Some("checked by hand".into()),
        resolved_by: Some("operator".into()),
        now: 2000,
    }
}

/// Admit a receipt and leave its run running (the live case).
async fn live(store: &SqliteStore) -> Lease {
    store.submit(task("run", "chat")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    assert!(
        store
            .admit_execution(lease.fence(), admission("exec"))
            .await
            .unwrap()
    );
    lease
}

/// Admit a receipt, then end its run as a crash would.
async fn admitted(store: &SqliteStore) -> Lease {
    let lease = live(store).await;
    finish(store, lease.clone(), TaskStatus::Failed, 1060).await;
    lease
}

#[tokio::test]
async fn not_started_and_acknowledged_resolve_without_a_result() {
    for outcome in [
        ResolutionOutcome::NotStarted,
        ResolutionOutcome::Acknowledged,
    ] {
        let store = SqliteStore::memory(8).unwrap();
        admitted(&store).await;
        let record = store
            .resolve_execution("exec".into(), resolution(outcome, None))
            .await
            .unwrap();
        assert_eq!(record.status, ExecutionReceiptStatus::Resolved);
        assert!(record.result.is_none());
        let audit = record.resolution.unwrap();
        assert_eq!(audit.outcome, outcome);
        assert_eq!(audit.resolved_at, 2000);
        assert_eq!(audit.resolved_by.as_deref(), Some("operator"));
        let stored = store.list_executions("run".into()).await.unwrap();
        assert_eq!(stored[0].status, ExecutionReceiptStatus::Resolved);
    }
}

#[tokio::test]
async fn completed_resolution_stores_the_supplied_result() {
    let store = SqliteStore::memory(8).unwrap();
    admitted(&store).await;
    let result = json!({"ok": true, "summary": "done by hand"});
    let record = store
        .resolve_execution(
            "exec".into(),
            resolution(ResolutionOutcome::Completed, Some(result.clone())),
        )
        .await
        .unwrap();
    assert_eq!(record.status, ExecutionReceiptStatus::Completed);
    assert_eq!(record.result, Some(result));
    assert_eq!(
        record.resolution.unwrap().outcome,
        ResolutionOutcome::Completed
    );
}

#[tokio::test]
async fn repeat_is_idempotent_and_conflicting_repeat_is_refused() {
    let store = SqliteStore::memory(8).unwrap();
    admitted(&store).await;
    let first = store
        .resolve_execution(
            "exec".into(),
            resolution(ResolutionOutcome::NotStarted, None),
        )
        .await
        .unwrap();
    let mut again = resolution(ResolutionOutcome::NotStarted, None);
    again.now = 9999;
    again.note = Some("different note".into());
    assert_eq!(
        store.resolve_execution("exec".into(), again).await.unwrap(),
        first
    );
    let conflict = store
        .resolve_execution(
            "exec".into(),
            resolution(ResolutionOutcome::Acknowledged, None),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&conflict, StoreError::ReceiptTransition { reason: ReceiptTransitionReason::NotUncertain, message: m } if m.starts_with(RECEIPT_NOT_UNCERTAIN)),
        "{conflict:?}"
    );
}

#[tokio::test]
async fn normally_completed_receipt_cannot_be_resolved() {
    let store = SqliteStore::memory(8).unwrap();
    let lease = live(&store).await;
    store
        .complete_execution(lease.fence(), "exec".into(), json!({"ok": true}), 1050)
        .await
        .unwrap();
    finish(&store, lease, TaskStatus::Completed, 1060).await;
    for outcome in [ResolutionOutcome::NotStarted, ResolutionOutcome::Completed] {
        let result = (outcome == ResolutionOutcome::Completed).then(|| json!({"ok": true}));
        let error = store
            .resolve_execution("exec".into(), resolution(outcome, result))
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                StoreError::ReceiptTransition {
                    reason: ReceiptTransitionReason::NotUncertain,
                    ..
                }
            ),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn invalid_requests_and_missing_receipts_are_refused() {
    let store = SqliteStore::memory(8).unwrap();
    admitted(&store).await;
    for bad in [
        resolution(ResolutionOutcome::Completed, None),
        resolution(ResolutionOutcome::Completed, Some(json!("text"))),
        resolution(ResolutionOutcome::NotStarted, Some(json!({"ok": true}))),
        ResolveExecution {
            note: Some("x".repeat(2001)),
            ..resolution(ResolutionOutcome::NotStarted, None)
        },
    ] {
        let error = store
            .resolve_execution("exec".into(), bad)
            .await
            .unwrap_err();
        assert!(matches!(error, StoreError::InvalidInput(_)), "{error:?}");
    }
    let missing = store
        .resolve_execution(
            "nope".into(),
            resolution(ResolutionOutcome::NotStarted, None),
        )
        .await
        .unwrap_err();
    assert!(matches!(missing, StoreError::NotFound(_)));
}

#[tokio::test]
async fn resolving_is_refused_while_the_run_is_active() {
    let store = SqliteStore::memory(8).unwrap();
    let lease = live(&store).await;
    let error = store
        .resolve_execution(
            "exec".into(),
            resolution(ResolutionOutcome::NotStarted, None),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::ReceiptTransition { reason: ReceiptTransitionReason::RunActive, message: m } if m.starts_with(RECEIPT_RUN_ACTIVE)),
        "{error:?}"
    );
    let rows = store.list_executions("run".into()).await.unwrap();
    assert_eq!(rows[0].status, ExecutionReceiptStatus::Admitted);
    finish(&store, lease, TaskStatus::Failed, 1060).await;
    store
        .resolve_execution(
            "exec".into(),
            resolution(ResolutionOutcome::NotStarted, None),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn completed_repeat_must_name_the_same_result() {
    let store = SqliteStore::memory(8).unwrap();
    admitted(&store).await;
    let first = json!({"ok": true, "summary": "a"});
    let record = store
        .resolve_execution(
            "exec".into(),
            resolution(ResolutionOutcome::Completed, Some(first.clone())),
        )
        .await
        .unwrap();
    let same = store
        .resolve_execution(
            "exec".into(),
            resolution(ResolutionOutcome::Completed, Some(first)),
        )
        .await
        .unwrap();
    assert_eq!(same, record);
    let different = store
        .resolve_execution(
            "exec".into(),
            resolution(
                ResolutionOutcome::Completed,
                Some(json!({"ok": true, "summary": "b"})),
            ),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&different, StoreError::ReceiptTransition { reason: ReceiptTransitionReason::NotUncertain, message: m } if m.starts_with(RECEIPT_NOT_UNCERTAIN)),
        "{different:?}"
    );
}

#[tokio::test]
async fn oversized_result_and_actor_are_refused() {
    let store = SqliteStore::memory(8).unwrap();
    admitted(&store).await;
    let huge = json!({"ok": true, "blob": "y".repeat(MAX_RESOLUTION_RESULT_BYTES)});
    for bad in [
        resolution(ResolutionOutcome::Completed, Some(huge)),
        ResolveExecution {
            resolved_by: Some("x".repeat(201)),
            ..resolution(ResolutionOutcome::NotStarted, None)
        },
    ] {
        let error = store
            .resolve_execution("exec".into(), bad)
            .await
            .unwrap_err();
        assert!(matches!(error, StoreError::InvalidInput(_)), "{error:?}");
    }
}

#[tokio::test]
async fn a_late_completion_cannot_overwrite_a_resolved_receipt() {
    let store = SqliteStore::memory(8).unwrap();
    let lease = admitted(&store).await;
    store
        .resolve_execution(
            "exec".into(),
            resolution(ResolutionOutcome::NotStarted, None),
        )
        .await
        .unwrap();
    let before = store.list_executions("run".into()).await.unwrap();
    // The settled run's lease may already be gone, so this is refused by the
    // fence or by the status guard; the row must be unchanged regardless.
    assert!(
        store
            .complete_execution(lease.fence(), "exec".into(), json!({"ok": true}), 1050)
            .await
            .is_err()
    );
    assert_eq!(store.list_executions("run".into()).await.unwrap(), before);
}

/// A barrier for `write` with the arguments `admission` uses, left by the
/// tolerant import for execution `source`, scoped to `chat` (or to nothing).
fn barrier(source: &str, chat: Option<&str>) -> UncertaintyBarrierRecord {
    let args = "a".repeat(64);
    UncertaintyBarrierRecord {
        barrier_id: barrier_id_of(Some(source), None, "write", Some(&args)),
        chat_id: chat.map(str::to_owned),
        run_id: None,
        tool_name: "write".into(),
        args_sha256: Some(args),
        args_hash_provenance: ArgsHashProvenance::Canonical,
        reason: "unplaceable".into(),
    }
}

/// An operator lifts a barrier the chat sees by its id, and only that one:
/// another chat's barrier and a missing one are both not found. Once the
/// barriers that held the operation are gone, it is admitted.
#[tokio::test]
async fn an_operator_lifts_a_barrier_the_chat_sees_and_no_other() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("barriers.db");
    let store = SqliteStore::open(&path, 8).unwrap();
    let mine = barrier("exec-1", Some("chat"));
    let other = barrier("exec-2", Some("other-chat"));
    let global = barrier("exec-3", None);
    let db = rusqlite::Connection::open(&path).unwrap();
    for seeded in [&mine, &other, &global] {
        assert!(insert_barrier(&db, seeded).unwrap());
    }
    store.submit(task("run", "chat")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    let admit = || store.admit_execution(lease.fence(), admission("exec"));
    assert!(!admit().await.unwrap(), "held by the barriers");
    for id in [other.barrier_id.as_str(), "barrier:missing"] {
        let refused = store
            .resolve_uncertainty_barrier("chat".into(), id.into())
            .await
            .unwrap_err();
        assert!(matches!(refused, StoreError::NotFound(_)), "{refused:?}");
    }
    let lifted = store
        .resolve_uncertainty_barrier("chat".into(), mine.barrier_id.clone())
        .await
        .unwrap();
    assert_eq!(lifted, mine, "the lifted barrier is returned");
    assert!(
        !admit().await.unwrap(),
        "the unattributed barrier still holds"
    );
    store
        .resolve_uncertainty_barrier("chat".into(), global.barrier_id.clone())
        .await
        .unwrap();
    assert!(admit().await.unwrap(), "nothing holds the operation now");
    let left = store
        .list_uncertainty_barriers("other-chat".into(), "other-run".into())
        .await
        .unwrap();
    assert_eq!(left, vec![other], "another chat's barrier is untouched");
    let repeat = store
        .resolve_uncertainty_barrier("chat".into(), mine.barrier_id)
        .await
        .unwrap_err();
    assert!(matches!(repeat, StoreError::NotFound(_)), "{repeat:?}");
}
