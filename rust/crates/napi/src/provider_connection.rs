//! The Rust-native provider lane: a binding-local [`TrustedTransport`] over
//! reqwest plus the binding-instance connection registry.
//!
//! API keys never cross the JSON boundary: `registerProviderConnection` is
//! the only door a key travels through, and the key stays inside Rust
//! memory. `providerConfig` carries only the connection id, so the options
//! blob may be logged or persisted without ever leaking credentials. URL
//! joining and header rules mirror the TypeScript reference client
//! (`packages/core/src/providers/openai-compatible.ts`) exactly: one
//! trailing slash stripped from the base URL, `/{operation}` appended,
//! `accept: application/json` always, `content-type: application/json` on
//! POST operations, and `authorization: Bearer` only when a key is present.

use std::collections::HashMap;
use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicBool, Ordering},
};

use agentkit_core::providers::{AuthKind, ConnectionIdentity, TrustedTransport};
use agentkit_core::{CancellationToken, ChatProvider, ChatRequest, CoreError, ProviderStream};
use async_trait::async_trait;
use napi::bindgen_prelude::*;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::errors;

/// A registered provider connection. This map entry is the only place a
/// key exists after registration; nothing else clones it across the
/// boundary. `generation` is the connection's fence counter: for keyed
/// (logical) registrations it mirrors the store-persisted generation set
/// by `agentkit_store::register_provider_connection` — stable across
/// restarts, advanced only by credential rotation — so the durable
/// continuation scope binds `{logical identity, generation}` rather than
/// a process-local accident.
#[derive(Clone)]
struct Connection {
    base_url: String,
    api_key: Option<String>,
    generation: u64,
    /// Canonicalized path of the store a keyed registration was persisted
    /// in; `None` for process-local (unkeyed) registrations. The durable
    /// namespace isolates identical logical keys belonging to different stores.
    store_path: Option<std::path::PathBuf>,
    scope_key: Option<String>,
    account_scope: String,
    authority: Arc<AtomicBool>,
}

/// Binding-instance connection registry, keyed by connection id (the same
/// global-routing pattern the callback bridge uses for in-flight calls):
/// exactly one registration door, and every request resolves through the
/// same map — revoking a connection fails the next request honestly with
/// `connection_changed` instead of letting a stale key keep firing.
static CONNECTIONS: LazyLock<Mutex<HashMap<String, Connection>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[path = "provider_ports.rs"]
pub(crate) mod ports;
#[path = "provider_registration.rs"]
mod registration;
#[cfg(test)]
use registration::{register_provider_connection, revoke_provider_connection};

/// The revocable-state fence `check_current` consults: the same
/// `Open → Closing → Closed` lifecycle that makes new JS dispatches fail
/// fast, so a binding that has started closing never issues a fresh
/// provider request. Injected (rather than naming the bridge directly) so
/// the transport's fence semantics are testable without a Node host.
pub(crate) type Fence = Arc<dyn (Fn() -> std::result::Result<(), CoreError>) + Send + Sync>;

/// One service's transport: an immutable identity plus the shared client.
/// The connection's base URL and key are resolved per request so revocation
/// and rotation take effect immediately, never from a stale snapshot.
pub(crate) struct BindingTransport {
    identity: ConnectionIdentity,
    /// The base URL the connection had when this transport's identity was
    /// minted (not a secret). A request resolves the live entry and refuses
    /// when its generation or base URL no longer match, so a credential
    /// rotation or re-registration mid-run surfaces as `connection_changed`
    /// instead of silently sending the run's traffic to a new target.
    base_url: String,
    fence: Fence,
    client: reqwest::Client,
    authority: Arc<AtomicBool>,
    store_path: Option<std::path::PathBuf>,
}

impl BindingTransport {
    /// Resolve a registered connection into the transport for one
    /// `providerConfig`. An unknown connection id fails construction
    /// honestly (`invalid_connection`) — a service never starts with a
    /// provider whose connection does not exist.
    pub(crate) async fn connect(
        provider_id: &str,
        protocol: &str,
        connection_id: &str,
        fence: Fence,
    ) -> Result<Arc<Self>> {
        if provider_id.is_empty() {
            return Err(errors::connection(
                "providerConfig.providerId must be non-empty",
            ));
        }
        let connection = CONNECTIONS
            .lock()
            .await
            .get(connection_id)
            .cloned()
            .ok_or_else(|| {
                errors::connection(format!(
                    "providerConfig.connectionId \"{connection_id}\" is not a registered connection"
                ))
            })?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| errors::internal("the HTTP client"))?;
        Ok(Arc::new(Self {
            identity: ConnectionIdentity {
                provider_id: provider_id.to_owned(),
                protocol: protocol.to_owned(),
                connection_id: connection_id.to_owned(),
                // The registration's generation — durable for keyed
                // connections, 1 for process-local ones. A service built
                // after a rotation carries the new generation; a stale
                // pre-rotation object is rejected at the next admission
                // (the host's scope comparison), never silently trusted.
                generation: connection.generation,
            },
            fence,
            base_url: connection.base_url.clone(),
            authority: connection.authority.clone(),
            store_path: connection.store_path.clone(),
            client,
        }))
    }
}

#[async_trait]
impl TrustedTransport for BindingTransport {
    fn check_current(&self) -> std::result::Result<(), CoreError> {
        (self.fence)()?;
        if !self.authority.load(Ordering::Acquire) {
            return Err(CoreError::responses("connection_changed"));
        }
        Ok(())
    }

    async fn check_authority(&self) -> std::result::Result<(), CoreError> {
        self.check_current()?;
        if let Some(path) = self.store_path.clone() {
            let id = self.identity.connection_id.clone();
            let generation = self.identity.generation;
            tokio::task::spawn_blocking(move || {
                agentkit_store::check_provider_connection(&path, &id, generation)
            })
            .await
            .map_err(|_| CoreError::responses("connection_changed"))?
            .map_err(|_| CoreError::responses("connection_changed"))?;
        }
        self.check_current()
    }

    fn identity(&self) -> ConnectionIdentity {
        self.identity.clone()
    }

    /// This lane is API-key shaped only (no subscription account exists to
    /// impersonate), matching the reference client's key-or-none auth.
    fn auth_kind(&self) -> AuthKind {
        AuthKind::ApiKey
    }

    async fn request(
        &self,
        identity: &ConnectionIdentity,
        operation: &str,
        body: Option<Value>,
        cancellation: CancellationToken,
    ) -> std::result::Result<reqwest::Response, CoreError> {
        self.request_guarded(identity, operation, body, None, cancellation)
            .await
    }

    async fn request_guarded(
        &self,
        identity: &ConnectionIdentity,
        operation: &str,
        body: Option<Value>,
        guard: Option<Arc<dyn agentkit_core::DispatchGuard>>,
        cancellation: CancellationToken,
    ) -> std::result::Result<reqwest::Response, CoreError> {
        self.check_current()?;
        if cancellation.is_cancelled() {
            return Err(CoreError::new("cancelled", "aborted"));
        }
        if identity != &self.identity {
            return Err(CoreError::responses("connection_changed"));
        }
        let path = match operation {
            "chat/completions" | "responses" => operation,
            "models" => "models",
            _ => return Err(CoreError::responses("invalid_input")),
        };
        let connection = CONNECTIONS
            .lock()
            .await
            .get(&self.identity.connection_id)
            .cloned()
            .ok_or_else(|| CoreError::responses("connection_changed"))?;
        if connection.generation != self.identity.generation || connection.base_url != self.base_url
        {
            return Err(CoreError::responses("connection_changed"));
        }
        self.check_authority().await?;
        // `models` is a keyless GET in the reference client (no body, no
        // content type); the completion operations POST a JSON body.
        let get = path == "models";
        let mut request = if get {
            self.client.get(join_url(&connection.base_url, path))
        } else {
            self.client.post(join_url(&connection.base_url, path))
        };
        request = request.header("accept", "application/json");
        if let Some(key) = &connection.api_key {
            request = request.header("authorization", format!("Bearer {key}"));
        }
        if !get {
            request = request.header("content-type", "application/json");
        }
        if let Some(body) = body.filter(|_| !get) {
            request = request.json(&body);
        }
        if let Some(guard) = guard {
            guard.check().await?;
        }
        self.check_current()?;
        let response = request
            .send()
            .await
            .map_err(|_| CoreError::responses("transport_error"))?;
        self.check_authority().await?;
        Ok(response)
    }
}

/// Join `{baseUrl}/{operation}` the way the TypeScript reference client
/// does: exactly one trailing slash is stripped from the base, then the
/// operation path is appended with a single separator.
fn join_url(base_url: &str, operation: &str) -> String {
    let base = base_url.strip_suffix('/').unwrap_or(base_url);
    format!("{base}/{operation}")
}

/// Applies the service-level `temperature`/`maxTokens` as REQUEST DEFAULTS
/// through [`ChatProvider::apply_request_defaults`] — i.e. BEFORE the
/// host's context admission — and then delegates untouched. The host
/// builds the chat request without sampling parameters, so the configured
/// lane options reach the request here; admission then clamps the output
/// cap, and the admitted value is what goes on the wire (P1-F): the
/// transmitted cap is min(context cap, configured maxTokens), and no
/// layer after admission — including this one — may override token limits.
/// A request that already carries a value keeps it; the config supplies
/// defaults, never overrides.
pub(crate) struct ConfiguredProvider {
    inner: Arc<dyn ChatProvider>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
}

impl ConfiguredProvider {
    pub(crate) fn new(
        inner: Arc<dyn ChatProvider>,
        temperature: Option<f64>,
        max_tokens: Option<u64>,
    ) -> Self {
        Self {
            inner,
            temperature,
            max_tokens,
        }
    }
}

#[async_trait]
impl ChatProvider for ConfiguredProvider {
    fn allows_implicit_output_limit(&self) -> bool {
        self.inner.allows_implicit_output_limit()
    }
    fn apply_request_defaults(&self, input: &mut ChatRequest) {
        self.inner.apply_request_defaults(input);
        if input.temperature.is_none() {
            input.temperature = self.temperature;
        }
        if input.max_output_tokens.is_none() {
            input.max_output_tokens = self.max_tokens;
        }
    }
    fn supports_usage_cleanup(&self) -> bool {
        self.inner.supports_usage_cleanup()
    }
    fn connection_identity(&self) -> Option<ConnectionIdentity> {
        self.inner.connection_identity()
    }
    fn tracks_transport_requests(&self) -> bool {
        self.inner.tracks_transport_requests()
    }
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn stream_chat(&self, input: ChatRequest, cancellation: CancellationToken) -> ProviderStream {
        // The admitted request IS the transmitted request: nothing here
        // may touch the token budget or sampling parameters the host
        // resolved before admission.
        self.inner.stream_chat(input, cancellation)
    }
    async fn list_models(
        &self,
        cancellation: CancellationToken,
    ) -> std::result::Result<Vec<Value>, CoreError> {
        self.inner.list_models(cancellation).await
    }
}

#[cfg(test)]
#[path = "provider_connection_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "provider_authority_tests.rs"]
mod authority_tests;
