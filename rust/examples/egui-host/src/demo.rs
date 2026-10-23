use crate::bridge::{ConsentPrompt, UiSender, Update};
use agentkit_connections::*;
use agentkit_core::{
    CancellationToken, ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream,
};
use async_trait::async_trait;
use serde_json::json;
use std::{collections::BTreeSet, sync::Arc, time::Duration};

pub struct DemoProvider {
    native: NativeConnections,
    id: ConnectionId,
    ui: UiSender,
}

impl DemoProvider {
    pub fn new(ui: UiSender) -> Result<Self, ConnectionError> {
        let repository =
            ConnectionRepository::default().for_app(AppId::new("agentkit-egui-reference")?);
        let id = ConnectionId::new("offline-demo")?;
        let capabilities = BTreeSet::from([Capability::Chat]);
        repository.create(
            id.clone(),
            ConnectionConfig {
                label: "In-process demo".into(),
                provider_kind: "offline-demo".into(),
                endpoint: "http://127.0.0.1:1/demo"
                    .parse()
                    .map_err(|_| ConnectionError::InvalidConfiguration)?,
                account: None,
                credential_required: false,
                allow_insecure_http: false,
                extra_header_names: BTreeSet::new(),
                enabled: true,
                capabilities: capabilities.clone(),
                models: ["demo-echo", "demo-slow"]
                    .map(|id| Model {
                        id: id.into(),
                        display_name: None,
                        capabilities: capabilities.clone(),
                        context_window_tokens: None,
                    })
                    .into(),
            },
        )?;
        let native = NativeConnections::new(
            repository,
            Arc::new(MemorySecretStore::default()),
            Arc::new(DemoNetwork),
        );
        Ok(Self { native, id, ui })
    }
}

#[async_trait]
impl ChatProvider for DemoProvider {
    fn id(&self) -> &str {
        "offline-demo"
    }

    fn stream_chat(&self, input: ChatRequest, cancel: CancellationToken) -> ProviderStream {
        let native = self.native.clone();
        let id = self.id.clone();
        let ui = self.ui.clone();
        Box::pin(async_stream::try_stream! {
            let plan = native.plan(&id, PlanInput { capability: Capability::Chat, model: Some(input.model.clone()),
                disclosed_data: BTreeSet::from(["messages".into()]),
                payload: serde_json::to_vec(&json!({"model":input.model,"messages":input.messages})).map_err(|_| CoreError::new("serialization_error","Could not serialize demo input"))?,
            }).map_err(connection_error)?;
            let approved = approve(&ui, &plan, true, cancel.clone()).await?;
            if !approved { Err(CoreError::new("consent_denied","Request denied by user"))?; }
            let token = native.approve(&plan).map_err(connection_error)?;
            let fence = native.fence(&plan, &token).map_err(connection_error)?;
            native.execute(&plan, &token).await.map_err(connection_error)?;
            let prompt = input.messages.iter().rev().find(|message| message.role == "user")
                .and_then(|message| message.content.as_str()).unwrap_or("multimodal input");
            let answer = format!("Offline demo response: {prompt}. No network request was made.");
            yield ProviderEvent::Started;
            for word in answer.split_inclusive(' ') {
                let cancelled = tokio::select! {
                    _ = cancel.cancelled() => true,
                    _ = tokio::time::sleep(Duration::from_millis(if input.model == "demo-slow" { 350 } else { 35 })) => false
                };
                if cancelled { Err(CoreError::new("cancelled", "aborted"))?; }
                fence.check().map_err(connection_error)?;
                yield ProviderEvent::Delta(word.into());
            }
            yield ProviderEvent::Completed { content: answer, tool_calls: vec![], finish_reason: "stop".into() };
        })
    }
}

pub async fn approve(
    ui: &UiSender,
    plan: &RequestPlan,
    local_demo: bool,
    cancel: CancellationToken,
) -> Result<bool, CoreError> {
    let connection = plan.connection();
    let (reply, receive) = tokio::sync::oneshot::channel();
    ui.send(Update::Consent(ConsentPrompt {
        app: connection.app_id.as_str().into(),
        connection: connection.id.as_str().into(),
        generation: connection.generation,
        account: connection.config.account.clone(),
        destination: plan.destination().to_string(),
        model: plan.model().unwrap_or("").into(),
        categories: plan.disclosed_data().iter().cloned().collect(),
        local_demo,
        reply,
    }));
    tokio::select! {
        _ = cancel.cancelled() => Err(CoreError::new("cancelled","aborted")),
        result = receive => Ok(result.unwrap_or(false)),
    }
}

pub fn connection_error(error: ConnectionError) -> CoreError {
    CoreError::new("connection_error", &error.to_string())
}

struct DemoNetwork;
#[async_trait]
impl ConnectionNetwork for DemoNetwork {
    fn check_available(&self, plan: &RequestPlan) -> Result<(), ConnectionError> {
        if plan.connection().config.provider_kind != "offline-demo" {
            return Err(ConnectionError::NetworkUnavailable);
        }
        Ok(())
    }
    async fn send(
        &self,
        plan: &RequestPlan,
        _secret: Option<&Secret>,
    ) -> Result<NetworkResponse, ConnectionError> {
        self.check_available(plan)?;
        Ok(NetworkResponse {
            status: 200,
            body: b"{}".to_vec(),
        })
    }
}
