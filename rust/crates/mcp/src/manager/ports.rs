use super::TransportConfig;
use crate::{ClientOptions, McpClient, McpClientError, model};
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

/// The exact connection a tool handle was minted from.
///
/// A handle is only ever valid for the `(registration, generation)` pair it was
/// listed from: `registration` identifies ONE `Session` record installed under
/// an alias (a config replacement installs a new one), and `generation`
/// identifies one established transport of that session (every reconnect
/// installs a new one, so a second server process behind the same alias is a
/// different generation). Both halves are checked again at dispatch — see
/// [`McpConnection::call_tool_bound`] — so a descriptor from one generation can
/// never authorize a callback belonging to another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolBinding {
    pub registration: u64,
    pub generation: u64,
}

#[async_trait]
pub trait McpConnection: Send + Sync {
    async fn list_tools(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<model::Tool>, McpClientError>;
    /// Closed proves rejection before dispatch. A closed transport after dispatch is Disconnected.
    async fn call_tool(
        &self,
        name: &str,
        arguments: model::JsonObject,
        cancellation: CancellationToken,
    ) -> Result<model::CallToolResult, McpClientError>;
    async fn close(&self) -> Result<(), McpClientError>;
    async fn call_tool_guarded(
        &self,
        name: &str,
        arguments: model::JsonObject,
        cancellation: CancellationToken,
        guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
    ) -> Result<model::CallToolResult, McpClientError> {
        super::check_dispatch(guard, &cancellation).await?;
        self.call_tool(name, arguments, cancellation).await
    }
    /// The connection a handle must be pinned to, or `None` for a connection
    /// that has no registration (a bare [`McpClient`], a test double). A
    /// manager session answers with `(registration, generation)` of its LIVE
    /// transport and `None` whenever it has none, so a catalogue can never be
    /// minted against a connection that is not currently established.
    fn binding(&self) -> Option<ToolBinding> {
        None
    }
    /// Dispatch on the ONE connection `binding` names, or refuse.
    ///
    /// The invariant is that the callback which runs is the transport this
    /// handle was listed from — not whichever one the session holds now. A
    /// session that owns generations re-verifies the binding on BOTH sides of
    /// the host's authorization: up front (a handle whose transport is already
    /// gone never reaches the guard) and from inside the guard the connection
    /// lane consults immediately before it calls out, host guard first, so a
    /// lease that expired during a reconnect is still reported by the lease.
    /// A reconnected or replaced transport therefore fails with
    /// [`McpClientError::Authority`] instead of serving the stale handle.
    /// Connections that own no registrations (a bare [`McpClient`], a test
    /// double) have one transport and keep the guarded path.
    async fn call_tool_bound(
        &self,
        binding: ToolBinding,
        name: &str,
        arguments: model::JsonObject,
        cancellation: CancellationToken,
        guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
    ) -> Result<model::CallToolResult, McpClientError> {
        let _ = binding;
        self.call_tool_guarded(name, arguments, cancellation, guard)
            .await
    }
}
#[async_trait]
impl McpConnection for McpClient {
    async fn list_tools(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<model::Tool>, McpClientError> {
        self.list_tools(cancellation).await
    }
    async fn call_tool(
        &self,
        name: &str,
        arguments: model::JsonObject,
        cancellation: CancellationToken,
    ) -> Result<model::CallToolResult, McpClientError> {
        self.call_tool(name, arguments, cancellation).await
    }
    async fn close(&self) -> Result<(), McpClientError> {
        self.close().await
    }
    async fn call_tool_guarded(
        &self,
        name: &str,
        arguments: model::JsonObject,
        cancellation: CancellationToken,
        guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
    ) -> Result<model::CallToolResult, McpClientError> {
        self.call_tool_guarded(name, arguments, cancellation, guard)
            .await
    }
}

#[async_trait]
pub trait McpSecretResolver: Send + Sync {
    async fn get(&self, reference: &str) -> Result<Option<String>, McpClientError>;
    /// Changes whenever any referenced value rotates or becomes inaccessible.
    async fn generation(
        &self,
        _references: &BTreeMap<String, String>,
    ) -> Result<u64, McpClientError> {
        Ok(0)
    }
}

#[async_trait]
pub trait McpConnector: Send + Sync {
    /// Named connection context permits host ports to bind OAuth and elicitation
    /// to a configured alias rather than to a global current account.
    async fn connect_named(
        &self,
        _alias: &str,
        transport: TransportConfig,
        options: ClientOptions,
        cancellation: CancellationToken,
    ) -> Result<Arc<dyn McpConnection>, McpClientError> {
        self.connect(transport, options, cancellation).await
    }
    /// The future must release partially opened transports when dropped. Any detached
    /// operation must observe cancellation and close a client before returning it late.
    async fn connect(
        &self,
        transport: TransportConfig,
        options: ClientOptions,
        cancellation: CancellationToken,
    ) -> Result<Arc<dyn McpConnection>, McpClientError>;
}

pub struct DefaultConnector;
#[async_trait]
impl McpConnector for DefaultConnector {
    async fn connect(
        &self,
        transport: TransportConfig,
        options: ClientOptions,
        cancellation: CancellationToken,
    ) -> Result<Arc<dyn McpConnection>, McpClientError> {
        let connect = async {
            let client = match transport {
                TransportConfig::Stdio { command, args, env } => {
                    let mut environment = inherited_environment();
                    environment.extend(env);
                    McpClient::stdio(&command, &args, &environment, options).await?
                }
                TransportConfig::Http { url, headers } => {
                    let mut values = http::HeaderMap::new();
                    for (name, value) in headers {
                        values.insert(
                            http::HeaderName::try_from(name)
                                .map_err(|_| McpClientError::Configuration)?,
                            http::HeaderValue::try_from(value)
                                .map_err(|_| McpClientError::Configuration)?,
                        );
                    }
                    McpClient::http_with_headers(&url, None, values, options).await?
                }
            };
            Ok(Arc::new(client) as Arc<dyn McpConnection>)
        };
        tokio::select! {biased; _ = cancellation.cancelled() => Err(McpClientError::Cancelled), result = connect => result}
    }
}

fn inherited_environment() -> BTreeMap<String, String> {
    let names = if cfg!(windows) {
        &[
            "APPDATA",
            "HOMEDRIVE",
            "HOMEPATH",
            "LOCALAPPDATA",
            "PATH",
            "PROCESSOR_ARCHITECTURE",
            "SYSTEMDRIVE",
            "SYSTEMROOT",
            "TEMP",
            "USERNAME",
            "USERPROFILE",
            "PROGRAMFILES",
        ][..]
    } else {
        &["HOME", "LOGNAME", "PATH", "SHELL", "TERM", "USER"][..]
    };
    names
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.starts_with("()"))
                .map(|value| ((*name).into(), value))
        })
        .collect()
}

#[async_trait]
pub trait ManagerClock: Send + Sync {
    fn now_ms(&self) -> u64;
    async fn sleep(
        &self,
        duration: Duration,
        cancellation: CancellationToken,
    ) -> Result<(), McpClientError>;
}
pub struct SystemClock;
#[async_trait]
impl ManagerClock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
    async fn sleep(
        &self,
        duration: Duration,
        cancellation: CancellationToken,
    ) -> Result<(), McpClientError> {
        tokio::select! {biased; _ = cancellation.cancelled() => Err(McpClientError::Cancelled), _ = tokio::time::sleep(duration) => Ok(())}
    }
}
