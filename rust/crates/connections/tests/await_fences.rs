mod common;
use agentkit_connections::*;
use async_trait::async_trait;
use common::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

#[derive(Default)]
struct DelayedSecret {
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl SecretStore for DelayedSecret {
    async fn get(&self, _key: &SecretKey) -> Result<Secret, ConnectionError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(Secret::new("never-log-this"))
    }
    async fn put(&self, _key: &SecretKey, _secret: Secret) -> Result<(), ConnectionError> {
        Ok(())
    }
    async fn delete(&self, _key: &SecretKey) -> Result<(), ConnectionError> {
        Ok(())
    }
}

#[tokio::test]
async fn revocation_while_secret_read_pending_prevents_network_dispatch() {
    let fixture = fixture();
    let secrets = Arc::new(DelayedSecret::default());
    let native = NativeConnections::new(fixture.app, secrets.clone(), fixture.network.clone());
    let plan = native.plan(&id(), input()).unwrap();
    let consent = native.approve(&plan).unwrap();
    let revoke = async {
        secrets.entered.notified().await;
        native.revoke(&consent).unwrap();
        secrets.release.notify_one();
    };
    let (result, ()) = tokio::join!(native.execute(&plan, &consent), revoke);
    assert!(matches!(result, Err(ConnectionError::ConsentDenied)));
    assert_eq!(fixture.network.sends.load(Ordering::SeqCst), 0);
}

#[derive(Default)]
struct DelayedNetwork {
    entered: Notify,
    release: Notify,
    sends: AtomicUsize,
}

#[async_trait]
impl ConnectionNetwork for DelayedNetwork {
    fn check_available(&self, _plan: &RequestPlan) -> Result<(), ConnectionError> {
        Ok(())
    }
    async fn send(
        &self,
        _plan: &RequestPlan,
        _credential: Option<&Secret>,
    ) -> Result<NetworkResponse, ConnectionError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        Ok(NetworkResponse {
            status: 200,
            body: b"stale result".to_vec(),
        })
    }
}

#[tokio::test]
async fn generation_change_while_request_pending_discards_result() {
    let fixture = fixture();
    let network = Arc::new(DelayedNetwork::default());
    let native = NativeConnections::new(fixture.app.clone(), fixture.secrets, network.clone());
    let plan = native.plan(&id(), input()).unwrap();
    let consent = native.approve(&plan).unwrap();
    let update = async {
        network.entered.notified().await;
        let current = fixture.app.get(&id()).unwrap();
        fixture
            .app
            .update(&id(), current.generation, current.config)
            .unwrap();
        network.release.notify_one();
    };
    let (result, ()) = tokio::join!(native.execute(&plan, &consent), update);
    assert!(matches!(result, Err(ConnectionError::StaleGeneration)));
    assert_eq!(network.sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn secret_keys_isolate_app_connection_generation_and_account() {
    let repository = ConnectionRepository::default();
    let one = app(&repository, "one");
    let two = app(&repository, "two");
    one.create(id(), config()).unwrap();
    two.create(id(), config()).unwrap();
    let store = Arc::new(MemorySecretStore::default());
    let native_one =
        NativeConnections::new(one.clone(), store.clone(), Arc::new(UnavailableNetwork));
    let native_two = NativeConnections::new(two, store.clone(), Arc::new(UnavailableNetwork));
    let key = native_one.credential_key(&id()).unwrap();
    store.put(&key, Secret::new("private-value")).await.unwrap();
    assert_eq!(store.get(&key).await.unwrap().expose(), "private-value");
    assert!(matches!(
        store.get(&native_two.credential_key(&id()).unwrap()).await,
        Err(ConnectionError::SecretMissing)
    ));
    let current = one.get(&id()).unwrap();
    let mut changed = current.config;
    changed.account = Some("other-account".into());
    one.update(&id(), current.generation, changed).unwrap();
    assert!(matches!(
        store.get(&native_one.credential_key(&id()).unwrap()).await,
        Err(ConnectionError::SecretMissing)
    ));
    store.delete(&key).await.unwrap();
    assert!(matches!(
        store.get(&key).await,
        Err(ConnectionError::SecretMissing)
    ));
}
