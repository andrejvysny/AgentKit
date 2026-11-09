//! Adapts the HTTP consent port and actor to the providers crate's binding seam.
use super::{
    ProviderConsentPort,
    native_provider_operations::{approve, connection_problem},
};
use crate::{Actor, Problem};
use agentkit_connections::{AppConnections, NativeConnections, RequestPlan, SharedSecretStore};
use agentkit_core::CoreError;
use agentkit_providers::{
    Approval, BindError, NativeProviderBinding, NativeProviderFactory, ProviderApprover,
};
use async_trait::async_trait;
use std::any::Any;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct HttpApprover {
    consent: Arc<dyn ProviderConsentPort>,
    actor: Actor,
}

#[async_trait]
impl ProviderApprover for HttpApprover {
    async fn approve(
        &self,
        native: &NativeConnections,
        plan: &RequestPlan,
        cancel: CancellationToken,
    ) -> Result<Approval, CoreError> {
        approve(native, self.consent.as_ref(), &self.actor, plan, cancel)
            .await
            .map_err(|problem| CoreError::new(&problem.code, &problem.detail))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn same_consent(&self, other: &dyn ProviderApprover) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|other| Arc::ptr_eq(&self.consent, &other.consent))
    }

    fn same_actor(&self, other: &dyn ProviderApprover) -> bool {
        other.as_any().downcast_ref::<Self>().is_some_and(|other| {
            self.actor.id == other.actor.id && self.actor.metadata == other.actor.metadata
        })
    }
}

pub(super) fn bind(
    factory: &NativeProviderFactory,
    connections: AppConnections,
    secrets: SharedSecretStore,
    consent: Arc<dyn ProviderConsentPort>,
    actor: Actor,
) -> Result<Arc<NativeProviderBinding>, Problem> {
    factory
        .bind(connections, secrets, Arc::new(HttpApprover { consent, actor }))
        .map_err(|error| match error {
            BindError::Lock => Problem::host("internal_error", "Provider authority lock failed."),
            BindError::VaultOrConsentChanged => Problem::new(
                409,
                "revision_conflict",
                "Provider vault or consent changed. Create a new service or reuse the same port handles.",
            ),
            BindError::AuthorityDiffers => Problem::new(
                409,
                "revision_conflict",
                "Provider authority differs from the restored store.",
            ),
            BindError::Connection(error) => connection_problem(error),
        })
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
        bind(
            &factory,
            connections.clone(),
            secrets.clone(),
            consent.clone(),
            actor.clone(),
        )
        .unwrap();
        bind(
            &factory,
            connections.clone(),
            secrets.clone(),
            consent.clone(),
            actor.clone(),
        )
        .unwrap();
        let error = bind(
            &factory,
            connections.clone(),
            Arc::new(agentkit_connections::MemorySecretStore::default()),
            consent,
            actor.clone(),
        )
        .err()
        .unwrap();
        assert_eq!(error.status, 409);
        let error = bind(&factory, connections, secrets, Arc::new(Consent), actor)
            .err()
            .unwrap();
        assert_eq!(error.status, 409);
    }
}
