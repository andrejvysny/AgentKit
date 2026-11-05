use crate::*;
use agentkit_core::{
    CoreError, Tool, ToolContext, ToolDefinition, ToolLimits, ToolRegistry, ToolResult,
};
use agentkit_store::{ClaimInput, SqliteStore, SubmitTask};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

const DIGEST: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

struct Permission(Arc<AtomicBool>, bool);

#[async_trait::async_trait]
impl ToolGuard for Permission {
    async fn can_execute(&self, _: &ToolGuardContext) -> HostResult<ToolGuardVerdict> {
        Ok(if self.0.load(Ordering::SeqCst) {
            ToolGuardVerdict::Refused("permission revoked".into())
        } else if self.1 {
            ToolGuardVerdict::AwaitingHuman(ApprovalAsk {
                reason: "approval_required: run_command".into(),
                ..Default::default()
            })
        } else {
            ToolGuardVerdict::Allowed
        })
    }
}

struct Standing;

#[async_trait::async_trait]
impl StandingApprovalPolicy for Standing {
    async fn key_for(
        &self,
        _: &ToolGuardContext,
        _: &[String],
    ) -> HostResult<Option<(String, String)>> {
        Ok(Some((DIGEST.into(), "echo".into())))
    }
}

struct WaitingCommand {
    definition: ToolDefinition,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    effects: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for WaitingCommand {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn records_command_start(&self) -> bool {
        true
    }

    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.entered.notify_one();
        self.release.notified().await;
        let refused = context
            .dispatch_guard
            .as_ref()
            .unwrap()
            .check()
            .await
            .is_err();
        if !refused {
            self.effects.fetch_add(1, Ordering::SeqCst);
        }
        Ok(ToolResult {
            ok: !refused,
            data: Some(if refused {
                json!({"errorCode":"command_not_started","phase":"before_spawn","started":false})
            } else {
                json!({"started":true})
            }),
            model_data: None,
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            summary: None,
            status: None,
            model_content: None,
        })
    }
}

async fn durable() -> (SqliteStore, ExecutionContext) {
    let store = SqliteStore::memory(8).unwrap();
    let now = Runtime::now_ms(&SystemRuntime);
    store
        .create_chat("chat", None, json!({}), now)
        .await
        .unwrap();
    store
        .submit(SubmitTask {
            task_id: "run".into(),
            kind: "chat.turn".into(),
            scope_id: "chat".into(),
            payload: json!({"chatId":"chat"}),
            priority: 0,
            now,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .unwrap();
    let claimed = store
        .claim_with_concurrency(
            ClaimInput {
                owner_id: "worker".into(),
                now,
                ttl_ms: 60_000,
                kinds: vec![],
                busy_scopes: vec![],
                max_attempts: 3,
            },
            1,
        )
        .await
        .unwrap()
        .unwrap();
    let context = ExecutionContext {
        task: claimed.task,
        attempt_id: claimed.lease.attempt_id.clone(),
        cancellation: CancellationToken::new(),
        interruption: CancellationToken::new(),
        suspension: Arc::new(Mutex::new(None)),
        store: store.clone(),
        fence: claimed.lease.fence(),
        runtime: Arc::new(SystemRuntime),
        event_gate: Arc::new(Mutex::new(())),
    };
    (store, context)
}

async fn guarded(
    tool: Arc<dyn Tool>,
    execution: &ExecutionContext,
    permission: Arc<AtomicBool>,
    standing: bool,
) -> Arc<dyn Tool> {
    let durable = Arc::new(DurableExecution {
        context: execution.clone(),
        chat_id: "chat".into(),
        is_mutating: None,
        approvals: Arc::new(Default::default()),
        standing: standing.then(|| Arc::new(Standing) as Arc<dyn StandingApprovalPolicy>),
    });
    let mut registry = ToolRegistry::new();
    registry.register(tool).unwrap();
    let staged = stage_tools(
        &[Arc::new(StaticToolContributor::new(Arc::new(registry)))],
        &[Arc::new(Permission(permission, standing))],
        ToolContributionContext {
            chat_id: Some("chat".into()),
            run_id: "run".into(),
            scope_id: None,
            actor: Value::Null,
            actor_id: None,
            bindings: vec![],
            limits: ToolLimits::default(),
            cancellation: execution.cancellation.clone(),
            has_primary_binding: true,
            timeout_ms: 5000,
            durable: Some(durable),
        },
    )
    .await
    .unwrap();
    staged.registry.get("run_command").unwrap()
}

async fn post_lease_host_change(standing: bool, drop_wrapper: bool, end_task: bool) {
    let (store, execution) = durable().await;
    let permission = Arc::new(AtomicBool::new(false));
    if standing {
        let mut chat = store.get_chat("chat").await.unwrap().unwrap();
        chat.metadata[agentkit_store::STANDING_APPROVALS_METADATA_KEY] = json!({"version":1,"grants":[{
            "grantId":"grant", "digest":DIGEST, "label":"echo", "toolName":"run_command",
            "sourceRequestId":"request", "answerId":"answer", "grantedAt":"2026-01-01T00:00:00.000Z"
        }]});
        store
            .update_chat("chat", None, Some(chat.metadata), None, 0)
            .await
            .unwrap();
    }
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let effects = Arc::new(AtomicUsize::new(0));
    let body = Arc::new(WaitingCommand {
        definition: serde_json::from_value(json!({"name":"run_command","version":"1","effect":"write","capability":"process.exec","description":"waiting command","inputSchema":{}})).unwrap(),
        entered: entered.clone(), release: release.clone(), effects: effects.clone(),
    });
    let tool = guarded(body, &execution, permission.clone(), standing).await;
    let context = ToolContext {
        run_id: "run".into(),
        tool_call_id: Some("call".into()),
        attempt_id: Some(execution.attempt_id.clone()),
        chat_id: Some("chat".into()),
        scope_id: None,
        user_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: execution.cancellation.clone(),
        dispatch_guard: Some(execution.dispatch_guard()),
        metadata: Default::default(),
        suspension: None,
    };
    let pending = tokio::spawn(async move {
        tool.execute(context, json!({"command":"echo effect"}))
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    if drop_wrapper {
        pending.abort();
    }
    if end_task {
        store
            .settle(agentkit_store::SettleTask {
                fence: execution.fence.clone(),
                now: Runtime::now_ms(&SystemRuntime),
                status: agentkit_store::TaskStatus::Cancelled,
                error: None,
                events: vec![],
                messages: vec![],
            })
            .await
            .unwrap();
        execution.cancellation.cancel();
    }
    if standing {
        store
            .revoke_standing_approval(
                "chat".into(),
                "grant".into(),
                Runtime::now_ms(&SystemRuntime),
            )
            .await
            .unwrap();
    } else {
        permission.store(true, Ordering::SeqCst);
    }
    release.notify_one();
    if drop_wrapper {
        assert!(pending.await.unwrap_err().is_cancelled());
    } else {
        let result = pending.await.unwrap();
        assert!(!result.ok, "{result:?}");
    }
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let receipts = loop {
        let receipts = store.list_executions("run".into()).await.unwrap();
        if receipts.first().is_some_and(|receipt| {
            receipt.status == agentkit_store::ExecutionReceiptStatus::Completed
        }) {
            break receipts;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no-start receipt did not settle: {receipts:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        receipts[0].status,
        agentkit_store::ExecutionReceiptStatus::Completed
    );
}

#[tokio::test]
async fn post_lease_permission_revocation_records_durable_no_start() {
    post_lease_host_change(false, false, false).await;
}

#[tokio::test]
async fn post_lease_standing_revocation_records_durable_no_start() {
    post_lease_host_change(true, false, false).await;
}

#[tokio::test]
async fn post_lease_dropped_wrapper_still_records_no_start() {
    post_lease_host_change(false, true, false).await;
}

#[tokio::test]
async fn post_lease_dropped_cancelled_wrapper_records_no_start_after_task_lease_ended() {
    post_lease_host_change(false, true, true).await;
}
