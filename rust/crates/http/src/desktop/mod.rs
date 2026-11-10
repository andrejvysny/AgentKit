//! Optional desktop HTTP boundary. One immutable actor owns one local store.
mod chats;
mod managed_content;
mod native_provider_binding;
mod native_provider_operations;
pub use native_provider_operations::{NativeProviderOperations, ProviderConsentPort};
mod management;
mod mcp_configuration;
mod ports;
pub use ports::{McpConfigurationPort, ProviderOperations, ToolCatalogPort, WritePolicyPort};
mod projections;
mod provider_commit;
mod provider_config;
mod provider_secrets;
mod providers;
mod teammates;
mod turns;

use crate::{Actor, Backend, OperationRequest, OperationResponse, Problem};
use agentkit_sessions::{AgentHost, HostError};
use agentkit_store::StoreError;
use async_trait::async_trait;
use axum::http::HeaderMap;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct DesktopBackend {
    host: AgentHost,
    actor: Actor,
    token: String,
    default_model: String,
    session_profile: Option<String>,
    content_authority: Option<Arc<dyn agentkit_sessions::ContentAuthority>>,
    profiles: Arc<agentkit_sessions::ProfileRegistry>,
    pinned_provider_id: Option<String>,
    allow_insecure_http: bool,
    write_gate: Mutex<()>,
    connections: agentkit_connections::AppConnections,
    native_connections: Option<Arc<agentkit_connections::NativeConnections>>,
    secrets: Option<agentkit_connections::SharedSecretStore>,
    proposals: Option<Arc<agentkit_chat::ProposalService>>,
    write_policy: Option<Arc<dyn WritePolicyPort>>,
    mcp_configuration: Option<Arc<dyn McpConfigurationPort>>,
    tool_catalog: Option<Arc<dyn ToolCatalogPort>>,
    provider_operations: Option<Arc<dyn ProviderOperations>>,
    provider_factory: Option<Arc<dyn agentkit_sessions::ChatProviderFactory>>,
    /// AGENTKIT-90: the host defaults the `RunProfileService` resolves
    /// against when an admission asks for a sealed effective profile.
    /// `None` is the service's documented default (native provider,
    /// `ask` ceiling, no ambient tools) — never a per-request value.
    run_profile_defaults: Option<agentkit_sessions::RunProfileDefaults>,
}

impl DesktopBackend {
    pub fn new(
        host: AgentHost,
        actor: Actor,
        token: &str,
        default_model: &str,
    ) -> Result<Self, Problem> {
        if actor.id.is_empty() || token.len() < 32 || default_model.is_empty() {
            return Err(Problem::new(
                400,
                "invalid_request",
                "Actor, model, and a token of at least 32 bytes are required.",
            ));
        }
        let app_id = agentkit_connections::AppId::new(actor.id.clone())
            .map_err(providers::connection_error)?;
        let connections = agentkit_connections::ConnectionRepository::default().for_app(app_id);
        Ok(Self {
            host,
            actor,
            token: token.into(),
            default_model: default_model.into(),
            session_profile: None,
            content_authority: None,
            profiles: Arc::new(agentkit_sessions::ProfileRegistry::default()),
            write_gate: Mutex::new(()),
            pinned_provider_id: None,
            allow_insecure_http: false,
            connections,
            native_connections: None,
            secrets: None,
            proposals: None,
            write_policy: None,
            mcp_configuration: None,
            tool_catalog: None,
            provider_operations: None,
            provider_factory: None,
            run_profile_defaults: None,
        })
    }

    pub async fn open(
        host: AgentHost,
        actor: Actor,
        token: &str,
        default_model: &str,
    ) -> Result<Self, Problem> {
        let mut backend = Self::new(host, actor, token, default_model)?;
        backend.ensure_actor(&backend.actor).await?;
        if let Some(snapshot) = backend
            .host
            .store()
            .get_setting("http", "connections")
            .await
            .map_err(store_error)?
        {
            let app_id = snapshot
                .get("appId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let metadata = snapshot.get("repository").cloned().unwrap_or(snapshot);
            let snapshot: agentkit_connections::RepositorySnapshot =
                serde_json::from_value(metadata)
                    .map_err(|e| Problem::host("internal_error", e.to_string()))?;
            let mut apps: std::collections::BTreeSet<_> = snapshot
                .connections
                .iter()
                .map(|c| c.app_id.as_str().to_owned())
                .collect();
            if let Some(id) = app_id {
                apps.insert(id);
            }
            if apps.len() != 1 {
                return Err(Problem::host(
                    "internal_error",
                    "Connection snapshot must name exactly one application authority.",
                ));
            }
            let app_id = apps.into_iter().next().expect("single application checked");
            let app_id =
                agentkit_connections::AppId::new(app_id).map_err(providers::connection_error)?;
            let repository = agentkit_connections::ConnectionRepository::restore(snapshot)
                .map_err(providers::connection_error)?;
            backend.connections = repository.for_app(app_id);
        }
        Ok(backend)
    }

    pub fn with_connections(
        mut self,
        connections: agentkit_connections::AppConnections,
        native: Option<Arc<agentkit_connections::NativeConnections>>,
        secrets: Option<agentkit_connections::SharedSecretStore>,
    ) -> Self {
        self.connections = connections;
        self.native_connections = native;
        self.secrets = secrets;
        self
    }
    /// Make the provider-management routes real for this backend: the
    /// connection authority (credential-key derivation, generation fences)
    /// rides the backend's OWN restored repository, and the credential
    /// vault is the caller's port. With both wired, `createProvider` /
    /// `updateProvider` accept `apiKey`/`extraHeaders`; without them those
    /// routes answer 501 exactly as the TypeScript transport does when no
    /// `SecretStore` is wired. Network stays unavailable: the provider
    /// probe routes (`testProvider`/`refreshProviderModels`) require a
    /// `ProviderOperations` port via `with_provider_operations`, so this
    /// wiring never opens an egress path on its own.
    pub fn with_provider_secrets(
        mut self,
        secrets: agentkit_connections::SharedSecretStore,
    ) -> Self {
        self.native_connections = Some(Arc::new(agentkit_connections::NativeConnections::new(
            self.connections.clone(),
            Arc::clone(&secrets),
            Arc::new(agentkit_connections::UnavailableNetwork),
        )));
        self.secrets = Some(secrets);
        self
    }
    /// Create REST chats as validated agent sessions with one durable insert.
    pub fn with_profiles(mut self, profiles: Arc<agentkit_sessions::ProfileRegistry>) -> Self {
        self.profiles = profiles;
        self
    }

    pub fn with_content_authority(
        mut self,
        authority: Arc<dyn agentkit_sessions::ContentAuthority>,
    ) -> Self {
        self.content_authority = Some(authority);
        self
    }

    /// The explicit factory validates selected provider identities before admission.
    pub fn with_provider_factory(
        mut self,
        factory: Arc<dyn agentkit_sessions::ChatProviderFactory>,
    ) -> Self {
        self.provider_factory = Some(factory);
        self
    }

    pub fn with_session_profile(mut self, profile_id: &str) -> Result<Self, Problem> {
        if profile_id.trim().is_empty() {
            return Err(Problem::new(
                400,
                "invalid_request",
                "Session profile must not be empty.",
            ));
        }
        self.session_profile = Some(profile_id.to_owned());
        Ok(self)
    }

    pub fn with_pinned_provider(mut self, id: &str) -> Self {
        self.pinned_provider_id = Some(id.into());
        self
    }
    pub fn with_insecure_http(mut self, allowed: bool) -> Self {
        self.allow_insecure_http = allowed;
        self
    }
    pub fn with_write_policy(mut self, port: Arc<dyn WritePolicyPort>) -> Self {
        self.write_policy = Some(port);
        self
    }
    pub fn with_mcp_configuration(mut self, port: Arc<dyn McpConfigurationPort>) -> Self {
        self.mcp_configuration = Some(port);
        self
    }
    pub fn with_tool_catalog(mut self, port: Arc<dyn ToolCatalogPort>) -> Self {
        self.tool_catalog = Some(port);
        self
    }
    pub fn with_provider_operations(mut self, port: Arc<dyn ProviderOperations>) -> Self {
        self.provider_operations = Some(port);
        self
    }
    pub fn with_proposals(mut self, service: Arc<agentkit_chat::ProposalService>) -> Self {
        self.proposals = Some(service);
        self
    }

    /// AGENTKIT-90: host-wide defaults for effective-run-profile
    /// resolution (native provider id, permission ceiling, ambient tool
    /// allowlist). Set once at wiring; a request can never choose these.
    pub fn with_run_profile_defaults(
        mut self,
        defaults: agentkit_sessions::RunProfileDefaults,
    ) -> Self {
        self.run_profile_defaults = Some(defaults);
        self
    }

    pub fn into_router(self: Arc<Self>, config: crate::Config) -> axum::Router {
        crate::router(self, config)
    }

    async fn ensure_actor(&self, actor: &Actor) -> Result<(), Problem> {
        if actor.id == self.actor.id {
            if !self
                .host
                .store()
                .bind_setting(
                    "agentkit.internal",
                    "desktopActor",
                    serde_json::json!(actor.id),
                )
                .await
                .map_err(store_error)?
            {
                return Err(Problem::new(
                    403,
                    "forbidden",
                    "This store belongs to a different desktop actor.",
                ));
            }
            Ok(())
        } else {
            Err(Problem::new(
                403,
                "forbidden",
                "Actor does not own this desktop store.",
            ))
        }
    }

    async fn operation(&self, input: &OperationRequest) -> Result<OperationResponse, Problem> {
        if let Some(output) = self.chats(input).await? {
            return Ok(output);
        }
        if let Some(output) = self.turns(input).await? {
            return Ok(output);
        }
        if let Some(output) = self.teammates(input).await? {
            return Ok(output);
        }
        self.management(input).await
    }
}

#[async_trait]
impl Backend for DesktopBackend {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Actor, Problem> {
        let actual = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if actual.is_some_and(|actual| constant_equal(actual.as_bytes(), self.token.as_bytes())) {
            Ok(self.actor.clone())
        } else {
            Err(Problem::new(
                401,
                "unauthorized",
                "A valid desktop bearer token is required.",
            ))
        }
    }
    async fn authorize(&self, input: &OperationRequest) -> Result<(), Problem> {
        self.ensure_actor(&input.actor).await?;
        // SSE visibility is decided BEFORE the first event frame, and
        // through the same doors as the JSON routes: a `streamRun` id
        // must name a chat run, a `streamAgentTask` id must name a task
        // under one of the caller's own agents. Foreign and unknown ids
        // are indistinguishable refusals — the stream never becomes an
        // existence oracle.
        match input.operation.as_str() {
            "streamRun" => {
                self.read_run(&input.params["runId"]).await?;
            }
            "streamAgentTask" => {
                self.owned_agent_task(
                    &input.params["taskId"],
                    &teammates::actor_scope(&input.actor),
                )
                .await?;
            }
            _ => {}
        }
        Ok(())
    }
    async fn dispatch(&self, input: OperationRequest) -> Result<OperationResponse, Problem> {
        self.ensure_actor(&input.actor).await?;
        let _guard = self.write_gate.lock().await;
        self.operation(&input).await
    }
    async fn events(
        &self,
        actor: &Actor,
        run_id: &str,
        after: Option<f64>,
        limit: usize,
    ) -> Result<Vec<Value>, Problem> {
        self.ensure_actor(actor).await?;
        let task = self
            .stream_task(actor, run_id)
            .await?
            .ok_or_else(|| missing("Run", run_id))?;
        let records = self
            .host
            .store()
            .events_after_number(run_id, after, limit)
            .await
            .map_err(store_error)?;
        // Agent-task replay passes through the consumer redactor
        // (credential-named values, header/env bags, oversized tool
        // texts). Chat replay keeps its established bytes: existing
        // Chat/Code clients parse those frames today, and this lane is
        // additive compatibility.
        let agent_bound = task.agent_id.is_some();
        Ok(records
            .into_iter()
            .map(|e| {
                if agent_bound {
                    agentkit_store::redact_event(&e.event)
                } else {
                    e.event
                }
            })
            .collect())
    }
    async fn run_status(&self, actor: &Actor, run_id: &str) -> Result<Option<String>, Problem> {
        self.ensure_actor(actor).await?;
        let Some(task) = self.stream_task(actor, run_id).await? else {
            return Ok(None);
        };
        Ok(serde_json::to_value(task.status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned)))
    }
}

fn constant_equal(a: &[u8], b: &[u8]) -> bool {
    let mut difference = a.len() ^ b.len();
    for (index, byte) in b.iter().enumerate() {
        difference |= usize::from(*byte ^ a.get(index).copied().unwrap_or(0));
    }
    difference == 0
}

fn store_error(error: StoreError) -> Problem {
    let code = match &error {
        StoreError::NotFound(_) => "not_found",
        StoreError::Duplicate(_) => "duplicate_task",
        StoreError::ScopeBusy(_) => "chat_busy",
        StoreError::LeaseLost(_) => "lease_lost",
        StoreError::InvalidTransition(_) => "invalid_task_transition",
        StoreError::InvalidInput(message) if message == "invalid_fork_point" => {
            "invalid_fork_point"
        }
        StoreError::InvalidInput(_) => "invalid_request",
        _ => "internal_error",
    };
    if code == "invalid_request" {
        Problem::new(400, code, error.to_string())
    } else {
        Problem::host(code, error.to_string())
    }
}
fn host_error(error: HostError) -> Problem {
    match error {
        HostError::Store(error) => store_error(error),
        HostError::UnknownKind(kind) => Problem::host("executor_not_found", kind),
        HostError::NoExecutorForKind(kind) => Problem::host("no_executor_for_kind", kind),
        _ => Problem::host("internal_error", error.to_string()),
    }
}
fn missing(kind: &str, id: &str) -> Problem {
    Problem::new(404, "not_found", format!("{kind} not found: {id}"))
}
fn output(status: u16, value: Value) -> OperationResponse {
    OperationResponse::json(status, value)
}
fn optional(detail: &str) -> Problem {
    Problem::new(501, "not_implemented", detail)
}
fn limit(input: &OperationRequest, default: usize) -> usize {
    input
        .query
        .get("limit")
        .and_then(|s| {
            let s = s.trim();
            if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                usize::from_str_radix(hex, 16).ok()
            } else if let Some(octal) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
                usize::from_str_radix(octal, 8).ok()
            } else if let Some(binary) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
                usize::from_str_radix(binary, 2).ok()
            } else {
                s.parse::<f64>().ok().map(|n| n as usize)
            }
        })
        .unwrap_or(default)
}
