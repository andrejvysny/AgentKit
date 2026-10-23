//! Consent-bound bridge from native connections to the provider streaming port.
use agentkit_connections::{
    Capability, ConnectionError, ConnectionNetwork, ConsentToken, NativeConnections, PlanInput,
    RequestFence, RequestPlan, ReqwestConnectionNetwork,
};
use agentkit_core::{
    CoreError,
    providers::{AuthKind, ConnectionIdentity, TrustedTransport},
};
use async_trait::async_trait;
use serde_json::Value;
use std::{collections::BTreeSet, sync::Arc};
use tokio_util::sync::CancellationToken;

/// Selection is immutable. Account OAuth requires a separately qualified adapter.
pub struct NativeConnectionTransport {
    native: NativeConnections,
    template: RequestPlan,
    consent: ConsentToken,
    network: Arc<ReqwestConnectionNetwork>,
    identity: ConnectionIdentity,
    fence: RequestFence,
}

impl NativeConnectionTransport {
    pub fn new(
        native: NativeConnections,
        approved_plan: RequestPlan,
        consent: ConsentToken,
        network: Arc<ReqwestConnectionNetwork>,
    ) -> Result<Self, CoreError> {
        let protocol = match approved_plan.capability() {
            Capability::Chat => "chat-completions",
            Capability::Responses => "responses",
            _ => return Err(connection_error(ConnectionError::UnsupportedCapability)),
        };
        if approved_plan.model().is_none() {
            return Err(connection_error(ConnectionError::UnsupportedModel));
        }
        let connection = approved_plan.connection();
        let identity = ConnectionIdentity {
            provider_id: connection.id.as_str().into(),
            protocol: protocol.into(),
            connection_id: connection.id.as_str().into(),
            generation: connection.generation,
        };
        let fence = native
            .fence(&approved_plan, &consent)
            .map_err(connection_error)?;
        Ok(Self {
            native,
            template: approved_plan,
            consent,
            network,
            identity,
            fence,
        })
    }

    fn prepare(&self, operation: &str, body: Option<Value>) -> Result<RequestPlan, CoreError> {
        let expected = match self.template.capability() {
            Capability::Chat => "chat/completions",
            Capability::Responses => "responses",
            _ => return Err(connection_error(ConnectionError::UnsupportedCapability)),
        };
        if operation != expected {
            return Err(CoreError::new(
                "consent_required",
                "Operation requires separate consent",
            ));
        }
        let body = body
            .ok_or_else(|| CoreError::new("invalid_request", "Provider request body required"))?;
        if body.get("model").and_then(Value::as_str) != self.template.model() {
            return Err(connection_error(ConnectionError::UnsupportedModel));
        }
        if !disclosed_categories(&body).is_subset(self.template.disclosed_data()) {
            return Err(CoreError::new(
                "consent_required",
                "Request contains undisclosed data categories",
            ));
        }
        let payload = agentkit_contracts::stringify_javascript_value(&body)
            .map(String::into_bytes)
            .map_err(|_| CoreError::new("invalid_request", "Invalid provider request"))?;
        if payload == self.template.payload() {
            return Ok(self.template.clone());
        }
        let input = PlanInput {
            capability: self.template.capability().clone(),
            model: self.template.model().map(str::to_owned),
            disclosed_data: self.template.disclosed_data().clone(),
            payload,
        };
        self.native
            .plan_to(
                &self.template.connection().id,
                input,
                self.template.destination().clone(),
            )
            .map_err(connection_error)
    }
}

#[async_trait]
impl TrustedTransport for NativeConnectionTransport {
    fn identity(&self) -> ConnectionIdentity {
        self.identity.clone()
    }
    fn auth_kind(&self) -> AuthKind {
        AuthKind::ApiKey
    }
    fn check_current(&self) -> Result<(), CoreError> {
        self.fence.check().map_err(connection_error)
    }

    async fn request(
        &self,
        identity: &ConnectionIdentity,
        operation: &str,
        body: Option<Value>,
        cancellation: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        self.check_current()?;
        if identity != &self.identity {
            return Err(CoreError::new(
                "connection_changed",
                "Provider connection changed",
            ));
        }
        let plan = self.prepare(operation, body)?;
        self.network
            .check_available(&plan)
            .map_err(connection_error)?;
        let network = self.network.clone();
        tokio::select! { biased;
            _ = cancellation.cancelled() => Err(CoreError::new("cancelled", "aborted")),
            response = self.native.execute_resolved_with(&plan, &self.consent, move |plan, secrets| {
                Box::pin(async move { network.send_streaming_resolved(plan, secrets).await })
            }) => response.map_err(connection_error),
        }
    }
}

/// Category names are public consent vocabulary shared with native approval UI.
pub fn disclosed_categories(body: &Value) -> BTreeSet<String> {
    let mut categories = BTreeSet::new();
    if body.get("messages").is_some() || body.get("input").is_some() {
        categories.insert("messages".into());
    }
    if body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
    {
        categories.insert("tool-definitions".into());
    }
    if body.get("metadata").is_some() {
        categories.insert("metadata".into());
    }
    if body.get("previous_response_id").is_some() {
        categories.insert("provider-continuation".into());
    }
    collect_content_categories(body, &mut categories);
    categories
}

fn collect_content_categories(value: &Value, categories: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            let kind = object.get("type").and_then(Value::as_str);
            if matches!(kind, Some("image_url" | "input_image")) {
                categories.insert("images".into());
            }
            if matches!(kind, Some("reasoning" | "item_reference"))
                || object.contains_key("encrypted_content")
            {
                categories.insert("provider-continuation".into());
            }
            if kind == Some("function_call_output")
                || object.get("role").and_then(Value::as_str) == Some("tool")
            {
                categories.insert("tool-results".into());
            }
            for child in object.values() {
                collect_content_categories(child, categories);
            }
        }
        Value::Array(array) => {
            for child in array {
                collect_content_categories(child, categories);
            }
        }
        _ => {}
    }
}

fn connection_error(error: ConnectionError) -> CoreError {
    let code = match error {
        ConnectionError::ConsentDenied => "consent_required",
        ConnectionError::StaleGeneration | ConnectionError::NotFound => "connection_changed",
        ConnectionError::SecretMissing => "authentication_required",
        ConnectionError::SecretUnavailable => "credential_store_unavailable",
        ConnectionError::NetworkUnavailable => "adapter_unavailable",
        ConnectionError::UnsupportedModel => "unsupported_model",
        ConnectionError::UnsupportedCapability => "unsupported_capability",
        _ => "connection_error",
    };
    CoreError::new(code, &error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_connections::{
        AppId, ConnectionConfig, ConnectionId, ConnectionRepository, MemorySecretStore, Model,
    };
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn categories_include_tools_images_results_and_continuation() {
        let body = json!({"input":[{"type":"function_call_output","output":"result"},
            {"type":"input_image","image_url":"image"}, {"type":"reasoning","encrypted_content":"state"}],
            "tools":[{"name":"tool"}],"metadata":{"tag":"value"}});
        assert_eq!(
            disclosed_categories(&body),
            BTreeSet::from(
                [
                    "messages",
                    "tool-definitions",
                    "tool-results",
                    "images",
                    "provider-continuation",
                    "metadata"
                ]
                .map(str::to_owned)
            )
        );
    }

    fn configuration() -> ConnectionConfig {
        ConnectionConfig {
            label: "Test".into(),
            provider_kind: "openai-compatible".into(),
            endpoint: "http://127.0.0.1:1/v1".parse().unwrap(),
            account: None,
            credential_required: false,
            extra_header_names: BTreeSet::new(),
            allow_insecure_http: false,
            enabled: true,
            capabilities: BTreeSet::from([Capability::Chat]),
            models: vec![Model {
                id: "model".into(),
                display_name: None,
                capabilities: BTreeSet::from([Capability::Chat]),
                context_window_tokens: None,
            }],
        }
    }

    fn bridge_fixture() -> (
        agentkit_connections::AppConnections,
        agentkit_connections::Connection,
        NativeConnectionTransport,
    ) {
        let repository = ConnectionRepository::default();
        let app = repository.for_app(AppId::new("host-tests").unwrap());
        let id = ConnectionId::new("provider").unwrap();
        let connection = app.create(id.clone(), configuration()).unwrap();
        let network =
            Arc::new(ReqwestConnectionNetwork::new(Duration::from_secs(1), 4096).unwrap());
        let native = NativeConnections::new(
            app.clone(),
            Arc::new(MemorySecretStore::default()),
            network.clone(),
        );
        let input = PlanInput {
            capability: Capability::Chat,
            model: Some("model".into()),
            disclosed_data: BTreeSet::from(["messages".into()]),
            payload: serde_json::to_vec(&json!({"model":"model","messages":[]})).unwrap(),
        };
        let plan = native
            .plan_to(
                &id,
                input,
                "http://127.0.0.1:1/v1/chat/completions".parse().unwrap(),
            )
            .unwrap();
        let consent = native.approve_scope(&plan).unwrap();
        let transport = NativeConnectionTransport::new(native, plan, consent, network).unwrap();
        (app, connection, transport)
    }

    #[test]
    fn bridge_uses_javascript_stringification_before_network_dispatch() {
        let (_, _, transport) = bridge_fixture();
        let body = agentkit_contracts::parse_javascript_json(
            r#"{"model":"model","messages":[{"role":"user","content":{"value":1e400}}]}"#,
        )
        .unwrap();
        let plan = transport.prepare("chat/completions", Some(body)).unwrap();
        let payload: Value = serde_json::from_slice(plan.payload()).unwrap();
        assert!(payload["messages"][0]["content"]["value"].is_null());
    }

    #[test]
    fn bridge_rejects_stale_connection_and_undeclared_categories() {
        let (app, connection, transport) = bridge_fixture();
        assert!(
            transport
                .prepare(
                    "chat/completions",
                    Some(json!({"model":"model","messages":[],"tools":[{}]}))
                )
                .is_err()
        );
        assert!(
            transport
                .prepare(
                    "chat/completions",
                    Some(json!({"model":"other","messages":[]}))
                )
                .is_err()
        );
        app.update(&connection.id, connection.generation, connection.config)
            .unwrap();
        assert_eq!(
            transport.check_current().unwrap_err().code,
            "connection_changed"
        );
    }
}
