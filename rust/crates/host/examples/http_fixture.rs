//! Test server backed by the real native host and SQLite. Domain I/O is scripted.
#[cfg(feature = "http")]
#[path = "http_fixture/ports.rs"]
mod ports;

#[cfg(feature = "http")]
#[path = "http_fixture/backend.rs"]
mod backend;
#[cfg(feature = "http")]
#[path = "http_fixture/callback.rs"]
mod callback;
#[cfg(feature = "http")]
#[path = "http_fixture/contributor.rs"]
mod contributor;
#[cfg(feature = "http")]
#[path = "http_fixture/provider.rs"]
mod provider;
#[cfg(feature = "http")]
use agentkit_core::ToolRegistry;
#[cfg(feature = "http")]
use agentkit_host::{
    AgentHost, ChatExecutor, HostOptions, SystemRuntime, TaskExecutor, http::DesktopBackend,
};
#[cfg(feature = "http")]
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

#[cfg(feature = "http")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use agentkit_connections::{
        AppId, ConnectionRepository, MemorySecretStore, NativeConnections, UnavailableNetwork,
    };
    use agentkit_transport_http::{Actor, Config};
    let path = std::env::args()
        .nth(1)
        .ok_or("SQLite path argument is required")?;
    let token = std::env::var("AGENTKIT_TEST_TOKEN")?;
    let store = agentkit_store::SqliteStore::open(path, 64)?;
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
    let mut options = agentkit_host::ChatExecutorOptions::default();
    if let Ok(names) = std::env::var("AGENTKIT_TEST_CONTRIBUTORS") {
        let names: Vec<String> = serde_json::from_str(&names)?;
        let url = std::env::var("AGENTKIT_TEST_PROVIDER_URL")?;
        let token = std::env::var("AGENTKIT_TEST_PROVIDER_TOKEN")?;
        options.tool_contributors = names
            .into_iter()
            .enumerate()
            .map(|(index, namespace)| {
                Arc::new(contributor::Contributor {
                    index,
                    namespace,
                    url: url.trim_end_matches("/provider").into(),
                    token: token.clone(),
                }) as Arc<dyn agentkit_host::ToolContributor>
            })
            .collect();
    }
    let executor: Arc<dyn TaskExecutor> =
        Arc::new(ChatExecutor::new(provider, Arc::new(ToolRegistry::new())).with_options(options));
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([("chat.turn".into(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )?;
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
    let mcp = Arc::new(agentkit_host::McpConfigRepository::new(
        store.clone(),
        "fixture-actor",
        Arc::new(SystemRuntime),
    )?);
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
    let worker = host.start_worker();
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    println!(
        "{}",
        serde_json::json!({"baseUrl":format!("http://{}",socket.local_addr()?),"controlUrl":control_url})
    );
    let mut config = Config::default();
    config.streaming.poll_interval = std::time::Duration::from_millis(2);
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
    axum::serve(
        socket,
        agentkit_transport_http::router(Arc::new(backend), config),
    )
    .await?;
    worker.shutdown().await?;
    Ok(())
}
#[cfg(not(feature = "http"))]
fn main() {
    eprintln!("Enable feature http to run the native HTTP fixture.");
    std::process::exit(1);
}
