use super::support::*;
use agentkit_core::{Tool, ToolContext};
use agentkit_host::*;
use agentkit_store::ProposalStatus;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct RevokingBuilder {
    inner: Arc<Builder>,
    permission: Arc<Permission>,
    revision_reads: AtomicUsize,
}

#[async_trait]
impl ProposalBuilder for RevokingBuilder {
    async fn build(&self, context: &ToolContext, input: &Value) -> HostResult<ProposalBuild> {
        self.inner.build(context, input).await
    }

    async fn current_revision(&self, scope: &str) -> HostResult<Option<String>> {
        if self.revision_reads.fetch_add(1, Ordering::SeqCst) == 1 {
            tokio::task::yield_now().await;
            self.permission.allowed.store(false, Ordering::SeqCst);
        }
        self.inner.current_revision(scope).await
    }
}

fn tool(fixture: &Fixture) -> GovernedProposalTool {
    let mut options = options(fixture);
    options.builder = Arc::new(RevokingBuilder {
        inner: fixture.builder.clone(),
        permission: fixture.permission.clone(),
        revision_reads: AtomicUsize::new(0),
    });
    GovernedProposalTool::new(definition("write_document", "write"), options).unwrap()
}

fn options(fixture: &Fixture) -> ProposalToolOptions {
    ProposalToolOptions {
        store: fixture.store.clone(),
        service: fixture.service.clone(),
        builder: fixture.builder.clone(),
        authorization: fixture.permission.clone(),
        policy: fixture.policy.clone(),
        runtime: Arc::new(SystemRuntime),
        actor: json!({"id":"trusted-user"}),
        actor_id: None,
        mode: WriteExecutionMode::DirectWhenAuthorized,
        policy_id: "session-write-policy".into(),
    }
}

struct FinalAuthorization {
    inner: Arc<Permission>,
    policy: Arc<SessionWritePolicy>,
    cancellation: tokio_util::sync::CancellationToken,
    cancel: bool,
    apply_checks: AtomicUsize,
}

#[async_trait]
impl Authorization for FinalAuthorization {
    async fn authorize(&self, actor: &Value, action: &str, resource: &Value) -> HostResult<()> {
        if action == "apply" && self.apply_checks.fetch_add(1, Ordering::SeqCst) == 2 {
            tokio::task::yield_now().await;
            if self.cancel {
                self.cancellation.cancel();
            } else {
                self.policy.set_mode(WritePolicyMode::ConfirmAllWrites)?;
            }
        }
        self.inner.authorize(actor, action, resource).await
    }
}

#[tokio::test]
async fn policy_or_cancellation_revoked_during_final_authorization_prevents_dispatch() {
    for cancel in [false, true] {
        let fixture = fixture(WriteExecutionMode::ProposeOnly, false).await;
        let context = context();
        let authorization = Arc::new(FinalAuthorization {
            inner: fixture.permission.clone(),
            policy: fixture.policy.clone(),
            cancellation: context.cancellation.clone(),
            cancel,
            apply_checks: AtomicUsize::new(0),
        });
        let mut options = options(&fixture);
        options.authorization = authorization.clone();
        options.service = Arc::new(ProposalService::new(
            fixture.store.clone(),
            fixture.domain.clone(),
            authorization.clone(),
            Arc::new(SystemRuntime),
        ));
        let tool =
            GovernedProposalTool::new(definition("write_document", "write"), options).unwrap();
        let result = tool.execute(context, json!({"change":1})).await.unwrap();
        assert!(!result.ok);
        assert_eq!(authorization.apply_checks.load(Ordering::SeqCst), 3);
        assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
        let proposal = fixture.store.proposals(None, None).await.unwrap().remove(0);
        assert_eq!(proposal.status, ProposalStatus::Failed);
        let receipt = fixture
            .store
            .receipt(proposal.operation_id.as_deref().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.outcome.applied_ops, 0);
    }
}

#[tokio::test]
async fn authority_revoked_during_final_builder_guard_prevents_domain_dispatch() {
    let fixture = fixture(WriteExecutionMode::ProposeOnly, false).await;
    let result = tool(&fixture)
        .execute(context(), json!({"change":1}))
        .await
        .unwrap();
    assert!(!fixture.permission.allowed.load(Ordering::SeqCst));
    assert!(!result.ok);
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
    let proposal = fixture.store.proposals(None, None).await.unwrap().remove(0);
    assert_eq!(proposal.status, ProposalStatus::Failed);
    let operation = proposal.operation_id.as_deref().unwrap();
    let receipt = fixture.store.receipt(operation).await.unwrap().unwrap();
    assert_eq!(receipt.proposal_id, proposal.id);
    assert_eq!(receipt.outcome.status, "failed");
    assert_eq!(receipt.outcome.applied_ops, 0);
    fixture.permission.allowed.store(true, Ordering::SeqCst);
    let replay = fixture
        .service
        .apply(&proposal.id, operation, &json!({"id":"trusted-user"}))
        .await
        .unwrap();
    assert_eq!(receipt, replay);
    assert_eq!(fixture.domain.calls.load(Ordering::SeqCst), 0);
}
