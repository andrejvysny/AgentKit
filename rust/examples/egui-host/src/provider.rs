use crate::{
    bridge::UiSender,
    demo::{DemoProvider, approve, connection_error},
};
use agentkit_connections::*;
use agentkit_core::{
    CancellationToken, ChatProvider, CoreError,
    providers::{AuthKind, ChatCompletionsProvider, ConnectionIdentity, TrustedTransport},
};
use agentkit_host::NativeConnectionTransport;
use async_trait::async_trait;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};

type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct ProviderSelection {
    pub provider: Arc<dyn ChatProvider>,
    pub models: Vec<String>,
    pub label: String,
    pub binding: Value,
}

pub fn select(ui: UiSender) -> Result<ProviderSelection, Error> {
    let Some(endpoint) = std::env::var_os("AGENTKIT_PROVIDER_URL") else {
        return offline(ui);
    };
    let endpoint = endpoint
        .into_string()
        .map_err(|_| "AGENTKIT_PROVIDER_URL must be UTF-8")?;
    let model = std::env::var("AGENTKIT_PROVIDER_MODEL")
        .map_err(|_| "AGENTKIT_PROVIDER_MODEL required for a live provider")?;
    let repository =
        ConnectionRepository::default().for_app(AppId::new("agentkit-egui-reference")?);
    let secret_env = std::env::var("AGENTKIT_PROVIDER_KEY_ENV").ok();
    let connection = repository.create(
        ConnectionId::new("configured-provider")?,
        config(&endpoint, &model, secret_env.is_some())?,
    )?;
    let binding = serde_json::json!({"provider":connection.provider(&model)?,"credentialEnvironment":secret_env});
    let transport = transport(connection, repository, secret_env, ui)?;
    Ok(ProviderSelection {
        provider: Arc::new(ChatCompletionsProvider::new(Arc::new(transport))?),
        models: vec![model],
        label: "Live OpenAI-compatible provider — exact approval per request".into(),
        binding,
    })
}

fn config(
    endpoint: &str,
    model: &str,
    credential_required: bool,
) -> Result<ConnectionConfig, Error> {
    let capabilities = BTreeSet::from([Capability::Chat]);
    Ok(ConnectionConfig {
        label: "Configured OpenAI-compatible provider".into(),
        provider_kind: "openai-compatible".into(),
        endpoint: endpoint.parse()?,
        account: None,
        credential_required,
        allow_insecure_http: std::env::var("AGENTKIT_ALLOW_INSECURE_HTTP").as_deref() == Ok("1"),
        extra_header_names: BTreeSet::new(),
        enabled: true,
        capabilities: capabilities.clone(),
        models: vec![Model {
            id: model.into(),
            display_name: None,
            capabilities,
            context_window_tokens: None,
        }],
    })
}

fn transport(
    connection: Connection,
    repository: AppConnections,
    secret_env: Option<String>,
    ui: UiSender,
) -> Result<UiTransport, Error> {
    let network = Arc::new(ReqwestConnectionNetwork::new(
        Duration::from_secs(120),
        1024 * 1024,
    )?);
    let temporary = NativeConnections::new(
        repository.clone(),
        Arc::new(MemorySecretStore::default()),
        network.clone(),
    );
    let secrets: SharedSecretStore = if let Some(name) = secret_env {
        Arc::new(EnvironmentSecretStore::new(BTreeMap::from([(
            temporary.credential_key(&connection.id)?,
            name,
        )])))
    } else {
        Arc::new(MemorySecretStore::default())
    };
    let identity = ConnectionIdentity {
        provider_id: connection.id.as_str().into(),
        protocol: "chat-completions".into(),
        connection_id: connection.id.as_str().into(),
        generation: connection.generation,
    };
    Ok(UiTransport {
        native: NativeConnections::new(repository, secrets, network.clone()),
        id: connection.id,
        base: connection.config.endpoint,
        network,
        identity,
        ui,
        active_fence: Mutex::new(None),
    })
}

struct UiTransport {
    native: NativeConnections,
    id: ConnectionId,
    base: url::Url,
    network: Arc<ReqwestConnectionNetwork>,
    identity: ConnectionIdentity,
    ui: UiSender,
    active_fence: Mutex<Option<RequestFence>>,
}

impl UiTransport {
    fn prepare(&self, body: &Value) -> Result<RequestPlan, CoreError> {
        let model = body["model"]
            .as_str()
            .ok_or_else(|| CoreError::new("invalid_request", "Model required"))?
            .to_owned();
        let mut destination = self.base.clone();
        destination.set_path(&format!(
            "{}/chat/completions",
            self.base.path().trim_end_matches('/')
        ));
        self.native
            .plan_to(
                &self.id,
                PlanInput {
                    capability: Capability::Chat,
                    model: Some(model),
                    disclosed_data: agentkit_host::disclosed_categories(body),
                    payload: serde_json::to_vec(body).map_err(|_| {
                        CoreError::new("invalid_request", "Invalid provider payload")
                    })?,
                },
                destination,
            )
            .map_err(connection_error)
    }
}

#[async_trait]
impl TrustedTransport for UiTransport {
    fn identity(&self) -> ConnectionIdentity {
        self.identity.clone()
    }
    fn auth_kind(&self) -> AuthKind {
        AuthKind::ApiKey
    }
    fn check_current(&self) -> Result<(), CoreError> {
        let fence = self
            .active_fence
            .lock()
            .map_err(|_| CoreError::new("connection_error", "Connection lock failed"))?;
        if let Some(fence) = fence.as_ref() {
            fence.check().map_err(connection_error)?;
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
        if identity != &self.identity || operation != "chat/completions" {
            return Err(CoreError::new(
                "connection_changed",
                "Provider operation changed",
            ));
        }
        let body =
            body.ok_or_else(|| CoreError::new("invalid_request", "Provider body required"))?;
        let plan = self.prepare(&body)?;
        if !approve(&self.ui, &plan, false, cancel.clone()).await? {
            return Err(CoreError::new("consent_denied", "Request denied by user"));
        }
        let consent = self.native.approve(&plan).map_err(connection_error)?;
        let fence = self
            .native
            .fence(&plan, &consent)
            .map_err(connection_error)?;
        let transport = NativeConnectionTransport::new(
            self.native.clone(),
            plan,
            consent,
            self.network.clone(),
        )?;
        *self
            .active_fence
            .lock()
            .map_err(|_| CoreError::new("connection_error", "Connection lock failed"))? =
            Some(fence);
        transport
            .request(identity, operation, Some(body), cancel)
            .await
    }
}

pub fn offline(ui: UiSender) -> Result<ProviderSelection, Error> {
    Ok(ProviderSelection {
        provider: Arc::new(DemoProvider::new(ui)?),
        models: vec!["demo-echo".into(), "demo-slow".into()],
        label: "Offline demo — no network".into(),
        binding: serde_json::json!({"provider":"offline-demo","models":["demo-echo","demo-slow"]}),
    })
}
