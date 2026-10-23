#[tokio::test]
async fn secret_failure_preserves_committed_generation_across_restart_and_retry() {
    let database = Database::new();
    let secrets = Arc::new(Secrets::default());
    let first = fixture(&database.path(), secrets.clone()).await;
    let original = create(&first, "provider").await;
    secrets.fail_put.store(true, Ordering::SeqCst);
    let error = first
        .backend
        .dispatch(provider_request("updateProvider", "provider", changed()))
        .await
        .unwrap_err();
    assert_eq!(error.code, "secret_write_failed");
    let committed = first.connections.get(&original.id).unwrap();
    assert!(committed.generation > original.generation);
    assert_eq!(
        snapshot(&first).await["repository"]["last_generation"],
        committed.generation
    );
    assert!(matches!(
        secrets
            .get(&first.native.credential_key(&original.id).unwrap())
            .await,
        Err(ConnectionError::SecretMissing)
    ));
    let queued = queue(&first, "provider", "secret-failure").await;
    assert_eq!(
        queued.payload["providerSnapshot"]["connection"]["generation"],
        committed.generation
    );
    drop(first);
    secrets.fail_put.store(false, Ordering::SeqCst);
    let restarted = fixture(&database.path(), secrets).await;
    assert_eq!(restarted.connections.get(&original.id).unwrap(), committed);
    restarted
        .backend
        .dispatch(provider_request("updateProvider", "provider", changed()))
        .await
        .unwrap();
    assert!(restarted.connections.get(&original.id).unwrap().generation > committed.generation);
}

#[tokio::test]
async fn snapshot_rollback_keeps_authority_model_and_secrets_unchanged() {
    let database = Database::new();
    let first = fixture(&database.path(), Arc::new(Secrets::default())).await;
    let original = create(&first, "provider").await;
    let durable = snapshot(&first).await;
    let puts = first.secrets.puts.load(Ordering::SeqCst);
    database.reject_snapshot(true);
    let mut body = changed();
    body["defaultModel"] = json!("new-model");
    assert!(
        first
            .backend
            .dispatch(provider_request("updateProvider", "provider", body))
            .await
            .is_err()
    );
    assert_eq!(first.connections.get(&original.id).unwrap(), original);
    assert_eq!(snapshot(&first).await, durable);
    assert_eq!(first.secrets.puts.load(Ordering::SeqCst), puts);
    assert_eq!(
        first
            .host
            .store()
            .get_setting("http.provider-model", "provider")
            .await
            .unwrap(),
        Some(json!("model"))
    );
    let queued = queue(&first, "provider", "rollback").await;
    assert_eq!(
        queued.payload["providerSnapshot"]["connection"]["generation"],
        original.generation
    );
    database.reject_snapshot(false);
    let secrets = first.secrets.clone();
    drop(first);
    let restarted = fixture(&database.path(), secrets).await;
    assert_eq!(restarted.connections.get(&original.id).unwrap(), original);
    restarted
        .backend
        .dispatch(provider_request("updateProvider", "provider", changed()))
        .await
        .unwrap();
    assert!(restarted.connections.get(&original.id).unwrap().generation > original.generation);
}

#[tokio::test]
async fn failed_create_has_no_visible_or_durable_provider_model_or_secret() {
    let database = Database::new();
    let first = fixture(&database.path(), Arc::new(Secrets::default())).await;
    create(&first, "committed").await;
    let durable = snapshot(&first).await;
    let puts = first.secrets.puts.load(Ordering::SeqCst);
    database.reject_snapshot(true);
    assert!(
        first
            .backend
            .dispatch(request("createProvider", configuration("ghost")))
            .await
            .is_err()
    );
    assert_eq!(
        first.connections.get(&ConnectionId::new("ghost").unwrap()),
        Err(ConnectionError::NotFound)
    );
    assert_eq!(snapshot(&first).await, durable);
    assert_eq!(first.secrets.puts.load(Ordering::SeqCst), puts);
    assert_eq!(
        first
            .host
            .store()
            .get_setting("http.provider-model", "ghost")
            .await
            .unwrap(),
        None
    );
    database.reject_snapshot(false);
    let secrets = first.secrets.clone();
    drop(first);
    let restarted = fixture(&database.path(), secrets).await;
    assert_eq!(
        restarted
            .connections
            .get(&ConnectionId::new("ghost").unwrap()),
        Err(ConnectionError::NotFound)
    );
    create(&restarted, "ghost").await;
}

#[tokio::test]
async fn stale_independent_authority_cannot_overwrite_newer_durable_snapshot() {
    let database = Database::new();
    let first = fixture(&database.path(), Arc::new(Secrets::default())).await;
    let stale = fixture_with_host(first.host.clone(), Arc::new(Secrets::default())).await;
    create(&first, "one").await;
    let durable = snapshot(&first).await;
    let error = stale
        .backend
        .dispatch(request("createProvider", configuration("two")))
        .await
        .unwrap_err();
    assert_eq!(error.code, "revision_conflict");
    assert_eq!(snapshot(&first).await, durable);
    assert!(matches!(
        stale.connections.list(),
        Err(ConnectionError::AuthorityPoisoned)
    ));
    assert_eq!(stale.secrets.puts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_after_publication_keeps_metadata_and_releases_writer_reservation() {
    let database = Database::new();
    let first = fixture(&database.path(), Arc::new(Secrets::default())).await;
    let original = create(&first, "provider").await;
    first.secrets.block_put.store(true, Ordering::SeqCst);
    let backend = first.backend.clone();
    let job = tokio::spawn(async move {
        backend
            .dispatch(provider_request("updateProvider", "provider", changed()))
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        first.secrets.started.notified(),
    )
    .await
    .unwrap();
    let durable = snapshot(&first).await;
    assert!(durable["repository"]["last_generation"].as_u64().unwrap() > original.generation);
    assert_eq!(
        first.connections.get(&original.id),
        Err(ConnectionError::MutationPending)
    );
    assert_eq!(
        first.connections.delete(&original.id, original.generation),
        Err(ConnectionError::MutationPending)
    );
    concurrent_update_refused(&first).await;
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    let committed = first.connections.get(&original.id).unwrap();
    assert_eq!(
        committed.generation,
        durable["repository"]["last_generation"].as_u64().unwrap()
    );
    assert!(matches!(
        first
            .secrets
            .get(&first.native.credential_key(&original.id).unwrap())
            .await,
        Err(ConnectionError::SecretMissing)
    ));
    first.secrets.block_put.store(false, Ordering::SeqCst);
    let secrets = first.secrets.clone();
    drop(first);
    let restarted = fixture(&database.path(), secrets).await;
    assert_eq!(restarted.connections.get(&original.id).unwrap(), committed);
}

#[tokio::test]
async fn cancellation_while_sql_commit_queued_poisons_until_durable_reopen() {
    let database = Database::new();
    let first = fixture(&database.path(), Arc::new(Secrets::default())).await;
    let original = create(&first, "provider").await;
    let lock = rusqlite::Connection::open(database.path()).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let backend = first.backend.clone();
    let job = tokio::spawn(async move {
        backend
            .dispatch(provider_request("updateProvider", "provider", changed()))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !matches!(
            first.connections.get(&original.id),
            Err(ConnectionError::MutationPending)
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    assert_eq!(
        first.connections.get(&original.id),
        Err(ConnectionError::AuthorityPoisoned)
    );
    lock.execute_batch("COMMIT").unwrap();
    let durable = snapshot(&first).await;
    let committed = durable["repository"]["last_generation"].as_u64().unwrap();
    assert!(committed > original.generation);
    let secrets = first.secrets.clone();
    drop(first);
    let restarted = fixture(&database.path(), secrets).await;
    assert_eq!(
        restarted.connections.get(&original.id).unwrap().generation,
        committed
    );
    restarted
        .backend
        .dispatch(provider_request("updateProvider", "provider", changed()))
        .await
        .unwrap();
    assert!(restarted.connections.get(&original.id).unwrap().generation > committed);
}

#[tokio::test]
async fn delete_snapshot_rollback_preserves_model_catalog_and_connection() {
    let database = Database::new();
    let first = fixture(&database.path(), Arc::new(Secrets::default())).await;
    let original = create(&first, "provider").await;
    first
        .host
        .store()
        .set_setting("http.provider-catalog", "provider", json!([{"id":"model"}]))
        .await
        .unwrap();
    database.reject_snapshot(true);
    assert!(
        first
            .backend
            .dispatch(provider_request("deleteProvider", "provider", Value::Null))
            .await
            .is_err()
    );
    assert_eq!(first.connections.get(&original.id).unwrap(), original);
    assert_eq!(
        first
            .host
            .store()
            .get_setting("http.provider-model", "provider")
            .await
            .unwrap(),
        Some(json!("model"))
    );
    assert_eq!(
        first
            .host
            .store()
            .get_setting("http.provider-catalog", "provider")
            .await
            .unwrap(),
        Some(json!([{"id":"model"}]))
    );
    assert!(
        first
            .secrets
            .get(&first.native.credential_key(&original.id).unwrap())
            .await
            .is_ok()
    );
    database.reject_snapshot(false);
}

async fn concurrent_update_refused(first: &Fixture) {
    let concurrent = DesktopBackend::open(first.host.clone(), actor(), TOKEN, "model")
        .await
        .unwrap()
        .with_connections(
            first.connections.clone(),
            Some(first.native.clone()),
            Some(first.secrets.clone()),
        );
    let error = concurrent
        .dispatch(provider_request("updateProvider", "provider", changed()))
        .await
        .unwrap_err();
    assert_eq!(error.code, "connection_mutation_pending");
}
