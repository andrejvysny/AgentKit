//! Explicit host approval connects native discovery and probes to HTTP operations.
use super::{DesktopBackend, ProviderOperations, native_provider_binding};
use crate::{Actor, Problem};
use agentkit_connections::{ConnectionError, NativeConnections, RequestPlan};
use agentkit_contracts::{AiProviderModel, TestProviderResponse};
use agentkit_providers::{Approval, NativeProviderFactory};
use async_trait::async_trait;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait ProviderConsentPort: Send + Sync {
    /// Approve this exact destination, generation, model, payload and disclosed categories.
    async fn approve(
        &self,
        actor: &Actor,
        plan: &RequestPlan,
        cancel: CancellationToken,
    ) -> Result<bool, Problem>;
}

pub struct NativeProviderOperations {
    native: NativeConnections,
    consent: Arc<dyn ProviderConsentPort>,
    runtime: agentkit_engine::SharedRuntime,
    timeout: Duration,
}

impl NativeProviderOperations {
    pub fn new(
        native: NativeConnections,
        consent: Arc<dyn ProviderConsentPort>,
        runtime: agentkit_engine::SharedRuntime,
    ) -> Self {
        Self {
            native,
            consent,
            runtime,
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, Problem> {
        if timeout.is_zero() {
            return Err(Problem::new(
                400,
                "invalid_request",
                "Provider timeout must be positive.",
            ));
        }
        self.timeout = timeout;
        Ok(self)
    }

    async fn approve(&self, actor: &Actor, plan: &RequestPlan) -> Result<Approval, Problem> {
        approve(
            &self.native,
            self.consent.as_ref(),
            actor,
            plan,
            CancellationToken::new(),
        )
        .await
    }
}

pub(super) async fn approve(
    native: &NativeConnections,
    port: &dyn ProviderConsentPort,
    actor: &Actor,
    plan: &RequestPlan,
    cancel: CancellationToken,
) -> Result<Approval, Problem> {
    let child = cancel.child_token();
    let _cancel_on_drop = CancelApproval(child.clone());
    let accepted = tokio::select! { biased;
        _ = cancel.cancelled() => return Err(consent_timeout()),
        result = tokio::time::timeout(Duration::from_secs(30), port.approve(actor, plan, child)) => result.map_err(|_| consent_timeout())??,
    };
    if !accepted {
        return Err(Problem::new(
            403,
            "consent_required",
            "Provider request was not approved.",
        ));
    }
    let token = native.approve(plan).map_err(connection_problem)?;
    Ok(Approval::new(native.clone(), token))
}

impl NativeProviderOperations {
    async fn refresh_native(
        &self,
        actor: &Actor,
        provider_id: &str,
    ) -> Result<Vec<AiProviderModel>, Problem> {
        let id =
            agentkit_connections::ConnectionId::new(provider_id).map_err(connection_problem)?;
        let plan = self
            .native
            .plan_discovery(&id)
            .map_err(connection_problem)?;
        let approval = self.approve(actor, &plan).await?;
        let models = self
            .native
            .discover_models(&plan, &approval.token)
            .await
            .map_err(connection_problem)?;
        let fetched_at =
            agentkit_store::ConversationTimestamp::from_epoch_millis(self.runtime.now_ms())
                .map_err(|_| {
                    Problem::host("internal_error", "Invalid provider discovery timestamp.")
                })?
                .wire_text()
                .map_err(|_| {
                    Problem::host("internal_error", "Invalid provider discovery timestamp.")
                })?
                .into_owned();
        models.into_iter().map(|model| {
            let mut value = json!({"providerId":provider_id,"modelId":model.id,"displayName":model.display_name,"fetchedAt":fetched_at});
            if let Some(tokens) = model.context_window_tokens { value["contextWindowTokens"] = json!(tokens); }
            serde_json::from_value(value).map_err(|_| Problem::host("internal_error", "Invalid discovered model projection."))
        }).collect()
    }

    async fn test_native(
        &self,
        actor: &Actor,
        provider_id: &str,
    ) -> Result<TestProviderResponse, Problem> {
        let id =
            agentkit_connections::ConnectionId::new(provider_id).map_err(connection_problem)?;
        let plan = self.native.plan_probe(&id).map_err(connection_problem)?;
        let approval = self.approve(actor, &plan).await?;
        let result = self
            .native
            .probe(&plan, &approval.token)
            .await
            .map_err(connection_problem)?;
        let mut value = json!({"ok":result.reachable});
        if !result.reachable {
            value["error"] = json!(format!("Provider returned HTTP {}.", result.http_status));
        }
        serde_json::from_value(value)
            .map_err(|_| Problem::host("internal_error", "Invalid provider probe projection."))
    }
}

struct CancelApproval(CancellationToken);
impl Drop for CancelApproval {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
fn consent_timeout() -> Problem {
    Problem::new(
        504,
        "consent_unavailable",
        "Provider consent timed out or was cancelled.",
    )
}

#[async_trait]
impl ProviderOperations for NativeProviderOperations {
    async fn refresh_models(
        &self,
        actor: &Actor,
        id: &str,
    ) -> Result<Vec<AiProviderModel>, Problem> {
        tokio::time::timeout(self.timeout, self.refresh_native(actor, id))
            .await
            .map_err(|_| {
                Problem::new(
                    504,
                    "provider_operation_timeout",
                    "Provider operation timed out.",
                )
            })?
    }
    async fn test(&self, actor: &Actor, id: &str) -> Result<TestProviderResponse, Problem> {
        tokio::time::timeout(self.timeout, self.test_native(actor, id))
            .await
            .map_err(|_| {
                Problem::new(
                    504,
                    "provider_operation_timeout",
                    "Provider operation timed out.",
                )
            })?
    }
}

impl DesktopBackend {
    /// Both management and execution share the same restored authority and explicit vault.
    pub fn with_native_provider_operations(
        mut self,
        consent: Arc<dyn ProviderConsentPort>,
        factory: Arc<NativeProviderFactory>,
    ) -> Result<Self, Problem> {
        let has_vault = self.secrets.is_some();
        let secrets = self
            .secrets
            .clone()
            .unwrap_or_else(|| Arc::new(agentkit_connections::UnavailableSecretStore));
        let binding = native_provider_binding::bind(
            &factory,
            self.connections.clone(),
            secrets,
            consent.clone(),
            self.actor.clone(),
        )?;
        self.provider_factory = Some(factory);
        self.connections = binding.connections.clone();
        self.native_connections = Some(Arc::new(binding.native.clone()));
        self.secrets = has_vault.then(|| binding.secrets.clone());
        self.provider_operations = Some(Arc::new(NativeProviderOperations::new(
            binding.native.clone(),
            consent,
            self.host.runtime().clone(),
        )));
        Ok(self)
    }
}

pub(super) fn connection_problem(error: ConnectionError) -> Problem {
    match error {
        ConnectionError::ConsentDenied => Problem::new(403, "consent_required", error.to_string()),
        ConnectionError::SecretUnavailable | ConnectionError::SecretMissing => {
            Problem::new(503, "credential_store_unavailable", error.to_string())
        }
        ConnectionError::NetworkUnavailable => {
            Problem::new(501, "optional_dependency_unavailable", error.to_string())
        }
        ConnectionError::NetworkFailed => {
            Problem::new(502, "provider_request_failed", error.to_string())
        }
        other => super::providers::connection_error(other),
    }
}
