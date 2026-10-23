use super::support::*;
use agentkit_core::Tool;
use agentkit_host::*;
use agentkit_store::ProposalStatus;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct InvalidOutcome {
    value: Value,
    calls: AtomicUsize,
}

#[async_trait]
impl ProposalApplier for InvalidOutcome {
    async fn apply(&self, _identity: MutationIdentity, _operations: Value) -> HostResult<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.value.clone())
    }
    async fn reconcile(&self, _identity: MutationIdentity) -> HostResult<Option<Value>> {
        Ok(None)
    }
}

#[tokio::test]
async fn malformed_domain_receipts_preserve_uncertainty_without_claiming_false_success() {
    let outcomes = [
        json!({"status":"applied","appliedOps":2,"failedOps":[]}),
        json!({"status":"partial","appliedOps":0,"failedOps":[{"opIndex":1,"error":"missing"}]}),
        json!({"status":"failed","appliedOps":1,"failedOps":[]}),
        json!({"status":"applied","appliedOps":0,"failedOps":[{"opIndex":0,"error":"failed"}]}),
        json!({"status":"partial","appliedOps":0,"failedOps":[{"opIndex":0,"error":12}]}),
    ];
    for value in outcomes {
        let fixture = fixture(WriteExecutionMode::ProposeOnly, false).await;
        let id = stage(&fixture).await;
        let (service, domain) = service(&fixture, value);
        service
            .approve(&id, &json!({"id":"trusted-user"}))
            .await
            .unwrap();
        assert!(
            service
                .apply(&id, "operation", &json!({"id":"trusted-user"}))
                .await
                .is_err()
        );
        assert!(fixture.store.receipt("operation").await.unwrap().is_none());
        let proposal = fixture.store.get_proposal(&id).await.unwrap().unwrap();
        assert_eq!(proposal.status, ProposalStatus::Applying);
        assert!(
            service
                .apply(&id, "operation", &json!({"id":"trusted-user"}))
                .await
                .is_err()
        );
        assert_eq!(domain.calls.load(Ordering::SeqCst), 1);
    }
}

async fn stage(fixture: &Fixture) -> String {
    let result = fixture
        .tool
        .execute(context(), json!({"action_id":"edit_item_document"}))
        .await
        .unwrap();
    result.data.unwrap()["proposalId"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn service(fixture: &Fixture, value: Value) -> (ProposalService, Arc<InvalidOutcome>) {
    let domain = Arc::new(InvalidOutcome {
        value,
        calls: AtomicUsize::new(0),
    });
    let service = ProposalService::new(
        fixture.store.clone(),
        domain.clone(),
        fixture.permission.clone(),
        Arc::new(SystemRuntime),
    );
    (service, domain)
}
