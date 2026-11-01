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
    // Once the receipt is committed, the terminal stamp is receipt-bound:
    // the plain status CAS must refuse (finalizing goes through
    // `complete_proposal`). Without any receipt the CAS is the baseline's
    // plain status move — pinned by `unrecorded_terminal_cas_writes_no_receipt`.
    store
        .record_outcome("operation", outcome(), 1003)
        .await
        .unwrap();
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

#[tokio::test]
async fn delete_by_chat_removes_only_that_chat_and_cascades_receipts() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store
        .create_chat("other", None, json!({}), 1000)
        .await
        .unwrap();
    store.create_proposal(proposal("first")).await.unwrap();
    let mut foreign = proposal("foreign");
    foreign.chat_id = "other".into();
    foreign.id = "foreign".into();
    foreign.scope_key = "other-scope".into();
    foreign.action_id = None;
    store.create_proposal(foreign).await.unwrap();
    applying(&store, "first", "operation").await;
    store
        .record_outcome("operation", outcome(), 1002)
        .await
        .unwrap();

    assert_eq!(store.delete_proposals_by_chat("chat").await.unwrap(), 1);
    assert!(store.get_proposal("first").await.unwrap().is_none());
    // The committed receipt row cascaded with its proposal.
    assert!(store.receipt("operation").await.unwrap().is_none());
    // The other chat's proposal is untouched.
    assert!(store.get_proposal("foreign").await.unwrap().is_some());
    // Deleting an empty scope removes nothing and answers zero.
    assert_eq!(store.delete_proposals_by_chat("chat").await.unwrap(), 0);
}

#[tokio::test]
async fn record_outcome_is_idempotent_and_requires_the_claimed_operation() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store.create_proposal(proposal("first")).await.unwrap();
    applying(&store, "first", "operation").await;

    let first = store
        .record_outcome("operation", outcome(), 1002)
        .await
        .unwrap();
    assert_eq!(first.proposal_id, "first");
    // A replay cannot restate the evidence: the first outcome stands.
    let mut restated = outcome();
    restated.status = "failed".into();
    restated.applied_ops = 0;
    let replayed = store
        .record_outcome("operation", restated, 1003)
        .await
        .unwrap();
    assert_eq!(replayed, first);

    // The port records an outcome by operation id ALONE: an id no proposal
    // carries is still the caller's idempotency key and its receipt stands
    // (proposal_id NULL, the record JSON authoritative). The old
    // "requires the claimed operation" refusal contradicted the shared
    // conformance suite, which records outcomes for free-floating ids.
    let unclaimed = store
        .record_outcome("never-claimed", outcome(), 1004)
        .await
        .unwrap();
    assert!(unclaimed.proposal_id.is_empty());
    assert_eq!(unclaimed.operation_id, "never-claimed");
}

#[tokio::test]
async fn unrecorded_terminal_cas_writes_no_receipt() {
    // A status request must never manufacture appliedOps evidence: without
    // a recorded outcome the terminal transition is the plain CAS the
    // TypeScript baseline had, it leaves NO receipt row, and a later
    // `recordOutcome` can still stand up the real evidence.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.db");
    let store = SqliteStore::open(&path, 4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store.create_proposal(proposal("first")).await.unwrap();
    applying(&store, "first", "operation").await;
    let applied = store
        .transition_proposal(
            "first",
            vec![ProposalStatus::Applying],
            ProposalStatus::Applied,
            ProposalPatch::default(),
            1003,
        )
        .await
        .unwrap();
    assert_eq!(applied.status, ProposalStatus::Applied);
    assert_eq!(applied.applied_at, Some(1003));
    assert!(store.receipt("operation").await.unwrap().is_none());
    drop(store);
    // Fresh process: the evidence was never written, not even durably.
    let reopened = SqliteStore::open(&path, 4).unwrap();
    assert!(reopened.receipt("operation").await.unwrap().is_none());
    assert_eq!(
        reopened
            .get_proposal("first")
            .await
            .unwrap()
            .unwrap()
            .status,
        ProposalStatus::Applied
    );
    // Late evidence still stands on its own, as the port contract allows.
    let late = reopened
        .record_outcome("operation", outcome(), 1100)
        .await
        .unwrap();
    assert_eq!(late.proposal_id, "first");
    assert_eq!(late.outcome.status, "applied");

    // The failed direction is the same plain CAS.
    let store2 = SqliteStore::memory(4).unwrap();
    store2
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    let mut second = proposal("second");
    second.action_id = None;
    store2.create_proposal(second).await.unwrap();
    applying(&store2, "second", "operation-two").await;
    let failed = store2
        .transition_proposal(
            "second",
            vec![ProposalStatus::Applying],
            ProposalStatus::Failed,
            ProposalPatch {
                reason: Some("apply refused".into()),
                ..Default::default()
            },
            1004,
        )
        .await
        .unwrap();
    assert_eq!(failed.status, ProposalStatus::Failed);
    assert_eq!(failed.reason.as_deref(), Some("apply refused"));
    assert!(store2.receipt("operation-two").await.unwrap().is_none());
}

#[tokio::test]
async fn recorded_outcome_then_complete_finalizes_an_applying_proposal() {
    // The port's apply order is recordOutcome(operation) FIRST — which
    // inserts the receipt — and the terminal completion second. The receipt
    // stays the byte-identical first committed outcome, but the Applying
    // proposal must still finalize: the early return used to skip the stamp
    // and left every such apply durably "applying" (host ProposalService
    // applies over the native port were stuck).
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store.create_proposal(proposal("first")).await.unwrap();
    applying(&store, "first", "operation").await;
    store
        .record_outcome("operation", outcome(), 1002)
        .await
        .unwrap();

    let receipt = store
        .complete_proposal("first", "operation", outcome(), 1003)
        .await
        .unwrap();
    assert_eq!(receipt.created_at, 1002, "first committed outcome wins");
    let done = store.get_proposal("first").await.unwrap().unwrap();
    assert_eq!(done.status, ProposalStatus::Applied);
    assert_eq!(done.applied_at, Some(1003));

    // Replay stays idempotent: the receipt is untouched and an already
    // terminal proposal is not re-stamped.
    let replay = store
        .complete_proposal("first", "operation", outcome(), 1004)
        .await
        .unwrap();
    assert_eq!(replay.created_at, 1002);
    assert_eq!(
        store
            .get_proposal("first")
            .await
            .unwrap()
            .unwrap()
            .applied_at,
        Some(1003)
    );
}

#[tokio::test]
async fn pre_recorded_failed_outcome_completes_failed_regardless_of_the_replayed_argument() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store.create_proposal(proposal("first")).await.unwrap();
    applying(&store, "first", "operation").await;
    let mut failed = outcome();
    failed.status = "failed".into();
    failed.applied_ops = 0;
    failed.failed_ops = vec![json!({"opIndex": 0, "error": "apply failed"})];
    store
        .record_outcome("operation", failed, 1002)
        .await
        .unwrap();

    // The completion argument claims "applied"; the committed receipt says
    // otherwise and the receipt is authoritative.
    store
        .complete_proposal("first", "operation", outcome(), 1003)
        .await
        .unwrap();
    let done = store.get_proposal("first").await.unwrap().unwrap();
    assert_eq!(done.status, ProposalStatus::Failed);
    assert_eq!(done.applied_at, Some(1003));
}
