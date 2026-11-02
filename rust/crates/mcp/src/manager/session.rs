use super::{
    ManagerClock, McpConnection, McpConnector, McpSecretResolver, ServerConfig, ToolBinding,
    TransportConfig,
};
use crate::{McpClientError, model};
use async_trait::async_trait;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

struct State {
    client: Option<Arc<dyn McpConnection>>,
    /// Resolved secret values for the CURRENT connection, kept only to redact
    /// server-echoed content. Cleared whenever the connection is torn down.
    material: Arc<SecretMaterial>,
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
    /// Which config record this session IS. A config replacement under the
    /// same alias mints a new one, so a tool handle can tell "the transport
    /// behind my server reconnected" apart from "my server was replaced".
    registration: u64,
    secrets: Arc<dyn McpSecretResolver>,
    connector: Arc<dyn McpConnector>,
    clock: Arc<dyn ManagerClock>,
    state: Arc<Mutex<State>>,
    lifecycle: AsyncMutex<()>,
}

/// Process-wide registration counter; two sessions never share one id, so a
/// handle can never be re-admitted by a look-alike replacement.
static REGISTRATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

type Connected = (
    Arc<dyn McpConnection>,
    u64,
    CancellationToken,
    Arc<SecretMaterial>,
);

/// Resolved secret material for ONE connection, mirroring the TypeScript
/// `createSecretMaterial` discipline: whatever value we substituted INTO a
/// transport config, we substitute back OUT of anything the server echoes.
/// Redaction is a bounded case-sensitive replace per secret (longest value
/// first so a token containing a shorter one leaves no readable suffix) —
/// no regexes, O(secrets x text length).
#[derive(Default)]
pub(super) struct SecretMaterial {
    values: Vec<String>,
}

const REDACTION: &str = "[redacted]";

impl SecretMaterial {
    fn new<'a>(resolved: impl IntoIterator<Item = &'a String>) -> Self {
        let mut values: Vec<String> = resolved
            .into_iter()
            .filter(|value| !value.is_empty())
            .cloned()
            .collect();
        values.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        values.dedup();
        Self { values }
    }
    fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    fn redact(&self, text: &str) -> String {
        if self.is_empty() {
            return text.to_owned();
        }
        let mut out = text.to_owned();
        for value in &self.values {
            out = out.replace(value.as_str(), REDACTION);
        }
        out
    }
    fn redact_error(&self, error: McpClientError) -> McpClientError {
        match error {
            McpClientError::Remote(message) if !self.is_empty() => {
                McpClientError::Remote(self.redact(&message))
            }
            error => error,
        }
    }
}

fn redact_json(value: &mut Value, material: &SecretMaterial) {
    match value {
        Value::String(text) => *text = material.redact(text),
        Value::Array(items) => {
            for item in items {
                redact_json(item, material);
            }
        }
        Value::Object(entries) => {
            for (_, item) in entries {
                redact_json(item, material);
            }
        }
        _ => {}
    }
}

fn redact_call(result: &mut model::CallToolResult, material: &SecretMaterial) {
    if material.is_empty() {
        return;
    }
    for block in &mut result.content {
        match block {
            model::ContentBlock::Text(text) => text.text = material.redact(&text.text),
            model::ContentBlock::Resource(embedded) => {
                if let model::ResourceContents::TextResourceContents { text, .. } =
                    &mut embedded.resource
                {
                    *text = material.redact(text);
                }
            }
            _ => {}
        }
    }
    if let Some(structured) = &mut result.structured_content {
        redact_json(structured, material);
    }
}

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
            registration: REGISTRATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            secrets,
            connector,
            clock,
            lifecycle: AsyncMutex::new(()),
            state: Arc::new(Mutex::new(State {
                client: None,
                material: Arc::new(SecretMaterial::default()),
                generation: 0,
                secret_generation: None,
                opened_at: None,
                disposed: false,
                revoked: false,
                epoch: 0,
                lifetime: CancellationToken::new(),
            })),
        }
    }
    // Every `state` lock site in this file recovers from poisoning rather
    // than panicking: `State` holds independent lifecycle flags, counters, and
    // `Option` slots, and every reader decides conservatively (a missing
    // client / disposed / revoked state yields `Closed` or a reconnect), so a
    // panic mid-update can only leave a field at a previously valid value --
    // the invariant is restore-safe, and the registration counter still bars a
    // stale transport from re-admission.
    pub fn connected(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        !state.disposed && !state.revoked && state.client.is_some()
    }
    pub fn revoke(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.disposed = true;
            state.epoch = state.epoch.wrapping_add(1);
            state.lifetime.cancel();
        }
        let _guard = self.lifecycle.lock().await;
        self.teardown().await
    }
    async fn teardown(&self) -> Result<(), McpClientError> {
        let client = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let client = state.client.take();
            state.material = Arc::new(SecretMaterial::default());
            client
        };
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
        let epoch = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .epoch;
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
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state
            .secret_generation
            .is_some_and(|previous| previous != generation)
        {
            state.opened_at = None;
            return None;
        }
        state.client.as_ref().map(|client| {
            (
                client.clone(),
                state.generation,
                state.lifetime.clone(),
                state.material.clone(),
            )
        })
    }
    fn assert_circuit(&self) -> Result<(), McpClientError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .secret_generation = Some(secret_generation);
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
                Ok((client, material)) => {
                    match self.install(client.clone(), material.clone(), secret_generation) {
                        Ok(connected) => return Ok(connected),
                        Err(error) => {
                            let _ =
                                tokio::time::timeout(Duration::from_secs(5), client.close()).await;
                            return Err(error);
                        }
                    }
                }
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
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .opened_at = Some(self.clock.now_ms());
        Err(McpClientError::Connect)
    }
    fn install(
        &self,
        client: Arc<dyn McpConnection>,
        material: Arc<SecretMaterial>,
        secret_generation: u64,
    ) -> Result<Connected, McpClientError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The lifecycle lock excludes other installers; close/revoke cancels before waiting for it.
        if state.disposed || state.revoked || state.lifetime.is_cancelled() {
            return Err(McpClientError::Closed);
        }
        state.generation = state.generation.wrapping_add(1);
        state.secret_generation = Some(secret_generation);
        state.opened_at = None;
        state.client = Some(client.clone());
        state.material = material.clone();
        Ok((client, state.generation, state.lifetime.clone(), material))
    }
    async fn open_once(
        &self,
        generation: u64,
    ) -> Result<(Arc<dyn McpConnection>, Arc<SecretMaterial>), McpClientError> {
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
        let material = Arc::new(SecretMaterial::new(values.values()));
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
        let cancellation = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .lifetime
            .child_token();
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
        Ok((
            pending.0.take().expect("unclaimed MCP connection"),
            material,
        ))
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
        if self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .generation
            != generation
        {
            return Ok(());
        }
        self.teardown().await
    }
    /// The `(registration, generation)` of the live transport, or `None` when
    /// this session has none.
    fn live_binding(&self) -> Option<ToolBinding> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (!state.disposed && !state.revoked && state.client.is_some()).then(|| ToolBinding {
            registration: self.registration,
            generation: state.generation,
        })
    }
    /// Capture the ONE connection `want` designates, refusing anything else.
    ///
    /// Every check happens under the single state lock, so the connection this
    /// returns IS the transport the binding named at that instant — a
    /// replacement (new registration), a revoke/dispose, or a reconnect (new
    /// generation) each fail it. `Closed` reports a session that no longer has
    /// a transport at all; `Authority` reports a handle whose transport has
    /// been superseded while the server itself stayed connected.
    fn pinned(&self, want: ToolBinding) -> Result<Connected, McpClientError> {
        pinned_in(&self.state, self.registration, want)
    }
    /// Dispatch pinned to one binding through the session's ordinary request
    /// machinery (same `ensure`/reconnect/retry semantics as the unbound lane),
    /// with the handle verified on BOTH sides of the host's authorization.
    ///
    /// The up-front check is the authorization side: a handle whose transport
    /// is gone never reaches the guard. The execution side runs INSIDE the
    /// guard the connection lane consults immediately before it dispatches —
    /// wrapped, so the host's own verdict is still taken first (a lease that
    /// expired while the session reconnected is reported by the lease, exactly
    /// as on the unbound path), and only then the handle. Dispatch therefore
    /// cannot happen on a transport that stopped being the one this handle was
    /// minted from, with no await left between the two decisions.
    async fn call_bound(
        &self,
        want: ToolBinding,
        name: &str,
        arguments: &model::JsonObject,
        cancellation: CancellationToken,
        guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
    ) -> Result<model::CallToolResult, McpClientError> {
        self.pinned(want)?;
        let guard = Some(Arc::new(BoundGuard {
            inner: guard,
            state: Arc::clone(&self.state),
            registration: self.registration,
            want,
        }) as Arc<dyn agentkit_core::DispatchGuard>);
        match self
            .request(Some(name), arguments.clone(), cancellation, guard)
            .await
        {
            Ok(Response::Call(result)) => Ok(result),
            Ok(Response::Tools(_)) => unreachable!(),
            // The request machinery reports a torn-down transport as `Closed`,
            // or as `Cancelled` when the session's lifetime went away with it.
            // For a pinned handle that situation IS an authority loss — the
            // transport this handle was minted from is gone — so the pin's own
            // verdict is what the host hears, never a cancellation it never
            // asked for. A failure the pin does not explain (a remote error, a
            // genuine cancellation) is reported unchanged.
            Err(error) => Err(match self.pinned(want) {
                Ok(_) => error,
                Err(pin) => pin,
            }),
        }
    }
    async fn invoke(
        &self,
        connected: &Connected,
        name: Option<&str>,
        arguments: &model::JsonObject,
        cancellation: &CancellationToken,
        guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
    ) -> Result<Response, McpClientError> {
        let (client, _, lifetime, material) = connected;
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
        .map(|response| match (name, response) {
            // Anything the server echoes back may have absorbed a value we
            // substituted into this connection's config; scrub it before the
            // response leaves the session.
            (Some(_), Response::Call(mut result)) => {
                redact_call(&mut result, material);
                Response::Call(result)
            }
            (_, response) => response,
        })
        .map_err(|error| material.redact_error(error))
    }
    async fn retry_after(
        &self,
        error: McpClientError,
        read: bool,
        retries: &mut u32,
        connected: &Connected,
        cancellation: &CancellationToken,
    ) -> Result<(), McpClientError> {
        let (_, generation, lifetime, _) = connected;
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
    fn binding(&self) -> Option<ToolBinding> {
        self.live_binding()
    }
    async fn call_tool_bound(
        &self,
        binding: ToolBinding,
        name: &str,
        arguments: model::JsonObject,
        cancellation: CancellationToken,
        guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
    ) -> Result<model::CallToolResult, McpClientError> {
        self.call_bound(binding, name, &arguments, cancellation, guard)
            .await
    }
}

/// The live-transport check [`Session::pinned`] performs, factored out so the
/// bound dispatch path can run it from inside the guard the connection lane
/// consults immediately before it calls out.
fn pinned_in(
    state: &Arc<Mutex<State>>,
    registration: u64,
    want: ToolBinding,
) -> Result<Connected, McpClientError> {
    let state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.revoked || state.disposed || state.lifetime.is_cancelled() {
        return Err(McpClientError::Closed);
    }
    if state.generation != want.generation || registration != want.registration {
        return Err(McpClientError::Authority);
    }
    state
        .client
        .as_ref()
        .map(|client| {
            (
                client.clone(),
                state.generation,
                state.lifetime.clone(),
                state.material.clone(),
            )
        })
        .ok_or(McpClientError::Closed)
}

/// The guard the bound path hands the connection lane: the host's own guard
/// first (its verdict — a lost lease, a cancelled dispatch — is never
/// preempted by ours), then this handle's transport. `check_dispatch` turns any
/// error here into [`McpClientError::Authority`], the same stable refusal the
/// unbound lane gives for a denied authorization.
struct BoundGuard {
    inner: Option<Arc<dyn agentkit_core::DispatchGuard>>,
    state: Arc<Mutex<State>>,
    registration: u64,
    want: ToolBinding,
}
impl std::fmt::Debug for BoundGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoundGuard")
            .field("inner", &self.inner.as_ref().map(|_| "host guard"))
            .field("registration", &self.registration)
            .field("want", &self.want)
            .finish_non_exhaustive()
    }
}
#[async_trait]
impl agentkit_core::DispatchGuard for BoundGuard {
    async fn check(&self) -> Result<(), agentkit_core::CoreError> {
        if let Some(inner) = &self.inner {
            inner.check().await?;
        }
        pinned_in(&self.state, self.registration, self.want)
            .map(|_| ())
            .map_err(|error| {
                let reason =
                    format!("MCP tool handle is not the session's live transport: {error}");
                agentkit_core::CoreError::new("mcp_authority_revoked", &reason)
            })
    }
    fn fence(&self) -> agentkit_core::DispatchFence {
        match &self.inner {
            Some(inner) => inner.fence(),
            // No host fence exists on this call; the pin adds no fence of its
            // own — it is an authority check, not a lease.
            None => agentkit_core::DispatchFence {
                task_id: String::new(),
                attempt_id: String::new(),
                fencing_token: 0,
            },
        }
    }
}
