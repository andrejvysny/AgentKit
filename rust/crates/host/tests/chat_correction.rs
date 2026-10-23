use agentkit_core::{
    BudgetLimits, ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool,
    ToolCall, ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_host::*;
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;
struct Provider {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    fail_first: bool,
    empty_first: bool,
}
#[async_trait]
impl ChatProvider for Provider {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let index = requests.len();
        let has_tools = !input.tools.is_empty();
        requests.push(input);
        let events = if index == 0 && self.fail_first {
            vec![Err(CoreError::new("400", "tools unsupported"))]
        } else if index == 0 && self.empty_first {
            vec![Ok(ProviderEvent::Completed {
                content: String::new(),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            })]
        } else if has_tools && index.is_multiple_of(2) {
            vec![Ok(ProviderEvent::Completed {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("call{index}"),
                    name: "echo".into(),
                    arguments_json: "{}".into(),
                }],
                finish_reason: "tool_calls".into(),
            })]
        } else {
            vec![Ok(ProviderEvent::Completed {
                content: format!("answer{index}"),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            })]
        };
        Box::pin(futures::stream::iter(events))
    }
}
struct Echo {
    definition: ToolDefinition,
}
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"ok":true})),
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
struct Verify {
    reports: Mutex<Vec<HostResult<VerificationReport>>>,
    seen: Mutex<Vec<(String, usize, u32)>>,
}
#[async_trait]
impl VerificationHook for Verify {
    async fn verify(&self, _: ExecutionContext) -> HostResult<VerificationReport> {
        panic!("typed pass verifier expected")
    }
    async fn verify_pass(&self, input: VerificationInput) -> HostResult<VerificationReport> {
        self.seen
            .lock()
            .unwrap()
            .push((input.final_content, input.tool_call_count, input.pass));
        self.reports.lock().unwrap().remove(0)
    }
}
fn partial(count: usize) -> HostResult<VerificationReport> {
    Ok(VerificationReport {
        status: "partial".into(),
        deficiencies: (0..count).map(|index| format!("defect{index}")).collect(),
    })
}
async fn make(
    options: ChatExecutorOptions,
    fail_first: bool,
    empty_first: bool,
) -> (AgentHost, Arc<Mutex<Vec<ChatRequest>>>) {
    let store = SqliteStore::memory(64).unwrap();
    store.create_chat("chat", None, json!({}), 0).await.unwrap();
    let requests = Arc::new(Mutex::new(vec![]));
    let provider = Provider {
        requests: requests.clone(),
        fail_first,
        empty_first,
    };
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Echo {definition:serde_json::from_value(json!({"name":"echo","version":"1","effect":"read","capability":"test","description":"Echo","inputSchema":{"type":"object"}})).unwrap()})).unwrap();
    let executor: Arc<dyn TaskExecutor> =
        Arc::new(ChatExecutor::new(Arc::new(provider), Arc::new(tools)).with_options(options));
    let host = AgentHost::new(
        store,
        HashMap::from([("chat.turn".into(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    (host, requests)
}
async fn execute(host: &AgentHost) {
    host.submit_chat(SubmitChat {
        task_id: "run".into(),
        chat_id: "chat".into(),
        model: "model".into(),
        content: "original request".into(),
        parent_message_id: None,
    })
    .await
    .unwrap();
    host.run_once().await.unwrap();
}
#[tokio::test]
async fn correction_shrinks_then_stalls_with_replaced_answer_and_durable_writebacks() {
    let verifier = Arc::new(Verify {
        reports: Mutex::new(vec![partial(2), partial(1), partial(1)]),
        seen: Mutex::new(vec![]),
    });
    let options = ChatExecutorOptions {
        verification: Some(verifier.clone()),
        correction: Some(CorrectionConfig::default()),
        ..Default::default()
    };
    let (host, requests) = make(options, false, false).await;
    execute(&host).await;
    assert_eq!(
        host.store().get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert_eq!(requests.lock().unwrap().len(), 6);
    assert_eq!(verifier.seen.lock().unwrap().len(), 3);
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests[2].messages[0].content, "original request");
        assert!(
            requests[2]
                .messages
                .last()
                .unwrap()
                .content
                .as_str()
                .unwrap()
                .contains("2 item(s)")
        );
    }
    let messages = host.store().messages("chat", false).await.unwrap();
    assert_eq!(
        messages
            .iter()
            .find(|message| message.id == "run_assistant")
            .unwrap()
            .content,
        "answer5"
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.metadata.get("correctionPass").is_some())
            .count(),
        2
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.metadata["banner"] == "verification")
            .count(),
        1
    );
    let events = host.store().events("run", None, 1000).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event["type"] == "run.verification")
            .count(),
        3
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event["data"]["reason"] == "correction")
            .count(),
        2
    );
}
#[tokio::test]
async fn correction_verifier_failure_is_unavailable_never_pass() {
    let verifier = Arc::new(Verify {
        reports: Mutex::new(vec![
            partial(2),
            Err(HostError::Execution("offline".into())),
        ]),
        seen: Mutex::new(vec![]),
    });
    let (host, requests) = make(
        ChatExecutorOptions {
            verification: Some(verifier),
            correction: Some(CorrectionConfig::default()),
            ..Default::default()
        },
        false,
        false,
    )
    .await;
    execute(&host).await;
    assert_eq!(requests.lock().unwrap().len(), 4);
    let events = host.store().events("run", None, 1000).await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.event["type"] == "run.verification"
                && event.event["data"]["status"] == "unavailable")
    );
    assert_eq!(
        host.store().get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
}
#[tokio::test]
async fn correction_passes_share_cumulative_request_budget() {
    let verifier = Arc::new(Verify {
        reports: Mutex::new(vec![partial(2), partial(1)]),
        seen: Mutex::new(vec![]),
    });
    let options = ChatExecutorOptions {
        verification: Some(verifier),
        correction: Some(CorrectionConfig::default()),
        execution_budgets: BudgetLimits {
            provider_requests: Some(3),
            ..Default::default()
        },
        ..Default::default()
    };
    let (host, requests) = make(options, false, false).await;
    execute(&host).await;
    assert_eq!(requests.lock().unwrap().len(), 3);
    assert_eq!(
        host.store().get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Failed
    );
}
#[tokio::test]
async fn tool_schema_rejection_retries_once_without_tools_and_empty_answer_reasks_original() {
    for (fail, empty) in [(true, false), (false, true)] {
        let (host, requests) = make(ChatExecutorOptions::default(), fail, empty).await;
        execute(&host).await;
        assert_eq!(
            host.store().get_task("run").await.unwrap().unwrap().status,
            TaskStatus::Completed
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].tools.is_empty());
        assert_eq!(requests[1].messages.len(), 1);
        assert_eq!(requests[1].messages[0].content, "original request");
    }
}

#[tokio::test]
async fn correction_hard_cap_is_five_despite_larger_host_configuration() {
    let verifier = Arc::new(Verify {
        reports: Mutex::new((1..=6).rev().map(partial).collect()),
        seen: Mutex::new(vec![]),
    });
    let (host, requests) = make(
        ChatExecutorOptions {
            verification: Some(verifier.clone()),
            correction: Some(CorrectionConfig {
                max_passes: 99,
                ..Default::default()
            }),
            ..Default::default()
        },
        false,
        false,
    )
    .await;
    execute(&host).await;
    assert_eq!(requests.lock().unwrap().len(), 12);
    assert_eq!(verifier.seen.lock().unwrap().len(), 6);
    assert_eq!(
        host.store().get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
}
struct NullVerifier;
#[async_trait]
impl VerificationHook for NullVerifier {
    async fn verify(&self, _: ExecutionContext) -> HostResult<VerificationReport> {
        panic!("optional verifier expected")
    }
    async fn verify_optional(
        &self,
        _: VerificationInput,
    ) -> HostResult<Option<VerificationReport>> {
        Ok(None)
    }
}
#[tokio::test]
async fn null_verifier_stops_harness_as_unavailable() {
    let (host, requests) = make(
        ChatExecutorOptions {
            verification: Some(Arc::new(NullVerifier)),
            correction: Some(CorrectionConfig::default()),
            ..Default::default()
        },
        false,
        false,
    )
    .await;
    execute(&host).await;
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert!(
        host.store()
            .events("run", None, 1000)
            .await
            .unwrap()
            .iter()
            .any(|event| event.event["data"]["status"] == "unavailable")
    );
}

struct ExtraContributor(Arc<std::sync::atomic::AtomicUsize>);
#[async_trait]
impl ToolContributor for ExtraContributor {
    fn namespace(&self) -> &str {
        "extra"
    }
    async fn contribute(
        &self,
        context: &ToolContributionContext,
    ) -> HostResult<Vec<ContributedTool>> {
        assert_eq!(context.actor["id"], "actor");
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(vec![ContributedTool::Read(Arc::new(Echo {definition:serde_json::from_value(json!({"name":"extra","version":"1","effect":"read","capability":"test","description":"Extra","inputSchema":{"type":"object"}})).unwrap()}))])
    }
}
struct CountingGuard(Arc<std::sync::atomic::AtomicUsize>);
#[async_trait]
impl ToolGuard for CountingGuard {
    async fn can_execute(&self, context: &ToolGuardContext) -> HostResult<ToolGuardVerdict> {
        assert_eq!(context.contribution.run_id, "run");
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolGuardVerdict::Allowed)
    }
}
#[tokio::test]
async fn contributors_stage_once_and_same_guarded_registry_serves_correction_passes() {
    let contributions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let verifier = Arc::new(Verify {
        reports: Mutex::new(vec![
            partial(1),
            Ok(VerificationReport {
                status: "pass".into(),
                deficiencies: vec![],
            }),
        ]),
        seen: Mutex::new(vec![]),
    });
    let options = ChatExecutorOptions {
        verification: Some(verifier),
        correction: Some(CorrectionConfig::default()),
        tool_contributors: vec![Arc::new(ExtraContributor(contributions.clone()))],
        tool_guards: vec![Arc::new(CountingGuard(checks.clone()))],
        actor: json!({"id":"actor"}),
        actor_id: Some("actor".into()),
        ..Default::default()
    };
    let (host, requests) = make(options, false, false).await;
    execute(&host).await;
    assert_eq!(
        host.store().get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert_eq!(contributions.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(checks.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.tools.len() == 2)
    );
}
