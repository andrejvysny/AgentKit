#[path = "governance/support.rs"]
mod support;
use agentkit_core::{DispatchFence, Tool, ToolContext};
use agentkit_host::*;
use agentkit_store::SubmitTask;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI64, AtomicUsize, Ordering},
};

struct Clock(AtomicI64);
impl Runtime for Clock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    fn id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}
struct Builder {
    inner: Arc<support::Builder>,
    clock: Arc<Clock>,
    expire: bool,
    checks: AtomicUsize,
}
#[async_trait]
impl ProposalBuilder for Builder {
    async fn build(&self, context: &ToolContext, input: &Value) -> HostResult<ProposalBuild> {
        self.inner.build(context, input).await
    }
    async fn current_revision(&self, _: &str) -> HostResult<Option<String>> {
        if self.expire && self.checks.fetch_add(1, Ordering::SeqCst) == 1 {
            self.clock.0.store(10000, Ordering::SeqCst);
        }
        Ok(Some("1".into()))
    }
}
struct Domain {
    calls: AtomicUsize,
    identities: Mutex<Vec<DispatchFence>>,
    newest_token: Mutex<u64>,
    supersede_during_apply: bool,
}
#[async_trait]
impl ProposalApplier for Domain {
    async fn apply(&self, identity: MutationIdentity, _: Value) -> HostResult<Value> {
        let fence = identity
            .execution_fence
            .expect("Durable tool must supply its trusted fence");
        self.identities.lock().unwrap().push(fence.clone());
        if self.supersede_during_apply {
            tokio::task::yield_now().await;
            *self.newest_token.lock().unwrap() = fence.fencing_token + 1;
        }
        let mut newest = self.newest_token.lock().unwrap();
        if fence.fencing_token < *newest {
            return Err(HostError::Execution("domain_fence_superseded".into()));
        }
        *newest = fence.fencing_token;
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(
            json!({"status":"applied","appliedOps":1,"failedOps":[],"resultJson":null,"revision":"2"}),
        )
    }
    async fn reconcile(&self, _: MutationIdentity) -> HostResult<Option<Value>> {
        Ok(None)
    }
}
struct RunTool(Arc<GovernedProposalTool>);
#[async_trait]
impl TaskExecutor for RunTool {
    async fn execute(&self, context: ExecutionContext) -> HostResult<()> {
        let mut tool_context = support::context();
        tool_context.run_id = context.task.task_id.clone();
        tool_context.cancellation = context.cancellation.clone();
        tool_context.dispatch_guard = Some(context.dispatch_guard());
        self.0
            .execute(tool_context, json!({"action_id":"edit_item_document"}))
            .await
            .map_err(|error| HostError::Execution(error.to_string()))?;
        Ok(())
    }
}
async fn setup(expire: bool, supersede: bool) -> (AgentHost, Arc<Domain>) {
    let fixture = support::fixture(WriteExecutionMode::DirectWhenAuthorized, false).await;
    let clock = Arc::new(Clock(AtomicI64::new(1000)));
    let domain = Arc::new(Domain {
        calls: AtomicUsize::new(0),
        identities: Mutex::new(vec![]),
        newest_token: Mutex::new(0),
        supersede_during_apply: supersede,
    });
    let service = Arc::new(ProposalService::new(
        fixture.store.clone(),
        domain.clone(),
        fixture.permission.clone(),
        clock.clone(),
    ));
    let tool = GovernedProposalTool::new(
        support::definition("write_document", "write"),
        ProposalToolOptions {
            store: fixture.store.clone(),
            service,
            builder: Arc::new(Builder {
                inner: fixture.builder,
                clock: clock.clone(),
                expire,
                checks: AtomicUsize::new(0),
            }),
            authorization: fixture.permission,
            policy: fixture.policy,
            runtime: clock.clone(),
            actor: json!({"id":"trusted-user"}),
            actor_id: None,
            mode: WriteExecutionMode::DirectWhenAuthorized,
            policy_id: "policy".into(),
        },
    )
    .unwrap();
    let host = AgentHost::new(
        fixture.store,
        std::collections::HashMap::from([(
            "test".into(),
            Arc::new(RunTool(Arc::new(tool))) as Arc<dyn TaskExecutor>,
        )]),
        clock,
        HostOptions {
            lease_ttl_ms: 100,
            ..Default::default()
        },
    )
    .unwrap();
    host.submit(task()).await.unwrap();
    (host, domain)
}
fn task() -> SubmitTask {
    SubmitTask {
        task_id: "run".into(),
        kind: "test".into(),
        scope_id: "chat".into(),
        payload: json!({}),
        priority: 0,
        now: 1000,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: false,
    }
}
#[tokio::test]
async fn expiry_during_final_revision_read_prevents_domain_dispatch() {
    let (host, domain) = setup(true, false).await;
    assert!(host.run_once().await.is_err());
    assert_eq!(domain.calls.load(Ordering::SeqCst), 0);
    assert!(domain.identities.lock().unwrap().is_empty());
    let proposals = host.store().proposals(None, None).await.unwrap();
    let receipt = host
        .store()
        .receipt(proposals[0].operation_id.as_deref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.outcome.applied_ops, 0);
}
#[tokio::test]
async fn durable_governed_apply_carries_task_attempt_and_monotonic_fence() {
    let (host, domain) = setup(false, false).await;
    assert!(host.run_once().await.unwrap());
    assert_eq!(domain.calls.load(Ordering::SeqCst), 1);
    let attempts = host.store().attempts("run").await.unwrap();
    let identities = domain.identities.lock().unwrap();
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].task_id, "run");
    assert_eq!(identities[0].attempt_id, attempts[0].attempt_id);
    assert!(identities[0].fencing_token > 0);
}
#[tokio::test]
async fn domain_atomic_fence_rejects_superseded_dispatch_after_local_check() {
    let (host, domain) = setup(false, true).await;
    assert!(host.run_once().await.unwrap());
    assert_eq!(domain.calls.load(Ordering::SeqCst), 0);
    assert_eq!(domain.identities.lock().unwrap().len(), 1);
    let proposal = host.store().proposals(None, None).await.unwrap().remove(0);
    assert_eq!(proposal.status, agentkit_store::ProposalStatus::Applying);
    assert!(
        host.store()
            .receipt(proposal.operation_id.as_deref().unwrap())
            .await
            .unwrap()
            .is_none()
    );
}
