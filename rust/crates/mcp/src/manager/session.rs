use super::{
    ManagerClock, McpConnection, McpConnector, McpSecretResolver, ServerConfig, TransportConfig,
};
use crate::{McpClientError, model};
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

struct State {
    client: Option<Arc<dyn McpConnection>>,
    generation: u64,
    secret_generation: Option<u64>,
    opened_at: Option<u64>,
    disposed: bool,
    revoked: bool,
    epoch: u64,
    lifetime: CancellationToken,
}
pub(super) struct Session {
    pub config: ServerConfig,
    secrets: Arc<dyn McpSecretResolver>,
    connector: Arc<dyn McpConnector>,
    clock: Arc<dyn ManagerClock>,
    state: Mutex<State>,
    lifecycle: AsyncMutex<()>,
}
type Connected = (Arc<dyn McpConnection>, u64, CancellationToken);

struct PendingClient(Option<Arc<dyn McpConnection>>);
impl PendingClient {
    async fn close(&mut self) {
        if let Some(client) = self.0.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), client.close()).await;
        }
    }
}
impl Drop for PendingClient {
    fn drop(&mut self) {
        if let Some(client) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(5), client.close()).await;
            });
        }
    }
}

impl Session {
    pub fn new(
        config: ServerConfig,
        secrets: Arc<dyn McpSecretResolver>,
        connector: Arc<dyn McpConnector>,
        clock: Arc<dyn ManagerClock>,
    ) -> Self {
        Self {
            config,
            secrets,
            connector,
            clock,
            lifecycle: AsyncMutex::new(()),
            state: Mutex::new(State {
                client: None,
                generation: 0,
                secret_generation: None,
                opened_at: None,
                disposed: false,
                revoked: false,
                epoch: 0,
                lifetime: CancellationToken::new(),
            }),
        }
    }
    pub fn connected(&self) -> bool {
        let state = self.state.lock().unwrap();
        !state.disposed && !state.revoked && state.client.is_some()
    }
    pub fn revoke(&self) {
        let mut state = self.state.lock().unwrap();
        state.revoked = true;
        state.disposed = true;
        state.epoch = state.epoch.wrapping_add(1);
        state.lifetime.cancel();
    }
    pub async fn connect(&self, cancellation: CancellationToken) -> Result<(), McpClientError> {
        self.ensure(true, cancellation).await.map(|_| ())
    }
    pub async fn close_session(&self) -> Result<(), McpClientError> {
        {
            let mut state = self.state.lock().unwrap();
            state.disposed = true;
            state.epoch = state.epoch.wrapping_add(1);
            state.lifetime.cancel();
        }
        let _guard = self.lifecycle.lock().await;
        self.teardown().await
    }
    async fn teardown(&self) -> Result<(), McpClientError> {
        let client = self.state.lock().unwrap().client.take();
        if let Some(client) = client {
            tokio::time::timeout(Duration::from_secs(5), client.close())
                .await
                .map_err(|_| McpClientError::Timeout)??;
        }
        Ok(())
    }
    async fn ensure(
        &self,
        explicit: bool,
        cancellation: CancellationToken,
    ) -> Result<Connected, McpClientError> {
        let epoch = self.state.lock().unwrap().epoch;
        let _guard = tokio::select! {biased; _ = cancellation.cancelled() => return Err(McpClientError::Cancelled), guard = self.lifecycle.lock() => guard};
        let lifetime = self.prepare(explicit, epoch)?;
        let generation = self
            .bounded(
                self.secrets.generation(&self.config.secret_refs),
                &cancellation,
                &lifetime,
                self.config.resilience.connect_timeout_ms,
            )
            .await;
        let generation = match generation {
            Ok(generation) => generation,
            Err(error) => {
                let _ = self.teardown().await;
                return Err(error);
            }
        };
        let cached = self.cached(generation);
        if let Some(client) = cached {
            return Ok(client);
        }
        self.teardown().await?;
        self.assert_circuit()?;
        self.connect_cycle(generation, cancellation, lifetime).await
    }
    fn prepare(&self, explicit: bool, epoch: u64) -> Result<CancellationToken, McpClientError> {
        let mut state = self.state.lock().unwrap();
        if state.revoked
            || !self.config.enabled
            || state.epoch != epoch
            || state.disposed && !explicit
        {
            return Err(McpClientError::Closed);
        }
        if explicit && state.disposed {
            state.disposed = false;
            state.lifetime = CancellationToken::new();
        }
        Ok(state.lifetime.clone())
    }
    fn cached(&self, generation: u64) -> Option<Connected> {
        let mut state = self.state.lock().unwrap();
        if state
            .secret_generation
            .is_some_and(|previous| previous != generation)
        {
            state.opened_at = None;
            return None;
        }
        state
            .client
            .as_ref()
            .map(|client| (client.clone(), state.generation, state.lifetime.clone()))
    }
    fn assert_circuit(&self) -> Result<(), McpClientError> {
        let mut state = self.state.lock().unwrap();
        if let Some(opened) = state.opened_at {
            if self.clock.now_ms().saturating_sub(opened) < self.config.resilience.circuit_open_ms {
                return Err(McpClientError::CircuitOpen);
            }
            state.opened_at = None;
        }
        Ok(())
    }
    async fn connect_cycle(
        &self,
        secret_generation: u64,
        cancellation: CancellationToken,
        lifetime: CancellationToken,
    ) -> Result<Connected, McpClientError> {
        self.state.lock().unwrap().secret_generation = Some(secret_generation);
        for attempt in 1..=self.config.resilience.max_connect_attempts {
            let result = self
                .bounded(
                    self.open_once(secret_generation),
                    &cancellation,
                    &lifetime,
                    self.config.resilience.connect_timeout_ms,
                )
                .await;
            match result {
                Ok(client) => match self.install(client.clone(), secret_generation) {
                    Ok(connected) => return Ok(connected),
                    Err(error) => {
                        let _ = tokio::time::timeout(Duration::from_secs(5), client.close()).await;
                        return Err(error);
                    }
                },
                Err(error)
                    if matches!(
                        error,
                        McpClientError::Configuration
                            | McpClientError::SecretMissing
                            | McpClientError::Cancelled
                            | McpClientError::Closed
                            | McpClientError::Protocol
                    ) =>
                {
                    return Err(error);
                }
                Err(_) if attempt < self.config.resilience.max_connect_attempts => {
                    self.pause(
                        self.config.resilience.backoff(attempt, 2.0),
                        &cancellation,
                        &lifetime,
                    )
                    .await?;
                }
                Err(_) => break,
            }
        }
        self.state.lock().unwrap().opened_at = Some(self.clock.now_ms());
        Err(McpClientError::Connect)
    }
    fn install(
        &self,
        client: Arc<dyn McpConnection>,
        secret_generation: u64,
    ) -> Result<Connected, McpClientError> {
        let mut state = self.state.lock().unwrap();
        // The lifecycle lock excludes other installers; close/revoke cancels before waiting for it.
        if state.disposed || state.revoked || state.lifetime.is_cancelled() {
            return Err(McpClientError::Closed);
        }
        state.generation = state.generation.wrapping_add(1);
        state.secret_generation = Some(secret_generation);
        state.opened_at = None;
        state.client = Some(client.clone());
        Ok((client, state.generation, state.lifetime.clone()))
    }
    async fn open_once(&self, generation: u64) -> Result<Arc<dyn McpConnection>, McpClientError> {
        let mut values = BTreeMap::new();
        for (placeholder, reference) in &self.config.secret_refs {
            values.insert(
                placeholder.clone(),
                self.secrets
                    .get(reference)
                    .await?
                    .ok_or(McpClientError::SecretMissing)?,
            );
        }
        if self.secrets.generation(&self.config.secret_refs).await? != generation {
            return Err(McpClientError::Closed);
        }
        let mut transport = self.config.transport.clone();
        let record = match &mut transport {
            TransportConfig::Stdio { env, .. } => env,
            TransportConfig::Http { headers, .. } => headers,
        };
        for value in record.values_mut() {
            for (placeholder, secret) in &values {
                *value = value.replace(&format!("${{{placeholder}}}"), secret);
            }
        }
        let cancellation = self.state.lock().unwrap().lifetime.child_token();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let client = self
            .connector
            .connect(
                transport,
                self.config.resilience.client_options(),
                cancellation,
            )
            .await?;
        let mut pending = PendingClient(Some(client));
        let current = self.secrets.generation(&self.config.secret_refs).await;
        if current != Ok(generation) {
            pending.close().await;
            return Err(current.err().unwrap_or(McpClientError::Closed));
        }
        Ok(pending.0.take().expect("unclaimed MCP connection"))
    }
    async fn bounded<T>(
        &self,
        future: impl Future<Output = Result<T, McpClientError>>,
        cancellation: &CancellationToken,
        lifetime: &CancellationToken,
        milliseconds: u64,
    ) -> Result<T, McpClientError> {
        tokio::select! {biased;
            _ = cancellation.cancelled() => Err(McpClientError::Cancelled),
            _ = lifetime.cancelled() => Err(McpClientError::Closed),
            result = tokio::time::timeout(Duration::from_millis(milliseconds), future) => result.map_err(|_| McpClientError::Timeout)?,
        }
    }
    async fn pause(
        &self,
        delay: Duration,
        cancellation: &CancellationToken,
        lifetime: &CancellationToken,
    ) -> Result<(), McpClientError> {
        tokio::select! {biased;
            _ = cancellation.cancelled() => Err(McpClientError::Cancelled),
            _ = lifetime.cancelled() => Err(McpClientError::Closed),
            result = self.clock.sleep(delay, lifetime.child_token()) => result,
        }
    }
    async fn invalidate_generation(&self, generation: u64) -> Result<(), McpClientError> {
        let _guard = self.lifecycle.lock().await;
        if self.state.lock().unwrap().generation != generation {
            return Ok(());
        }
        self.teardown().await
    }
    async fn invoke(
        &self,
        connected: &Connected,
        name: Option<&str>,
        arguments: &model::JsonObject,
        cancellation: &CancellationToken,
        guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
    ) -> Result<Response, McpClientError> {
        let (client, _, lifetime) = connected;
        let operation = async {
            match name {
                Some(name) => client
                    .call_tool_guarded(name, arguments.clone(), cancellation.child_token(), guard)
                    .await
                    .map(Response::Call),
                None => client
                    .list_tools(cancellation.child_token())
                    .await
                    .map(Response::Tools),
            }
        };
        self.bounded(
            operation,
            cancellation,
            lifetime,
            self.config.resilience.request_timeout_ms,
        )
        .await
    }
    async fn retry_after(
        &self,
        error: McpClientError,
        read: bool,
        retries: &mut u32,
        connected: &Connected,
        cancellation: &CancellationToken,
    ) -> Result<(), McpClientError> {
        let (_, generation, lifetime) = connected;
        if error == McpClientError::Closed && lifetime.is_cancelled() {
            return Err(McpClientError::Cancelled);
        }
        if matches!(error, McpClientError::Closed | McpClientError::Disconnected) {
            self.invalidate_generation(*generation).await?;
        }
        // A call may only replay a Closed result whose port proves it was not sent.
        let retry = error == McpClientError::Closed
            || read
                && (error == McpClientError::Disconnected
                    || error == McpClientError::Timeout && self.config.resilience.retry_timeouts);
        if !retry {
            return Err(error);
        }
        if *retries >= self.config.resilience.reconnect_max_attempts {
            return Err(if *retries == 0 {
                error
            } else {
                McpClientError::ReconnectExhausted
            });
        }
        *retries += 1;
        self.pause(
            self.config
                .resilience
                .backoff(*retries, self.config.resilience.reconnect_backoff_factor),
            cancellation,
            lifetime,
        )
        .await?;
        self.invalidate_generation(*generation).await
    }
    async fn request(
        &self,
        name: Option<&str>,
        arguments: model::JsonObject,
        cancellation: CancellationToken,
        guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
    ) -> Result<Response, McpClientError> {
        let mut retries = 0;
        loop {
            let connected = self.ensure(false, cancellation.clone()).await?;
            match self
                .invoke(&connected, name, &arguments, &cancellation, guard.clone())
                .await
            {
                Ok(response) => return Ok(response),
                Err(error) => {
                    self.retry_after(
                        error,
                        name.is_none(),
                        &mut retries,
                        &connected,
                        &cancellation,
                    )
                    .await?
                }
            }
        }
    }
}
enum Response {
    Tools(Vec<model::Tool>),
    Call(model::CallToolResult),
}
#[async_trait]
impl McpConnection for Session {
    async fn list_tools(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<model::Tool>, McpClientError> {
        match self
            .request(None, model::JsonObject::new(), cancellation, None)
            .await?
        {
            Response::Tools(tools) => Ok(tools),
            _ => unreachable!(),
        }
    }
    async fn call_tool(
        &self,
        name: &str,
        arguments: model::JsonObject,
        cancellation: CancellationToken,
    ) -> Result<model::CallToolResult, McpClientError> {
        self.call_tool_guarded(name, arguments, cancellation, None)
            .await
    }
    async fn call_tool_guarded(
        &self,
        name: &str,
        arguments: model::JsonObject,
        cancellation: CancellationToken,
        guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
    ) -> Result<model::CallToolResult, McpClientError> {
        match self
            .request(Some(name), arguments, cancellation, guard)
            .await?
        {
            Response::Call(result) => Ok(result),
            _ => unreachable!(),
        }
    }
    async fn close(&self) -> Result<(), McpClientError> {
        self.close_session().await
    }
}
