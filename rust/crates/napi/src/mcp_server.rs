//! The MCP server side, served by the Rust MCP crate over the service's
//! own JS tool lane.
//!
//! This is NOT a second runtime: the accept loop binds exactly like the
//! desktop REST server (same tokio runtime, same shutdown discipline), and
//! a remote `tools/call` executes through the exact `JsTool::execute` lane
//! a run-scoped local tool call takes — same bridge request, same pending
//! kind, same result mapping (`tool.rs::invoke`). The catalogue a remote
//! client sees is the service's registered JS tool descriptors themselves
//! (`JsToolContributor::tool_definitions` — the list `contribute` hands
//! the executor), with `writes` derived from the descriptor's host-owned
//! `effect` and the crate's `writes_enabled` gate applied to list AND call.
//!
//! SCOPE OF WHAT A REMOTE CLIENT CAN INVOKE (documented reduction): the
//! engine's run-scoped layers — the `ToolGuard` chain (the `BindingAuthority`
//! and `JsToolGuard` pair, plan-mode blocking, durable ask/approve) and the
//! exactly-once execution receipts — are bound to task-lease execution
//! inside a turn (see `agentkit_host::governance` and
//! `agentkit_core::ToolContext::dispatch_guard`: "standalone calls have no
//! task lease"). Fabricating a run to borrow them would be host surgery,
//! so the binding instead enforces what genuinely holds at this layer:
//! bearer authentication (constant-time digest compare, ≥ 32-byte
//! tokens), the crate's Host/Origin/size/concurrency/batch/deadline/
//! session caps, schema validation of every call's arguments against the
//! descriptor's recorded `inputSchema` (the crate's handler compiles and
//! checks it), the `writesEnabled` gate (default FALSE, enforced here as
//! well as in the handler), and immediate lease revocation of in-flight JS
//! callbacks on stop. A remote call therefore reaches the same JS handler
//! a run would have called — the service's own JS code remains its policy
//! surface — but WITHOUT plan-mode gating and without a durable execution
//! receipt.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentkit_core::{ToolContext, ToolLimits, ToolResult};
use agentkit_mcp::model::{CallToolResult, ContentBlock, ErrorData, JsonObject, Tool};
use agentkit_mcp::{
    Authenticator, Backend, BearerAuth, CatalogTool, McpServer, ServerOptions, SessionScope,
};
use async_trait::async_trait;
use napi::bindgen_prelude::*;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use agentkit_core::permission::ToolClass;

use super::SessionService;
use super::http_server::{
    CONNECTION_SHUTDOWN_GRACE, bind_listener, ensure_bound_loopback, ensure_loopback_bind,
    optional_sized, optional_text, optional_u64, url_host,
};
use crate::authority::effective_class;
use crate::errors;
use crate::tool::JsToolContributor;

/// Floor mirrored from the REST server's bearer policy: the crate's
/// `BearerAuth` only refuses empty tokens, so the binding enforces the
/// ≥ 32-byte constant-time-comparison floor itself, as `invalid_input`.
const MIN_TOKEN_BYTES: usize = 32;
const DEFAULT_HOST: &str = "127.0.0.1";

/// One running MCP server: the crate-level server (sessions, limits,
/// leases) plus the accept-loop task and its drain token. `shutdown`
/// mirrors `HttpServer::shutdown`: crate sessions first (leases revoked,
/// in-flight JS callbacks cancelled), then the listener, then a bounded
/// join before force-aborting.
pub(super) struct McpServerHandle {
    server: McpServer,
    token: CancellationToken,
    join: JoinHandle<()>,
}

impl McpServerHandle {
    /// Idempotent stop. `McpServer::close` revokes every session lease —
    /// which cancels each scope's token, and the tracked child tokens
    /// below turn that into cancellation of any in-flight JS tool callback
    /// — then the accept loop drains under the close deadline.
    /// Environment-teardown stop: revoke leases, close the listener, abort.
    pub(super) async fn shutdown_now(self) {
        self.server.close().await;
        self.token.cancel();
        self.join.abort();
    }

    pub(super) async fn shutdown(self, grace: Duration) {
        self.server.close().await;
        self.token.cancel();
        let deadline = tokio::time::Instant::now() + grace.max(CONNECTION_SHUTDOWN_GRACE);
        while !self.join.is_finished() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.join.abort();
    }
}

/// The service-side `Backend`: catalogue from the contributor's registered
/// descriptors, execution through the contributor's `invoke` (the very
/// `JsTool::execute` a run-scoped call takes). In-flight callbacks are
/// tracked per principal-bound actor so the crate's `revoke_actor` —
/// invoked on session expiry, DELETE, and close — cancels them rather
/// than letting a revoked scope linger in JS.
struct BindingBackend {
    contributor: Arc<JsToolContributor>,
    /// Mirrors the parsed `writesEnabled`: the crate handler gates list
    /// and call from its own copy of the option; the backend keeps an
    /// independent copy so no second mount of this backend type could
    /// ever expose writes that the start call did not open.
    writes_enabled: bool,
    active: Mutex<BTreeMap<String, Vec<CancellationToken>>>,
}

#[async_trait]
impl Backend for BindingBackend {
    async fn list_tools(
        &self,
        _scope: &SessionScope,
    ) -> std::result::Result<Vec<CatalogTool>, ErrorData> {
        // `lookup` — the resolution `call_tool` uses — takes the FIRST
        // definition under a name. Advertising every registered definition
        // would therefore let a client be shown one callback and be served
        // another: `t@1.0.0` plus a later `t@2.0.0` is two catalogue entries
        // with one name, and a streamable-HTTP client keys tools by name. The
        // advertised set is deduplicated by the contributor with exactly the
        // resolver's rule, so what a remote client sees is always what it will
        // execute.
        Ok(self
            .contributor
            .advertised_definitions()
            .into_iter()
            .map(|definition| CatalogTool {
                definition: Tool::new(
                    definition.name,
                    definition.description,
                    match definition.input_schema {
                        Value::Object(schema) => schema,
                        _ => JsonObject::new(),
                    },
                ),
                // Host-owned effect: the descriptor's declared taxonomy,
                // never anything a remote client could influence.
                // Same stricter-of classification as the run-scoped JS guard,
                // so a write/exec capability under a "read" effect (or the
                // reverse) is still a write for the gate.
                writes: effective_class(&definition.capability, &definition.effect)
                    != ToolClass::Read,
            })
            .collect())
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: JsonObject,
        scope: &SessionScope,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        // ONE definition decides everything about this call: the write gate,
        // the argument validation below, and the callback that runs. Reading
        // them from separate lookups would let a tool re-registered between the
        // handler's catalogue check and this line be authorized against one
        // descriptor and executed as another.
        let Some(definition) = self.contributor.lookup(name) else {
            return Err(ErrorData::invalid_params("Unknown tool", None));
        };
        if effective_class(&definition.capability, &definition.effect) != ToolClass::Read
            && !self.writes_enabled
        {
            return Err(ErrorData::invalid_params("Unknown tool", None));
        }
        let schema = definition.input_schema.clone();
        let validator = agentkit_contracts::compile_tool_schema(&schema)
            .map_err(|_| ErrorData::internal_error("Internal error", None))?;
        if !validator.is_valid(&Value::Object(arguments.clone())) {
            // Same verdict the crate's handler gives for invalid arguments: a
            // tool-reported failure, not a transport error.
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "Invalid tool arguments",
            )]));
        }
        let cancellation = scope.cancellation.child_token();
        let actor = scope.actor_id.clone();
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(actor.clone())
            .or_default()
            .push(cancellation.clone());
        // Released on every exit path, including this future being dropped
        // mid-invoke (client disconnect, call deadline).
        let _release = ReleaseGuard {
            backend: self,
            actor: &actor,
            token: &cancellation,
        };
        // The engine path for a tool that has no durable run: a synthetic
        // mcp-run identity, no task lease (therefore no dispatch guard —
        // see the module header for exactly what this does and does not
        // bypass), and cancellation wired to the session scope.
        let context = ToolContext {
            run_id: format!("mcp:{}", scope.actor_id),
            tool_call_id: Some(uuid::Uuid::new_v4().to_string()),
            attempt_id: None,
            chat_id: scope.chat_id.clone(),
            scope_id: None,
            user_id: Some(scope.principal.to_string()),
            bindings: Vec::new(),
            limits: ToolLimits::default(),
            cancellation: cancellation.clone(),
            dispatch_guard: None,
            metadata: serde_json::Map::new(),
            suspension: None,
        };
        let result = self
            .contributor
            .invoke(definition, context, Value::Object(arguments))
            .await;
        match result {
            Ok(result) => Ok(to_call_result(&result)),
            Err(error) => Err(ErrorData::internal_error(
                format!("Tool execution failed: {}", error.code),
                None,
            )),
        }
    }

    fn revoke_actor(&self, actor_id: &str) {
        let tokens = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(actor_id)
            .unwrap_or_default();
        for token in tokens {
            token.cancel();
        }
    }
}

/// Drops the in-flight token registration when the call ends for any reason.
struct ReleaseGuard<'a> {
    backend: &'a BindingBackend,
    actor: &'a str,
    token: &'a CancellationToken,
}

impl Drop for ReleaseGuard<'_> {
    fn drop(&mut self) {
        self.backend.release(self.actor, self.token);
    }
}

impl BindingBackend {
    fn release(&self, actor: &str, token: &CancellationToken) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(tokens) = active.get_mut(actor) {
            // CancellationToken equality is token-identity, not state.
            tokens.retain(|candidate| candidate != token);
            if tokens.is_empty() {
                active.remove(actor);
            }
        }
    }
}

/// Map the host tool-result contract onto an MCP call result: the full
/// result JSON travels as `structuredContent` and as one JSON text block,
/// and `isError` mirrors `ok`. Nothing is fabricated — an unusable tool
/// outcome stays an error result.
fn to_call_result(result: &ToolResult) -> CallToolResult {
    let payload = json!({
        "ok": result.ok,
        "data": result.data,
        "sources": result.sources,
        "warnings": result.warnings,
        "truncated": result.truncated,
        "summary": result.summary,
        "status": result.status,
    });
    let text = serde_json::to_string(&payload);
    project_call_result(text, payload, result.ok)
}

/// Fold the (attempted) payload serialization onto the wire shape. A payload
/// that refuses to serialize is an unusable tool outcome: it stays an error
/// result and the wire carries only `tool_result_unserializable` — never a
/// fabricated success, never the half-written payload (the frozen projection
/// lane's unserializable case; a success here would advertise content the
/// transport cannot deliver).
fn project_call_result(
    text: std::result::Result<String, serde_json::Error>,
    payload: Value,
    ok: bool,
) -> CallToolResult {
    let (text, payload, ok) = match text {
        Ok(text) => (text, payload, ok),
        Err(_) => {
            let detail = json!({"errorCode": "tool_result_unserializable"});
            (detail.to_string(), detail, false)
        }
    };
    let mut call = CallToolResult::success(vec![ContentBlock::text(text)]);
    call.structured_content = Some(payload);
    call.is_error = if ok { None } else { Some(true) };
    call
}

/// The binding's bearer set: one crate `BearerAuth` (constant-time digest
/// compare) per configured token; the first match authenticates. Each
/// token maps to a distinct principal, so the crate's per-principal
/// session caps and fingerprint-bound session reuse hold per token.
struct BearerSet {
    bearers: Vec<BearerAuth>,
}

#[async_trait]
impl Authenticator for BearerSet {
    async fn authenticate(&self, headers: &http::HeaderMap) -> Option<(Value, Option<String>)> {
        for bearer in &self.bearers {
            if let Some(identity) = bearer.authenticate(headers).await {
                return Some(identity);
            }
        }
        None
    }
}

/// Validated `startMcpServer` options (see the TS wrapper for the public
/// shape). Everything here selects real behavior; an unsupported value
/// fails the call with `invalid_input` instead of being ignored.
struct McpServerConfig {
    host: String,
    allow_non_loopback: bool,
    port: u16,
    bearer_tokens: Vec<String>,
    options: ServerOptions,
}

// Tokens are secrets: the debug view counts them, never prints them.
impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("bearer_tokens", &self.bearer_tokens.len())
            .finish_non_exhaustive()
    }
}

fn parse_mcp_options(text: &str) -> Result<McpServerConfig> {
    let object = super::parse_object(text, "MCP server options")?;
    let tokens = match object.get("bearerTokens") {
        Some(Value::Array(tokens)) => tokens.clone(),
        None | Some(Value::Null) => {
            return Err(errors::invalid(
                "bearerTokens is required: the MCP server never serves unauthenticated",
            ));
        }
        Some(_) => {
            return Err(errors::invalid("bearerTokens must be an array of strings"));
        }
    };
    if tokens.is_empty() {
        return Err(errors::invalid(
            "bearerTokens must contain at least one token",
        ));
    }
    let mut bearer_tokens: Vec<String> = Vec::new();
    for token in tokens {
        let Value::String(token) = token else {
            return Err(errors::invalid("bearerTokens entries must be strings"));
        };
        if token.len() < MIN_TOKEN_BYTES {
            return Err(errors::invalid(format!(
                "every bearer token must be at least {MIN_TOKEN_BYTES} bytes"
            )));
        }
        if token.bytes().any(|byte| byte <= 0x20 || byte >= 0x7f) {
            return Err(errors::invalid(
                "bearer tokens must be printable ASCII without whitespace",
            ));
        }
        if !bearer_tokens.iter().any(|seen| seen == &token) {
            bearer_tokens.push(token);
        }
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
    let writes_enabled = match object.get("writesEnabled") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(enabled)) => *enabled,
        Some(_) => return Err(errors::invalid("writesEnabled must be a boolean")),
    };
    let mut options = ServerOptions {
        writes_enabled,
        ..ServerOptions::default()
    };
    if let Some(hosts) = object.get("allowedHosts").filter(|value| !value.is_null()) {
        options.allowed_hosts = string_list(hosts, "allowedHosts")?;
        if options.allowed_hosts.is_empty() {
            return Err(errors::invalid(
                "allowedHosts must not be empty when present",
            ));
        }
    }
    if let Some(origins) = object
        .get("allowedOrigins")
        .filter(|value| !value.is_null())
    {
        options.allowed_origins = Some(string_list(origins, "allowedOrigins")?);
    }
    if let Some(value) = optional_sized(&object, "maxConcurrentCalls")? {
        options.max_concurrent_calls = value;
    }
    if let Some(value) = optional_sized(&object, "maxBatchSize")? {
        options.max_batch_size = value;
    }
    if let Some(value) = optional_sized(&object, "maxBodyBytes")? {
        options.max_request_bytes = value;
    }
    if let Some(value) = optional_sized(&object, "maxSessionsPerPrincipal")? {
        options.max_sessions_per_principal = value;
    }
    if let Some(ms) = optional_u64(&object, "callDeadlineMs")? {
        if ms == 0 {
            return Err(errors::invalid(
                "callDeadlineMs must be positive when present",
            ));
        }
        options.max_call_duration = Duration::from_millis(ms);
    }
    if let Some(ms) = optional_u64(&object, "idleTtlMs")? {
        if ms == 0 {
            return Err(errors::invalid("idleTtlMs must be positive when present"));
        }
        options.session_idle_ttl = Duration::from_millis(ms);
    }
    Ok(McpServerConfig {
        host,
        allow_non_loopback,
        port,
        bearer_tokens,
        options,
    })
}

fn string_list(value: &Value, key: &str) -> Result<Vec<String>> {
    let entries = value
        .as_array()
        .ok_or_else(|| errors::invalid(format!("{key} must be an array of strings")))?;
    entries
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_owned)
                .filter(|text| !text.trim().is_empty())
                .ok_or_else(|| errors::invalid(format!("{key} entries must be non-empty strings")))
        })
        .collect()
}

#[napi]
impl SessionService {
    /// Start the Rust MCP server (streamable HTTP, official-SDK-backed)
    /// on this service's JS tool lane. `optionsJson`: `{host?, port?,
    /// bearerTokens, allowedHosts?, allowedOrigins?, writesEnabled?,
    /// maxConcurrentCalls?, maxBatchSize?, callDeadlineMs?, idleTtlMs?,
    /// maxBodyBytes?, maxSessionsPerPrincipal?}` — `bearerTokens` is a
    /// REQUIRED non-empty array, every entry at least 32 bytes; invalid
    /// values fail with `invalid_input`, an unusable address with
    /// `service_unavailable`. Defaults: `127.0.0.1`, ephemeral port,
    /// `writesEnabled: false`, the crate's frozen limit defaults. Returns
    /// `{port, url}` as JSON text. The catalogue a remote client sees is
    /// exactly this service's registered JS tools (list AND call hide
    /// write tools while `writesEnabled` is false); a `tools/call`
    /// executes the same `JsTool::execute` bridge request a local tool
    /// call takes. Fails with `invalid_input` if an MCP server already
    /// runs on this service, and `closed` after `close()`.
    #[napi]
    pub async fn start_mcp_server(&self, options_json: String) -> Result<String> {
        let config = parse_mcp_options(&options_json)?;
        let listener = ensure_bound_loopback(
            bind_listener(&config.host, config.port).await?,
            config.allow_non_loopback,
        )?;
        let port = listener.local_addr().map_err(|error| {
            errors::stable(
                "service_unavailable",
                "the MCP listener lost its bound address",
                Some(json!({ "cause": error.to_string() })),
            )
        })?;
        let port = port.port();
        let mut guard = self.core.lock().await;
        let core = guard
            .as_mut()
            .ok_or_else(|| errors::closed("the session service is closed"))?;
        if core.mcp.is_some() {
            return Err(errors::invalid(
                "an MCP server is already running on this service; call stopMcpServer() first",
            ));
        }
        let backend = Arc::new(BindingBackend {
            contributor: Arc::clone(&core.contributor),
            writes_enabled: config.options.writes_enabled,
            active: Mutex::new(BTreeMap::new()),
        });
        let mut bearers = Vec::new();
        for (index, token) in config.bearer_tokens.iter().enumerate() {
            let auth = BearerAuth::new(token, json!({ "kind": "mcp-bearer", "tokenIndex": index }))
                .map_err(|problem| errors::invalid(format!("bearerTokens: {problem}")))?;
            bearers.push(auth);
        }
        let server = McpServer::new(backend, Arc::new(BearerSet { bearers }), config.options)
            .map_err(|problem| errors::invalid(format!("MCP server options: {problem}")))?;
        let router = server.router();
        let token = CancellationToken::new();
        let serve_token = token.clone();
        let join = tokio::spawn(async move {
            // Same contract as the desktop accept loop: only the shutdown
            // signal ends it; the listener is dropped and in-flight
            // requests are cut off by `McpServerHandle::shutdown`'s grace.
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move { serve_token.cancelled().await })
                .await;
        });
        core.mcp = Some(McpServerHandle {
            server,
            token,
            join,
        });
        Ok(json!({
            "port": port,
            "url": format!("http://{}:{}", url_host(&config.host), port),
        })
        .to_string())
    }

    /// Stop the MCP server: every session lease is revoked first (cancels
    /// in-flight JS tool callbacks), the listener closes immediately
    /// (follow-up connections are refused), and the accept loop is
    /// force-stopped after the close deadline. Idempotent — stopping a
    /// service that never started, or was already stopped or closed, is a
    /// no-op success. `close()` stops a live server itself, before the
    /// bridge begins Closing.
    #[napi]
    pub async fn stop_mcp_server(&self) -> Result<()> {
        let server = {
            let mut guard = self.core.lock().await;
            guard.as_mut().and_then(|core| core.mcp.take())
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

    fn parse(text: &str) -> Result<McpServerConfig> {
        parse_mcp_options(text)
    }

    fn code(error: napi::Error) -> String {
        serde_json::from_str::<Value>(&error.reason).expect("error payload is JSON")["code"]
            .as_str()
            .expect("payload carries a code")
            .to_owned()
    }

    const TOKEN: &str = "12345678901234567890123456789012";

    #[test]
    fn defaults_fill_every_field_but_the_required_tokens() {
        let config = parse(&json!({ "bearerTokens": [TOKEN] }).to_string()).unwrap();
        assert_eq!(config.host, DEFAULT_HOST);
        assert_eq!(config.port, 0);
        assert_eq!(config.bearer_tokens, vec![TOKEN.to_owned()]);
        assert!(!config.options.writes_enabled);
        assert_eq!(config.options.max_concurrent_calls, 4);
        assert_eq!(config.options.max_batch_size, 8);
        assert_eq!(config.options.max_request_bytes, 4 * 1024 * 1024);
        assert_eq!(config.options.max_sessions_per_principal, 64);
        assert_eq!(config.options.max_call_duration, Duration::from_secs(120));
        assert_eq!(config.options.session_idle_ttl, Duration::from_secs(1800));
    }

    #[test]
    fn tokens_are_required_printable_and_at_least_32_bytes() {
        assert_eq!(code(parse("{}").unwrap_err()), "invalid_input");
        assert_eq!(
            code(parse(&json!({ "bearerTokens": null }).to_string()).unwrap_err()),
            "invalid_input"
        );
        assert_eq!(
            code(parse(&json!({ "bearerTokens": [] }).to_string()).unwrap_err()),
            "invalid_input"
        );
        assert_eq!(
            code(parse(&json!({ "bearerTokens": ["short"] }).to_string()).unwrap_err()),
            "invalid_input"
        );
        assert_eq!(
            code(
                parse(&json!({ "bearerTokens": [TOKEN, {"not": "a string"}] }).to_string())
                    .unwrap_err()
            ),
            "invalid_input"
        );
        // 31 bytes fails, 32 passes — the mirrored bearer floor.
        assert!(parse(&json!({ "bearerTokens": ["x".repeat(31)] }).to_string()).is_err());
        assert!(parse(&json!({ "bearerTokens": ["x".repeat(32)] }).to_string()).is_ok());
        // Whitespace or control characters would corrupt the header.
        assert!(
            parse(&json!({ "bearerTokens": [format!("{} ", "x".repeat(32))] }).to_string())
                .is_err()
        );
        // Duplicates collapse; order (and therefore principal identity)
        // is preserved for the survivors.
        let config = parse(&json!({ "bearerTokens": [TOKEN, TOKEN] }).to_string()).unwrap();
        assert_eq!(config.bearer_tokens.len(), 1);
    }

    #[test]
    fn limits_map_onto_the_crate_options_and_must_be_positive() {
        let config = parse(
            &json!({
                "bearerTokens": [TOKEN],
                "writesEnabled": true,
                "maxConcurrentCalls": 2,
                "maxBatchSize": 3,
                "maxBodyBytes": 1024,
                "maxSessionsPerPrincipal": 5,
                "callDeadlineMs": 1500,
                "idleTtlMs": 60000,
                "allowedHosts": ["toolhost"],
                "allowedOrigins": ["http://localhost:3000"],
            })
            .to_string(),
        )
        .unwrap();
        assert!(config.options.writes_enabled);
        assert_eq!(config.options.max_concurrent_calls, 2);
        assert_eq!(config.options.max_batch_size, 3);
        assert_eq!(config.options.max_request_bytes, 1024);
        assert_eq!(config.options.max_sessions_per_principal, 5);
        assert_eq!(
            config.options.max_call_duration,
            Duration::from_millis(1500)
        );
        assert_eq!(
            config.options.session_idle_ttl,
            Duration::from_millis(60000)
        );
        assert_eq!(config.options.allowed_hosts, vec!["toolhost".to_owned()]);
        assert_eq!(
            config.options.allowed_origins,
            Some(vec!["http://localhost:3000".to_owned()])
        );
        for bad in [
            json!({"bearerTokens": [TOKEN], "maxConcurrentCalls": 0}),
            json!({"bearerTokens": [TOKEN], "maxBatchSize": 0}),
            json!({"bearerTokens": [TOKEN], "maxBodyBytes": 0}),
            json!({"bearerTokens": [TOKEN], "maxSessionsPerPrincipal": 0}),
            json!({"bearerTokens": [TOKEN], "callDeadlineMs": 0}),
            json!({"bearerTokens": [TOKEN], "idleTtlMs": 0}),
            json!({"bearerTokens": [TOKEN], "port": 65536}),
            json!({"bearerTokens": [TOKEN], "host": "a b"}),
            json!({"bearerTokens": [TOKEN], "writesEnabled": "yes"}),
            json!({"bearerTokens": [TOKEN], "allowedHosts": []}),
            json!({"bearerTokens": [TOKEN], "allowedHosts": ["  "]}),
            json!({"bearerTokens": [TOKEN], "allowedOrigins": "http://x"}),
        ] {
            let error = parse(&bad.to_string()).unwrap_err();
            assert_eq!(code(error), "invalid_input", "must reject {bad}");
        }
        assert!(parse("[]").is_err());
        assert!(parse("nonsense").is_err());
    }

    fn tool_result(ok: bool) -> ToolResult {
        ToolResult {
            ok,
            data: Some(json!({"appliedCount": 1, "note": "partial lane"})),
            sources: vec![json!({"kind": "file", "path": "src/app.ts"})],
            warnings: vec!["one warning".to_owned()],
            truncated: false,
            limits: ToolLimits::default(),
            model_data: None,
            summary: Some("applied one edit".to_owned()),
            status: Some(if ok { "ok" } else { "partial" }.to_owned()),
            model_content: None,
        }
    }

    fn text_block(call: &CallToolResult, index: usize) -> String {
        call.content[index]
            .as_text()
            .expect("text content block")
            .text
            .clone()
    }

    #[test]
    fn to_call_result_carries_a_failed_tool_payload_as_an_error_result() {
        // The native sibling of the frozen failed-envelope case: ok:false
        // surfaces as isError:true and the full result payload survives —
        // as structuredContent and as the one mirrored JSON text block.
        let call = to_call_result(&tool_result(false));
        assert_eq!(call.is_error, Some(true), "ok:false is isError");
        let structured = call
            .structured_content
            .clone()
            .expect("the payload travels as structured content");
        assert_eq!(structured["ok"], json!(false));
        assert_eq!(
            structured["data"],
            json!({"appliedCount": 1, "note": "partial lane"})
        );
        assert_eq!(structured["status"], json!("partial"));
        assert_eq!(structured["warnings"], json!(["one warning"]));
        let text: Value =
            serde_json::from_str(&text_block(&call, 0)).expect("text block is payload JSON");
        assert_eq!(
            text, structured,
            "the text block mirrors the structured content"
        );
    }

    #[test]
    fn to_call_result_keeps_a_successful_tool_unflagged() {
        let call = to_call_result(&tool_result(true));
        assert_eq!(call.is_error, None, "ok:true is not an error result");
        let structured = call.structured_content.expect("payload survives");
        assert_eq!(structured["ok"], json!(true));
        assert_eq!(structured["status"], json!("ok"));
    }

    #[test]
    fn an_unserializable_tool_payload_stays_an_error_result() {
        // Ports the frozen projection lane's unserializable case: a result
        // whose payload refuses to serialize is an unusable outcome — it
        // must never masquerade as success, and the wire carries only the
        // code, never the half-written payload.
        // A real serde_json failure: a map with non-string keys cannot be
        // written as JSON, which is exactly the unusable-payload condition.
        let mut broken = std::collections::BTreeMap::new();
        broken.insert((1u8, 2u8), 3u8);
        let failure =
            serde_json::to_string(&broken).expect_err("non-string map keys never serialize");
        let call = project_call_result(Err(failure), json!({"half": "written"}), true);
        assert_eq!(
            call.is_error,
            Some(true),
            "an unusable outcome is never a fabricated success"
        );
        assert_eq!(call.content.len(), 1);
        let text: Value =
            serde_json::from_str(&text_block(&call, 0)).expect("refusal text is JSON");
        assert_eq!(
            text,
            json!({"errorCode": "tool_result_unserializable"}),
            "the wire carries the code, not the broken payload"
        );
        assert_eq!(
            call.structured_content,
            Some(json!({"errorCode": "tool_result_unserializable"})),
            "the structured content is the code, not the half-written payload"
        );
    }
}
