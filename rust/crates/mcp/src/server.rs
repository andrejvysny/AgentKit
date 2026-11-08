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
use http_body_util::BodyExt;
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
    /// Observability seam: invoked once per handler request (tools,
    /// prompts, resources) as it is ADMITTED, before the concurrency
    /// permit is taken. Tests use it to tell a call that is queued behind
    /// the semaphore apart from one that never reached the handler at
    /// all; production wiring leaves it `None`.
    #[doc(hidden)]
    pub call_admissions: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
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
            call_admissions: None,
        }
    }
}

struct Session {
    fingerprint: [u8; 32],
    lease: Arc<ScopeLease>,
    last_used: Instant,
}
/// A session slot held for an in-flight initialize. Dropping it without
/// `promote` (error, abort, client hang-up) releases the slot.
struct Reservation {
    state: Arc<State>,
    lease: Arc<ScopeLease>,
    key: Option<String>,
}
impl Reservation {
    async fn promote(mut self, id: &str) {
        // Lock first: if this future is dropped while waiting, the key is
        // still owned by `Drop`, which releases the placeholder. No await
        // may sit between taking the key and finishing the move.
        let state = self.state.clone();
        let mut sessions = state.sessions.lock().await;
        let Some(key) = self.key.take() else { return };
        if let Some(session) = sessions.remove(&key) {
            sessions.insert(id.to_owned(), session);
        }
        self.lease.active.fetch_sub(1, Ordering::SeqCst);
    }
}
#[cfg(test)]
mod reservation_tests {
    use super::*;
    use futures::FutureExt;

    struct Stub;
    #[async_trait::async_trait]
    impl Backend for Stub {
        async fn list_tools(
            &self,
            _: &SessionScope,
        ) -> Result<Vec<crate::CatalogTool>, rmcp::ErrorData> {
            Ok(vec![])
        }
        async fn call_tool(
            &self,
            _: &str,
            _: rmcp::model::JsonObject,
            _: &SessionScope,
        ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
            Err(rmcp::ErrorData::internal_error("unused", None))
        }
        fn revoke_actor(&self, _: &str) {}
    }
    #[async_trait::async_trait]
    impl Authenticator for Stub {
        async fn authenticate(&self, _: &HeaderMap) -> Option<(Value, Option<String>)> {
            None
        }
    }
    fn test_state() -> Arc<State> {
        McpServer::new(Arc::new(Stub), Arc::new(Stub), ServerOptions::default())
            .unwrap()
            .state
    }

    #[tokio::test]
    async fn promote_dropped_while_waiting_for_the_lock_releases_the_slot() {
        let state = test_state();
        let lease = Arc::new(ScopeLease::new(
            SessionScope {
                principal: Value::Null,
                chat_id: None,
                actor_id: "a".into(),
                request_id: None,
                cancellation: CancellationToken::new(),
            },
            state.backend.clone(),
        ));
        lease.active.fetch_add(1, Ordering::SeqCst);
        state.sessions.lock().await.insert(
            "pending:x".into(),
            Session {
                fingerprint: [0; 32],
                lease: lease.clone(),
                last_used: Instant::now(),
            },
        );
        let reservation = Reservation {
            state: state.clone(),
            lease: lease.clone(),
            key: Some("pending:x".into()),
        };
        // Hold the lock so promote parks on it, then drop that future.
        let guard = state.sessions.lock().await;
        let mut promote = Box::pin(reservation.promote("real"));
        assert!(promote.as_mut().now_or_never().is_none());
        drop(promote);
        drop(guard);
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.sessions.lock().await.contains_key("pending:x") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("placeholder released");
        assert_eq!(lease.active.load(Ordering::SeqCst), 0);
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else { return };
        self.lease.active.fetch_sub(1, Ordering::SeqCst);
        let state = self.state.clone();
        tokio::spawn(async move {
            if let Some(session) = state.sessions.lock().await.remove(&key) {
                session.lease.revoke();
            }
        });
    }
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
            return unauthorized();
        };
        let Some(fingerprint) = credential_fingerprint(request.headers()) else {
            return unauthorized();
        };
        if !crate::guards::allowed(request.headers(), &state.options) {
            return response(StatusCode::FORBIDDEN);
        }
        if state.shutdown.is_cancelled() {
            return response(StatusCode::SERVICE_UNAVAILABLE);
        }
        if !matches!(
            request.method(),
            &Method::GET | &Method::POST | &Method::DELETE
        ) {
            // Only after authenticating: a stranger learns nothing about the
            // transport shape of this endpoint.
            return response_with(
                StatusCode::METHOD_NOT_ALLOWED,
                &[("allow", "GET, POST, DELETE")],
                Some(-32000),
                "Unsupported MCP transport method",
            );
        }
        let session_id = request
            .headers()
            .get("mcp-session-id")
            .and_then(|id| id.to_str().ok())
            .map(str::to_owned);
        let deleting = request.method() == Method::DELETE;
        // The initialization lock covers the capacity check AND the slot
        // reservation (a busy placeholder session), then is released:
        // holding it while reading the body would let one slow client stall
        // every other initialize, and releasing it before the reservation
        // would let concurrent initializes all pass the check.
        let admitted = {
            let _initialization = if session_id.is_none() {
                Some(state.initialization.lock().await)
            } else {
                None
            };
            self.session(session_id.as_deref(), fingerprint, principal, chat_id)
                .await
        };
        let (lease, reserved) = match admitted {
            Ok(admitted) => admitted,
            // Admission control is temporary and the client's correct move is
            // to come back, so it is the one refusal that carries Retry-After.
            Err(StatusCode::SERVICE_UNAVAILABLE) => return capacity_refusal(),
            Err(status) => return response(status),
        };
        let reservation = Reservation {
            state: state.clone(),
            lease: lease.clone(),
            key: reserved,
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
        let mut batch: Option<Vec<Value>> = None;
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
            if let Some(messages) = value.as_array() {
                if messages.is_empty() {
                    return self
                        .finish(session_id.as_deref(), response(StatusCode::BAD_REQUEST))
                        .await;
                }
                if messages.len() > state.options.max_batch_size {
                    // Refused as one protocol error before a single message of
                    // the batch is dispatched.
                    return self
                        .finish(
                            session_id.as_deref(),
                            protocol_error(
                                StatusCode::BAD_REQUEST,
                                -32600,
                                &format!(
                                    "Invalid Request: batch of {} messages exceeds the limit of {}",
                                    messages.len(),
                                    state.options.max_batch_size
                                ),
                            ),
                        )
                        .await;
                }
                batch = Some(messages.clone());
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
        let result = {
            let abort = CancellationToken::new();
            parts
                .extensions
                .insert(crate::handler::RequestCancellation(abort.clone()));
            if let Some(members) = batch {
                // A batch within the cap: rmcp's HTTP layer speaks only single
                // messages, so every member is dispatched through the session
                // here, concurrently — the handler's own per-session semaphore
                // is what caps how many run at once. The answers fold into one
                // SSE stream, and hanging up on it (the AbortStream below)
                // cancels every member, running or queued alike.
                use futures::StreamExt as _;
                let state = state.clone();
                let bound = state.options.max_request_bytes;
                let count = members.len();
                let stream = futures::stream::iter(members)
                    .map(move |member| {
                        let state = state.clone();
                        let parts = parts.clone();
                        let bytes = serde_json::to_vec(&crate::json_boundary::escape(&member))
                            .expect("parsed MCP JSON");
                        let id = member.get("id").cloned().unwrap_or(Value::Null);
                        Box::pin(async move {
                            let response = state
                                .service
                                .handle(Request::from_parts(parts, Body::from(bytes)))
                                .await;
                            let (parts, body) = response.into_parts();
                            let answer = if parts.status.is_success() {
                                http_body_util::Limited::new(body, bound)
                                    .collect()
                                    .await
                                    .ok()
                                    .map(|collected| collected.to_bytes())
                            } else {
                                None
                            };
                            Ok::<_, axum::BoxError>(answer.unwrap_or_else(|| {
                                axum::body::Bytes::from(format!(
                                    "data: {}\n\n",
                                    serde_json::json!(
                                        {"jsonrpc":"2.0","id":id,
                                         "error":{"code":-32603,
                                                  "message":"Host MCP operation failed"}}
                                    )
                                ))
                            }))
                        })
                            as std::pin::Pin<
                                Box<
                                    dyn futures::Future<
                                            Output = Result<axum::body::Bytes, axum::BoxError>,
                                        > + Send,
                                >,
                            >
                    })
                    .buffered(count);
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(AbortStream::new(stream, abort)))
                    .expect("batch fold response")
            } else {
                state
                    .service
                    .handle(Request::from_parts(parts, Body::from(bytes)))
                    .await
                    .map(|body| Body::from_stream(AbortStream::new(body.into_data_stream(), abort)))
            }
        };
        if session_id.is_none()
            && result.status().is_success()
            && let Some(id) = result
                .headers()
                .get("mcp-session-id")
                .and_then(|id| id.to_str().ok())
        {
            reservation.promote(id).await;
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
    ) -> Result<(Arc<ScopeLease>, Option<String>), StatusCode> {
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
            return Ok((session.lease.clone(), None));
        }
        self.make_room(&mut sessions, fingerprint, None, 0)?;
        let lease = Arc::new(ScopeLease::new(
            SessionScope {
                principal,
                chat_id,
                actor_id: format!("mcp_{}", uuid::Uuid::new_v4()),
                request_id: None,
                cancellation: self.state.shutdown.child_token(),
            },
            self.state.backend.clone(),
        ));
        // Placeholder under a key no client can address: counted by the caps
        // and busy (never evicted) until the initialize settles.
        let key = format!("pending:{}", uuid::Uuid::new_v4());
        lease.active.fetch_add(1, Ordering::SeqCst);
        sessions.insert(
            key.clone(),
            Session {
                fingerprint,
                lease: lease.clone(),
                last_used: Instant::now(),
            },
        );
        Ok((lease, Some(key)))
    }
    /// Evicts idle sessions until `slack` more sessions than the caps allow
    /// fit. `keep` is never evicted (the session just created).
    fn make_room(
        &self,
        sessions: &mut HashMap<String, Session>,
        fingerprint: [u8; 32],
        keep: Option<&str>,
        slack: usize,
    ) -> Result<(), StatusCode> {
        let per_principal = self.state.options.max_sessions_per_principal;
        while sessions
            .values()
            .filter(|session| session.fingerprint == fingerprint)
            .count()
            >= per_principal + slack
        {
            let oldest = sessions
                .iter()
                .filter(|(id, session)| {
                    session.fingerprint == fingerprint
                        && !busy(session)
                        && Some(id.as_str()) != keep
                })
                .min_by_key(|(_, session)| session.last_used)
                .map(|(id, _)| id.clone());
            let Some(oldest) = oldest else {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            };
            self.remove_session(sessions, &oldest);
        }
        while sessions.len() >= per_principal.saturating_mul(16) + slack {
            let oldest = sessions
                .iter()
                .filter(|(id, session)| !busy(session) && Some(id.as_str()) != keep)
                .min_by_key(|(_, session)| session.last_used)
                .map(|(id, _)| id.clone());
            let Some(oldest) = oldest else {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            };
            self.remove_session(sessions, &oldest);
        }
        Ok(())
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
/// Streams the response through untouched and cancels the request token the
/// moment the body is abandoned — the only signal that survives a client
/// hanging up mid `tools/call`.
struct AbortStream<S> {
    inner: S,
    cancel: Option<CancellationToken>,
}
impl<S> AbortStream<S> {
    fn new(inner: S, cancel: CancellationToken) -> Self {
        Self {
            inner,
            cancel: Some(cancel),
        }
    }
}
impl<S> Drop for AbortStream<S> {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
    }
}
impl<S: futures::Stream + Unpin> futures::Stream for AbortStream<S> {
    type Item = S::Item;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(context)
    }
}
fn response(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("static response status")
}
const SESSION_CAPACITY_RETRY_AFTER_SECONDS: &str = "5";
fn unauthorized() -> Response {
    // The 401 says how to authenticate without confirming anything about the
    // resource: bodyless, with the challenge the client should answer.
    response_with(
        StatusCode::UNAUTHORIZED,
        &[("www-authenticate", "Bearer realm=\"agentkit-mcp\"")],
        None,
        "",
    )
}
fn capacity_refusal() -> Response {
    response_with(
        StatusCode::SERVICE_UNAVAILABLE,
        &[("retry-after", SESSION_CAPACITY_RETRY_AFTER_SECONDS)],
        Some(-32000),
        "Server has no session capacity available",
    )
}
fn response_with(
    status: StatusCode,
    headers: &[(&'static str, &'static str)],
    code: Option<i64>,
    message: &str,
) -> Response {
    let body = match code {
        Some(code) => serde_json::json!(
            {"jsonrpc":"2.0","id":null,"error":{"code":code,"message":message}}
        )
        .to_string(),
        None => String::new(),
    };
    let mut builder = Response::builder().status(status);
    if code.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(Body::from(body))
        .expect("static refusal response")
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
