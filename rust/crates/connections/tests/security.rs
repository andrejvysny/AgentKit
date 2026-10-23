mod common;
use agentkit_connections::*;
use common::*;
use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::Ordering},
};

#[test]
fn repository_isolates_apps_and_generation_survives_delete() {
    let repository = ConnectionRepository::default();
    let one = app(&repository, "one");
    let two = app(&repository, "two");
    let first = one.create(id(), config()).unwrap();
    assert_eq!(two.get(&id()).unwrap_err(), ConnectionError::NotFound);
    let second = two.create(id(), config()).unwrap();
    assert_eq!(one.list().unwrap().len(), 1);
    assert_eq!(two.list().unwrap().len(), 1);
    assert_ne!(first.generation, second.generation);
    one.delete(&id(), first.generation).unwrap();
    let recreated = one.create(id(), config()).unwrap();
    assert!(recreated.generation > first.generation);
    assert_eq!(two.get(&id()).unwrap(), second);
    assert_eq!(
        one.update(&id(), first.generation, config()).unwrap_err(),
        ConnectionError::StaleGeneration
    );
}

#[tokio::test]
async fn revoked_consent_denies_before_secret_access() {
    let fixture = fixture();
    let plan = fixture.native.plan(&id(), input()).unwrap();
    let consent = fixture.native.approve(&plan).unwrap();
    fixture.native.revoke(&consent).unwrap();
    assert!(matches!(
        fixture.native.execute(&plan, &consent).await,
        Err(ConnectionError::ConsentDenied)
    ));
    assert_eq!(fixture.secrets.reads.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.network.sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn endpoint_account_and_capability_changes_invalidate_before_secret_access() {
    for change in 0..3 {
        let fixture = fixture();
        let plan = fixture.native.plan(&id(), input()).unwrap();
        let consent = fixture.native.approve(&plan).unwrap();
        let current = fixture.app.get(&id()).unwrap();
        let mut changed = current.config.clone();
        match change {
            0 => changed.endpoint = "https://other.example/v1".parse().unwrap(),
            1 => changed.account = Some("account-two".into()),
            _ => changed.capabilities.clear(),
        }
        fixture
            .app
            .update(&id(), current.generation, changed)
            .unwrap();
        assert!(matches!(
            fixture.native.execute(&plan, &consent).await,
            Err(ConnectionError::StaleGeneration)
        ));
        assert_eq!(fixture.secrets.reads.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn disclosure_payload_and_model_bind_exact_plan() {
    let fixture = fixture();
    let approved = fixture.native.plan(&id(), input()).unwrap();
    let consent = fixture.native.approve(&approved).unwrap();
    for change in 0..2 {
        let mut changed = input();
        if change == 0 {
            changed.disclosed_data = BTreeSet::from(["files".into()]);
        } else {
            changed.payload = b"unapproved payload".to_vec();
        }
        let other = fixture.native.plan(&id(), changed).unwrap();
        assert!(matches!(
            fixture.native.execute(&other, &consent).await,
            Err(ConnectionError::ConsentDenied)
        ));
    }
    let mut changed = input();
    changed.model = Some("missing-model".into());
    assert!(matches!(
        fixture.native.plan(&id(), changed),
        Err(ConnectionError::UnsupportedModel)
    ));
    assert_eq!(fixture.secrets.reads.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture
            .native
            .execute(&approved, &consent)
            .await
            .unwrap()
            .status,
        200
    );
}

#[tokio::test]
async fn consent_cannot_cross_apps_or_service_authorities() {
    let fixture = fixture();
    let plan = fixture.native.plan(&id(), input()).unwrap();
    let consent = fixture.native.approve(&plan).unwrap();
    let other = NativeConnections::new(
        fixture.app.clone(),
        fixture.secrets.clone(),
        fixture.network.clone(),
    );
    let other_plan = other.plan(&id(), input()).unwrap();
    assert!(matches!(
        other.execute(&other_plan, &consent).await,
        Err(ConnectionError::ConsentDenied)
    ));
    assert!(matches!(
        other.approve(&plan),
        Err(ConnectionError::ConsentDenied)
    ));
    let repository = ConnectionRepository::default();
    let other_app = app(&repository, "other-app");
    other_app.create(id(), config()).unwrap();
    let other = NativeConnections::new(other_app, fixture.secrets.clone(), fixture.network.clone());
    assert!(matches!(
        other.execute(&plan, &consent).await,
        Err(ConnectionError::ConsentDenied)
    ));
    assert_eq!(fixture.secrets.reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unavailable_adapters_fail_without_fallback() {
    let fixture = fixture();
    let native = NativeConnections::new(
        fixture.app.clone(),
        Arc::new(UnavailableSecretStore),
        fixture.network.clone(),
    );
    let plan = native.plan(&id(), input()).unwrap();
    let consent = native.approve(&plan).unwrap();
    assert!(matches!(
        native.execute(&plan, &consent).await,
        Err(ConnectionError::SecretUnavailable)
    ));
    assert_eq!(fixture.network.sends.load(Ordering::SeqCst), 0);
    let native = NativeConnections::new(
        fixture.app,
        fixture.secrets.clone(),
        Arc::new(UnavailableNetwork),
    );
    let plan = native.plan(&id(), input()).unwrap();
    let consent = native.approve(&plan).unwrap();
    assert!(matches!(
        native.execute(&plan, &consent).await,
        Err(ConnectionError::NetworkUnavailable)
    ));
    assert_eq!(fixture.secrets.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn projection_selects_only_requested_model_and_rejects_secret_url_fields() {
    let fixture = fixture();
    let connection = fixture.app.get(&id()).unwrap();
    let projection = connection.provider("model-one").unwrap();
    assert_eq!(projection.kind, "openai-compatible");
    assert_eq!(projection.default_model, "model-one");
    assert_eq!(
        connection.provider("missing").unwrap_err(),
        ConnectionError::UnsupportedModel
    );
    for endpoint in [
        "https://user:password@example.com",
        "https://example.com?key=secret",
        "http://example.com",
    ] {
        let mut changed = config();
        changed.endpoint = endpoint.parse().unwrap();
        assert_eq!(
            fixture
                .app
                .update(&id(), connection.generation, changed)
                .unwrap_err(),
            ConnectionError::InvalidConfiguration
        );
    }
}
