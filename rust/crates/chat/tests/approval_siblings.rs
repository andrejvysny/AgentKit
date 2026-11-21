//! A model message with several tool calls where one asks for approval: the
//! turn parks at that call, and after the answer every call of the message
//! that has not run yet runs, in the order the model sent them. Before this,
//! only the approved call resumed and its later siblings were dropped, so
//! the model re-did their work (first full Code eval, 21 Nov 2026).

use agentkit_chat::*;
use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolCall,
    ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_engine::{SharedRuntime, SystemRuntime, TaskExecutor};
use agentkit_governance::{
    ApprovalAsk, ContributedTool, GovernanceResult, ToolContributionContext, ToolContributor,
    ToolGuard, ToolGuardContext, ToolGuardVerdict,
};
use agentkit_sessions::*;
use agentkit_store::{AgentInputKind, AgentInputQuery, AgentInputStatus, SqliteStore, TaskStatus};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

/// The first request answers with `calls`; every later one finishes.
struct Scripted {
    calls: Vec<(&'static str, &'static str)>,
    served: Mutex<bool>,
}

#[async_trait]
impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let first = !std::mem::replace(&mut *self.served.lock().unwrap(), true);
        let calls: Vec<ToolCall> = if first {
            self.calls
                .iter()
                .map(|(id, tool)| ToolCall {
                    id: (*id).into(),
                    name: (*tool).into(),
                    arguments_json: format!("{{\"id\":\"{id}\"}}"),
                })
                .collect()
        } else {
            vec![]
        };
        let done = calls.is_empty();
        Box::pin(futures::stream::iter(vec![Ok(ProviderEvent::Completed {
            content: if done { "done" } else { "" }.into(),
            finish_reason: if done { "stop" } else { "tool_calls" }.into(),
            tool_calls: calls,
        })]))
    }
}

/// Records which call ran, in order.
struct Recorder {
    definition: ToolDefinition,
    ran: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for Recorder {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, args: Value) -> Result<ToolResult, CoreError> {
        let id = args["id"].as_str().unwrap_or_default().to_owned();
        self.ran.lock().unwrap().push(id);
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"done": true})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
            model_content: None,
        })
    }
}

struct Contributor(Vec<Arc<dyn Tool>>);

#[async_trait]
impl ToolContributor for Contributor {
    fn namespace(&self) -> &str {
        "coding"
    }
    async fn contribute(
        &self,
        _: &ToolContributionContext,
    ) -> GovernanceResult<Vec<ContributedTool>> {
        Ok(self
            .0
            .iter()
            .map(|tool| ContributedTool::DirectWrite(tool.clone()))
            .collect())
    }
}

/// Asks for every `write_file` call; lets `edit_file` through. Both are
/// `coding` profile names, so the session offers them.
struct AskGuard;

#[async_trait]
impl ToolGuard for AskGuard {
    async fn can_execute(&self, context: &ToolGuardContext) -> GovernanceResult<ToolGuardVerdict> {
        if context.tool.name != "write_file" {
            return Ok(ToolGuardVerdict::Allowed);
        }
        Ok(ToolGuardVerdict::AwaitingHuman(ApprovalAsk {
            reason: "approval_required: write_file".into(),
            preview: None,
            requested_mode: None,
            uncertainty_generation: None,
        }))
    }
}

struct Fixture {
    store: SqliteStore,
    host: AgentHost,
    service: AgentSessionService<SqliteStore>,
    ran: Arc<Mutex<Vec<String>>>,
}

fn tool(name: &str, ran: &Arc<Mutex<Vec<String>>>) -> Arc<dyn Tool> {
    let definition: ToolDefinition = serde_json::from_value(json!({
        "name": name, "version": "1", "effect": "write",
        "capability": "fs.write", "description": "record",
        "inputSchema": {"type": "object"},
    }))
    .unwrap();
    Arc::new(Recorder {
        definition,
        ran: ran.clone(),
    })
}

async fn fixture(calls: Vec<(&'static str, &'static str)>) -> Fixture {
    let store = SqliteStore::memory(64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let ran = Arc::new(Mutex::new(vec![]));
    let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(
        AgentExecutor::new(
            Arc::new(Scripted {
                calls,
                served: Mutex::new(false),
            }),
            Arc::new(ToolRegistry::new()),
        )
        .with_options(AgentExecutorOptions {
            tool_contributors: vec![Arc::new(Contributor(vec![
                tool("edit_file", &ran),
                tool("write_file", &ran),
            ]))],
            tool_guards: vec![Arc::new(AskGuard)],
            ..Default::default()
        }),
    );
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
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
    host.set_session_flush_planner(Arc::new(service.clone()))
        .await;
    service
        .create_session(&CreateSessionInput {
            session_id: Some("s1".into()),
            profile_id: "coding".into(),
            model: Some("model".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    Fixture {
        store,
        host,
        service,
        ran,
    }
}

impl Fixture {
    async fn send(&self) -> String {
        let result = self
            .service
            .send(
                "s1",
                &SendInput {
                    frozen_payload: None,
                    content: json!("work"),
                    input_id: Some("t1".into()),
                    when_busy: WhenBusy::Reject,
                    turn_id: None,
                    on_settled: None,
                },
            )
            .await
            .unwrap();
        match result {
            AgentSendResult::Turn { turn_id, .. } => turn_id,
            AgentSendResult::Queued { .. } => panic!("an idle session starts the turn"),
        }
    }

    async fn status(&self, turn_id: &str) -> TaskStatus {
        self.store.get_task(turn_id).await.unwrap().unwrap().status
    }

    async fn pending(&self, turn_id: &str) -> Vec<agentkit_store::AgentInputRecord> {
        self.store
            .list_inputs(AgentInputQuery {
                session_id: "s1".into(),
                kinds: Some(vec![AgentInputKind::HumanRequest]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: Some(turn_id.to_owned()),
            })
            .await
            .unwrap()
    }

    /// Approves the turn's one pending ask and runs the resumed attempt.
    async fn approve_next(&self, turn: &str, answer: &str) {
        let pending = self.pending(turn).await;
        assert_eq!(pending.len(), 1, "one ask at a time");
        self.service
            .approve(&pending[0].input_id, answer, None, None)
            .await
            .unwrap();
        self.host.run_once().await.unwrap();
    }

    fn ran(&self) -> Vec<String> {
        self.ran.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn calls_after_an_approved_call_run_once_it_is_approved() {
    let f = fixture(vec![
        ("c1", "edit_file"),
        ("c2", "write_file"),
        ("c3", "edit_file"),
    ])
    .await;
    let turn = f.send().await;
    f.host.run_once().await.unwrap();
    assert_eq!(f.status(&turn).await, TaskStatus::WaitingApproval);
    assert_eq!(f.ran(), ["c1"], "the turn parks at the asking call");

    f.approve_next(&turn, "a1").await;
    assert_eq!(f.ran(), ["c1", "c2", "c3"]);
    assert_eq!(f.status(&turn).await, TaskStatus::Completed);
}

#[tokio::test]
async fn a_later_sibling_that_asks_parks_again_and_then_runs() {
    let f = fixture(vec![
        ("c1", "write_file"),
        ("c2", "edit_file"),
        ("c3", "write_file"),
    ])
    .await;
    let turn = f.send().await;
    f.host.run_once().await.unwrap();
    assert!(f.ran().is_empty());

    f.approve_next(&turn, "a1").await;
    assert_eq!(f.ran(), ["c1", "c2"], "c3 asks for its own approval");
    assert_eq!(f.status(&turn).await, TaskStatus::WaitingApproval);

    f.approve_next(&turn, "a2").await;
    assert_eq!(f.ran(), ["c1", "c2", "c3"], "each call runs exactly once");
    assert_eq!(f.status(&turn).await, TaskStatus::Completed);
}
