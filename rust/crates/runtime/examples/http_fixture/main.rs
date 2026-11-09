//! Test server backed by the real native host and SQLite. Domain I/O is scripted.
mod backend;
mod callback;
mod contributor;
mod mcp_secrets;
mod ports;
mod provider;

use agentkit_host::{SystemRuntime, ToolContributor};
use agentkit_http::DesktopBackend;
use agentkit_runtime::AgentRuntime;
use std::{collections::BTreeMap, sync::Arc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use agentkit_connections::{
        AppId, ConnectionRepository, MemorySecretStore, NativeConnections, UnavailableNetwork,
    };
    use agentkit_http::{Actor, Config};
    let path = std::env::args()
        .nth(1)
        .ok_or("SQLite path argument is required")?;
    let token = std::env::var("AGENTKIT_TEST_TOKEN")?;
    let store = agentkit_store::SqliteStore::open(&path, 64)?;
    let scripted = Arc::new(provider::Scripted::default());
    let provider: Arc<dyn agentkit_core::ChatProvider> =
        if let Ok(url) = std::env::var("AGENTKIT_TEST_PROVIDER_URL") {
            Arc::new(callback::Callback {
                url,
                token: std::env::var("AGENTKIT_TEST_PROVIDER_TOKEN")?,
            })
        } else {
            scripted.clone()
        };
    let mut contributors: Vec<Arc<dyn ToolContributor>> = Vec::new();
    if let Ok(names) = std::env::var("AGENTKIT_TEST_CONTRIBUTORS") {
        let names: Vec<String> = serde_json::from_str(&names)?;
        let url = std::env::var("AGENTKIT_TEST_PROVIDER_URL")?;
        let token = std::env::var("AGENTKIT_TEST_PROVIDER_TOKEN")?;
        contributors = names
            .into_iter()
            .enumerate()
            .map(|(index, namespace)| {
                Arc::new(contributor::Contributor {
                    index,
                    namespace,
                    url: url.trim_end_matches("/provider").into(),
                    token: token.clone(),
                }) as Arc<dyn ToolContributor>
            })
            .collect();
    }
    // The builder recovers and starts the worker; the fixture keeps the
    // handle so it can shut the worker down after the server stops.
    let mut runtime = AgentRuntime::builder(store.clone())
        .default_provider(provider)
        .extra_contributors(contributors)
        .build()
        .await?;
    let host = runtime.host().clone();
    let native_ports = Arc::new(ports::FixturePorts {
        store: store.clone(),
    });
    let proposals = Arc::new(agentkit_host::ProposalService::new(
        store.clone(),
        native_ports.clone(),
        native_ports.clone(),
        Arc::new(SystemRuntime),
    ));
    ports::seed(&store).await?;
    let actor = Actor {
        id: "fixture-actor".into(),
        metadata: BTreeMap::new(),
    };
    let repository = if let Some(snapshot) = store.get_setting("http", "connections").await? {
        ConnectionRepository::restore(serde_json::from_value(
            snapshot.get("repository").cloned().unwrap_or(snapshot),
        )?)?
    } else {
        ConnectionRepository::default()
    };
    let connections = repository.for_app(AppId::new("fixture-actor")?);
    let secrets = Arc::new(MemorySecretStore::default());
    let native = Arc::new(NativeConnections::new(
        connections.clone(),
        secrets.clone(),
        Arc::new(UnavailableNetwork),
    ));
    let mcp = Arc::new(
        agentkit_host::McpConfigRepository::new(
            store.clone(),
            "fixture-actor",
            Arc::new(SystemRuntime),
        )?
        .with_ports(
            Some(Arc::new(mcp_secrets::FixtureVault::open(
                std::path::Path::new(&path),
            )?)),
            None,
        )?,
    );
    mcp.initialize().await?;
    let mut backend = DesktopBackend::open(host.clone(), actor, &token, "m1")
        .await
        .map_err(|e| format!("{}: {}", e.code, e.detail))?
        .with_connections(
            connections.clone(),
            Some(native.clone()),
            Some(secrets.clone()),
        )
        .with_pinned_provider("p1")
        .with_proposals(proposals)
        .with_write_policy(native_ports.clone())
        .with_mcp_configuration(mcp)
        .with_tool_catalog(native_ports.clone());
    if std::env::var("AGENTKIT_TEST_COMPAT").as_deref() != Ok("1") {
        backend = backend.with_provider_operations(native_ports);
    }
    if std::env::var("AGENTKIT_TEST_COMPAT").as_deref() == Ok("1") {
        ports::seed_compat(&store, &backend)
            .await
            .map_err(|e| format!("{}: {}", e.code, e.detail))?;
    }
    let control = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let control_url = format!("http://{}", control.local_addr()?);
    let control_token = std::env::var("AGENTKIT_TEST_CONTROL_TOKEN")?;
    let control_store = store.clone();
    tokio::spawn(async move {
        axum::serve(
            control,
            provider::control_router(
                scripted,
                control_token,
                control_store,
                native,
                secrets,
                connections,
            ),
        )
        .await
        .unwrap();
    });
    let worker = runtime
        .take_worker()
        .ok_or("runtime worker already taken")?;
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    println!(
        "{}",
        serde_json::json!({"baseUrl":format!("http://{}",socket.local_addr()?),"controlUrl":control_url})
    );
    let mut config = Config::default();
    config.streaming.poll_interval = std::time::Duration::from_millis(2);
    // Match the TypeScript harness's `streaming` block: a reconnect must be
    // immediate enough that a test severing a stream ~100 times finishes
    // inside its budget, and the heartbeat must not interleave test frames.
    config.streaming.retry_hint = std::time::Duration::from_millis(10);
    config.streaming.heartbeat_interval = std::time::Duration::from_secs(60);
    let read_hook = std::env::var("AGENTKIT_TEST_PROVIDER_URL").ok().map(|url| {
        (
            format!("{}/events-read", url.trim_end_matches("/provider")),
            std::env::var("AGENTKIT_TEST_PROVIDER_TOKEN").unwrap(),
        )
    });
    let backend = backend::FixtureBackend {
        inner: Arc::new(backend),
        read_hook,
    };
    axum::serve(socket, agentkit_http::router(Arc::new(backend), config)).await?;
    worker.shutdown().await?;
    Ok(())
}
