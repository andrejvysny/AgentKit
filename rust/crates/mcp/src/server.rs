use crate::{
    Backend, SessionScope,
    handler::{NativeHandler, ScopeLease},
};
use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    response::Response,
    routing::any,
};
use http::{HeaderMap, Method, Request, StatusCode};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::{SessionManager, local::LocalSessionManager},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait Authenticator: Send + Sync + 'static {
    /// Return a trusted principal and optional chat. Request bodies are not identity.
    async fn authenticate(&self, headers: &HeaderMap) -> Option<(Value, Option<String>)>;
}

pub struct BearerAuth {
    digest: [u8; 32],
    principal: Value,
}
impl BearerAuth {
    pub fn new(token: &str, principal: Value) -> Result<Self, &'static str> {
        if token.is_empty() {
            return Err("MCP bearer token must not be empty");
        }
        Ok(Self {
            digest: Sha256::digest(format!("Bearer {token}").as_bytes()).into(),
            principal,
        })
    }
}
#[async_trait]
impl Authenticator for BearerAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Option<(Value, Option<String>)> {
        let supplied = credential_fingerprint(headers)?;
        bool::from(self.digest.ct_eq(&supplied)).then(|| (self.principal.clone(), None))
    }
}

#[derive(Clone)]
pub struct ServerOptions {
    pub writes_enabled: bool,
    pub allowed_hosts: Vec<String>,
    pub allowed_origins: Option<Vec<String>>,
    pub max_sessions_per_principal: usize,
    pub session_idle_ttl: Duration,
    pub max_request_bytes: usize,
    pub max_concurrent_calls: usize,
    pub max_batch_size: usize,
    pub max_call_duration: Duration,
    pub tool_page_size: usize,
}
impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            writes_enabled: false,
            allowed_hosts: vec!["localhost".into(), "127.0.0.1".into(), "[::1]".into()],
            allowed_origins: None,
            max_sessions_per_principal: 64,
            session_idle_ttl: Duration::from_secs(1800),
            max_request_bytes: 4 * 1024 * 1024,
            max_concurrent_calls: 4,
            max_batch_size: 8,
            max_call_duration: Duration::from_secs(120),
            tool_page_size: 200,
        }
    }
}

struct Session {
    fingerprint: [u8; 32],
    lease: Arc<ScopeLease>,
    last_used: Instant,
}
struct State {
    backend: Arc<dyn Backend>,
    auth: Arc<dyn Authenticator>,
    options: ServerOptions,
    service: StreamableHttpService<NativeHandler, LocalSessionManager>,
    sessions: Mutex<HashMap<String, Session>>,
    manager: Arc<LocalSessionManager>,
    initialization: Mutex<()>,
    shutdown: CancellationToken,
}
impl Drop for State {
    fn drop(&mut self) {
        self.shutdown.cancel();
        for session in self.sessions.get_mut().values() {
            session.lease.revoke();
        }
    }
}

/// Mount the router at the host's selected endpoint; this adapter opens no listener.
#[derive(Clone)]
pub struct McpServer {
    state: Arc<State>,
}
impl McpServer {
    pub fn new(
        backend: Arc<dyn Backend>,
        auth: Arc<dyn Authenticator>,
        options: ServerOptions,
    ) -> Result<Self, &'static str> {
        if options.max_sessions_per_principal == 0
            || options.max_request_bytes == 0
            || options.max_concurrent_calls == 0
            || options.max_batch_size == 0
            || options.tool_page_size == 0
            || options.max_call_duration.is_zero()
            || options.session_idle_ttl.is_zero()
        {
            return Err("MCP limits must be positive");
        }
        let shutdown = CancellationToken::new();
        let factory_backend = backend.clone();
        let factory_options = options.clone();
        let mut config = StreamableHttpServerConfig::default();
        config.legacy_session_mode = true;
        config.allowed_hosts = options.allowed_hosts.clone();
        config.allowed_origins = options.allowed_origins.clone().unwrap_or_default();
        config.cancellation_token = shutdown.clone();
        // The public raw-body limit runs before bounded number/key expansion.
        config.max_request_body_bytes = options.max_request_bytes.saturating_mul(8);
        let manager = Arc::new(LocalSessionManager::default());
        let service = StreamableHttpService::new(
            move || {
                Ok(NativeHandler::new(
                    factory_backend.clone(),
                    &factory_options,
                ))
            },
            manager.clone(),
            config,
        );
        Ok(Self {
            state: Arc::new(State {
                backend,
                auth,
                options,
                service,
                sessions: Mutex::new(HashMap::new()),
                manager,
                initialization: Mutex::new(()),
                shutdown,
            }),
        })
    }
    pub fn router(&self) -> Router {
        let server = self.clone();
        Router::new().route(
            "/",
            any(move |request| {
                let server = server.clone();
                async move { server.handle(request).await }
            }),
        )
    }
    pub async fn close(&self) {
        self.state.shutdown.cancel();
        let sessions: Vec<_> = self.state.sessions.lock().await.drain().collect();
        for (id, session) in sessions {
            session.lease.revoke();
            let _ = self.state.manager.close_session(&id.into()).await;
        }
    }
    pub async fn handle(&self, mut request: Request<Body>) -> Response {
        let state = &self.state;
        let Some((principal, chat_id)) = state.auth.authenticate(request.headers()).await else {
            return response(StatusCode::UNAUTHORIZED);
        };
        let Some(fingerprint) = credential_fingerprint(request.headers()) else {
            return response(StatusCode::UNAUTHORIZED);
        };
        if !crate::guards::allowed(request.headers(), &state.options) {
            return response(StatusCode::FORBIDDEN);
        }
        if state.shutdown.is_cancelled() {
            return response(StatusCode::SERVICE_UNAVAILABLE);
        }
        let session_id = request
            .headers()
            .get("mcp-session-id")
            .and_then(|id| id.to_str().ok())
            .map(str::to_owned);
        let _initialization = if session_id.is_none() {
            Some(state.initialization.lock().await)
        } else {
            None
        };
        let deleting = request.method() == Method::DELETE;
        let lease = match self
            .session(session_id.as_deref(), fingerprint, principal, chat_id)
            .await
        {
            Ok(lease) => lease,
            Err(status) => return response(status),
        };
        request.extensions_mut().insert(lease.clone());
        let _request_active = lease.begin();
        let (mut parts, body) = request.into_parts();
        let mut bytes = match tokio::time::timeout(
            state.options.max_call_duration,
            to_bytes(body, state.options.max_request_bytes),
        )
        .await
        {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => {
                return self
                    .finish(
                        session_id.as_deref(),
                        response(StatusCode::PAYLOAD_TOO_LARGE),
                    )
                    .await;
            }
            Err(_) => {
                return self
                    .finish(session_id.as_deref(), response(StatusCode::REQUEST_TIMEOUT))
                    .await;
            }
        };
        if parts.method == Method::POST {
            let value: Value = match std::str::from_utf8(&bytes)
                .ok()
                .and_then(|text| agentkit_contracts::parse_javascript_json(text).ok())
            {
                Some(value) => value,
                None => {
                    return self
                        .finish(session_id.as_deref(), response(StatusCode::BAD_REQUEST))
                        .await;
                }
            };
            // Overflow remains a numeric node for callbacks; coercion happens on output.
            bytes = serde_json::to_vec(&crate::json_boundary::escape(&value))
                .expect("parsed MCP JSON")
                .into();
            if let Some(batch) = value.as_array()
                && (batch.is_empty() || batch.len() > state.options.max_batch_size)
            {
                return self
                    .finish(session_id.as_deref(), response(StatusCode::BAD_REQUEST))
                    .await;
            }
            if session_id.is_none()
                && value.get("method").and_then(Value::as_str) != Some("initialize")
            {
                return response(StatusCode::BAD_REQUEST);
            }
            if session_id.is_none()
                && !value
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str)
                    .is_some_and(|version| crate::TS_PROTOCOL_VERSIONS.contains(&version))
            {
                return protocol_error(
                    StatusCode::BAD_REQUEST,
                    -32600,
                    "Unsupported MCP protocol version",
                );
            }
            if value.get("method").and_then(Value::as_str) == Some("initialize")
                && let Some(header) = parts
                    .headers
                    .get("mcp-protocol-version")
                    .and_then(|header| header.to_str().ok())
                && value
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str)
                    != Some(header)
            {
                return self
                    .finish(
                        session_id.as_deref(),
                        protocol_error(
                            StatusCode::BAD_REQUEST,
                            -32600,
                            "MCP protocol version mismatch",
                        ),
                    )
                    .await;
            }
        } else if session_id.is_none() {
            return response(StatusCode::BAD_REQUEST);
        }
        if let Some(version) = parts
            .headers
            .get("mcp-protocol-version")
            .and_then(|value| value.to_str().ok())
        {
            if !crate::TS_PROTOCOL_VERSIONS.contains(&version) {
                return self
                    .finish(
                        session_id.as_deref(),
                        protocol_error(
                            StatusCode::BAD_REQUEST,
                            -32600,
                            "Unsupported MCP protocol version",
                        ),
                    )
                    .await;
            }
            // The TS SDK supports this revision; rmcp's HTTP header allowlist
            // starts at 2024-11-05. Its initialize body still supports the
            // custom revision through our exact supported-version override.
            if version == "2024-10-07" {
                parts.headers.remove("mcp-protocol-version");
            }
        }
        let result = state
            .service
            .handle(Request::from_parts(parts, Body::from(bytes)))
            .await
            .map(Body::new);
        if session_id.is_none()
            && result.status().is_success()
            && let Some(id) = result
                .headers()
                .get("mcp-session-id")
                .and_then(|id| id.to_str().ok())
        {
            state.sessions.lock().await.insert(
                id.to_owned(),
                Session {
                    fingerprint,
                    lease,
                    last_used: Instant::now(),
                },
            );
        }
        if deleting
            && result.status().is_success()
            && let Some(id) = session_id.as_deref()
            && let Some(session) = state.sessions.lock().await.remove(id)
        {
            session.lease.revoke();
        }
        self.finish(session_id.as_deref(), result).await
    }
    async fn session(
        &self,
        id: Option<&str>,
        fingerprint: [u8; 32],
        principal: Value,
        chat_id: Option<String>,
    ) -> Result<Arc<ScopeLease>, StatusCode> {
        let mut sessions = self.state.sessions.lock().await;
        let stale: Vec<_> = sessions
            .iter()
            .filter(|(_, session)| {
                !busy(session)
                    && session.last_used.elapsed() >= self.state.options.session_idle_ttl
                    && session
                        .lease
                        .last_used
                        .lock()
                        .map(|used| used.elapsed() >= self.state.options.session_idle_ttl)
                        .unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.remove_session(&mut sessions, &id);
        }
        if let Some(id) = id {
            let session = sessions
                .get_mut(id)
                .filter(|session| bool::from(session.fingerprint.ct_eq(&fingerprint)))
                .ok_or(StatusCode::NOT_FOUND)?;
            session.last_used = Instant::now();
            return Ok(session.lease.clone());
        }
        let per_principal = self.state.options.max_sessions_per_principal;
        while sessions
            .values()
            .filter(|session| session.fingerprint == fingerprint)
            .count()
            >= per_principal
        {
            let oldest = sessions
                .iter()
                .filter(|(_, session)| session.fingerprint == fingerprint && !busy(session))
                .min_by_key(|(_, session)| session.last_used)
                .map(|(id, _)| id.clone());
            let Some(oldest) = oldest else {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            };
            self.remove_session(&mut sessions, &oldest);
        }
        while sessions.len() >= per_principal.saturating_mul(16) {
            let oldest = sessions
                .iter()
                .filter(|(_, session)| !busy(session))
                .min_by_key(|(_, session)| session.last_used)
                .map(|(id, _)| id.clone());
            let Some(oldest) = oldest else {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            };
            self.remove_session(&mut sessions, &oldest);
        }
        Ok(Arc::new(ScopeLease::new(
            SessionScope {
                principal,
                chat_id,
                actor_id: format!("mcp_{}", uuid::Uuid::new_v4()),
                cancellation: self.state.shutdown.child_token(),
            },
            self.state.backend.clone(),
        )))
    }
    fn remove_session(&self, sessions: &mut HashMap<String, Session>, id: &str) {
        if let Some(session) = sessions.remove(id) {
            session.lease.revoke();
        }
        let manager = self.state.manager.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let _ = manager.close_session(&id.into()).await;
        });
    }
    async fn finish(&self, id: Option<&str>, result: Response) -> Response {
        if let Some(id) = id
            && let Some(session) = self.state.sessions.lock().await.get_mut(id)
        {
            session.last_used = Instant::now();
        }
        result
    }
}
fn busy(session: &Session) -> bool {
    session.lease.active.load(Ordering::SeqCst) > 0
}
fn credential_fingerprint(headers: &HeaderMap) -> Option<[u8; 32]> {
    Some(Sha256::digest(headers.get(http::header::AUTHORIZATION)?.as_bytes()).into())
}
fn response(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("static response status")
}
fn protocol_error(status: StatusCode, code: i64, message: &str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"jsonrpc":"2.0","id":null,"error":{"code":code,"message":message}})
                .to_string(),
        ))
        .expect("static protocol error response")
}
