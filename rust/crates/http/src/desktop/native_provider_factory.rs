//! Late-bound native execution keeps HTTP connection writes and streaming on one authority.
use super::{
    ProviderConsentPort,
    native_provider_operations::{Approval, approve, connection_problem},
};
use crate::{Actor, Problem};
use agentkit_connections::{
    AppConnections, Capability, Connection, NativeConnections, PlanInput, RequestFence,
    ReqwestConnectionNetwork, SharedSecretStore,
};
use agentkit_core::{
    ChatProvider, CoreError,
    providers::{
        AuthKind, ChatCompletionsProvider, ConnectionIdentity, ResponsesProvider, TrustedTransport,
    },
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct NativeProviderFactory {
    static_provider_ids: BTreeSet<String>,
    binding: RwLock<Option<Arc<NativeProviderBinding>>>,
}

pub(super) struct NativeProviderBinding {
    pub connections: AppConnections,
    pub native: NativeConnections,
    pub secrets: SharedSecretStore,
    network: Arc<ReqwestConnectionNetwork>,
    consent: Arc<dyn ProviderConsentPort>,
    actor: Actor,
}

impl NativeProviderFactory {
    pub fn with_static_provider_ids(mut self, ids: BTreeSet<String>) -> Self {
        self.static_provider_ids = ids;
        self
    }

    pub(super) fn bind(
        &self,
        connections: AppConnections,
        secrets: SharedSecretStore,
        consent: Arc<dyn ProviderConsentPort>,
        actor: Actor,
    ) -> Result<Arc<NativeProviderBinding>, Problem> {
        let mut current = self
            .binding
            .write()
            .map_err(|_| Problem::host("internal_error", "Provider authority lock failed."))?;
        if let Some(binding) = current.as_ref() {
            if !Arc::ptr_eq(&binding.secrets, &secrets) || !Arc::ptr_eq(&binding.consent, &consent)
            {
                return Err(Problem::new(
                    409,
                    "revision_conflict",
                    "Provider vault or consent changed. Create a new service or reuse the same port handles.",
                ));
            }
            if binding.actor.id != actor.id
                || binding.actor.metadata != actor.metadata
                || binding.connections.app_id() != connections.app_id()
                || binding.connections.export().map_err(connection_problem)?
                    != connections.export().map_err(connection_problem)?
            {
                return Err(Problem::new(
                    409,
                    "revision_conflict",
                    "Provider authority differs from the restored store.",
                ));
            }
            return Ok(binding.clone());
        }
        let network = Arc::new(
            ReqwestConnectionNetwork::new(Duration::from_secs(120), 4 * 1024 * 1024)
                .map_err(connection_problem)?,
        );
        let binding = Arc::new(NativeProviderBinding {
            native: NativeConnections::new(connections.clone(), secrets.clone(), network.clone()),
            connections,
            secrets,
            network,
            consent,
            actor,
        });
        *current = Some(binding.clone());
        Ok(binding)
    }
}

#[async_trait]
impl agentkit_host::ChatProviderFactory for NativeProviderFactory {
    fn available(&self) -> bool {
        self.binding.read().is_ok_and(|binding| binding.is_some())
    }

    async fn snapshot(
        &self,
        provider_id: &str,
        model: &str,
        cancel: CancellationToken,
    ) -> agentkit_host::HostResult<Option<Value>> {
        if cancel.is_cancelled() {
            return Err(agentkit_host::HostError::Execution("cancelled".into()));
        }
        let binding = self
            .binding
            .read()
            .map_err(|_| {
                agentkit_host::HostError::Execution("provider_authority_unavailable".into())
            })?
            .clone();
        let Some(binding) = binding else {
            return self.static_snapshot(provider_id);
        };
        let id = agentkit_connections::ConnectionId::new(provider_id)
            .map_err(|_| agentkit_host::HostError::Execution("invalid_provider_id".into()))?;
        let selected = match binding.connections.get(&id) {
            Ok(connection) => connection,
            Err(agentkit_connections::ConnectionError::NotFound) => {
                return self.static_snapshot(provider_id);
            }
            Err(_) => {
                return Err(agentkit_host::HostError::Execution(
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
    ) -> agentkit_host::HostResult<Arc<dyn ChatProvider>> {
        if cancel.is_cancelled() {
            return Err(agentkit_host::HostError::Execution("cancelled".into()));
        }
        let binding = self
            .binding
            .read()
            .map_err(|_| {
                agentkit_host::HostError::Execution("provider_authority_unavailable".into())
            })?
            .clone()
            .ok_or_else(|| {
                agentkit_host::HostError::Execution("provider_authority_unavailable".into())
            })?;
        let selected: Connection = serde_json::from_value(snapshot["connection"].clone())
            .map_err(|_| agentkit_host::HostError::Execution("invalid_provider_snapshot".into()))?;
        let current = binding
            .connections
            .get(&selected.id)
            .map_err(|_| agentkit_host::HostError::Execution("connection_changed".into()))?;
        let model = snapshot["model"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| agentkit_host::HostError::Execution("unsupported_model".into()))?;
        if current != selected || !current.config.enabled {
            return Err(agentkit_host::HostError::Execution(
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
        .map_err(|error| agentkit_host::HostError::Execution(error.code))
    }
}

impl NativeProviderFactory {
    fn static_snapshot(&self, provider_id: &str) -> agentkit_host::HostResult<Option<Value>> {
        if self.static_provider_ids.contains(provider_id) {
            Ok(None)
        } else {
            Err(agentkit_host::HostError::Execution(
                "unknown_provider".into(),
            ))
        }
    }
}

fn validate_selection(
    selected: &Connection,
    model: &str,
    capability: &Capability,
) -> agentkit_host::HostResult<()> {
    if !selected.config.enabled {
        return Err(agentkit_host::HostError::Execution(
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
        return Err(agentkit_host::HostError::Execution(
            "unsupported_model".into(),
        ));
    }
    Ok(())
}

fn protocol<'a>(snapshot: &'a Value, selected: &Connection) -> agentkit_host::HostResult<&'a str> {
    match snapshot.get("protocol").and_then(Value::as_str) {
        Some(value @ ("responses" | "chat-completions")) => Ok(value),
        Some(_) => Err(agentkit_host::HostError::Execution(
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
        let mut disclosed_data = agentkit_host::disclosed_categories(&body);
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
            value=approve(&self.binding.native, self.binding.consent.as_ref(), &self.binding.actor, &plan, cancel.clone())=>value.map_err(|_| CoreError::responses("consent_required"))?,
        };
        let fence = self
            .binding
            .native
            .fence(&plan, &approval.token)
            .map_err(|_| CoreError::responses("connection_changed"))?;
        let transport = agentkit_host::NativeConnectionTransport::new(
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

#[cfg(test)]
mod tests {
    use super::*;
    struct Consent;
    #[async_trait]
    impl ProviderConsentPort for Consent {
        async fn approve(
            &self,
            _: &Actor,
            _: &agentkit_connections::RequestPlan,
            _: CancellationToken,
        ) -> Result<bool, Problem> {
            Ok(false)
        }
    }
    #[test]
    fn rebind_requires_identical_vault_and_consent_handles() {
        let factory = NativeProviderFactory::default();
        let connections = agentkit_connections::ConnectionRepository::default()
            .for_app(agentkit_connections::AppId::new("app").unwrap());
        let secrets: SharedSecretStore =
            Arc::new(agentkit_connections::MemorySecretStore::default());
        let consent: Arc<dyn ProviderConsentPort> = Arc::new(Consent);
        let actor = Actor {
            id: "app".into(),
            metadata: Default::default(),
        };
        factory
            .bind(
                connections.clone(),
                secrets.clone(),
                consent.clone(),
                actor.clone(),
            )
            .unwrap();
        factory
            .bind(
                connections.clone(),
                secrets.clone(),
                consent.clone(),
                actor.clone(),
            )
            .unwrap();
        let error = factory
            .bind(
                connections.clone(),
                Arc::new(agentkit_connections::MemorySecretStore::default()),
                consent,
                actor.clone(),
            )
            .err()
            .unwrap();
        assert_eq!(error.status, 409);
        let error = factory
            .bind(connections, secrets, Arc::new(Consent), actor)
            .err()
            .unwrap();
        assert_eq!(error.status, 409);
    }
}
