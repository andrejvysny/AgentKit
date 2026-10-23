use agentkit_store::*;
use serde_json::json;

fn proposal(id: &str) -> ProposalRecord {
    ProposalRecord {
        id: id.into(),
        chat_id: "chat".into(),
        run_id: None,
        scope_key: "document".into(),
        action_id: Some("change".into()),
        tool_name: "edit".into(),
        kind: "test".into(),
        risk: "low".into(),
        status: ProposalStatus::Pending,
        envelope: json!({}),
        operations: vec![json!({"set":1})],
        warnings: vec![],
        truncated: false,
        revision_at_create: Some("v1".into()),
        operation_id: None,
        decision: None,
        reason: None,
        created_at: 1000,
        decided_at: None,
        claimed_at: None,
        applied_at: None,
    }
}

fn outcome() -> ApplyOutcome {
    ApplyOutcome {
        status: "applied".into(),
        applied_ops: 1,
        failed_ops: vec![],
        result_json: None,
        revision: Some("v2".into()),
    }
}

fn invalid_outcomes() -> Vec<ApplyOutcome> {
    let mut outcomes = vec![];
    let mut invalid = outcome();
    invalid.applied_ops = 2;
    outcomes.push(invalid);
    for failed in [
        json!({"opIndex":1,"error":"outside"}),
        json!({"opIndex":0,"error":false}),
        json!({"opIndex":0.5,"error":"fraction"}),
    ] {
        let mut invalid = outcome();
        invalid.status = "partial".into();
        invalid.applied_ops = 0;
        invalid.failed_ops = vec![failed];
        outcomes.push(invalid);
    }
    let mut invalid = outcome();
    invalid.status = "failed".into();
    outcomes.push(invalid);
    let mut invalid = outcome();
    invalid.applied_ops = 0;
    invalid.failed_ops = vec![json!({"opIndex":0,"error":"failed"})];
    outcomes.push(invalid);
    let mut invalid = outcome();
    invalid.status = "partial".into();
    invalid.applied_ops = 0;
    invalid.failed_ops = vec![
        json!({"opIndex":0,"error":"one"}),
        json!({"opIndex":0,"error":"twice"}),
    ];
    outcomes.push(invalid);
    outcomes
}

#[tokio::test]
async fn invalid_domain_outcomes_cannot_bypass_durable_receipt_validation() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store.create_proposal(proposal("first")).await.unwrap();
    applying(&store, "first", "operation").await;
    for outcome in invalid_outcomes() {
        assert!(
            matches!(store.complete_proposal("first","operation",outcome,1003).await,Err(StoreError::InvalidInput(message)) if message=="invalid domain apply outcome; reconciliation required")
        );
        assert!(store.receipt("operation").await.unwrap().is_none());
        assert_eq!(
            store.get_proposal("first").await.unwrap().unwrap().status,
            ProposalStatus::Applying
        );
    }
    store
        .complete_proposal("first", "operation", outcome(), 1004)
        .await
        .unwrap();
}

#[tokio::test]
async fn proposal_operations_decisions_and_receipts_preserve_private_marker_keys() {
    let directory = tempfile::tempdir().unwrap();
    for store in [
        SqliteStore::memory(4).unwrap(),
        SqliteStore::open(directory.path().join("markers.db"), 4).unwrap(),
    ] {
        store
            .create_chat("chat", None, json!({}), 1000)
            .await
            .unwrap();
        let marker = agentkit_contracts::parse_javascript_json(
            r#"{"$serde_json::private::Number":"1e400","other":true}"#,
        )
        .unwrap();
        let mut input = proposal("markers");
        input.envelope = marker.clone();
        input.operations = vec![marker.clone()];
        input.decision = Some(marker.clone());
        let encoded = serde_json::to_string(&input).unwrap();
        assert_eq!(
            serde_json::from_value::<ProposalRecord>(
                agentkit_contracts::parse_javascript_json(&encoded).unwrap()
            )
            .unwrap(),
            input
        );
        store.create_proposal(input.clone()).await.unwrap();
        assert_eq!(store.get_proposal("markers").await.unwrap(), Some(input));
        applying(&store, "markers", "operation").await;
        let failed = agentkit_contracts::parse_javascript_json(
            r#"{"$serde_json::private::Number":"not a number","opIndex":0,"error":"failed"}"#,
        )
        .unwrap();
        let outcome = ApplyOutcome {
            status: "partial".into(),
            applied_ops: 0,
            failed_ops: vec![failed],
            result_json: None,
            revision: None,
        };
        let receipt = store
            .complete_proposal("markers", "operation", outcome, 1003)
            .await
            .unwrap();
        assert_eq!(store.receipt("operation").await.unwrap(), Some(receipt));
    }
}

async fn applying(store: &SqliteStore, id: &str, operation: &str) {
    store
        .transition_proposal(
            id,
            vec![ProposalStatus::Pending],
            ProposalStatus::Approved,
            ProposalPatch {
                decision: Some(json!({"actor":"user"})),
                ..Default::default()
            },
            1001,
        )
        .await
        .unwrap();
    store
        .transition_proposal(
            id,
            vec![ProposalStatus::Approved],
            ProposalStatus::Applying,
            ProposalPatch {
                operation_id: Some(operation.into()),
                ..Default::default()
            },
            1002,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn action_reservations_release_only_after_no_effect_terminal() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store.create_proposal(proposal("first")).await.unwrap();
    assert!(store.create_proposal(proposal("second")).await.is_err());
    store
        .transition_proposal(
            "first",
            vec![ProposalStatus::Pending],
            ProposalStatus::Rejected,
            ProposalPatch::default(),
            1001,
        )
        .await
        .unwrap();
    let mut second = proposal("second");
    second.created_at = 1002;
    store.create_proposal(second).await.unwrap();
    assert_eq!(
        store
            .proposal_by_action("document", "change")
            .await
            .unwrap()
            .unwrap()
            .id,
        "second"
    );
    applying(&store, "second", "operation").await;
    store
        .complete_proposal("second", "operation", outcome(), 1003)
        .await
        .unwrap();
    assert!(store.create_proposal(proposal("third")).await.is_err());
}

#[tokio::test]
async fn receipt_binds_proposal_and_operation_and_first_outcome_wins_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.db");
    let store = SqliteStore::open(&path, 4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store.create_proposal(proposal("first")).await.unwrap();
    applying(&store, "first", "operation").await;
    assert!(
        store
            .complete_proposal("first", "foreign", outcome(), 1003)
            .await
            .is_err()
    );
    assert!(
        store
            .transition_proposal(
                "first",
                vec![ProposalStatus::Applying],
                ProposalStatus::Applied,
                ProposalPatch::default(),
                1003
            )
            .await
            .is_err()
    );
    let receipt = store
        .complete_proposal("first", "operation", outcome(), 1003)
        .await
        .unwrap();
    drop(store);
    let reopened = SqliteStore::open(&path, 4).unwrap();
    let mut changed = outcome();
    changed.status = "failed".into();
    assert_eq!(
        reopened
            .complete_proposal("first", "operation", changed, 2000)
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(
        reopened
            .get_proposal("first")
            .await
            .unwrap()
            .unwrap()
            .status,
        ProposalStatus::Applied
    );
    let mut other = proposal("other");
    other.action_id = None;
    reopened.create_proposal(other).await.unwrap();
    assert!(
        reopened
            .complete_proposal("other", "operation", outcome(), 2000)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn operation_claim_is_unique_and_policy_decision_requires_identity() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store.create_proposal(proposal("first")).await.unwrap();
    assert!(
        store
            .transition_proposal(
                "first",
                vec![ProposalStatus::Pending],
                ProposalStatus::Approved,
                ProposalPatch {
                    decision: Some(json!({"actor":"policy"})),
                    ..Default::default()
                },
                1001
            )
            .await
            .is_err()
    );
    applying(&store, "first", "operation").await;
    let mut second = proposal("second");
    second.action_id = None;
    store.create_proposal(second).await.unwrap();
    store
        .transition_proposal(
            "second",
            vec![ProposalStatus::Pending],
            ProposalStatus::Approved,
            ProposalPatch::default(),
            1001,
        )
        .await
        .unwrap();
    assert!(
        store
            .transition_proposal(
                "second",
                vec![ProposalStatus::Approved],
                ProposalStatus::Applying,
                ProposalPatch {
                    operation_id: Some("operation".into()),
                    ..Default::default()
                },
                1002
            )
            .await
            .is_err()
    );
    assert_eq!(
        store.get_proposal("second").await.unwrap().unwrap().status,
        ProposalStatus::Approved
    );
}
