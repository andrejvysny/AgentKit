//! End-to-end host authorization with inert tools; no shell commands execute.
use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_host::*;
use agentkit_store::{
    AgentInputKind, AgentInputQuery, AgentInputStatus, STANDING_APPROVALS_METADATA_KEY, SqliteStore,
};
use agentkit_toolpacks_coding::command_policy::{
    CodingStandingApproval, StandingApprovalWorkspace, WorkspaceResolver,
};
use agentkit_toolpacks_coding::types::CodingMode;
use agentkit_toolpacks_coding::{
    CodingPermissionGuard, CodingPermissionGuardOptions, HighRiskCommandGuard,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

struct Provider {
    command: String,
}
#[async_trait]
impl ChatProvider for Provider {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let call = agentkit_core::ToolCall {
            id: "call".into(),
            name: "run_command".into(),
            arguments_json: json!({"command":self.command}).to_string(),
        };
        Box::pin(futures::stream::iter([Ok(ProviderEvent::Completed {
            content: "".into(),
            tool_calls: vec![call],
            finish_reason: "tool_calls".into(),
        })]))
    }
}

struct Inert {
    definition: ToolDefinition,
    hits: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for Inert {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            ok: true,
            data: None,
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
        })
    }
}
struct Contributor(Arc<dyn Tool>);
#[async_trait]
impl ToolContributor for Contributor {
    fn namespace(&self) -> &str {
        "coding"
    }
    async fn contribute(&self, _: &ToolContributionContext) -> HostResult<Vec<ContributedTool>> {
        Ok(vec![ContributedTool::DirectWrite(self.0.clone())])
    }
}
struct Resolver(String);
#[async_trait]
impl WorkspaceResolver for Resolver {
    async fn resolve(&self, _: &str) -> Option<StandingApprovalWorkspace> {
        Some(StandingApprovalWorkspace {
            root: self.0.clone(),
            identity: "test".into(),
            context: None,
        })
    }
}

struct Fixture {
    store: SqliteStore,
    service: AgentSessionService,
    host: AgentHost,
    hits: Arc<AtomicUsize>,
}
async fn fixture(mode: CodingMode, command: &str, root: &str) -> Fixture {
    let store = SqliteStore::memory(64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let hits = Arc::new(AtomicUsize::new(0));
    let definition = serde_json::from_value(json!({"name":"run_command", "version":"1", "effect":"write", "capability":"process.exec", "description":"inert", "inputSchema":{"type":"object"}})).unwrap();
    let tool = Arc::new(Inert {
        definition,
        hits: hits.clone(),
    });
    let guard = CodingPermissionGuard::new(CodingPermissionGuardOptions {
        mode,
        role: None,
        authority: None,
        plan_tool_names: None,
    });
    let executor: Arc<dyn TaskExecutor> = Arc::new(
        ChatExecutor::new(
            Arc::new(Provider {
                command: command.into(),
            }),
            Arc::new(ToolRegistry::new()),
        )
        .with_options(ChatExecutorOptions {
            tool_contributors: vec![Arc::new(Contributor(tool))],
            tool_guards: vec![Arc::new(guard), Arc::new(HighRiskCommandGuard)],
            standing: Some(Arc::new(CodingStandingApproval {
                resolver: Resolver(root.into()),
            })),
            ..Default::default()
        }),
    );
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.into(), executor)]),
        runtime.clone(),
        HostOptions::default(),
    )
    .unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime,
        plan_approval: None,
        wake: Some(host.wake_handle()),
    });
    create_session(&service).await;
    Fixture {
        store,
        service,
        host,
        hits,
    }
}
async fn create_session(service: &AgentSessionService) {
    service
        .create_session(&CreateSessionInput {
            session_id: Some("chat".into()),
            title: None,
            profile_id: "p".into(),
            profile_digest: None,
            provider_id: None,
            model: Some("model".into()),
            workspace: None,
            context: None,
            skills: None,
            turn_kind: None,
            execution_mode: None,
            metadata: None,
        })
        .await
        .unwrap();
}
async fn run(f: &Fixture) -> String {
    let sent = f
        .service
        .send(
            "chat",
            &SendInput {
                content: json!("test"),
                input_id: None,
                when_busy: WhenBusy::Reject,
                turn_id: None,
                on_settled: None,
            },
        )
        .await
        .unwrap();
    let AgentSendResult::Turn { turn_id, .. } = sent else {
        panic!("expected turn")
    };
    f.host.run_once().await.unwrap();
    turn_id
}
async fn pending(f: &Fixture, turn: &str) -> Vec<agentkit_store::AgentInputRecord> {
    f.store
        .list_inputs(AgentInputQuery {
            session_id: "chat".into(),
            kinds: Some(vec![AgentInputKind::HumanRequest]),
            statuses: Some(vec![AgentInputStatus::Pending]),
            target_task_id: Some(turn.into()),
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn risky_commands_stop_before_inert_executor_in_every_mode() {
    let root = tempfile::tempdir().unwrap();
    for mode in [
        CodingMode::ReadOnly,
        CodingMode::Ask,
        CodingMode::AutoEdit,
        CodingMode::FullAuto,
        CodingMode::Plan,
    ] {
        for command in [
            "npx rimraf x",
            "git -c core.sshCommand=evil status",
            "\"/bin/rm\" inert",
            "node --eval=1",
        ] {
            let f = fixture(mode, command, root.path().to_str().unwrap()).await;
            let turn = run(&f).await;
            assert_eq!(f.hits.load(Ordering::SeqCst), 0, "{mode:?}: {command}");
            let requests = pending(&f, &turn).await;
            let approvable = matches!(
                mode,
                CodingMode::Ask | CodingMode::AutoEdit | CodingMode::FullAuto
            );
            assert_eq!(
                requests.len(),
                usize::from(approvable),
                "{mode:?}: {command}"
            );
            if let Some(request) = requests.first() {
                assert!(request.content.get("standing").is_none());
            }
            assert!(f.store.list_executions(turn).await.unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn old_unbound_standing_grant_never_authorizes_script() {
    let root = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(root.path()).unwrap();
    let root = canonical.to_str().unwrap();
    let f = fixture(CodingMode::Ask, "npm run test", root).await;
    // Former v1 keys contained no executable or environment fingerprint.
    let payload = json!({"v":1,"chatId":"chat","tool":"run_command","identity":"test","root":root,"cwd":root,"command":"npm run test","context":null});
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_string(&payload).unwrap().as_bytes())
    );
    let mut chat = f.store.get_chat("chat").await.unwrap().unwrap();
    chat.metadata[STANDING_APPROVALS_METADATA_KEY] = json!({"version":1,"grants":[{"grantId":"old","digest":digest,"label":"npm run test","toolName":"run_command","sourceRequestId":"old-request","answerId":"old-answer","grantedAt":"2026-01-01T00:00:00.000Z"}]});
    assert!(agentkit_store::find_active_standing_grant(&chat.metadata, &digest).is_some());
    f.store
        .update_chat("chat", None, Some(chat.metadata), None, 0)
        .await
        .unwrap();
    let turn = run(&f).await;
    assert_eq!(f.hits.load(Ordering::SeqCst), 0);
    let requests = pending(&f, &turn).await;
    assert_eq!(requests.len(), 1);
    assert!(requests[0].content.get("standing").is_none());
    assert!(f.store.list_executions(turn).await.unwrap().is_empty());
}
