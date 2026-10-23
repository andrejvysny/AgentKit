use agentkit_contracts::{AiProviderModel, TestProviderResponse};
use agentkit_host::http::ProviderOperations;
use agentkit_transport_http::Problem;

struct RefreshPort {
    backend: Arc<DesktopBackend>,
    mutate: bool,
}
#[async_trait]
impl ProviderOperations for RefreshPort {
    async fn refresh_models(
        &self,
        _: &Actor,
        provider: &str,
    ) -> Result<Vec<AiProviderModel>, Problem> {
        if self.mutate {
            self.backend
                .dispatch(provider_request(
                    "updateProvider",
                    provider,
                    json!({"label":"Changed during refresh"}),
                ))
                .await?;
        }
        Ok(vec![])
    }
    async fn test(&self, _: &Actor, _: &str) -> Result<TestProviderResponse, Problem> {
        Err(Problem::host("unused", "Unused test adapter"))
    }
}

#[tokio::test]
async fn refresh_rejects_catalog_from_previous_generation_and_preserves_current_metadata() {
    let database = Database::new();
    let first = fixture(&database.path(), Arc::new(Secrets::default())).await;
    let original = create(&first, "provider").await;
    let port = Arc::new(RefreshPort {
        backend: first.backend.clone(),
        mutate: true,
    });
    let refreshing = DesktopBackend::open(first.host.clone(), actor(), TOKEN, "model")
        .await
        .unwrap()
        .with_connections(
            first.connections.clone(),
            Some(first.native.clone()),
            Some(first.secrets.clone()),
        )
        .with_provider_operations(port);
    let error = refreshing
        .dispatch(provider_request(
            "refreshProviderModels",
            "provider",
            Value::Null,
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code, "revision_conflict");
    let current = first.connections.get(&original.id).unwrap();
    assert!(current.generation > original.generation);
    assert_eq!(
        snapshot(&first).await["repository"]["last_generation"],
        current.generation
    );
    assert_eq!(
        first
            .host
            .store()
            .get_setting("http.provider-catalog", "provider")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn refresh_commits_catalog_for_unchanged_authority_without_changing_generation() {
    let database = Database::new();
    let first = fixture(&database.path(), Arc::new(Secrets::default())).await;
    let original = create(&first, "provider").await;
    let port = Arc::new(RefreshPort {
        backend: first.backend.clone(),
        mutate: false,
    });
    let refreshing = DesktopBackend::open(first.host.clone(), actor(), TOKEN, "model")
        .await
        .unwrap()
        .with_connections(
            first.connections.clone(),
            Some(first.native.clone()),
            Some(first.secrets.clone()),
        )
        .with_provider_operations(port);
    refreshing
        .dispatch(provider_request(
            "refreshProviderModels",
            "provider",
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(first.connections.get(&original.id).unwrap(), original);
    assert_eq!(
        first
            .host
            .store()
            .get_setting("http.provider-catalog", "provider")
            .await
            .unwrap(),
        Some(json!([]))
    );
}
