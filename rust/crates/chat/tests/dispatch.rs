mod governance_support;
use agentkit_chat::*;
use agentkit_core::{DispatchFence, Tool, ToolContext};
use agentkit_engine::{Runtime, TaskExecutor};
use agentkit_governance::*;
use agentkit_sessions::*;
use agentkit_store::SubmitTask;
use async_trait::async_trait;
use governance_support as support;
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
    async fn build(
        &self,
        context: &ToolContext,
        input: &Value,
    ) -> agentkit_governance::GovernanceResult<ProposalBuild> {
        self.inner.build(context, input).await
    }
    async fn current_revision(
        &self,
        _: &str,
    ) -> agentkit_governance::GovernanceResult<Option<String>> {
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
    async fn apply(
        &self,
        identity: MutationIdentity,
        _: Value,
    ) -> agentkit_governance::GovernanceResult<Value> {
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
            return Err(agentkit_governance::GovernanceError::Port(
                "domain_fence_superseded".into(),
            ));
        }
        *newest = fence.fencing_token;
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(
            json!({"status":"applied","appliedOps":1,"failedOps":[],"resultJson":null,"revision":"2"}),
        )
    }
    async fn reconcile(
        &self,
        _: MutationIdentity,
    ) -> agentkit_governance::GovernanceResult<Option<Value>> {
        Ok(None)
    }
}
struct RunTool {
    tool: Arc<GovernedProposalTool>,
    capture: Mutex<Option<tokio::sync::oneshot::Sender<ExecutionContext>>>,
}
#[async_trait]
impl TaskExecutor<agentkit_store::SqliteStore> for RunTool {
    async fn execute(
        &self,
        context: ExecutionContext,
    ) -> agentkit_engine::EngineResult<(), agentkit_store::StoreError> {
        let capture = self.capture.lock().unwrap().take();
        if let Some(capture) = capture {
            let _ = capture.send(context);
            return std::future::pending().await;
        }
        let mut tool_context = support::context();
        tool_context.run_id = context.task.task_id.clone();
        tool_context.cancellation = context.cancellation.clone();
        tool_context.dispatch_guard = Some(context.dispatch_guard());
        self.tool
            .execute(tool_context, json!({"action_id":"edit_item_document"}))
            .await
            .map_err(|error| agentkit_engine::EngineError::External {
                source: Box::new(error),
                failure_code: None,
            })?;
        Ok(())
    }
}
async fn setup(expire: bool, supersede: bool) -> (AgentHost, Arc<Domain>, Arc<RunTool>) {
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
    let executor = Arc::new(RunTool {
        tool: Arc::new(tool),
        capture: Mutex::new(None),
    });
    let host = AgentHost::new(
        fixture.store,
        std::collections::HashMap::from([(
            "test".into(),
            executor.clone() as Arc<dyn TaskExecutor<agentkit_store::SqliteStore>>,
        )]),
        clock,
        HostOptions {
            lease_ttl_ms: 100,
            ..Default::default()
        },
    )
    .unwrap();
    host.submit(task()).await.unwrap();
    (host, domain, executor)
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
async fn claimed_context(host: &AgentHost, executor: &RunTool) -> ExecutionContext {
    let (send, receive) = tokio::sync::oneshot::channel();
    *executor.capture.lock().unwrap() = Some(send);
    let running = host.clone();
    let runner = tokio::spawn(async move { running.run_once().await });
    let context = receive
        .await
        .expect("runner supplies its real claimed context");
    runner.abort();
    assert!(runner.await.unwrap_err().is_cancelled());
    context
}
#[tokio::test]
async fn expiry_during_final_revision_read_prevents_domain_dispatch() {
    let (host, domain, executor) = setup(true, false).await;
    let context = claimed_context(&host, &executor).await;
    // Isolate the final revision fence from the runner's competing heartbeat cancellation.
    context.check_dispatch().await.unwrap();
    executor.execute(context.clone()).await.unwrap();
    assert_eq!(host.runtime().now_ms(), 10000);
    assert!(context.cancellation.is_cancelled());
    assert_eq!(
        context.dispatch_guard().check().await.unwrap_err().code,
        "lease_lost"
    );
    assert_eq!(domain.calls.load(Ordering::SeqCst), 0);
    assert!(domain.identities.lock().unwrap().is_empty());
    let proposal = host.store().proposals(None, None).await.unwrap().remove(0);
    assert_eq!(proposal.status, agentkit_store::ProposalStatus::Failed);
    let receipt = host
        .store()
        .receipt(proposal.operation_id.as_deref().unwrap())
        .await
        .unwrap()
        .expect("a finished no-dispatch refusal records its outcome");
    assert_eq!(receipt.outcome.status, "failed");
    assert_eq!(receipt.outcome.applied_ops, 0);
    assert_eq!(receipt.outcome.failed_ops.len(), proposal.operations.len());
}
#[tokio::test]
async fn durable_governed_apply_carries_task_attempt_and_monotonic_fence() {
    let (host, domain, _) = setup(false, false).await;
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
    let (host, domain, _) = setup(false, true).await;
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
