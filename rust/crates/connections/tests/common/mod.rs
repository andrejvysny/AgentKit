#![allow(dead_code)]
use agentkit_connections::*;
use async_trait::async_trait;
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

pub fn config() -> ConnectionConfig {
    let capabilities = BTreeSet::from([
        Capability::Chat,
        Capability::Probe,
        Capability::ModelDiscovery,
    ]);
    ConnectionConfig {
        label: "Provider".into(),
        provider_kind: "openai-compatible".into(),
        endpoint: "https://example.com/v1".parse().unwrap(),
        account: Some("account-one".into()),
        credential_required: true,
        extra_header_names: BTreeSet::new(),
        allow_insecure_http: false,
        enabled: true,
        capabilities,
        models: vec![Model {
            id: "model-one".into(),
            display_name: None,
            capabilities: BTreeSet::from([Capability::Chat]),
            context_window_tokens: Some(8192),
        }],
    }
}

pub fn input() -> PlanInput {
    PlanInput {
        capability: Capability::Chat,
        model: Some("model-one".into()),
        disclosed_data: BTreeSet::from(["message-content".into()]),
        payload: b"approved payload".to_vec(),
    }
}

pub fn app(repository: &ConnectionRepository, name: &str) -> AppConnections {
    repository.for_app(AppId::new(name).unwrap())
}

pub fn id() -> ConnectionId {
    ConnectionId::new("same-id").unwrap()
}

#[derive(Default)]
pub struct CountingSecrets {
    pub reads: AtomicUsize,
}

#[async_trait]
impl SecretStore for CountingSecrets {
    async fn get(&self, _key: &SecretKey) -> Result<Secret, ConnectionError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(Secret::new("never-log-this"))
    }
    async fn put(&self, _key: &SecretKey, _secret: Secret) -> Result<(), ConnectionError> {
        Ok(())
    }
    async fn delete(&self, _key: &SecretKey) -> Result<(), ConnectionError> {
        Ok(())
    }
}

#[derive(Default)]
pub struct CountingNetwork {
    pub sends: AtomicUsize,
}

#[async_trait]
impl ConnectionNetwork for CountingNetwork {
    fn check_available(&self, _plan: &RequestPlan) -> Result<(), ConnectionError> {
        Ok(())
    }
    async fn send(
        &self,
        plan: &RequestPlan,
        credential: Option<&Secret>,
    ) -> Result<NetworkResponse, ConnectionError> {
        assert_eq!(plan.model(), Some("model-one"));
        assert_eq!(credential.unwrap().expose(), "never-log-this");
        self.sends.fetch_add(1, Ordering::SeqCst);
        Ok(NetworkResponse {
            status: 200,
            body: vec![],
        })
    }
}

pub struct Fixture {
    pub app: AppConnections,
    pub native: NativeConnections,
    pub secrets: Arc<CountingSecrets>,
    pub network: Arc<CountingNetwork>,
}

pub fn fixture() -> Fixture {
    let app = app(&ConnectionRepository::default(), "app-one");
    app.create(id(), config()).unwrap();
    let secrets = Arc::new(CountingSecrets::default());
    let network = Arc::new(CountingNetwork::default());
    let native = NativeConnections::new(app.clone(), secrets.clone(), network.clone());
    Fixture {
        app,
        native,
        secrets,
        network,
    }
}
