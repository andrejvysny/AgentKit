//! Late-bound native execution keeps connection writes and streaming on one authority.
use crate::{
    ChatCompletionsProvider, NativeConnectionTransport, ResponsesProvider, TrustedTransport,
    disclosed_categories,
};
use agentkit_connections::{
    AppConnections, Capability, Connection, ConnectionError, ConsentToken, NativeConnections,
    PlanInput, RequestFence, RequestPlan, ReqwestConnectionNetwork, SharedSecretStore,
};
use agentkit_core::{AuthKind, ChatProvider, ConnectionIdentity, CoreError};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::any::Any;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Revokes its consent token on drop so an approval never outlives the request it covered.
pub struct Approval {
    native: NativeConnections,
    pub token: ConsentToken,
}

impl Approval {
    pub fn new(native: NativeConnections, token: ConsentToken) -> Self {
        Self { native, token }
    }
}

impl Drop for Approval {
    fn drop(&mut self) {
        let _ = self.native.revoke(&self.token);
    }
}

/// Host-supplied consent. The host owns who is asking and how approval is obtained.
#[async_trait]
pub trait ProviderApprover: Send + Sync {
    /// Approve this exact plan and mint its consent token, or fail.
    async fn approve(
        &self,
        native: &NativeConnections,
        plan: &RequestPlan,
        cancel: CancellationToken,
    ) -> Result<Approval, CoreError>;
    fn as_any(&self) -> &dyn Any;
    /// Whether both approvers consult the same consent handle.
    fn same_consent(&self, other: &dyn ProviderApprover) -> bool;
    /// Whether both approvers act for the same actor.
    fn same_actor(&self, other: &dyn ProviderApprover) -> bool;
}

#[derive(Debug)]
pub enum BindError {
    /// The authority lock was poisoned.
    Lock,
    /// A different vault or consent handle was supplied after the first bind.
    VaultOrConsentChanged,
    /// The actor, app or connection export differs from the first bind.
    AuthorityDiffers,
    Connection(ConnectionError),
}

#[derive(Default)]
pub struct NativeProviderFactory {
    static_provider_ids: BTreeSet<String>,
    binding: RwLock<Option<Arc<NativeProviderBinding>>>,
}

pub struct NativeProviderBinding {
    pub connections: AppConnections,
    pub native: NativeConnections,
    pub secrets: SharedSecretStore,
    network: Arc<ReqwestConnectionNetwork>,
    approver: Arc<dyn ProviderApprover>,
}

impl NativeProviderFactory {
    pub fn with_static_provider_ids(mut self, ids: BTreeSet<String>) -> Self {
        self.static_provider_ids = ids;
        self
    }

    pub fn bind(
        &self,
        connections: AppConnections,
        secrets: SharedSecretStore,
        approver: Arc<dyn ProviderApprover>,
    ) -> Result<Arc<NativeProviderBinding>, BindError> {
        let mut current = self.binding.write().map_err(|_| BindError::Lock)?;
        if let Some(binding) = current.as_ref() {
            if !Arc::ptr_eq(&binding.secrets, &secrets)
                || !binding.approver.same_consent(approver.as_ref())
            {
                return Err(BindError::VaultOrConsentChanged);
            }
            if !binding.approver.same_actor(approver.as_ref())
                || binding.connections.app_id() != connections.app_id()
                || binding
                    .connections
                    .export()
                    .map_err(BindError::Connection)?
                    != connections.export().map_err(BindError::Connection)?
            {
                return Err(BindError::AuthorityDiffers);
            }
            return Ok(binding.clone());
        }
        let network = Arc::new(
            ReqwestConnectionNetwork::new(Duration::from_secs(120), 4 * 1024 * 1024)
                .map_err(BindError::Connection)?,
        );
        let binding = Arc::new(NativeProviderBinding {
            native: NativeConnections::new(connections.clone(), secrets.clone(), network.clone()),
            connections,
            secrets,
            network,
            approver,
        });
        *current = Some(binding.clone());
        Ok(binding)
    }
}

#[async_trait]
impl agentkit_sessions::ChatProviderFactory for NativeProviderFactory {
    fn available(&self) -> bool {
        self.binding.read().is_ok_and(|binding| binding.is_some())
    }

    async fn snapshot(
        &self,
        provider_id: &str,
        model: &str,
        cancel: CancellationToken,
    ) -> agentkit_sessions::HostResult<Option<Value>> {
        if cancel.is_cancelled() {
            return Err(agentkit_sessions::HostError::Execution("cancelled".into()));
        }
        let binding = self
            .binding
            .read()
            .map_err(|_| {
                agentkit_sessions::HostError::Execution("provider_authority_unavailable".into())
            })?
            .clone();
        let Some(binding) = binding else {
            return self.static_snapshot(provider_id);
        };
        let id = agentkit_connections::ConnectionId::new(provider_id)
            .map_err(|_| agentkit_sessions::HostError::Execution("invalid_provider_id".into()))?;
        let selected = match binding.connections.get(&id) {
            Ok(connection) => connection,
            Err(agentkit_connections::ConnectionError::NotFound) => {
                return self.static_snapshot(provider_id);
            }
            Err(_) => {
                return Err(agentkit_sessions::HostError::Execution(
                    "connection_changed".into(),
                ));
            }
        };
        let snapshot = json!({"connection":selected,"model":model});
        let protocol = protocol(&snapshot, &selected)?;
        let capability = if protocol == "responses" {
            Capability::Responses
        } else {
            Capability::Chat
        };
        validate_selection(&selected, model, &capability)?;
        Ok(Some(snapshot))
    }

    async fn resolve(
        &self,
        snapshot: &Value,
        cancel: CancellationToken,
    ) -> agentkit_sessions::HostResult<Arc<dyn ChatProvider>> {
        if cancel.is_cancelled() {
            return Err(agentkit_sessions::HostError::Execution("cancelled".into()));
        }
        let binding = self
            .binding
            .read()
            .map_err(|_| {
                agentkit_sessions::HostError::Execution("provider_authority_unavailable".into())
            })?
            .clone()
            .ok_or_else(|| {
                agentkit_sessions::HostError::Execution("provider_authority_unavailable".into())
            })?;
        let selected: Connection =
            serde_json::from_value(snapshot["connection"].clone()).map_err(|_| {
                agentkit_sessions::HostError::Execution("invalid_provider_snapshot".into())
            })?;
        let current = binding
            .connections
            .get(&selected.id)
            .map_err(|_| agentkit_sessions::HostError::Execution("connection_changed".into()))?;
        let model = snapshot["model"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| agentkit_sessions::HostError::Execution("unsupported_model".into()))?;
        if current != selected || !current.config.enabled {
            return Err(agentkit_sessions::HostError::Execution(
                "connection_changed".into(),
            ));
        }
        let protocol = protocol(snapshot, &selected)?;
        let capability = if protocol == "responses" {
            Capability::Responses
        } else {
            Capability::Chat
        };
        validate_selection(&selected, model, &capability)?;
        let transport: Arc<dyn TrustedTransport> = Arc::new(ApprovingTransport {
            identity: ConnectionIdentity {
                provider_id: selected.id.as_str().into(),
                protocol: protocol.into(),
                connection_id: selected.id.as_str().into(),
                generation: selected.generation,
            },
            binding,
            selected,
            capability,
            model: model.into(),
            approval: Mutex::new(None),
        });
        if protocol == "responses" {
            ResponsesProvider::new(transport)
                .map(|provider| Arc::new(provider) as Arc<dyn ChatProvider>)
        } else {
            ChatCompletionsProvider::new(transport)
                .map(|provider| Arc::new(provider) as Arc<dyn ChatProvider>)
        }
        .map_err(|error| agentkit_sessions::HostError::Execution(error.code))
    }
}

impl NativeProviderFactory {
    fn static_snapshot(&self, provider_id: &str) -> agentkit_sessions::HostResult<Option<Value>> {
        if self.static_provider_ids.contains(provider_id) {
            Ok(None)
        } else {
            Err(agentkit_sessions::HostError::Execution(
                "unknown_provider".into(),
            ))
        }
    }
}

fn validate_selection(
    selected: &Connection,
    model: &str,
    capability: &Capability,
) -> agentkit_sessions::HostResult<()> {
    if !selected.config.enabled {
        return Err(agentkit_sessions::HostError::Execution(
            "connection_changed".into(),
        ));
    }
    if !selected.config.capabilities.contains(capability)
        || !selected
            .config
            .models
            .iter()
            .any(|item| item.id == model && item.capabilities.contains(capability))
    {
        return Err(agentkit_sessions::HostError::Execution(
            "unsupported_model".into(),
        ));
    }
    Ok(())
}

fn protocol<'a>(
    snapshot: &'a Value,
    selected: &Connection,
) -> agentkit_sessions::HostResult<&'a str> {
    match snapshot.get("protocol").and_then(Value::as_str) {
        Some(value @ ("responses" | "chat-completions")) => Ok(value),
        Some(_) => Err(agentkit_sessions::HostError::Execution(
            "unsupported_capability".into(),
        )),
        None if selected
            .config
            .capabilities
            .contains(&Capability::Responses)
            && !selected.config.capabilities.contains(&Capability::Chat) =>
        {
            Ok("responses")
        }
        None => Ok("chat-completions"),
    }
}

struct ApprovingTransport {
    identity: ConnectionIdentity,
    binding: Arc<NativeProviderBinding>,
    selected: Connection,
    capability: Capability,
    model: String,
    approval: Mutex<Option<(Approval, RequestFence)>>,
}

#[async_trait]
impl TrustedTransport for ApprovingTransport {
    fn identity(&self) -> ConnectionIdentity {
        self.identity.clone()
    }
    fn auth_kind(&self) -> AuthKind {
        AuthKind::ApiKey
    }
    fn check_current(&self) -> Result<(), CoreError> {
        let current = self
            .binding
            .connections
            .get(&self.selected.id)
            .map_err(|_| CoreError::responses("connection_changed"))?;
        if current != self.selected {
            return Err(CoreError::responses("connection_changed"));
        }
        if let Some((_, fence)) = self
            .approval
            .lock()
            .map_err(|_| CoreError::responses("connection_changed"))?
            .as_ref()
        {
            fence
                .check()
                .map_err(|_| CoreError::responses("connection_changed"))?;
        }
        Ok(())
    }

    async fn request(
        &self,
        identity: &ConnectionIdentity,
        operation: &str,
        body: Option<Value>,
        cancel: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        self.request_guarded(identity, operation, body, None, cancel)
            .await
    }

    async fn request_guarded(
        &self,
        identity: &ConnectionIdentity,
        operation: &str,
        body: Option<Value>,
        guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
        cancel: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        self.check_current()?;
        let expected = if self.capability == Capability::Responses {
            "responses"
        } else {
            "chat/completions"
        };
        if identity != &self.identity || operation != expected {
            return Err(CoreError::responses("connection_changed"));
        }
        let body = body.ok_or_else(|| CoreError::responses("invalid_input"))?;
        if body["model"].as_str() != Some(&self.model) {
            return Err(CoreError::responses("unsupported_model"));
        }
        let mut destination = self.selected.config.endpoint.clone();
        destination.set_path(&format!(
            "{}/{}",
            destination.path().trim_end_matches('/'),
            expected
        ));
        let mut disclosed_data = disclosed_categories(&body);
        disclosed_data.insert("connection-authentication".into());
        let plan = self
            .binding
            .native
            .plan_to(
                &self.selected.id,
                PlanInput {
                    capability: self.capability.clone(),
                    model: Some(self.model.clone()),
                    disclosed_data,
                    payload: agentkit_contracts::stringify_javascript_value(&body)
                        .map_err(|_| CoreError::responses("invalid_input"))?
                        .into_bytes(),
                },
                destination,
            )
            .map_err(|_| CoreError::responses("connection_changed"))?;
        let approval = tokio::select! { biased;
            _=cancel.cancelled()=>return Err(CoreError::new("cancelled", "aborted")),
            value=self.binding.approver.approve(&self.binding.native, &plan, cancel.clone())=>value.map_err(|_| CoreError::responses("consent_required"))?,
        };
        let fence = self
            .binding
            .native
            .fence(&plan, &approval.token)
            .map_err(|_| CoreError::responses("connection_changed"))?;
        let transport = NativeConnectionTransport::new(
            self.binding.native.clone(),
            plan,
            approval.token.clone(),
            self.binding.network.clone(),
        )?
        .with_request_guard(guard);
        *self
            .approval
            .lock()
            .map_err(|_| CoreError::responses("connection_changed"))? = Some((approval, fence));
        transport
            .request(identity, operation, Some(body), cancel)
            .await
    }
}
