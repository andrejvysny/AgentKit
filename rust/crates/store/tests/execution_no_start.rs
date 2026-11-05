mod support;
use agentkit_store::*;
use serde_json::{Value, json};
use support::*;

fn result() -> Value {
    json!({"ok":false,"data":{"errorCode":"command_not_started","phase":"before_spawn","started":false}})
}

async fn admitted(store: &SqliteStore) -> Lease {
    store.submit(task("run", "chat")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    assert!(
        store
            .admit_execution(
                lease.fence(),
                ExecutionReceiptAdmission {
                    execution_id: "exec".into(),
                    run_id: "run".into(),
                    chat_id: "chat".into(),
                    tool_call_id: "call".into(),
                    tool_name: "run_command".into(),
                    args_sha256: "a".repeat(64),
                    ts_args_sha256: None,
                    args_hash_provenance: ArgsHashProvenance::Canonical,
                    context_sha256: "b".repeat(64),
                    attempt_id: lease.attempt_id.clone(),
                    approval_request_id: None,
                    approval_generation: None,
                    now: 1002,
                }
            )
            .await
            .unwrap()
    );
    lease
}

#[tokio::test]
async fn no_start_finishes_exact_admission_after_task_fence_ended() {
    let store = SqliteStore::memory(8).unwrap();
    let lease = admitted(&store).await;
    finish(&store, lease.clone(), TaskStatus::Failed, 1060).await;
    assert!(matches!(
        store
            .complete_execution(lease.fence(), "exec".into(), result(), 1061)
            .await,
        Err(StoreError::LeaseLost(_))
    ));
    assert!(
        store
            .complete_execution_not_started(
                lease.fence(),
                lease.attempt_id.clone(),
                "exec".into(),
                result()
            )
            .await
            .unwrap()
    );
    let records = store.list_executions("run".into()).await.unwrap();
    assert_eq!(records[0].status, ExecutionReceiptStatus::Completed);
    assert_eq!(records[0].result, Some(result()));
    assert!(matches!(
        store
            .complete_execution(lease.fence(), "exec".into(), json!({"ok":true}), 1062,)
            .await,
        Err(StoreError::LeaseLost(_))
    ));
    assert!(
        records[0].resolution.is_none(),
        "factual driver result is not operator recovery"
    );
    assert!(
        store
            .complete_execution_not_started(
                lease.fence(),
                lease.attempt_id.clone(),
                "exec".into(),
                result()
            )
            .await
            .unwrap()
    );
    let mut conflict = result();
    conflict["summary"] = json!("different refusal");
    assert!(
        !store
            .complete_execution_not_started(
                lease.fence(),
                lease.attempt_id.clone(),
                "exec".into(),
                conflict
            )
            .await
            .unwrap()
    );
    assert_eq!(store.list_executions("run".into()).await.unwrap(), records);
}

#[tokio::test]
async fn no_start_rejects_wrong_attempt_token_and_fencing_generation() {
    let store = SqliteStore::memory(8).unwrap();
    let lease = admitted(&store).await;
    let mut wrong_token = lease.fence();
    wrong_token.lease_token = "wrong".into();
    let mut wrong_generation = lease.fence();
    wrong_generation.fencing_token += 1;
    for proof in [wrong_token, wrong_generation] {
        assert!(
            !store
                .complete_execution_not_started(
                    proof,
                    lease.attempt_id.clone(),
                    "exec".into(),
                    result()
                )
                .await
                .unwrap()
        );
    }
    assert!(
        !store
            .complete_execution_not_started(
                lease.fence(),
                "other-attempt".into(),
                "exec".into(),
                result()
            )
            .await
            .unwrap()
    );
    assert_eq!(
        store.list_executions("run".into()).await.unwrap()[0].status,
        ExecutionReceiptStatus::Admitted
    );
}

#[tokio::test]
async fn no_start_rejects_effect_results_and_preserves_completed_effect() {
    let store = SqliteStore::memory(8).unwrap();
    let lease = admitted(&store).await;
    for invalid in [
        json!({"ok":true}),
        json!({"ok":false,"data":{"started":false}}),
    ] {
        assert!(matches!(
            store
                .complete_execution_not_started(
                    lease.fence(),
                    lease.attempt_id.clone(),
                    "exec".into(),
                    invalid
                )
                .await,
            Err(StoreError::InvalidInput(_))
        ));
    }
    store
        .complete_execution(lease.fence(), "exec".into(), json!({"ok":true}), 1003)
        .await
        .unwrap();
    assert!(
        !store
            .complete_execution_not_started(
                lease.fence(),
                lease.attempt_id.clone(),
                "exec".into(),
                result()
            )
            .await
            .unwrap()
    );
    assert_eq!(
        store.list_executions("run".into()).await.unwrap()[0].result,
        Some(json!({"ok":true}))
    );
}

#[tokio::test]
async fn no_start_preserves_a_prior_operator_resolution() {
    let store = SqliteStore::memory(8).unwrap();
    let lease = admitted(&store).await;
    finish(&store, lease.clone(), TaskStatus::Failed, 1060).await;
    let resolved = store
        .resolve_execution(
            "exec".into(),
            ResolveExecution {
                outcome: ResolutionOutcome::Acknowledged,
                result: None,
                note: Some("fixture operator decision must remain immutable".into()),
                resolved_by: Some("fixture-operator".into()),
                now: 1061,
            },
        )
        .await
        .unwrap();
    assert!(
        !store
            .complete_execution_not_started(
                lease.fence(),
                lease.attempt_id.clone(),
                "exec".into(),
                result(),
            )
            .await
            .unwrap()
    );
    assert_eq!(
        store.list_executions("run".into()).await.unwrap(),
        vec![resolved]
    );
}
