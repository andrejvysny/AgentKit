//! One executor routes each durable turn to the provider its payload names:
//! chat turns through the registry, and subagent roles pinned to another
//! provider (validated at spawn, preserved across a restart).

use agentkit_agents::*;
use agentkit_core::{ChatProvider, ChatRequest, ProviderEvent, ProviderStream, ToolRegistry};
use agentkit_host::*;
use agentkit_store::{MessageInput, SqliteStore, SubmitTask, TaskStatus};
use async_trait::async_trait;
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

/// Records the model of every request it receives.
struct Fake {
    id: &'static str,
    models: Mutex<Vec<String>>,
}

impl Fake {
    fn new(id: &'static str) -> Arc<Self> {
        Arc::new(Self {
            id,
            models: Mutex::new(vec![]),
        })
    }
    fn calls(&self) -> Vec<String> {
        self.models.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChatProvider for Fake {
    fn id(&self) -> &str {
        self.id
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        self.models.lock().unwrap().push(input.model);
        Box::pin(futures::stream::iter([Ok(ProviderEvent::Completed {
            content: "answer".into(),
            finish_reason: "stop".into(),
            tool_calls: vec![],
        })]))
    }
}

fn executor_options(registry: Option<Arc<ProviderRegistry>>) -> ChatExecutorOptions {
    ChatExecutorOptions {
        providers: registry,
        ..Default::default()
    }
}

fn host_over(
    store: &SqliteStore,
    default: &Arc<Fake>,
    registry: Option<Arc<ProviderRegistry>>,
) -> AgentHost {
    let executor: Arc<dyn TaskExecutor<agentkit_store::SqliteStore>> = Arc::new(
        ChatExecutor::new(
            default.clone() as Arc<dyn ChatProvider>,
            Arc::new(ToolRegistry::new()),
        )
        .with_options(executor_options(registry)),
    );
    AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap()
}

fn registry_of(providers: &[&Arc<Fake>]) -> Arc<ProviderRegistry> {
    Arc::new(ProviderRegistry::new(
        providers
            .iter()
            .map(|provider| Arc::clone(provider) as Arc<dyn ChatProvider>),
    ))
}

async fn submit_turn(store: &SqliteStore, task_id: &str, provider_id: &str) {
    let task = SubmitTask {
        task_id: task_id.into(),
        kind: CHAT_TURN_TASK_KIND.into(),
        scope_id: "chat".into(),
        payload: json!({
            "chatId": "chat", "model": "model", "providerId": provider_id,
            "assistantMessageId": format!("{task_id}_assistant"),
        }),
        priority: 0,
        now: 0,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: true,
    };
    store
        .submit_turn(
            task,
            vec![
                MessageInput {
                    id: Some(format!("{task_id}_user")),
                    chat_id: "chat".into(),
                    content: "question".into(),
                    ..Default::default()
                },
                MessageInput {
                    id: Some(format!("{task_id}_assistant")),
                    chat_id: "chat".into(),
                    run_id: Some(task_id.into()),
                    role: "assistant".into(),
                    metadata: json!({"placeholder": true}),
                    ..Default::default()
                },
            ],
        )
        .await
        .unwrap();
}

async fn chat_store() -> SqliteStore {
    let store = SqliteStore::memory(64).unwrap();
    store.create_chat("chat", None, json!({}), 0).await.unwrap();
    store
}

async fn task_error(store: &SqliteStore, task_id: &str) -> (TaskStatus, String) {
    let task = store.get_task(task_id).await.unwrap().unwrap();
    (task.status, task.error.unwrap_or_default())
}

#[tokio::test]
async fn payload_provider_id_routes_to_exactly_that_provider() {
    let store = chat_store().await;
    let (a, b) = (Fake::new("a"), Fake::new("b"));
    let host = host_over(&store, &a, Some(registry_of(&[&a, &b])));
    submit_turn(&store, "to-b", "b").await;
    host.run_once().await.unwrap();
    assert_eq!(b.calls(), vec!["model"]);
    assert!(a.calls().is_empty());
    submit_turn(&store, "to-a", "a").await;
    host.run_once().await.unwrap();
    assert_eq!((a.calls().len(), b.calls().len()), (1, 1));
}

#[tokio::test]
async fn unknown_provider_id_fails_with_binding_mismatch() {
    let store = chat_store().await;
    let (a, b) = (Fake::new("a"), Fake::new("b"));
    let host = host_over(&store, &a, Some(registry_of(&[&a, &b])));
    submit_turn(&store, "ghost", "ghost").await;
    host.run_once().await.unwrap();
    let (status, error) = task_error(&store, "ghost").await;
    assert_eq!(status, TaskStatus::Failed);
    assert!(error.contains("provider_binding_mismatch"), "{error}");
    assert!(a.calls().is_empty() && b.calls().is_empty());
}

#[tokio::test]
async fn absent_registry_keeps_the_single_provider_binding() {
    let store = chat_store().await;
    let (a, b) = (Fake::new("a"), Fake::new("b"));
    let host = host_over(&store, &a, None);
    submit_turn(&store, "to-b", "b").await;
    host.run_once().await.unwrap();
    let (status, error) = task_error(&store, "to-b").await;
    assert_eq!(status, TaskStatus::Failed);
    assert!(error.contains("provider_binding_mismatch"), "{error}");
    assert!(a.calls().is_empty() && b.calls().is_empty());
}

// -- subagents ---------------------------------------------------------------

struct Rig {
    a: Arc<Fake>,
    b: Arc<Fake>,
    subagents: SubagentService,
}

fn role(provider: Option<&str>, model: Option<&str>) -> RoleSpec {
    RoleSpec {
        id: String::new(),
        description: "test".into(),
        prompt: "p".into(),
        allowed_tools: vec![],
        read_only: true,
        provider_id: provider.map(str::to_owned),
        model: model.map(str::to_owned),
        ..Default::default()
    }
}

fn subagent_config() -> SubagentConfig {
    let mut roles = BTreeMap::new();
    for (name, spec) in [
        ("pinned", role(Some("b"), Some("model-b"))),
        ("ghost", role(Some("ghost"), Some("model-b"))),
        ("modelless", role(Some("b"), None)),
    ] {
        roles.insert(
            name.to_owned(),
            RoleSpec {
                id: name.into(),
                ..spec
            },
        );
    }
    SubagentConfig {
        enabled: true,
        roles,
        ..Default::default()
    }
    .effective()
}

/// One process life over `store`: fresh fakes, same durable rows.
async fn rig(store: &SqliteStore, worker: bool) -> Rig {
    let (a, b) = (Fake::new("a"), Fake::new("b"));
    let registry = registry_of(&[&a, &b]);
    let host = host_over(store, &a, Some(Arc::clone(&registry)));
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: runtime.clone(),
        plan_approval: None,
        wake: Some(host.wake_handle()),
    });
    host.set_session_flush_planner(Arc::new(service.clone()))
        .await;
    host.recover().await.unwrap();
    if worker {
        host.start_worker();
    }
    let subagents = SubagentService::new(store.clone(), service, runtime, subagent_config())
        .with_routable_providers(registry.ids().map(str::to_owned).collect());
    Rig { a, b, subagents }
}

async fn parent_store() -> SqliteStore {
    let store = SqliteStore::memory(64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: runtime.clone(),
        plan_approval: None,
        wake: None,
    });
    service
        .create_session(&CreateSessionInput {
            session_id: Some("parent".into()),
            title: None,
            profile_id: "coding".into(),
            profile_digest: None,
            reasoning_effort: None,
            content_selection: None,
            provider_id: Some("a".into()),
            model: Some("model-a".into()),
            workspace: None,
            context: None,
            skills: None,
            turn_kind: None,
            execution_mode: None,
            metadata: None,
        })
        .await
        .unwrap();
    // A live task stands in for the parent's running turn.
    store
        .submit(SubmitTask {
            task_id: "parent-turn".into(),
            kind: "parent.hold".into(),
            scope_id: "parent".into(),
            payload: json!({}),
            priority: 0,
            now: runtime.now_ms(),
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .unwrap();
    store
}

fn spawn_request(key: &str, role: &str) -> SpawnRequest {
    SpawnRequest {
        parent_chat_id: "parent".into(),
        parent_task_id: "parent-turn".into(),
        spawn_key: key.into(),
        role: role.into(),
        prompt: "look".into(),
        title: None,
        model: None,
        exclusive_workspace: false,
        max_iterations: None,
        max_provider_requests: None,
        overall_timeout_ms: None,
    }
}

async fn settle(subagents: &SubagentService) {
    let outcome = subagents
        .await_children("parent", None, Some(10_000), &CancellationToken::new())
        .await
        .unwrap();
    assert!(outcome.settled, "{outcome:?}");
    assert_eq!(outcome.results[0].status, "completed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn role_pinned_to_provider_b_runs_its_turn_on_b_only() {
    let store = parent_store().await;
    let rig = rig(&store, true).await;
    rig.subagents
        .spawn(&spawn_request("k", "pinned"))
        .await
        .unwrap();
    settle(&rig.subagents).await;
    assert_eq!(rig.b.calls(), vec!["model-b"]);
    assert!(rig.a.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unroutable_or_modelless_pin_is_refused_before_any_child_exists() {
    let store = parent_store().await;
    let rig = rig(&store, true).await;
    let error = rig
        .subagents
        .spawn(&spawn_request("g", "ghost"))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("subagent_provider_unavailable: ghost"),
        "{error}"
    );
    let error = rig
        .subagents
        .spawn(&spawn_request("m", "modelless"))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("subagent_model_required: role modelless pins provider b without a model"),
        "{error}"
    );
    // An explicit request model satisfies the rule.
    let mut explicit = spawn_request("m2", "modelless");
    explicit.model = Some("model-b2".into());
    rig.subagents.spawn(&explicit).await.unwrap();
    settle(&rig.subagents).await;
    assert_eq!(rig.b.calls(), vec!["model-b2"]);
    assert_eq!(rig.subagents.list("parent").await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_resume_still_routes_the_childs_turn_to_b() {
    let store = parent_store().await;
    // First life: no worker, so the child's turn is never executed.
    let first = rig(&store, false).await;
    let child = first
        .subagents
        .spawn(&spawn_request("k", "pinned"))
        .await
        .unwrap();
    assert!(first.a.calls().is_empty() && first.b.calls().is_empty());
    // Second life over the same durable rows.
    // Recovery interrupts the unexecuted turn; resume re-dispatches it from
    // the child's durable descriptor.
    let second = rig(&store, true).await;
    second
        .subagents
        .resume(&child.child_chat_id, "r1")
        .await
        .unwrap();
    settle(&second.subagents).await;
    assert_eq!(second.b.calls(), vec!["model-b"]);
    assert!(second.a.calls().is_empty());
}
