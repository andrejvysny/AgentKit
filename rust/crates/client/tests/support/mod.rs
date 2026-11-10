//! In-process native AgentKit server for the client's service suites: the real
//! `AgentRuntime` + `DesktopBackend` + axum on an ephemeral loopback port, with
//! a scripted provider and recording ports. No TypeScript runtime is involved.
use agentkit_chat::{Authorization, MutationIdentity, ProposalApplier, ProposalService};
use agentkit_connections::{
    AppId, ConnectionRepository, MemorySecretStore, NativeConnections, UnavailableNetwork,
};
use agentkit_contracts::{
    AiProviderModel, AiToolDefinition, GrantAllowanceRequest, TestProviderResponse,
    WriteAllowanceDto,
};
use agentkit_core::{ChatProvider, ChatRequest, ProviderEvent, ProviderStream};
use agentkit_engine::SystemRuntime;
use agentkit_http::{
    Actor, Config, DesktopBackend, Problem, ProviderOperations, ToolCatalogPort, WritePolicyPort,
};
use agentkit_mcp::McpConfigRepository;
use agentkit_runtime::AgentRuntime;
use agentkit_sessions::HostResult;
use agentkit_store::{MessageInput, ProposalRecord, ProposalStatus, SqliteStore};
use async_trait::async_trait;
use axum::{extract::Request, middleware::Next, response::Response};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    any::Any,
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub const TOKEN: &str = "client-suite-token-00000000000000";
pub const CHAT: &str = "chat-client";

/// The bearer header every client call to the native server carries.
pub fn bearer() -> reqwest::header::HeaderMap {
    reqwest::header::HeaderMap::from_iter([(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {TOKEN}").parse().unwrap(),
    )])
}

/// What the server records for a suite to audit afterwards: every request it
/// answered, in order (the independent proof that the client really crossed
/// the wire with the expected operation and status), and how many proposal
/// applications reached the applier.
#[derive(Clone, Default)]
pub struct Observer {
    pub evidence: Arc<Mutex<Vec<Value>>>,
    pub applied: Arc<AtomicUsize>,
}

pub struct Native {
    pub base_url: String,
    server: tokio::task::JoinHandle<()>,
    _keep: Vec<Box<dyn Any + Send>>,
}

impl Drop for Native {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// "Reverse answer N." per turn; a turn whose last message is "park" streams
/// one delta and then waits for cancellation.
#[derive(Default)]
struct Scripted {
    turns: AtomicUsize,
}

impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "p1"
    }
    fn stream_chat(&self, input: ChatRequest, cancel: CancellationToken) -> ProviderStream {
        let parked = input.messages.last().and_then(|m| m.content.as_str()) == Some("park");
        if parked {
            return Box::pin(futures::stream::unfold(
                (0usize, cancel),
                |(index, cancel)| async move {
                    if index == 0 {
                        return Some((Ok(ProviderEvent::Delta("waiting".into())), (1, cancel)));
                    }
                    cancel.cancelled().await;
                    None
                },
            ));
        }
        let text = format!(
            "Reverse answer {}.",
            self.turns.fetch_add(1, Ordering::SeqCst) + 1
        );
        Box::pin(futures::stream::iter([
            Ok(ProviderEvent::Delta(text.clone())),
            Ok(ProviderEvent::Completed {
                content: text,
                tool_calls: vec![],
                finish_reason: "stop".into(),
            }),
        ]))
    }
}

struct Ports {
    store: SqliteStore,
    applied: Arc<AtomicUsize>,
}

fn failure(error: impl std::fmt::Display) -> Problem {
    Problem::host("internal_error", error.to_string())
}
fn typed<T: DeserializeOwned>(value: Value) -> Result<T, Problem> {
    serde_json::from_value(value).map_err(failure)
}
fn encoded(value: impl Serialize) -> Result<Value, Problem> {
    serde_json::to_value(value).map_err(failure)
}

#[async_trait]
impl WritePolicyPort for Ports {
    async fn list(&self, _: &Actor, chat: &str) -> Result<Vec<WriteAllowanceDto>, Problem> {
        self.store
            .list_settings(&format!("allowance:{chat}"))
            .await
            .map_err(failure)?
            .into_iter()
            .map(|(_, value)| typed(value))
            .collect()
    }
    async fn grant(
        &self,
        _: &Actor,
        chat: &str,
        input: GrantAllowanceRequest,
    ) -> Result<WriteAllowanceDto, Problem> {
        let input = encoded(input)?;
        let key = format!(
            "{}:{}",
            input["toolName"].as_str().unwrap(),
            input["proposalKind"].as_str().unwrap()
        );
        let value = json!({
            "key": key, "chatId": chat, "toolName": input["toolName"],
            "proposalKind": input["proposalKind"], "maxRisk": input["maxRisk"],
            "createdAt": "2026-01-01T00:00:00.000Z",
        });
        self.store
            .set_setting(&format!("allowance:{chat}"), &key, value.clone())
            .await
            .map_err(failure)?;
        typed(value)
    }
    async fn revoke(&self, _: &Actor, chat: &str, key: &str) -> Result<(), Problem> {
        self.store
            .delete_setting(&format!("allowance:{chat}"), key)
            .await
            .map_err(failure)?;
        Ok(())
    }
}

#[async_trait]
impl ToolCatalogPort for Ports {
    async fn list(&self, _: &Actor) -> Result<Vec<AiToolDefinition>, Problem> {
        Ok(vec![typed(json!({
            "name": "fixture_read", "version": "1.0.0", "effect": "read",
            "capability": "read", "description": "Fixture read",
            "inputSchema": {"type": "object"},
        }))?])
    }
}

#[async_trait]
impl ProviderOperations for Ports {
    async fn refresh_models(&self, _: &Actor, id: &str) -> Result<Vec<AiProviderModel>, Problem> {
        Ok(vec![typed(json!({
            "providerId": id, "modelId": "m2", "displayName": "Refreshed",
            "fetchedAt": "1970-01-01T00:00:00.000Z",
        }))?])
    }
    async fn test(&self, _: &Actor, _: &str) -> Result<TestProviderResponse, Problem> {
        typed(json!({"ok": true, "latencyMs": 1}))
    }
}

#[async_trait]
impl Authorization for Ports {
    async fn authorize(&self, _: &Value, _: &str, _: &Value) -> HostResult<()> {
        Ok(())
    }
}

#[async_trait]
impl ProposalApplier for Ports {
    async fn apply(&self, _: MutationIdentity, _: Value) -> HostResult<Value> {
        self.applied.fetch_add(1, Ordering::SeqCst);
        Ok(json!({
            "status": "applied", "appliedOps": 1, "failedOps": [],
            "resultJson": null, "revision": null,
        }))
    }
    async fn reconcile(&self, _: MutationIdentity) -> HostResult<Option<Value>> {
        Ok(None)
    }
}

/// A moment no running worker reaches, so the live worker cannot claim the
/// turn seeded below before it is interrupted.
const FAR: i64 = 4_000_000_000_000;

/// The two pending proposals and the interrupted chat turn the service suites
/// act on, written through the store the server owns.
async fn seed(store: &SqliteStore) -> HostResult<()> {
    store.create_chat(CHAT, None, json!({}), 0).await?;
    store.create_chat("resume-chat", None, json!({}), 0).await?;
    for (id, role, parent) in [
        ("resume-user", "user", None),
        ("resume-assistant", "assistant", Some("resume-user")),
    ] {
        store
            .append_message(
                MessageInput {
                    id: Some(id.into()),
                    chat_id: "resume-chat".into(),
                    run_id: Some("resume-me".into()),
                    role: role.into(),
                    content: json!(if role == "user" {
                        "resume question"
                    } else {
                        ""
                    }),
                    parent_message_id: parent.map(str::to_owned),
                    metadata: if role == "user" {
                        json!({})
                    } else {
                        json!({"placeholder": true})
                    },
                    ..MessageInput::default()
                },
                None,
                0,
            )
            .await?;
    }
    store
        .submit(agentkit_store::SubmitTask {
            task_id: "resume-me".into(),
            kind: agentkit_sessions::CHAT_TURN_TASK_KIND.into(),
            scope_id: "resume-chat".into(),
            payload: json!({
                "chatId": "resume-chat", "model": "m1",
                "userMessageId": "resume-user", "assistantMessageId": "resume-assistant",
            }),
            priority: 0,
            now: 0,
            available_at: Some(FAR),
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await?;
    // Queued for a time no worker reaches, then interrupted: a crashed turn
    // that never started an attempt, as `resumeRun` expects.
    store
        .interrupt_task("resume-me", "process_crashed", None, 0)
        .await?;
    for id in ["approve-me", "reject-me"] {
        store
            .create_proposal(ProposalRecord {
                id: id.into(),
                chat_id: CHAT.into(),
                run_id: None,
                scope_key: CHAT.into(),
                action_id: None,
                tool_name: "edit".into(),
                kind: "edit".into(),
                risk: "low".into(),
                status: ProposalStatus::Pending,
                envelope: json!({"summary": "Fixture proposal"}),
                operations: vec![json!({"type": "edit"})],
                warnings: vec![],
                truncated: false,
                revision_at_create: None,
                operation_id: None,
                decision: None,
                reason: None,
                created_at: 0,
                decided_at: None,
                claimed_at: None,
                applied_at: None,
            })
            .await?;
    }
    Ok(())
}

/// Records `(case, operation, method, path, status)` for each answered request.
async fn record(evidence: Arc<Mutex<Vec<Value>>>, request: Request, next: Next) -> Response {
    let (case, operation, method, path) = {
        let header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        (
            header("x-agentkit-case").unwrap_or_else(|| "stream".into()),
            header("x-agentkit-operation").unwrap_or_default(),
            request.method().to_string(),
            request.uri().path().to_owned(),
        )
    };
    let response = next.run(request).await;
    evidence.lock().unwrap().push(json!({
        "case": case, "operation": operation, "method": method, "path": path,
        "status": response.status().as_u16(),
    }));
    response
}

impl Native {
    pub async fn start(observer: &Observer) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(dir.path().join("store.sqlite"), 16).unwrap();
        let mut runtime = AgentRuntime::builder(store.clone())
            .default_provider(Arc::new(Scripted::default()))
            .build()
            .await
            .unwrap();
        let worker = runtime.take_worker().unwrap();
        seed(&store).await.unwrap();

        let ports = Arc::new(Ports {
            store: store.clone(),
            applied: observer.applied.clone(),
        });
        let proposals = Arc::new(ProposalService::new(
            store.clone(),
            ports.clone(),
            ports.clone(),
            Arc::new(SystemRuntime),
        ));
        let connections =
            ConnectionRepository::default().for_app(AppId::new("client-actor").unwrap());
        let secrets = Arc::new(MemorySecretStore::default());
        let native = Arc::new(NativeConnections::new(
            connections.clone(),
            secrets.clone(),
            Arc::new(UnavailableNetwork),
        ));
        let mcp = Arc::new(
            McpConfigRepository::new(store.clone(), "client-actor", Arc::new(SystemRuntime))
                .unwrap()
                .with_ports(Some(Arc::new(MemoryVault::default())), None)
                .unwrap(),
        );
        mcp.initialize().await.unwrap();
        let actor = Actor {
            id: "client-actor".into(),
            metadata: BTreeMap::new(),
        };
        let backend = DesktopBackend::open(runtime.host().clone(), actor, TOKEN, "m1")
            .await
            .unwrap()
            .with_connections(connections, Some(native), Some(secrets))
            .with_pinned_provider("p1")
            .with_proposals(proposals)
            .with_write_policy(ports.clone())
            .with_mcp_configuration(mcp)
            .with_tool_catalog(ports.clone())
            .with_provider_operations(ports);

        let mut config = Config::default();
        config.streaming.poll_interval = Duration::from_millis(2);
        config.streaming.retry_hint = Duration::from_millis(10);
        config.streaming.heartbeat_interval = Duration::from_secs(60);
        let recorder = observer.evidence.clone();
        let app =
            agentkit_http::router(Arc::new(backend), config).layer(axum::middleware::from_fn(
                move |request: Request, next: Next| record(recorder.clone(), request, next),
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base_url,
            server,
            _keep: vec![Box::new(dir), Box::new(runtime), Box::new(worker)],
        }
    }
}

/// An in-memory credential vault: the MCP configuration route stores secret
/// references only, and the vault holds the bytes.
#[derive(Default)]
struct MemoryVault(Mutex<BTreeMap<String, String>>);

#[async_trait]
impl agentkit_mcp::McpConfigSecrets for MemoryVault {
    async fn get(&self, reference: &str) -> Result<Option<String>, agentkit_mcp::McpConfigError> {
        Ok(self.0.lock().unwrap().get(reference).cloned())
    }
    async fn set(&self, reference: &str, value: &str) -> Result<(), agentkit_mcp::McpConfigError> {
        self.0
            .lock()
            .unwrap()
            .insert(reference.to_owned(), value.to_owned());
        Ok(())
    }
    async fn delete(&self, reference: &str) -> Result<(), agentkit_mcp::McpConfigError> {
        self.0.lock().unwrap().remove(reference);
        Ok(())
    }
}
