mod common;
use agentkit_connections::*;
use async_trait::async_trait;
use common::*;
use std::sync::{Arc, atomic::Ordering};

struct Catalogue;
#[async_trait]
impl ConnectionNetwork for Catalogue {
    fn check_available(&self, _: &RequestPlan) -> Result<(), ConnectionError> {
        Ok(())
    }
    async fn send(
        &self,
        plan: &RequestPlan,
        _: Option<&Secret>,
    ) -> Result<NetworkResponse, ConnectionError> {
        assert_eq!(plan.destination().as_str(), "https://example.com/v1/models");
        Ok(NetworkResponse {
            status: 200,
            body: br#"{"data":[{"id":"new-model","name":"New"}]}"#.to_vec(),
        })
    }
}

#[tokio::test]
async fn discovery_requires_exact_consent_and_never_changes_selection() {
    let repository = app(&ConnectionRepository::default(), "app");
    let connection = repository.create(id(), config()).unwrap();
    let secrets = Arc::new(CountingSecrets::default());
    let native = NativeConnections::new(repository.clone(), secrets.clone(), Arc::new(Catalogue));
    let discovery = native.plan_discovery(&id()).unwrap();
    let probe = native.plan_probe(&id()).unwrap();
    let consent = native.approve(&probe).unwrap();
    assert_eq!(
        native.discover_models(&discovery, &consent).await,
        Err(ConnectionError::ConsentDenied)
    );
    assert_eq!(secrets.reads.load(Ordering::SeqCst), 0);
    assert!(native.probe(&probe, &consent).await.unwrap().reachable);
    let consent = native.approve(&discovery).unwrap();
    let models = native.discover_models(&discovery, &consent).await.unwrap();
    assert_eq!(models[0].id, "new-model");
    assert_eq!(repository.get(&id()).unwrap(), connection);
    native.revoke(&consent).unwrap();
    assert_eq!(
        native.discover_models(&discovery, &consent).await,
        Err(ConnectionError::ConsentDenied)
    );
}
