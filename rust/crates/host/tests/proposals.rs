use agentkit_host::*;
use agentkit_store::*;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct Permission {
    allowed: Arc<AtomicBool>,
    checks: AtomicUsize,
    deny_after_claim: bool,
}
#[async_trait]
impl Authorization for Permission {
    async fn authorize(&self, _: &Value, _: &str, _: &Value) -> HostResult<()> {
        let check = self.checks.fetch_add(1, Ordering::SeqCst);
        if self.allowed.load(Ordering::SeqCst) && !(self.deny_after_claim && check >= 2) {
            Ok(())
        } else {
            Err(HostError::Execution("forbidden".into()))
        }
    }
}

struct Domain {
    calls: Arc<AtomicUsize>,
    ambiguous: bool,
}
#[async_trait]
impl ProposalApplier for Domain {
    async fn apply(&self, _: MutationIdentity, _: Value) -> HostResult<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.ambiguous {
            return Err(HostError::Execution(
                "connection lost after domain commit".into(),
            ));
        }
        Ok(outcome())
    }
    async fn reconcile(&self, _: MutationIdentity) -> HostResult<Option<Value>> {
        Ok(None)
    }
}

fn outcome() -> Value {
    json!({"status":"applied","appliedOps":1,"failedOps":[],"resultJson":null,"revision":"2"})
}

async fn setup(ambiguous: bool) -> (ProposalService, Arc<AtomicBool>, Arc<AtomicUsize>) {
    setup_with_permission(ambiguous, false).await
}

async fn setup_with_permission(
    ambiguous: bool,
    deny_after_claim: bool,
) -> (ProposalService, Arc<AtomicBool>, Arc<AtomicUsize>) {
    let store = SqliteStore::memory(16).unwrap();
    store.create_chat("chat", None, json!({}), 0).await.unwrap();
    store
        .create_proposal(ProposalRecord {
            id: "proposal".into(),
            chat_id: "chat".into(),
            run_id: None,
            scope_key: "document".into(),
            action_id: None,
            tool_name: "write".into(),
            kind: "edit".into(),
            risk: "low".into(),
            status: ProposalStatus::Pending,
            envelope: json!({}),
            operations: vec![json!({"edit":1})],
            warnings: vec![],
            truncated: false,
            revision_at_create: Some("1".into()),
            operation_id: None,
            decision: None,
            reason: None,
            created_at: 0,
            decided_at: None,
            claimed_at: None,
            applied_at: None,
        })
        .await
        .unwrap();
    let allowed = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let service = ProposalService::new(
        store,
        Arc::new(Domain {
            calls: calls.clone(),
            ambiguous,
        }),
        Arc::new(Permission {
            allowed: allowed.clone(),
            checks: AtomicUsize::new(0),
            deny_after_claim,
        }),
        Arc::new(SystemRuntime),
    );
    service.approve("proposal", &json!("actor")).await.unwrap();
    (service, allowed, calls)
}

#[tokio::test]
async fn authority_revoked_during_claim_prevents_domain_mutation() {
    let (service, _, calls) = setup_with_permission(false, true).await;
    let error = service
        .apply("proposal", "operation", &json!("actor"))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "forbidden");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn replay_requires_existing_proposal_and_current_authority() {
    let (service, allowed, calls) = setup(false).await;
    service
        .apply("proposal", "operation", &json!("actor"))
        .await
        .unwrap();
    assert!(
        service
            .apply("missing", "operation", &json!("actor"))
            .await
            .is_err()
    );
    allowed.store(false, Ordering::SeqCst);
    let error = service
        .apply("proposal", "operation", &json!("actor"))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "forbidden");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ambiguous_effect_is_reconciled_without_automatic_reexecution() {
    let (service, _, calls) = setup(true).await;
    assert!(
        service
            .apply("proposal", "operation", &json!("actor"))
            .await
            .is_err()
    );
    let error = service
        .apply("proposal", "operation", &json!("actor"))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "proposal_outcome_unresolved");
    assert!(service.reconcile().await.unwrap().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
