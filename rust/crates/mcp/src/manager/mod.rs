mod config;
mod ports;
mod session;
use crate::{BridgeError, McpClientError, McpToolSet, model};
use agentkit_core::Tool;
pub use config::{Resilience, ServerConfig, TransportConfig};
pub use ports::{
    DefaultConnector, ManagerClock, McpConnection, McpConnector, McpSecretResolver, SystemClock,
    ToolBinding,
};
use session::Session;
use std::{
    collections::BTreeSet,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Default, Debug)]
pub struct ConnectReport {
    pub connected: Vec<String>,
    pub failed: Vec<(String, McpClientError)>,
    pub skipped: Vec<String>,
}
pub struct ContributionReport {
    pub tools: Vec<Arc<dyn Tool>>,
    pub connections: ConnectReport,
}

/// Owns native connections; callers persist only ServerConfig declarations and secret refs.
pub struct McpClientManager {
    sessions: RwLock<Vec<Arc<Session>>>,
    secrets: Arc<dyn McpSecretResolver>,
    connector: Arc<dyn McpConnector>,
    clock: Arc<dyn ManagerClock>,
    updates: Mutex<()>,
    disposed: AtomicBool,
}
impl McpClientManager {
    pub fn new(
        configs: Vec<ServerConfig>,
        secrets: Arc<dyn McpSecretResolver>,
    ) -> Result<Self, McpClientError> {
        Self::with_ports(
            configs,
            secrets,
            Arc::new(DefaultConnector),
            Arc::new(SystemClock),
        )
    }
    pub fn with_ports(
        configs: Vec<ServerConfig>,
        secrets: Arc<dyn McpSecretResolver>,
        connector: Arc<dyn McpConnector>,
        clock: Arc<dyn ManagerClock>,
    ) -> Result<Self, McpClientError> {
        let configs = validate(configs)?;
        let sessions = configs
            .into_iter()
            .map(|config| {
                Arc::new(Session::new(
                    config,
                    secrets.clone(),
                    connector.clone(),
                    clock.clone(),
                ))
            })
            .collect();
        Ok(Self {
            sessions: RwLock::new(sessions),
            secrets,
            connector,
            clock,
            updates: Mutex::new(()),
            disposed: AtomicBool::new(false),
        })
    }
    fn session(&self, alias: &str) -> Result<Arc<Session>, McpClientError> {
        if self.disposed.load(Ordering::Acquire) {
            return Err(McpClientError::Closed);
        }
        self.sessions
            .read()
            .unwrap()
            .iter()
            .find(|session| session.config.alias == alias)
            .cloned()
            .ok_or(McpClientError::Closed)
    }
    pub fn aliases(&self) -> Vec<String> {
        self.sessions
            .read()
            .unwrap()
            .iter()
            .map(|session| session.config.alias.clone())
            .collect()
    }
    pub fn connected_aliases(&self) -> Vec<String> {
        self.sessions
            .read()
            .unwrap()
            .iter()
            .filter(|session| session.connected())
            .map(|session| session.config.alias.clone())
            .collect()
    }
    pub async fn connect(
        &self,
        alias: &str,
        cancellation: CancellationToken,
    ) -> Result<(), McpClientError> {
        self.session(alias)?.connect(cancellation).await
    }
    pub async fn close(&self, alias: &str) -> Result<(), McpClientError> {
        self.session(alias)?.close_session().await
    }
    pub async fn connect_all(&self, cancellation: CancellationToken) -> ConnectReport {
        let sessions = self.sessions.read().unwrap().clone();
        let mut report = ConnectReport::default();
        for session in sessions {
            let alias = session.config.alias.clone();
            if !session.config.enabled {
                report.skipped.push(alias);
                continue;
            }
            match session.connect(cancellation.clone()).await {
                Ok(()) => report.connected.push(alias),
                Err(error) => report.failed.push((alias, error)),
            }
        }
        report
    }
    pub async fn list_tools(
        &self,
        alias: &str,
        cancellation: CancellationToken,
    ) -> Result<Vec<model::Tool>, McpClientError> {
        self.session(alias)?.list_tools(cancellation).await
    }
    pub async fn call_tool(
        &self,
        canonical_id: &str,
        arguments: model::JsonObject,
        cancellation: CancellationToken,
    ) -> Result<model::CallToolResult, McpClientError> {
        let rest = canonical_id
            .strip_prefix("mcp.")
            .ok_or(McpClientError::Configuration)?;
        let (alias, effective) = rest.split_once('.').ok_or(McpClientError::Configuration)?;
        if !crate::bridge::tool_name(effective) {
            return Err(McpClientError::Configuration);
        }
        let session = self.session(alias)?;
        // The same inverse the bridge projected with: a name the config has
        // renamed away is not this server's address, so it never reaches a
        // callback whose canonical identity is something else.
        let original = crate::bridge::resolve_original(&session.config.tool_aliases, effective)
            .ok_or(McpClientError::Configuration)?
            .to_owned();
        session.call_tool(&original, arguments, cancellation).await
    }
    /// The `(registration, generation)` of the live transport behind an
    /// alias, or `None` when the alias is unknown or has no transport. A
    /// host-side descriptor cache captures this alongside its listing so it
    /// can dispatch through [`Self::call_tool_bound`] later.
    pub fn binding(&self, alias: &str) -> Option<ToolBinding> {
        let session = self.session(alias).ok()?;
        session.binding()
    }
    /// [`Self::call_tool`] pinned to one transport generation: the call
    /// only ever reaches the session AND the connection `binding` names.
    /// A descriptor listed from generation N dispatches on generation N —
    /// never on a reconnect's replacement (that fails `Authority` at the
    /// session pin and again at the wire-last bound guard), exactly like
    /// the contributed-tool lane pins its handles (see `bridge::RemoteTool`).
    pub async fn call_tool_bound(
        &self,
        canonical_id: &str,
        binding: ToolBinding,
        arguments: model::JsonObject,
        cancellation: CancellationToken,
    ) -> Result<model::CallToolResult, McpClientError> {
        let rest = canonical_id
            .strip_prefix("mcp.")
            .ok_or(McpClientError::Configuration)?;
        let (alias, effective) = rest.split_once('.').ok_or(McpClientError::Configuration)?;
        if !crate::bridge::tool_name(effective) {
            return Err(McpClientError::Configuration);
        }
        let session = self.session(alias)?;
        let original = crate::bridge::resolve_original(&session.config.tool_aliases, effective)
            .ok_or(McpClientError::Configuration)?
            .to_owned();
        session
            .call_tool_bound(binding, &original, arguments, cancellation, None)
            .await
    }
    pub async fn contribute(
        &self,
        cancellation: CancellationToken,
    ) -> Result<ContributionReport, BridgeError> {
        let mut connections = self.connect_all(cancellation.clone()).await;
        let mut tools = Vec::new();
        let mut names = BTreeSet::new();
        for alias in connections.connected.clone() {
            let session = self.session(&alias)?;
            let read_only = session.config.read_only;
            let set = McpToolSet::new(&alias, session.config.tool_aliases.clone(), session)?
                .with_read_only(read_only);
            match set.contribute(cancellation.clone()).await {
                Ok(listed) => {
                    for tool in listed {
                        if !names.insert(tool.definition().name.clone()) {
                            return Err(BridgeError::Collision);
                        }
                        tools.push(tool);
                    }
                }
                Err(error) => {
                    connections.connected.retain(|item| item != &alias);
                    connections.failed.push((
                        alias,
                        match error {
                            BridgeError::Client(error) => error,
                            _ => McpClientError::Configuration,
                        },
                    ));
                }
            }
        }
        Ok(ContributionReport { tools, connections })
    }
    /// Swap the whole configuration set atomically, keeping every server the
    /// new set still declares.
    ///
    /// The contract a caller can rely on:
    /// - a configuration batch that fails to validate changes NOTHING (no
    ///   session is revoked, closed, or reordered — the previous set stays
    ///   exactly as it was and stays dispatchable);
    /// - a session whose `ServerConfig` is unchanged is REUSED, so a kept
    ///   server keeps its live transport, its generation counter, and every
    ///   tool handle already contributed from it;
    /// - only sessions the new set DROPS are revoked and closed, and the swap
    ///   is published before their transports are awaited, so no window ever
    ///   exposes an empty or half-installed server list;
    /// - the only error this returns after validation is a failure to CLOSE a
    ///   dropped session. Servers that survived the replacement are untouched
    ///   and remain usable; the dropped session is already out of the routing
    ///   table either way, so retrying the call is always safe.
    ///
    /// Nothing here connects the new side: connect/report failures belong to
    /// [`Self::connect_all`], which never revokes a session it did not open.
    pub async fn replace_configs(&self, configs: Vec<ServerConfig>) -> Result<(), McpClientError> {
        let configs = validate(configs)?;
        let _guard = self.updates.lock().await;
        if self.disposed.load(Ordering::Acquire) {
            return Err(McpClientError::Closed);
        }
        let old = self.sessions.read().unwrap().clone();
        let next: Vec<_> = configs
            .into_iter()
            .map(|config| {
                old.iter()
                    .find(|session| session.config == config)
                    .cloned()
                    .unwrap_or_else(|| {
                        Arc::new(Session::new(
                            config,
                            self.secrets.clone(),
                            self.connector.clone(),
                            self.clock.clone(),
                        ))
                    })
            })
            .collect();
        let removed: Vec<_> = old
            .into_iter()
            .filter(|session| !next.iter().any(|item| Arc::ptr_eq(session, item)))
            .collect();
        for session in &removed {
            session.revoke();
        }
        *self.sessions.write().unwrap() = next;
        close_all(removed).await
    }
    /// Immediately denies retained tool handles; dispose also awaits transport cleanup.
    pub fn revoke(&self) {
        self.disposed.store(true, Ordering::Release);
        for session in self.sessions.read().unwrap().iter() {
            session.revoke();
        }
    }
    pub async fn dispose(&self) -> Result<(), McpClientError> {
        self.revoke();
        let sessions = self.sessions.read().unwrap().clone();
        let _guard = self.updates.lock().await;
        let current = self.sessions.read().unwrap().clone();
        for session in &current {
            session.revoke();
        }
        let mut sessions = sessions;
        for session in current {
            if !sessions.iter().any(|item| Arc::ptr_eq(item, &session)) {
                sessions.push(session);
            }
        }
        close_all(sessions).await
    }
}
async fn close_all(sessions: Vec<Arc<Session>>) -> Result<(), McpClientError> {
    let mut error = None;
    for session in sessions {
        if let Err(failure) = session.close_session().await {
            error = Some(failure);
        }
    }
    error.map_or(Ok(()), Err)
}
impl Drop for McpClientManager {
    fn drop(&mut self) {
        for session in self.sessions.read().unwrap().iter() {
            session.revoke();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let session = session.clone();
                runtime.spawn(async move {
                    let _ = session.close_session().await;
                });
            }
        }
    }
}
fn validate(configs: Vec<ServerConfig>) -> Result<Vec<ServerConfig>, McpClientError> {
    let mut aliases = BTreeSet::new();
    configs
        .into_iter()
        .map(|config| {
            let config = config.normalize()?;
            if !aliases.insert(config.alias.clone()) {
                return Err(McpClientError::Configuration);
            }
            Ok(config)
        })
        .collect()
}
pub(crate) async fn check_dispatch(
    guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
    cancellation: &CancellationToken,
) -> Result<(), McpClientError> {
    if let Some(guard) = guard
        && guard.check().await.is_err()
    {
        cancellation.cancel();
        return Err(McpClientError::Authority);
    }
    if cancellation.is_cancelled() {
        return Err(McpClientError::Cancelled);
    }
    Ok(())
}
