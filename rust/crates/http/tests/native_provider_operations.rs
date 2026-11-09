use agentkit_connections::*;
use agentkit_host::SystemRuntime;
use agentkit_http::{
    Actor, NativeProviderOperations, Problem, ProviderConsentPort, ProviderOperations,
};
use async_trait::async_trait;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Secrets(AtomicUsize);
#[async_trait]
impl SecretStore for Secrets {
    async fn get(&self, _: &SecretKey) -> Result<Secret, ConnectionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Secret::new("synthetic-key"))
    }
    async fn put(&self, _: &SecretKey, _: Secret) -> Result<(), ConnectionError> {
        Ok(())
    }
    async fn delete(&self, _: &SecretKey) -> Result<(), ConnectionError> {
        Ok(())
    }
}
struct Network(AtomicUsize);
#[async_trait]
impl ConnectionNetwork for Network {
    fn check_available(&self, _: &RequestPlan) -> Result<(), ConnectionError> {
        Ok(())
    }
    async fn send(
        &self,
        plan: &RequestPlan,
        credential: Option<&Secret>,
    ) -> Result<NetworkResponse, ConnectionError> {
        assert_eq!(
            plan.destination().as_str(),
            "https://provider.test/v1/models"
        );
        assert_eq!(credential.unwrap().expose(), "synthetic-key");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(NetworkResponse {
            status: 200,
            body: br#"{"data":[{"id":"discovered","name":"Discovered model"}]}"#.to_vec(),
        })
    }
}
struct Consent {
    approved: bool,
    rotate: Option<AppConnections>,
}
#[async_trait]
impl ProviderConsentPort for Consent {
    async fn approve(
        &self,
        actor: &Actor,
        plan: &RequestPlan,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<bool, Problem> {
        assert_eq!(actor.id, "app");
        assert_eq!(
            plan.disclosed_data(),
            &BTreeSet::from(["connection-authentication".into()])
        );
        if let Some(connections) = &self.rotate {
            let old = connections.get(&plan.connection().id).unwrap();
            connections
                .update(&old.id, old.generation, old.config)
                .unwrap();
        }
        Ok(self.approved)
    }
}
fn fixture() -> (
    AppConnections,
    NativeConnections,
    Arc<Secrets>,
    Arc<Network>,
) {
    let connections = ConnectionRepository::default().for_app(AppId::new("app").unwrap());
    connections
        .create(
            ConnectionId::new("provider").unwrap(),
            ConnectionConfig {
                label: "Test".into(),
                provider_kind: "openai-compatible".into(),
                endpoint: "https://provider.test/v1".parse().unwrap(),
                account: None,
                credential_required: true,
                extra_header_names: BTreeSet::new(),
                allow_insecure_http: false,
                enabled: true,
                capabilities: BTreeSet::from([
                    Capability::Chat,
                    Capability::ModelDiscovery,
                    Capability::Probe,
                ]),
                models: vec![Model {
                    id: "selected".into(),
                    display_name: None,
                    capabilities: BTreeSet::from([Capability::Chat]),
                    context_window_tokens: None,
                }],
            },
        )
        .unwrap();
    let secrets = Arc::new(Secrets(AtomicUsize::new(0)));
    let network = Arc::new(Network(AtomicUsize::new(0)));
    let native = NativeConnections::new(connections.clone(), secrets.clone(), network.clone());
    (connections, native, secrets, network)
}
fn actor() -> Actor {
    Actor {
        id: "app".into(),
        metadata: BTreeMap::new(),
    }
}

#[tokio::test]
async fn denied_native_discovery_never_reads_credentials_or_dispatches() {
    let (_, native, secrets, network) = fixture();
    let operations = NativeProviderOperations::new(
        native,
        Arc::new(Consent {
            approved: false,
            rotate: None,
        }),
        Arc::new(SystemRuntime),
    );
    let error = operations
        .refresh_models(&actor(), "provider")
        .await
        .unwrap_err();
    assert_eq!(error.code, "consent_required");
    assert_eq!(secrets.0.load(Ordering::SeqCst), 0);
    assert_eq!(network.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn approved_native_discovery_projects_populated_catalog_and_probe() {
    let (_, native, secrets, network) = fixture();
    let operations = NativeProviderOperations::new(
        native,
        Arc::new(Consent {
            approved: true,
            rotate: None,
        }),
        Arc::new(SystemRuntime),
    );
    let models = operations
        .refresh_models(&actor(), "provider")
        .await
        .unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].provider_id, "provider");
    assert_eq!(models[0].model_id, "discovered");
    assert!(operations.test(&actor(), "provider").await.unwrap().ok);
    assert_eq!(secrets.0.load(Ordering::SeqCst), 2);
    assert_eq!(network.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn generation_changed_during_approval_never_uses_the_old_secret() {
    let (connections, native, secrets, network) = fixture();
    let operations = NativeProviderOperations::new(
        native,
        Arc::new(Consent {
            approved: true,
            rotate: Some(connections),
        }),
        Arc::new(SystemRuntime),
    );
    let error = operations.test(&actor(), "provider").await.unwrap_err();
    assert_eq!(error.code, "revision_conflict");
    assert_eq!(secrets.0.load(Ordering::SeqCst), 0);
    assert_eq!(network.0.load(Ordering::SeqCst), 0);
}

struct PendingConsent;
#[async_trait]
impl ProviderConsentPort for PendingConsent {
    async fn approve(
        &self,
        _: &Actor,
        _: &RequestPlan,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<bool, Problem> {
        std::future::pending().await
    }
}
#[tokio::test]
async fn native_discovery_bounds_a_host_consent_callback_that_never_returns() {
    let (_, native, secrets, network) = fixture();
    let operations =
        NativeProviderOperations::new(native, Arc::new(PendingConsent), Arc::new(SystemRuntime))
            .with_timeout(std::time::Duration::from_millis(20))
            .unwrap();
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        operations.refresh_models(&actor(), "provider"),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.status, 504);
    assert_eq!(secrets.0.load(Ordering::SeqCst), 0);
    assert_eq!(network.0.load(Ordering::SeqCst), 0);
}
