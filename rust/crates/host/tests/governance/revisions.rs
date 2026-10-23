use super::support::*;
use agentkit_core::Tool;
use agentkit_host::*;
use agentkit_store::ProposalStatus;
use async_trait::async_trait;
use serde_json::json;
use std::sync::{Arc, Mutex, atomic::Ordering};

struct Revisions {
    value: Mutex<Option<String>>,
    revoke_authority: Option<Arc<Permission>>,
}

#[async_trait]
impl ProposalRevisionSource for Revisions {
    async fn current_revision(&self, _scope: &str) -> HostResult<Option<String>> {
        if let Some(permission) = &self.revoke_authority {
            permission.allowed.store(false, Ordering::SeqCst);
        }
        Ok(self.value.lock().unwrap().clone())
    }
}

async fn staged(fixture: &Fixture) -> String {
    let result = fixture
        .tool
        .execute(
            context(),
            json!({"action_id":"edit_item_document","change":1}),
        )
        .await
        .unwrap();
    result.data.unwrap()["proposalId"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn service(fixture: &Fixture, revisions: Arc<Revisions>) -> ProposalService {
    ProposalService::new(
        fixture.store.clone(),
        fixture.domain.clone(),
        fixture.permission.clone(),
        Arc::new(SystemRuntime),
    )
    .with_revisions(revisions)
}

#[tokio::test]
async fn manual_apply_checks_current_revision_and_records_known_zero_effect_refusal() {
    for current in [Some("2".into()), None] {
        let fixture = fixture(WriteExecutionMode::ProposeOnly, false).await;
        let id = staged(&fixture).await;
        let revisions = Arc::new(Revisions {
            value: Mutex::new(current),
            revoke_authority: None,
        });
        let service = service(&fixture, revisions.clone());
        service
            .approve(&id, &json!({"id":"trusted-user"}))
            .await
            .unwrap();
        assert_eq!(
            service
                .apply(&id, "manual-operation", &json!({"id":"trusted-user"}))
                .await
                .unwrap_err()
                .to_string(),
            "revision_conflict"
        );
        let receipt = fixture
            .store
            .receipt("manual-operation")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.outcome.applied_ops, 0);
        assert_eq!(receipt.outcome.revision, None);
        let proposal = fixture.store.get_proposal(&id).await.unwrap().unwrap();
        assert_eq!(proposal.status, ProposalStatus::Failed);
        *revisions.value.lock().unwrap() = Some("1".into());
        assert_eq!(
            service
                .apply(&id, "manual-operation", &json!({"id":"trusted-user"}))
                .await
                .unwrap(),
            receipt
        );
        assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn manual_apply_with_current_revision_dispatches_once() {
    let fixture = fixture(WriteExecutionMode::ProposeOnly, false).await;
    let id = staged(&fixture).await;
    let service = service(
        &fixture,
        Arc::new(Revisions {
            value: Mutex::new(Some("1".into())),
            revoke_authority: None,
        }),
    );
    service
        .approve(&id, &json!({"id":"trusted-user"}))
        .await
        .unwrap();
    assert_eq!(
        service
            .apply(&id, "manual-operation", &json!({"id":"trusted-user"}))
            .await
            .unwrap()
            .outcome
            .applied_ops,
        1
    );
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn authority_revoked_during_revision_read_settles_without_dispatch() {
    let fixture = fixture(WriteExecutionMode::ProposeOnly, false).await;
    let id = staged(&fixture).await;
    let source = Arc::new(Revisions {
        value: Mutex::new(Some("1".into())),
        revoke_authority: Some(fixture.permission.clone()),
    });
    let service = service(&fixture, source);
    service
        .approve(&id, &json!({"id":"trusted-user"}))
        .await
        .unwrap();
    assert!(
        service
            .apply(&id, "manual-operation", &json!({"id":"trusted-user"}))
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .store
            .receipt("manual-operation")
            .await
            .unwrap()
            .unwrap()
            .outcome
            .applied_ops,
        0
    );
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
}
