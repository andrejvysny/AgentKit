mod common;
use agentkit_connections::*;
use common::*;
use std::sync::atomic::Ordering;

#[test]
fn snapshot_roundtrip_preserves_namespace_and_generation_high_water_mark() {
    let repository = ConnectionRepository::default();
    let one = app(&repository, "one");
    let created = one.create(id(), config()).unwrap();
    one.delete(&id(), created.generation).unwrap();
    let two = app(&repository, "two");
    let created = two.create(id(), config()).unwrap();
    let isolated = one.export().unwrap();
    assert!(isolated.connections.is_empty());
    assert!(isolated.last_generation >= created.generation);
    let restored = ConnectionRepository::restore(repository.export().unwrap()).unwrap();
    assert_eq!(app(&restored, "two").get(&id()).unwrap(), created);
    assert!(app(&restored, "one").list().unwrap().is_empty());
    let fresh = app(&restored, "one").create(id(), config()).unwrap();
    assert!(fresh.generation > created.generation);
}

#[tokio::test]
async fn exact_route_consent_cannot_authorize_another_route_or_origin() {
    let fixture = fixture();
    let first = fixture
        .native
        .plan_to(
            &id(),
            input(),
            "https://example.com/v1/chat/completions".parse().unwrap(),
        )
        .unwrap();
    let consent = fixture.native.approve_scope(&first).unwrap();
    let second = fixture
        .native
        .plan_to(
            &id(),
            input(),
            "https://example.com/v1/responses".parse().unwrap(),
        )
        .unwrap();
    assert!(matches!(
        fixture.native.execute(&second, &consent).await,
        Err(ConnectionError::ConsentDenied)
    ));
    for destination in [
        "https://elsewhere.example/v1/chat/completions",
        "https://example.com/v10/chat/completions",
        "https://example.com/v1/../other",
    ] {
        assert!(matches!(
            fixture
                .native
                .plan_to(&id(), input(), destination.parse().unwrap()),
            Err(ConnectionError::InvalidConfiguration)
        ));
    }
    assert_eq!(fixture.secrets.reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn environment_adapter_has_explicit_read_only_bindings_and_no_fallback() {
    let fixture = fixture();
    let key = fixture.native.credential_key(&id()).unwrap();
    let environment = EnvironmentSecretStore::new(std::collections::BTreeMap::from([(
        key.clone(),
        "AGENTKIT_TEST_SECRET_MUST_BE_UNDEFINED_3B51F209".into(),
    )]));
    assert!(matches!(
        environment.get(&key).await,
        Err(ConnectionError::SecretMissing)
    ));
    assert_eq!(
        environment
            .put(&key, Secret::new("not-persisted"))
            .await
            .unwrap_err(),
        ConnectionError::SecretUnavailable
    );
    assert_eq!(
        environment.delete(&key).await.unwrap_err(),
        ConnectionError::SecretUnavailable
    );
}

#[test]
fn corrupt_snapshots_are_rejected() {
    let repository = ConnectionRepository::default();
    app(&repository, "one").create(id(), config()).unwrap();
    let snapshot = repository.export().unwrap();
    let mut duplicate = snapshot.clone();
    duplicate.connections.push(duplicate.connections[0].clone());
    assert!(matches!(
        ConnectionRepository::restore(duplicate),
        Err(ConnectionError::InvalidConfiguration)
    ));
    let mut old_generation = snapshot.clone();
    old_generation.last_generation = 0;
    assert!(matches!(
        ConnectionRepository::restore(old_generation),
        Err(ConnectionError::InvalidConfiguration)
    ));
    let mut wrong_version = snapshot;
    wrong_version.version = 2;
    assert!(matches!(
        ConnectionRepository::restore(wrong_version),
        Err(ConnectionError::InvalidConfiguration)
    ));
}

#[tokio::test]
async fn reusable_scope_covers_payload_changes_but_no_new_disclosures_or_models() {
    let fixture = fixture();
    let current = fixture.app.get(&id()).unwrap();
    let mut changed = current.config;
    let mut extra_model = changed.models[0].clone();
    extra_model.id = "model-two".into();
    changed.models.push(extra_model);
    fixture
        .app
        .update(&id(), current.generation, changed)
        .unwrap();
    let plan = fixture.native.plan(&id(), input()).unwrap();
    let consent = fixture.native.approve_scope(&plan).unwrap();
    let mut new_payload = input();
    new_payload.payload = b"new message in approved category".to_vec();
    let next = fixture.native.plan(&id(), new_payload).unwrap();
    assert_eq!(
        fixture
            .native
            .execute(&next, &consent)
            .await
            .unwrap()
            .status,
        200
    );
    for extra_disclosure in [true, false] {
        let mut changed = input();
        if extra_disclosure {
            changed.disclosed_data.insert("files".into());
        } else {
            changed.model = Some("model-two".into());
        }
        let denied = fixture.native.plan(&id(), changed).unwrap();
        assert!(matches!(
            fixture.native.execute(&denied, &consent).await,
            Err(ConnectionError::ConsentDenied)
        ));
    }
    assert_eq!(fixture.secrets.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn streaming_response_remains_unbuffered_and_fence_rejects_late_events() {
    let fixture = fixture();
    let plan = fixture.native.plan(&id(), input()).unwrap();
    let consent = fixture.native.approve(&plan).unwrap();
    let fence = fixture.native.fence(&plan, &consent).unwrap();
    let (sender, receiver) = tokio::sync::mpsc::channel::<u8>(1);
    let mut stream = fixture
        .native
        .execute_with(&plan, &consent, move |approved, secret| {
            assert_eq!(approved.payload(), b"approved payload");
            assert_eq!(secret.unwrap().expose(), "never-log-this");
            Box::pin(async move { Ok(receiver) })
        })
        .await
        .unwrap();
    sender.send(1).await.unwrap();
    fence.check().unwrap();
    assert_eq!(stream.recv().await.unwrap(), 1);
    fixture.native.revoke(&consent).unwrap();
    sender.send(2).await.unwrap();
    assert_eq!(fence.check().unwrap_err(), ConnectionError::ConsentDenied);
}
