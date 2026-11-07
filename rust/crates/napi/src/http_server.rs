//! The desktop REST/SSE server, served by the Rust HTTP engine over the
//! service's own host and store.
//!
//! This is NOT a second runtime: the listener runs on the `AgentHost` the
//! service already owns (same executors, same worker, same SQLite actor), so
//! a REST-submitted turn is durable work exactly like a `send()` from JS —
//! recovery, leases, and close semantics are unchanged. Exactly one server
//! may run per service; `close()` stops it before the bridge closes, so no
//! new REST dispatch can enter mid-shutdown. The bearer token is validated
//! by `DesktopBackend` itself (constant-time compare, ≥ 32 bytes) — the
//! binding refuses weaker tokens at parse time rather than letting the
//! backend reject construction later.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agentkit_host::http::DesktopBackend;
use agentkit_transport_http::{Actor, Config, StreamOptions};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use futures::StreamExt;
use napi::bindgen_prelude::*;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{SessionService, parse_object};
use crate::errors;

/// Floor enforced by `DesktopBackend::new` (`token.len() < 32`); mirrored
/// here so the refusal crosses the boundary as `invalid_input`, not as an
/// opaque backend problem.
const MIN_TOKEN_BYTES: usize = 32;
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_ACTOR: &str = "desktop-user";
/// Last-resort model label recorded on REST-submitted turns when neither
/// the request body nor the stored `http/settings` name one.
const DEFAULT_MODEL: &str = "model";
/// Bound an accepted-then-stalled connection may outlive the shutdown
/// signal; keeps `stopHttpServer()`/`close()` deterministic even with a
/// client that never finishes reading (e.g. a parked SSE consumer).
pub(super) const CONNECTION_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Extra time, after in-flight connections were cut, for the accept loop to
/// observe their closure before it is force-aborted.
const CONNECTION_ABORT_GRACE: Duration = Duration::from_millis(250);

/// One running desktop HTTP server: the accept-loop task plus the token
/// that drains it. Cancelling closes the listener immediately (follow-up
/// connections are refused); in-flight requests get
/// `CONNECTION_SHUTDOWN_GRACE` before the task is force-stopped.
pub(super) struct HttpServer {
    token: CancellationToken,
    join: JoinHandle<()>,
    gate: Arc<ServerGate>,
}

/// Per-server admission gate: `closed` refuses new requests with a 503
/// `service_closed` the moment shutdown begins (a kept-alive connection can
/// otherwise keep submitting work after the listener is gone), and `abort`
/// cuts every remaining in-flight handler and streaming body after the
/// shutdown grace. `axum::serve` detaches its per-connection tasks, so
/// aborting the accept loop alone would leave them running.
struct ServerGate {
    closed: AtomicBool,
    abort: CancellationToken,
}

fn closed_response() -> Response {
    let body = json!({
        "code": "service_closed",
        "message": "the session service is closed",
    })
    .to_string();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = http::StatusCode::SERVICE_UNAVAILABLE;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    response
}

async fn gate_middleware(
    State(gate): State<Arc<ServerGate>>,
    request: Request,
    next: Next,
) -> Response {
    if gate.closed.load(Ordering::SeqCst) {
        return closed_response();
    }
    let response = tokio::select! {
        biased;
        () = gate.abort.cancelled() => return closed_response(),
        response = next.run(request) => response,
    };
    let (parts, body) = response.into_parts();
    let stream = body
        .into_data_stream()
        .take_until(gate.abort.clone().cancelled_owned());
    Response::from_parts(parts, Body::from_stream(stream))
}

impl HttpServer {
    /// Idempotent stop. Cancelling the token makes `axum` stop accepting
    /// (the listener is dropped immediately — follow-up connections are
    /// refused) and signals every connection task to finish its current
    /// response. The accept-loop join is awaited up to `grace`, then
    /// force-aborted so `stopHttpServer()`/`close()` stay deterministic
    /// even against a client that never finishes reading (e.g. a parked
    /// SSE consumer on an unsettled run; the socket is already closed, the
    /// orphaned connection task can only hold its store clone until the
    /// run finally settles).
    /// Environment-teardown stop: no grace for in-flight connections - the
    /// gate cuts handlers and bodies at once so the accept loop can finish.
    pub(super) async fn shutdown_now(self) {
        self.gate.closed.store(true, Ordering::SeqCst);
        self.gate.abort.cancel();
        self.token.cancel();
        let cut = tokio::time::Instant::now() + CONNECTION_ABORT_GRACE;
        while !self.join.is_finished() && tokio::time::Instant::now() < cut {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.join.abort();
    }

    pub(super) async fn shutdown(self, grace: Duration) {
        self.gate.closed.store(true, Ordering::SeqCst);
        self.token.cancel();
        let deadline = tokio::time::Instant::now() + grace.max(CONNECTION_SHUTDOWN_GRACE);
        while !self.join.is_finished() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Remaining connections (parked SSE consumers, stalled handlers)
        // are cut by the gate, which lets graceful shutdown complete.
        self.gate.abort.cancel();
        let cut = tokio::time::Instant::now() + CONNECTION_ABORT_GRACE;
        while !self.join.is_finished() && tokio::time::Instant::now() < cut {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.join.abort();
    }
}

/// Validated `startHttpServer` options (see the TS wrapper for the public
/// shape). Everything here selects real behavior; an unsupported value
/// fails the call with `invalid_input` instead of being ignored.
#[derive(Debug)]
struct ServerOptions {
    host: String,
    allow_non_loopback: bool,
    port: u16,
    base_path: String,
    max_body_bytes: usize,
    max_query_bytes: usize,
    actor_id: String,
    default_model: String,
    session_profile_id: Option<String>,
    bearer_token: String,
    /// Cross-origin policy mirrored from the TypeScript transport's
    /// `CorsConfig`: absent (default) answers plain HTTP with no CORS
    /// headers and OPTIONS with 405 — no browser was ever expected.
    cors: Option<agentkit_transport_http::CorsConfig>,
    /// `"memory"` wires a volatile credential vault so the provider
    /// management routes' `apiKey`/`extraHeaders` writes really store
    /// secrets (mirroring the desktop example's `MemorySecretStore`);
    /// omitted keeps those writes 501 exactly like the TypeScript
    /// transport without a `SecretStore` port.
    provider_secrets: Option<String>,
}

pub(super) fn optional_u64(object: &Value, key: &str) -> Result<Option<u64>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            errors::invalid(format!("{key} must be a non-negative integer when present"))
        }),
    }
}

pub(super) fn optional_sized(object: &Value, key: &str) -> Result<Option<usize>> {
    let Some(number) = optional_u64(object, key)? else {
        return Ok(None);
    };
    let bytes = usize::try_from(number)
        .map_err(|_| errors::invalid(format!("{key} is out of range for this platform")))?;
    if bytes == 0 {
        return Err(errors::invalid(format!(
            "{key} must be positive when present"
        )));
    }
    Ok(Some(bytes))
}

pub(super) fn optional_text(
    object: &Value,
    key: &str,
    allow_empty: bool,
) -> Result<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            if !allow_empty && text.trim().is_empty() {
                return Err(errors::invalid(format!(
                    "{key} must not be empty when present"
                )));
            }
            Ok(Some(text.clone()))
        }
        Some(_) => Err(errors::invalid(format!(
            "{key} must be a string when present"
        ))),
    }
}

/// Strictly parse `cors` into the transport's `CorsConfig`. Every accepted
/// shape changes observable behavior; anything malformed fails the call.
fn parse_cors(object: &Value) -> Result<Option<agentkit_transport_http::CorsConfig>> {
    use agentkit_transport_http::{CorsConfig, CorsOrigins};
    match object.get("cors") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(map)) => {
            for key in map.keys() {
                if !["origins", "allowHeaders", "exposeHeaders", "maxAgeSeconds"]
                    .contains(&key.as_str())
                {
                    return Err(errors::invalid(format!(
                        "cors.{key} is not a supported option"
                    )));
                }
            }
            let origins = match map.get("origins") {
                Some(Value::String(text)) if text == "*" => CorsOrigins::Any,
                Some(Value::String(_)) => {
                    return Err(errors::invalid(
                        "cors.origins must be \"*\" or an array of origin strings",
                    ));
                }
                Some(Value::Array(items)) => {
                    if items.is_empty() {
                        return Err(errors::invalid(
                            "cors.origins must not be empty; omit cors to disable it",
                        ));
                    }
                    let mut list = Vec::with_capacity(items.len());
                    for item in items {
                        let Some(text) = item.as_str() else {
                            return Err(errors::invalid("cors.origins entries must be strings"));
                        };
                        if text.trim().is_empty() {
                            return Err(errors::invalid(
                                "cors.origins entries must be non-empty origin strings",
                            ));
                        }
                        list.push(text.to_owned());
                    }
                    CorsOrigins::List(list)
                }
                _ => {
                    return Err(errors::invalid(
                        "cors.origins is required: \"*\" or an array of origin strings",
                    ));
                }
            };
            let string_list = |key: &str| -> Result<Option<Vec<String>>> {
                match map.get(key) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::Array(items)) => {
                        let mut list = Vec::with_capacity(items.len());
                        for item in items {
                            let Some(text) = item.as_str() else {
                                return Err(errors::invalid(format!(
                                    "cors.{key} entries must be strings"
                                )));
                            };
                            if text.trim().is_empty() {
                                return Err(errors::invalid(format!(
                                    "cors.{key} entries must be non-empty"
                                )));
                            }
                            list.push(text.to_owned());
                        }
                        Ok(Some(list))
                    }
                    Some(_) => Err(errors::invalid(format!(
                        "cors.{key} must be an array of strings when present"
                    ))),
                }
            };
            let max_age_seconds = match map.get("maxAgeSeconds") {
                None | Some(Value::Null) => None,
                Some(value) => {
                    let seconds = value.as_u64().ok_or_else(|| {
                        errors::invalid("cors.maxAgeSeconds must be a non-negative integer")
                    })?;
                    if seconds > u64::from(u32::MAX) {
                        return Err(errors::invalid("cors.maxAgeSeconds is out of range"));
                    }
                    Some(seconds as u32)
                }
            };
            Ok(Some(CorsConfig {
                origins,
                allow_headers: string_list("allowHeaders")?,
                expose_headers: string_list("exposeHeaders")?,
                max_age_seconds,
            }))
        }
        Some(_) => Err(errors::invalid("cors must be an object when present")),
    }
}

fn parse_server_options(text: &str) -> Result<ServerOptions> {
    let object = parse_object(text, "HTTP server options")?;
    let bearer_token = match object.get("bearerToken") {
        Some(Value::String(token)) => token.clone(),
        None | Some(Value::Null) => {
            return Err(errors::invalid(
                "bearerToken is required: the desktop server never serves unauthenticated",
            ));
        }
        Some(_) => {
            return Err(errors::invalid("bearerToken must be a string"));
        }
    };
    if bearer_token.len() < MIN_TOKEN_BYTES {
        return Err(errors::invalid(format!(
            "bearerToken must be at least {MIN_TOKEN_BYTES} bytes; the desktop backend refuses weaker tokens"
        )));
    }
    let host = optional_text(&object, "host", false)?.unwrap_or_else(|| DEFAULT_HOST.to_owned());
    if host.contains('/') || host.chars().any(char::is_whitespace) {
        return Err(errors::invalid(
            "host must be a bare hostname or IP literal",
        ));
    }
    let allow_non_loopback = ensure_loopback_bind(&object, &host)?;
    let port = optional_u64(&object, "port")?
        .map(|port| {
            u16::try_from(port).map_err(|_| {
                errors::invalid("port must be 0..=65535 (0 selects an ephemeral port)")
            })
        })
        .transpose()?
        .unwrap_or(0);
    Ok(ServerOptions {
        host,
        allow_non_loopback,
        port,
        base_path: optional_text(&object, "basePath", true)?.unwrap_or_default(),
        max_body_bytes: optional_sized(&object, "maxBodyBytes")?.unwrap_or(1_048_576),
        max_query_bytes: optional_sized(&object, "maxQueryBytes")?.unwrap_or(16_384),
        actor_id: optional_text(&object, "actorId", false)?
            .unwrap_or_else(|| DEFAULT_ACTOR.to_owned()),
        default_model: optional_text(&object, "defaultModel", false)?
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
        session_profile_id: optional_text(&object, "sessionProfileId", false)?,
        bearer_token,
        cors: parse_cors(&object)?,
        provider_secrets: match object.get("providerSecrets") {
            None | Some(Value::Null) => None,
            Some(Value::String(mode)) if mode == "memory" => Some(mode.to_owned()),
            Some(Value::String(mode)) => {
                return Err(errors::invalid(format!(
                    "providerSecrets must be \"memory\" (volatile vault) or omitted (provider credential writes stay 501); got {mode:?}"
                )));
            }
            Some(_) => {
                return Err(errors::invalid(
                    "providerSecrets must be a string when present",
                ));
            }
        },
    })
}

/// Normalize a base path exactly as `agentkit_transport_http::router` does,
/// so the advertised `baseUrl` is the path that actually routes.
fn normalize_base_path(path: &str) -> String {
    let path = path.trim().trim_end_matches('/').to_owned();
    if !path.is_empty() && !path.starts_with('/') {
        format!("/{path}")
    } else {
        path
    }
}

/// Refuse a non-loopback bind host unless `allowNonLoopback: true` is in the
/// options. Loopback is `127.0.0.0/8`, `::1`, or the name `localhost`; any
/// other name is refused because it may resolve to a routable interface.
/// The servers authenticate with a bearer token, but a token on a LAN-facing
/// plain-HTTP listener is still an exposure the caller must opt into.
pub(super) fn ensure_loopback_bind(object: &Value, host: &str) -> Result<bool> {
    let allow = match object.get("allowNonLoopback") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(allow)) => *allow,
        Some(_) => return Err(errors::invalid("allowNonLoopback must be a boolean")),
    };
    if allow || is_loopback_host(host) {
        return Ok(allow);
    }
    Err(errors::stable(
        "non_loopback_bind_refused",
        format!(
            "refusing to bind non-loopback host {host:?}; pass allowNonLoopback: true to opt in"
        ),
        None,
    ))
}

/// Post-bind check: a NAME that passed the textual test (`localhost`) is
/// only trusted if the socket really landed on a loopback address. The
/// listener is dropped (closing the socket) on refusal.
pub(super) fn ensure_bound_loopback(
    listener: tokio::net::TcpListener,
    allow_non_loopback: bool,
) -> Result<tokio::net::TcpListener> {
    let loopback = listener
        .local_addr()
        .is_ok_and(|address| address.ip().is_loopback());
    if allow_non_loopback || loopback {
        return Ok(listener);
    }
    drop(listener);
    Err(errors::stable(
        "non_loopback_bind_refused",
        "the host resolved to a non-loopback address; pass allowNonLoopback: true to opt in",
        None,
    ))
}

fn is_loopback_host(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    if bare.eq_ignore_ascii_case("localhost") {
        return true;
    }
    bare.parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

/// Bracket IPv6 literals so the advertised URL is fetchable as-is.
pub(super) fn url_host(host: &str) -> String {
    // A caller may hand over the host exactly as copied from a URL — the
    // bracketed literal `[::1]` — and the advertised baseUrl must stay
    // fetchable, never double-wrapped into `[[::1]]`.
    let bare = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    if bare.contains(':') {
        format!("[{bare}]")
    } else {
        bare.to_owned()
    }
}

/// Bind a socket before touching the store: an unusable address/port fails
/// with `service_unavailable` and no durable trace. Shared with the MCP
/// server listener, which binds the same way over the same host.
///
/// `host` may be an IPv4 literal, a bare IPv6 literal (`::1`), a bracketed
/// IPv6 literal (`[::1]` — how callers copy it out of a URL), or a host
/// name. The literal forms are parsed as SOCKET ADDRESSES, never handed to
/// the resolver: `("[::1]", port)` as a `ToSocketAddrs` pair asks DNS for a
/// host literally named `[::1]` — brackets are `SocketAddr` syntax, never
/// hostname syntax — and a perfectly valid IPv6 bind dies with "failed to
/// lookup address information".
pub(super) async fn bind_listener(host: &str, port: u16) -> Result<tokio::net::TcpListener> {
    let literal = match (host.starts_with('['), host.ends_with(']')) {
        (true, true) => host.to_owned(),
        _ if host.contains(':') => format!("[{host}]"),
        _ => host.to_owned(),
    };
    // Not a literal at all (`localhost`, an mDNS name): that one IS the
    // resolver's job, brackets or no.
    let bound = match format!("{literal}:{port}").parse::<std::net::SocketAddr>() {
        Ok(address) => tokio::net::TcpListener::bind(address).await,
        Err(_) => tokio::net::TcpListener::bind((host, port)).await,
    }
    .map_err(|error| {
        errors::stable(
            "service_unavailable",
            format!("unable to listen on {literal}:{port}"),
            Some(json!({ "cause": error.to_string() })),
        )
    })?;
    Ok(bound)
}

#[napi]
impl SessionService {
    /// Start the Rust REST v1 + SSE server on this service's host and
    /// store. `optionsJson`: `{host?, port?, basePath?, maxBodyBytes?,
    /// maxQueryBytes?, actorId?, defaultModel?, bearerToken, cors?,
    /// providerSecrets?}` —
    /// `bearerToken` is REQUIRED and at least 32 bytes (the backend's own
    /// floor); invalid values fail with `invalid_input`, an unusable
    /// address with `service_unavailable`. Defaults: `127.0.0.1`, ephemeral
    /// port, no base path, 1 MiB body / 16 KiB query caps.
    /// `cors` (`{origins: "*"|[string], allowHeaders?, exposeHeaders?,
    /// maxAgeSeconds?}`) pins the retired TypeScript transport's CORS
    /// semantics: matched origins decorate every response (including
    /// problems and SSE) and OPTIONS preflights answer 204; an unmatched
    /// origin preflight gets the ordinary 405, an unmatched path 404.
    /// Omitted, no response ever carries a CORS header.
    /// `providerSecrets: "memory"` stores provider credentials in a
    /// volatile vault so `createProvider`/`updateProvider` accept
    /// `apiKey`/`extraHeaders`; omitted, those writes answer 501 like the
    /// TypeScript transport without a `SecretStore` port. Returns
    /// `{port, baseUrl}` as JSON text (the wrapper parses it). Fails with
    /// `invalid_input` if a server already runs on this service, and
    /// `closed` after `close()`. Every request is authenticated by the
    /// backend's bearer check; a REST-submitted turn is executed by this
    /// service's worker like any other durable task.
    #[napi]
    pub async fn start_http_server(&self, options_json: String) -> Result<String> {
        let options = parse_server_options(&options_json)?;
        let listener = ensure_bound_loopback(
            bind_listener(&options.host, options.port).await?,
            options.allow_non_loopback,
        )?;
        let port = listener
            .local_addr()
            .map_err(|error| {
                errors::stable(
                    "service_unavailable",
                    "the desktop listener lost its bound address",
                    Some(json!({ "cause": error.to_string() })),
                )
            })?
            .port();
        // Held across the open/spawn: exactly one start can win, and close
        // can slip a stop in only under this same lock.
        let mut guard = self.core.lock().await;
        let core = guard
            .as_mut()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        if core.http.is_some() {
            return Err(errors::invalid(
                "an HTTP server is already running on this service; call stopHttpServer() first",
            ));
        }
        let mut backend = DesktopBackend::open(
            core.host.clone(),
            Actor {
                id: options.actor_id,
                metadata: BTreeMap::new(),
            },
            &options.bearer_token,
            &options.default_model,
        )
        .await
        .map_err(|problem| errors::stable(&problem.code, problem.detail, None))?;
        // One registry per service: `sessionProfileId` below is resolved at
        // chat creation by the same admission `createSession` uses, so the
        // boundary resolves against the profiles THIS service was given
        // (`createService({ profiles })`, or the host built-ins) — never a
        // second, narrower built-in-only registry that would 400 on a
        // profile the service itself admits.
        backend = backend.with_profiles(core.service.profiles());
        if let Some(profile_id) = &options.session_profile_id {
            backend = backend
                .with_session_profile(profile_id)
                .map_err(|problem| errors::stable(&problem.code, problem.detail, None))?;
        }
        if options.provider_secrets.as_deref() == Some("memory") {
            backend = backend.with_provider_secrets(Arc::new(
                agentkit_connections::MemorySecretStore::default(),
            ));
        }
        let gate = Arc::new(ServerGate {
            closed: AtomicBool::new(false),
            abort: CancellationToken::new(),
        });
        let router = Arc::new(backend).into_router(Config {
            base_path: options.base_path.clone(),
            max_body_bytes: options.max_body_bytes,
            max_query_bytes: options.max_query_bytes,
            streaming: StreamOptions::default(),
            cors: options.cors.clone(),
        });
        let router = router.layer(axum::middleware::from_fn_with_state(
            Arc::clone(&gate),
            gate_middleware,
        ));
        let token = CancellationToken::new();
        let serve_token = token.clone();
        let join = tokio::spawn(async move {
            // The accept loop only ends via the shutdown signal: the
            // listener is dropped (new connections refused) and each
            // connection drains under `HttpServer::shutdown`'s deadline.
            // An io failure ends it early; requests then fail at the
            // socket, which is what the caller observes either way.
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move { serve_token.cancelled().await })
                .await;
        });
        core.http = Some(HttpServer { token, join, gate });
        Ok(json!({
            "port": port,
            "baseUrl": format!("http://{}:{}{}", url_host(&options.host), port, normalize_base_path(&options.base_path)),
        })
        .to_string())
    }

    /// Stop the desktop server: the listener closes immediately (follow-up
    /// connections are refused), in-flight requests drain up to the
    /// service's close deadline, and the accept loop is force-stopped
    /// after. Idempotent — stopping a service that never started, or was
    /// already stopped or closed, is a no-op success.
    #[napi]
    pub async fn stop_http_server(&self) -> Result<()> {
        let server = {
            let mut guard = self.core.lock().await;
            guard.as_mut().and_then(|core| core.http.take())
        };
        if let Some(server) = server {
            server.shutdown(self.close_timeout).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<ServerOptions> {
        parse_server_options(text)
    }

    fn code(error: napi::Error) -> String {
        serde_json::from_str::<Value>(&error.reason).expect("error payload is JSON")["code"]
            .as_str()
            .expect("payload carries a code")
            .to_owned()
    }

    const TOKEN: &str = "12345678901234567890123456789012";

    #[test]
    fn defaults_fill_every_field_but_the_required_token() {
        let options = parse(&json!({ "bearerToken": TOKEN }).to_string()).unwrap();
        assert_eq!(options.host, DEFAULT_HOST);
        assert_eq!(options.port, 0);
        assert_eq!(options.base_path, "");
        assert_eq!(options.max_body_bytes, 1_048_576);
        assert_eq!(options.max_query_bytes, 16_384);
        assert_eq!(options.actor_id, DEFAULT_ACTOR);
        assert_eq!(options.default_model, DEFAULT_MODEL);
        assert_eq!(options.bearer_token, TOKEN);
    }

    #[test]
    fn token_is_required_with_the_backend_min_length() {
        assert_eq!(code(parse("{}").unwrap_err()), "invalid_input");
        assert_eq!(
            code(parse(&json!({ "bearerToken": null }).to_string()).unwrap_err()),
            "invalid_input"
        );
        assert_eq!(
            code(parse(&json!({ "bearerToken": "short" }).to_string()).unwrap_err()),
            "invalid_input"
        );
        // 31 bytes fails, 32 passes — the backend's exact byte floor.
        assert!(parse(&json!({ "bearerToken": "x".repeat(31) }).to_string()).is_err());
        assert!(parse(&json!({ "bearerToken": "x".repeat(32) }).to_string()).is_ok());
    }

    #[test]
    fn malformed_values_are_refused_as_invalid_input() {
        for bad in [
            json!({"bearerToken": 7}),
            json!({"bearerToken": TOKEN, "host": ""}),
            json!({"bearerToken": TOKEN, "host": "http://x"}),
            json!({"bearerToken": TOKEN, "host": "a b"}),
            json!({"bearerToken": TOKEN, "port": -1}),
            json!({"bearerToken": TOKEN, "port": 65536}),
            json!({"bearerToken": TOKEN, "port": 1.5}),
            json!({"bearerToken": TOKEN, "maxBodyBytes": 0}),
            json!({"bearerToken": TOKEN, "maxQueryBytes": "16"}),
            json!({"bearerToken": TOKEN, "actorId": "  "}),
            json!({"bearerToken": TOKEN, "defaultModel": ""}),
        ] {
            let error = parse(&bad.to_string()).unwrap_err();
            assert_eq!(code(error), "invalid_input", "must reject {bad}");
        }
        assert!(parse("[]").is_err());
        assert!(parse("nonsense").is_err());
    }

    #[test]
    fn base_url_mirrors_the_router_and_brackets_ipv6() {
        assert_eq!(normalize_base_path(" api/v1/ "), "/api/v1");
        assert_eq!(normalize_base_path("/api/v1///"), "/api/v1");
        assert_eq!(normalize_base_path(""), "");
        assert_eq!(url_host("127.0.0.1"), "127.0.0.1");
        assert_eq!(url_host("::1"), "[::1]");
        // Already-bracketed input (copied out of a URL) must not double-wrap;
        // the literal text passes through untouched, only the wrapper is owned.
        assert_eq!(url_host("[::1]"), "[::1]");
        assert_eq!(url_host("[0:0:0:0:0:0:0:1]"), "[0:0:0:0:0:0:0:1]");
    }

    /// A real IPv6 listener: bind `::1` (bare and bracketed), then drive one
    /// raw HTTP request through `[::1]:port`. IPv4 binds as the control in
    /// the same test. If the sandbox has no IPv6 loopback the bind itself
    /// fails — the test skips, printing the OS's own reason.
    #[tokio::test]
    async fn binds_ipv6_literals_by_address_not_by_name() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // IPv4 control: a bind failure here is a real failure, never a skip.
        let four = bind_listener("127.0.0.1", 0)
            .await
            .expect("IPv4 loopback must bind");
        assert!(four.local_addr().unwrap().ip().is_loopback());

        for host in ["::1", "[::1]"] {
            let listener = match bind_listener(host, 0).await {
                Ok(listener) => listener,
                Err(error) => {
                    eprintln!(
                        "SKIPPING IPv6 bind for {host:?}: the sandbox has no IPv6 loopback \
                         (bind failed with: {})",
                        error,
                    );
                    return;
                }
            };
            let address = listener.local_addr().unwrap();
            assert!(
                address.ip().is_ipv6() && address.ip().is_loopback(),
                "{host:?} must bind the IPv6 loopback, got {address}"
            );
            // Actual request through `[::1]:port` — minimal answering
            // listener, raw client, so the round trip needs no HTTP crate.
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut probe = [0_u8; 1024];
                let _ = socket.read(&mut probe).await.expect("read request");
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\n\r\npong")
                    .await
                    .expect("write response");
            });
            let mut client = tokio::net::TcpStream::connect(address)
                .await
                .unwrap_or_else(|error| panic!("connect to {address}: {error}"));
            client
                .write_all(b"GET /v1/ping HTTP/1.1\r\nhost: [::1]\r\n\r\n")
                .await
                .unwrap();
            let mut answer = String::new();
            client.read_to_string(&mut answer).await.unwrap();
            assert!(answer.starts_with("HTTP/1.1 200 OK"), "got {answer:?}");
            assert!(answer.ends_with("pong"), "got {answer:?}");
            server.await.expect("server task");
        }
    }
}
