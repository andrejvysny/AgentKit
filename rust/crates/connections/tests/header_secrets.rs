mod common;
use agentkit_connections::*;
use async_trait::async_trait;
use common::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct HeaderNetwork {
    sends: AtomicUsize,
}

#[async_trait]
impl ConnectionNetwork for HeaderNetwork {
    fn check_available(&self, _plan: &RequestPlan) -> Result<(), ConnectionError> {
        Ok(())
    }
    fn supports_extra_headers(&self) -> bool {
        true
    }
    async fn send(
        &self,
        _plan: &RequestPlan,
        _secret: Option<&Secret>,
    ) -> Result<NetworkResponse, ConnectionError> {
        unreachable!()
    }
    async fn send_resolved(
        &self,
        plan: &RequestPlan,
        secrets: &ResolvedSecrets,
    ) -> Result<NetworkResponse, ConnectionError> {
        assert_eq!(
            plan.connection().config.extra_header_names,
            BTreeSet::from(["X-Attribution".into()])
        );
        assert_eq!(
            secrets.headers.get("X-Attribution").unwrap().expose(),
            "private-header-value"
        );
        self.sends.fetch_add(1, Ordering::SeqCst);
        Ok(NetworkResponse {
            status: 200,
            body: vec![],
        })
    }
}

#[tokio::test]
async fn header_values_are_separate_secrets_and_absent_from_all_metadata() {
    let repository = ConnectionRepository::default();
    let app = app(&repository, "header-app");
    let mut configuration = config();
    configuration
        .extra_header_names
        .insert("X-Attribution".into());
    let connection = app.create(id(), configuration).unwrap();
    let store = Arc::new(MemorySecretStore::default());
    let network = Arc::new(HeaderNetwork {
        sends: AtomicUsize::new(0),
    });
    let native = NativeConnections::new(app.clone(), store.clone(), network.clone());
    store
        .put(
            &native.credential_key(&id()).unwrap(),
            Secret::new("api-key"),
        )
        .await
        .unwrap();
    native
        .provision_headers(
            &id(),
            BTreeMap::from([("X-Attribution".into(), Secret::new("private-header-value"))]),
        )
        .await
        .unwrap();
    let projection = connection.provider("model-one").unwrap();
    for metadata in [
        serde_json::to_string(&projection).unwrap(),
        serde_json::to_string(&app.export().unwrap()).unwrap(),
        format!("{connection:?}"),
    ] {
        assert!(!metadata.contains("private-header-value"));
        assert!(!metadata.contains("api-key"));
    }
    assert_ne!(
        native.credential_key(&id()).unwrap(),
        native.header_key(&id()).unwrap()
    );
    let plan = native.plan(&id(), input()).unwrap();
    let consent = native.approve(&plan).unwrap();
    assert_eq!(native.execute(&plan, &consent).await.unwrap().status, 200);
    assert_eq!(network.sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn revoked_consent_and_header_unavailable_adapter_reject_before_any_secret_read() {
    let app = app(&ConnectionRepository::default(), "header-denial");
    let mut configuration = config();
    configuration
        .extra_header_names
        .insert("X-Attribution".into());
    app.create(id(), configuration).unwrap();
    let secrets = Arc::new(CountingSecrets::default());
    let native = NativeConnections::new(app, secrets.clone(), Arc::new(CountingNetwork::default()));
    let plan = native.plan(&id(), input()).unwrap();
    let consent = native.approve(&plan).unwrap();
    assert!(matches!(
        native.execute(&plan, &consent).await,
        Err(ConnectionError::NetworkUnavailable)
    ));
    native.revoke(&consent).unwrap();
    assert!(matches!(
        native.execute(&plan, &consent).await,
        Err(ConnectionError::ConsentDenied)
    ));
    assert_eq!(secrets.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn insecure_http_requires_explicit_policy_and_reserved_header_overrides_are_rejected() {
    let app = app(&ConnectionRepository::default(), "policy-app");
    let mut configuration = config();
    configuration.endpoint = "http://example.com/v1".parse().unwrap();
    assert_eq!(
        app.create(id(), configuration.clone()).unwrap_err(),
        ConnectionError::InvalidConfiguration
    );
    configuration.allow_insecure_http = true;
    let connection = app.create(id(), configuration.clone()).unwrap();
    for reserved in [
        "Authorization",
        "Content-Type",
        "X-Header\r\nInjected",
        "Host",
    ] {
        configuration.extra_header_names = BTreeSet::from([reserved.into()]);
        assert_eq!(
            app.update(&id(), connection.generation, configuration.clone())
                .unwrap_err(),
            ConnectionError::InvalidConfiguration
        );
    }
}

#[tokio::test]
async fn generation_specific_secret_provisioning_cannot_follow_another_account_update() {
    let app = app(&ConnectionRepository::default(), "generation-secret-race");
    let mut configuration = config();
    configuration
        .extra_header_names
        .insert("X-Attribution".into());
    let first = app.create(id(), configuration).unwrap();
    let store = Arc::new(MemorySecretStore::default());
    let native = NativeConnections::new(app.clone(), store.clone(), Arc::new(UnavailableNetwork));
    let first_key = native.credential_key_at(&id(), first.generation).unwrap();
    let mut changed = first.config;
    changed.account = Some("another-account".into());
    let second = app.update(&id(), first.generation, changed).unwrap();
    assert_eq!(
        native
            .credential_key_at(&id(), first.generation)
            .unwrap_err(),
        ConnectionError::StaleGeneration
    );
    assert_eq!(
        native
            .provision_headers_at(
                &id(),
                first.generation,
                BTreeMap::from([(
                    "X-Attribution".into(),
                    Secret::new("must-not-follow-account")
                )])
            )
            .await
            .unwrap_err(),
        ConnectionError::StaleGeneration
    );
    assert_ne!(
        first_key,
        native.credential_key_at(&id(), second.generation).unwrap()
    );
    assert!(matches!(
        store
            .get(&native.header_key_at(&id(), second.generation).unwrap())
            .await,
        Err(ConnectionError::SecretMissing)
    ));
}
