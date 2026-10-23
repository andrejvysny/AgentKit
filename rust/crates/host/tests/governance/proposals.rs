use super::support::*;
use agentkit_core::Tool;
use agentkit_host::*;
use agentkit_store::ProposalStatus;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

fn input() -> Value {
    json!({"action_id":"edit_item-42_document","change":"new value"})
}

#[tokio::test]
async fn builder_and_memory_proposal_preserve_overflowing_numbers() {
    let fixture = fixture(WriteExecutionMode::ProposeOnly, false).await;
    let input = agentkit_contracts::parse_javascript_json(
        r#"{"change":1e400,"action_id":"edit_item_document"}"#,
    )
    .unwrap();
    fixture.tool.execute(context(), input).await.unwrap();
    let observed = fixture.builder.last_input.lock().unwrap().clone().unwrap();
    assert_eq!(
        agentkit_contracts::javascript_number(&observed["change"]),
        Some(f64::INFINITY)
    );
    let stored = fixture.store.proposals(None, None).await.unwrap().remove(0);
    assert_eq!(
        agentkit_contracts::javascript_number(&stored.operations[0]["change"]),
        Some(f64::INFINITY)
    );
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn builder_receives_overflow_before_file_proposal_persistence_coerces_it_to_null() {
    let directory =
        std::env::temp_dir().join(format!("agentkit-governance-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let fixture = fixture_with_store(
        WriteExecutionMode::ProposeOnly,
        false,
        agentkit_store::SqliteStore::open(directory.join("store.sqlite"), 32).unwrap(),
    )
    .await;
    let input = agentkit_contracts::parse_javascript_json(r#"{"change":1e400}"#).unwrap();
    fixture.tool.execute(context(), input).await.unwrap();
    let observed = fixture.builder.last_input.lock().unwrap().clone().unwrap();
    assert_eq!(
        agentkit_contracts::javascript_number(&observed["change"]),
        Some(f64::INFINITY)
    );
    let stored = fixture.store.proposals(None, None).await.unwrap().remove(0);
    assert!(stored.operations[0]["change"].is_null());
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
    drop(fixture);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn proposal_mode_always_stages_write_then_manual_apply_records_receipt() {
    let fixture = fixture(WriteExecutionMode::ProposeOnly, false).await;
    assert_eq!(fixture.tool.definition().effect, "write");
    assert!(
        fixture
            .tool
            .definition()
            .description
            .starts_with("Stages a write proposal")
    );
    let result = fixture.tool.execute(context(), input()).await.unwrap();
    assert_eq!(result.model_data.unwrap()["status"], "pending");
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
    let id = result.data.unwrap()["proposalId"]
        .as_str()
        .unwrap()
        .to_owned();
    fixture
        .service
        .approve(&id, &json!({"id":"trusted-user"}))
        .await
        .unwrap();
    let receipt = fixture
        .service
        .apply(&id, "manual-operation", &json!({"id":"trusted-user"}))
        .await
        .unwrap();
    let replay = fixture
        .service
        .apply(&id, "manual-operation", &json!({"id":"trusted-user"}))
        .await
        .unwrap();
    assert_eq!(receipt, replay);
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn missing_chat_and_cancellation_never_stage_or_apply() {
    let fixture = fixture(WriteExecutionMode::DirectWhenAuthorized, false).await;
    let mut missing = context();
    missing.chat_id = None;
    let result = fixture.tool.execute(missing, input()).await.unwrap();
    assert!(!result.ok);
    assert_eq!(
        result.model_data.unwrap()["skipped"][0]["reason"],
        "missing chatId"
    );
    let cancelled = context();
    cancelled.cancellation.cancel();
    assert_eq!(
        fixture
            .tool
            .execute(cancelled, input())
            .await
            .unwrap_err()
            .code,
        "cancelled"
    );
    assert!(
        fixture
            .store
            .proposals(None, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn direct_mode_persists_before_apply_and_replays_existing_receipt() {
    let fixture = fixture(WriteExecutionMode::DirectWhenAuthorized, false).await;
    let result = fixture.tool.execute(context(), input()).await.unwrap();
    assert!(result.ok);
    assert_eq!(result.data.as_ref().unwrap()["status"], "applied");
    assert_eq!(result.model_data.as_ref().unwrap()["appliedCount"], 1);
    let proposals = fixture.store.proposals(None, None).await.unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].decision.as_ref().unwrap()["actor"], "policy");
    assert_eq!(
        proposals[0].decision.as_ref().unwrap()["policyId"],
        "session-write-policy"
    );
    let replay = fixture.tool.execute(context(), input()).await.unwrap();
    assert_eq!(replay.model_data.unwrap()["status"], "already_applied");
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pending_duplicate_is_blocked_even_in_same_run_after_policy_changes() {
    let fixture = fixture(WriteExecutionMode::DirectWhenAuthorized, false).await;
    fixture
        .policy
        .set_mode(WritePolicyMode::ConfirmAllWrites)
        .unwrap();
    let first = fixture.tool.execute(context(), input()).await.unwrap();
    assert!(first.ok);
    fixture.policy.set_mode(WritePolicyMode::AutoAll).unwrap();
    let duplicate = fixture.tool.execute(context(), input()).await.unwrap();
    assert!(!duplicate.ok);
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.store.proposals(None, None).await.unwrap().len(), 1);
}

#[tokio::test]
async fn concurrent_duplicate_never_dispatches_domain_twice() {
    let fixture = fixture(WriteExecutionMode::DirectWhenAuthorized, false).await;
    let (one, two) = tokio::join!(
        fixture.tool.execute(context(), input()),
        fixture.tool.execute(context(), input())
    );
    assert!(one.is_ok() && two.is_ok());
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.store.proposals(None, None).await.unwrap().len(), 1);
}

#[tokio::test]
async fn destructive_warnings_empty_and_truncated_builds_wait_for_confirmation() {
    for changes in [
        json!({"destructive":true,"warning":true}),
        json!({"empty":true}),
        json!({"truncated":true}),
    ] {
        let fixture = fixture(WriteExecutionMode::DirectWhenAuthorized, false).await;
        let mut arguments = input();
        arguments
            .as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        let result = fixture.tool.execute(context(), arguments).await.unwrap();
        assert_eq!(result.data.unwrap()["status"], "pending");
        assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn policy_revoked_after_claim_blocks_domain_effect() {
    let fixture = fixture(WriteExecutionMode::DirectWhenAuthorized, true).await;
    let result = fixture.tool.execute(context(), input()).await.unwrap();
    assert!(!result.ok);
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
    let proposal = fixture.store.proposals(None, None).await.unwrap().remove(0);
    assert_eq!(proposal.status, ProposalStatus::Failed);
    let operation = proposal.operation_id.as_deref().unwrap();
    let receipt = fixture.store.receipt(operation).await.unwrap().unwrap();
    assert_eq!(receipt.outcome.applied_ops, 0);
    let replay = fixture
        .service
        .apply(&proposal.id, operation, &json!({"id":"trusted-user"}))
        .await
        .unwrap();
    assert_eq!(receipt, replay);
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ambiguous_commit_retains_claim_and_reconciles_without_retrying_effect() {
    let fixture = fixture(WriteExecutionMode::DirectWhenAuthorized, false).await;
    fixture.domain.ambiguous.store(true, Ordering::SeqCst);
    let result = fixture.tool.execute(context(), input()).await.unwrap();
    assert!(!result.ok);
    assert!(
        !serde_json::to_string(&result)
            .unwrap()
            .contains("private-domain-detail")
    );
    assert!(result.summary.unwrap().contains("unconfirmed"));
    assert!(!fixture.tool.execute(context(), input()).await.unwrap().ok);
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.service.reconcile().await.unwrap().len(), 1);
    assert_eq!(
        fixture.store.proposals(None, None).await.unwrap()[0].status,
        ProposalStatus::Applied
    );
}

#[tokio::test]
async fn partial_receipt_remains_partial_with_applied_and_skipped_counts() {
    let fixture = fixture(WriteExecutionMode::DirectWhenAuthorized, false).await;
    *fixture.domain.status.lock().unwrap() = "partial".into();
    let mut arguments = input();
    arguments["two_operations"] = json!(true);
    let result = fixture.tool.execute(context(), arguments).await.unwrap();
    assert!(!result.ok);
    assert_eq!(result.status.as_deref(), Some("partial"));
    let model = result.model_data.unwrap();
    assert_eq!(model["appliedCount"], 1);
    assert_eq!(model["skipped"][0]["id"], "op:1");
    assert_eq!(model["skipped"][0]["reason"], "item no longer exists");
}

#[tokio::test]
async fn authorization_denial_and_revision_change_never_apply_or_leak_errors() {
    let fixture = fixture(WriteExecutionMode::DirectWhenAuthorized, false).await;
    fixture.permission.allowed.store(false, Ordering::SeqCst);
    let error = fixture.tool.execute(context(), input()).await.unwrap_err();
    assert!(!error.message.contains("private-authorizer-detail"));
    assert!(
        fixture
            .store
            .proposals(None, None)
            .await
            .unwrap()
            .is_empty()
    );
    fixture.permission.allowed.store(true, Ordering::SeqCst);
    *fixture.builder.revision.lock().unwrap() = Some("2".into());
    assert!(!fixture.tool.execute(context(), input()).await.unwrap().ok);
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
}
