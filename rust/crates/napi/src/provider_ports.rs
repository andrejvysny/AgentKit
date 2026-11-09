//! Host-owned vault and consent callbacks. Callback failures never expose vault text.
use crate::jsbridge::{JsBridge, PendingKind};
use agentkit_connections::{ConnectionError, RequestPlan, Secret, SecretKey, SecretStore};
use agentkit_http::{Actor, Problem, ProviderConsentPort};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct BridgeProviderPorts {
    bridge: Arc<JsBridge>,
}

impl BridgeProviderPorts {
    pub(crate) fn new(bridge: Arc<JsBridge>) -> Self {
        Self { bridge }
    }

    async fn secret(&self, request: Value) -> Result<Value, ConnectionError> {
        self.bridge
            .clone()
            .request(
                PendingKind::ProviderSecret,
                &uuid::Uuid::new_v4().to_string(),
                &request,
                &agentkit_core::CancellationToken::new(),
            )
            .await
            .map_err(|_| ConnectionError::SecretUnavailable)
    }
}

#[async_trait]
impl SecretStore for BridgeProviderPorts {
    async fn get(&self, key: &SecretKey) -> Result<Secret, ConnectionError> {
        secret_value(
            self.secret(json!({"operation":"get","reference":key.reference()}))
                .await?,
        )
    }

    async fn put(&self, key: &SecretKey, secret: Secret) -> Result<(), ConnectionError> {
        secret_ack(
            self.secret(
                json!({"operation":"put","reference":key.reference(),"value":secret.expose()}),
            )
            .await?,
        )
    }

    async fn delete(&self, key: &SecretKey) -> Result<(), ConnectionError> {
        secret_ack(
            self.secret(json!({"operation":"delete","reference":key.reference()}))
                .await?,
        )
    }
}

fn secret_value(value: Value) -> Result<Secret, ConnectionError> {
    match value.get("value") {
        Some(Value::Null) => Err(ConnectionError::SecretMissing),
        Some(Value::String(secret)) if secret.len() <= 1024 * 1024 => Ok(Secret::new(secret)),
        _ => Err(ConnectionError::SecretUnavailable),
    }
}

fn secret_ack(value: Value) -> Result<(), ConnectionError> {
    if value.get("ok") == Some(&Value::Bool(true)) {
        Ok(())
    } else {
        Err(ConnectionError::SecretUnavailable)
    }
}

#[async_trait]
impl ProviderConsentPort for BridgeProviderPorts {
    async fn approve(
        &self,
        actor: &Actor,
        plan: &RequestPlan,
        cancel: agentkit_core::CancellationToken,
    ) -> Result<bool, Problem> {
        let selected = plan.connection();
        let request = json!({
            "actorId":actor.id,
            "connectionId":selected.id.as_str(),
            "generation":selected.generation,
            "account":selected.config.account.as_deref().unwrap_or(""),
            "providerKind":selected.config.provider_kind,
            "destination":plan.destination().as_str(),
            "capability":plan.capability(),
            "model":plan.model(),
            "disclosedData":plan.disclosed_data(),
            "payloadJson":String::from_utf8_lossy(plan.payload()),
        });
        let response = self
            .bridge
            .clone()
            .request(
                PendingKind::ProviderConsent,
                &uuid::Uuid::new_v4().to_string(),
                &request,
                &cancel,
            )
            .await
            .map_err(|_| {
                Problem::new(
                    503,
                    "consent_unavailable",
                    "Provider consent is unavailable.",
                )
            })?;
        response
            .get("approved")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                Problem::new(
                    503,
                    "consent_unavailable",
                    "Invalid provider consent response.",
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vault_responses_fail_closed_without_disclosing_callback_errors() {
        assert!(matches!(
            secret_value(json!({"value":null})),
            Err(ConnectionError::SecretMissing)
        ));
        for value in [
            json!({}),
            json!({"value":3}),
            json!({"error":"sensitive vault text"}),
        ] {
            assert!(matches!(
                secret_value(value),
                Err(ConnectionError::SecretUnavailable)
            ));
        }
        assert_eq!(
            secret_value(json!({"value":"synthetic"})).unwrap().expose(),
            "synthetic"
        );
        assert!(secret_ack(json!({"ok":true})).is_ok());
        assert_eq!(
            secret_ack(json!({"ok":false})).unwrap_err(),
            ConnectionError::SecretUnavailable
        );
    }
}
