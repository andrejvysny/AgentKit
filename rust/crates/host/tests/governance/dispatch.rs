use super::support::*;
use agentkit_core::{CoreError, DispatchFence, DispatchGuard, Tool, ToolRegistry};
use agentkit_host::*;
use async_trait::async_trait;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Debug)]
struct Guard {
    allowed: AtomicBool,
    checks: AtomicUsize,
}

#[async_trait]
impl DispatchGuard for Guard {
    async fn check(&self) -> Result<(), CoreError> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        if self.allowed.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(CoreError::new("lease_lost", "Expired"))
        }
    }
    fn fence(&self) -> DispatchFence {
        DispatchFence {
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            fencing_token: 7,
        }
    }
}

#[tokio::test]
async fn proposal_dispatch_requires_current_guard_and_forwards_trusted_fence() {
    for allowed in [false, true] {
        let fixture = fixture(WriteExecutionMode::DirectWhenAuthorized, false).await;
        let guard = Arc::new(Guard {
            allowed: AtomicBool::new(allowed),
            checks: AtomicUsize::new(0),
        });
        let mut context = context();
        context.dispatch_guard = Some(guard.clone());
        let result = fixture
            .tool
            .execute(context, json!({"change":1}))
            .await
            .unwrap();
        assert_eq!(result.ok, allowed);
        assert_eq!(guard.checks.load(Ordering::SeqCst), 1);
        assert_eq!(
            fixture.domain.calls.load(Ordering::SeqCst),
            usize::from(allowed)
        );
        if allowed {
            assert_eq!(
                *fixture.domain.execution_fences.lock().unwrap(),
                vec![Some(guard.fence())]
            );
        } else {
            let proposal = fixture.store.proposals(None, None).await.unwrap().remove(0);
            let receipt = fixture
                .store
                .receipt(proposal.operation_id.as_deref().unwrap())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(receipt.outcome.applied_ops, 0);
        }
    }
}

struct RevokeDispatch(Arc<Guard>);

#[async_trait]
impl ToolGuard for RevokeDispatch {
    async fn can_execute(&self, _context: &ToolGuardContext) -> HostResult<ToolGuardVerdict> {
        tokio::task::yield_now().await;
        self.0.allowed.store(false, Ordering::SeqCst);
        Ok(ToolGuardVerdict::Allowed)
    }
}

#[tokio::test]
async fn staged_tools_check_dispatch_after_awaiting_host_guards() {
    let fixture = fixture(WriteExecutionMode::DirectWhenAuthorized, false).await;
    let mut registry = ToolRegistry::new();
    registry.register(fixture.tool.clone()).unwrap();
    let guard = Arc::new(Guard {
        allowed: AtomicBool::new(true),
        checks: AtomicUsize::new(0),
    });
    let staged = stage_tools(
        &[Arc::new(StaticToolContributor::new(Arc::new(registry)))],
        &[Arc::new(RevokeDispatch(guard.clone()))],
        contribution(),
    )
    .await
    .unwrap();
    let mut context = context();
    context.dispatch_guard = Some(guard.clone());
    let error = staged
        .registry
        .get("write_document")
        .unwrap()
        .execute(context, json!({"change":1}))
        .await
        .unwrap_err();
    assert_eq!(error.code, "lease_lost");
    assert_eq!(guard.checks.load(Ordering::SeqCst), 1);
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
