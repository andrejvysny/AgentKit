//! Task-bound governed MCP broker (AGENTKIT-114).
//!
//! One loopback HTTP MCP endpoint per live native attempt, built on the
//! existing `agentkit-mcp` server (bearer auth with constant-time compare,
//! session/concurrency bounds, cancellation). The broker is the ONLY path
//! on which an application tool executes during a native turn:
//!
//! * the catalog is frozen per attempt (names + schemas); vendor tool
//!   CARDS from the stream are data, never dispatch (AGENTKIT-108);
//! * every call re-checks attempt liveness (generation), then the host
//!   `BrokerAuthority` validates arguments, gates approvals/questions
//!   durably, and re-checks authority immediately before dispatch;
//! * per-attempt bounds: call count, argument bytes, result bytes, call
//!   wall-clock — exhaustion is an in-band `is_error` result the model can
//!   observe, never a silent drop;
//! * revocation is immediate: at attempt settlement, cancellation or lease
//! * loss, tokens stop authenticating and live calls are cancelled.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use agentkit_mcp::ErrorData;
use agentkit_mcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use agentkit_mcp::{
    Backend, BearerAuth, CatalogTool, McpServer, RpcId, ServerOptions, SessionScope,
};

/// One catalog entry frozen at attempt start.
#[derive(Clone, Debug)]
pub struct BrokerToolDefinition {
    /// Server-local name; Claude sees `mcp__<server>__<name>`.
    pub name: String,
    pub description: String,
    /// JSON Schema for arguments (validated by the authority).
    pub input_schema: Value,
    /// Host-owned effect classification; never inferred from vendor data.
    pub writes: bool,
}

/// Identity context of one broker call, bound to the live attempt.
#[derive(Clone, Debug)]
pub struct BrokerInvocation {
    pub task_id: String,
    pub attempt_id: String,
    pub server_name: String,
    /// Process generation that owns the attempt (stale fencing, 116).
    pub generation: u64,
    /// Stable identity of the calling vendor session, as observed.
    pub actor_id: String,
    pub tool_name: String,
    /// UNIQUE per-dispatch identity, minted at broker ingress (AGENTKIT-115
    /// hardening): approvals, effects and durable receipts key on the
    /// invocation, never on argument digests. Argument hashes stay
    /// integrity evidence only.
    pub invocation_id: String,
    /// JSON-RPC request id when the transport carried one, kept TYPED
    /// (review R7): the only evidence that can PROVE a redelivery (as
    /// opposed to a new call that happens to carry identical arguments).
    /// Numeric `1` and string `"1"` are DIFFERENT ids — stringifying them
    /// for identity would fuse two provably distinct calls.
    pub request_id: Option<RpcId>,
}

/// Successful in-band execution result.
#[derive(Clone, Debug)]
pub struct BrokerExecuted {
    /// Content parts serialized into the MCP result (text-first).
    pub content: Vec<Value>,
    /// Optional structured output for durable evidence.
    pub structured: Option<Value>,
}

/// A rejection the model may observe in-band. `code` is stable.
#[derive(Clone, Debug)]
pub struct BrokerDenial {
    pub code: String,
    pub message: String,
}

/// The host authority: approval gating, argument validation, effect
/// execution (AGENTKIT-107 §6; AGENTKIT-115 supplies durable decisions).
#[async_trait]
pub trait BrokerAuthority: Send + Sync + 'static {
    /// `Some(generation)` while the attempt owns a live process; `None`
    /// means the attempt is settled/cancelled — every call is refused.
    fn attempt_generation(&self) -> Option<u64>;

    /// Validate the exact arguments and obtain (or replay) the durable
    /// human decision. Runs BEFORE any effect. Denials are durable-visible.
    async fn authorize(
        &self,
        invocation: &BrokerInvocation,
        arguments: &Value,
    ) -> Result<(), BrokerDenial>;

    /// Execute the authorized operation. Called only after `authorize`
    /// returned `Ok`; the host re-checks authority inside here.
    async fn execute(
        &self,
        invocation: &BrokerInvocation,
        arguments: Value,
    ) -> Result<BrokerExecuted, BrokerDenial>;

    /// Durable bookkeeping hook for every terminal call outcome.
    fn record(&self, _invocation: &BrokerInvocation, _outcome: &Value) {}
}

/// Per-attempt resource bounds (AGENTKIT-116).
#[derive(Clone, Copy, Debug)]
pub struct BrokerLimits {
    pub max_calls: usize,
    pub max_args_bytes: usize,
    pub max_result_bytes: usize,
    pub call_timeout_ms: i64,
    /// Hard bound on `BrokerAuthority::authorize` (the human approval
    /// gate). Review R5: the approval window a product advertises must be
    /// COMPATIBLE with, and strictly below, the layers above it — the
    /// vendor MCP request budget must exceed this gate plus the effect
    /// timeout, and the consumer's displayed approval deadline must fit
    /// inside this budget so an answered approval never races an outer
    /// transport expiry.
    pub authorize_timeout_ms: i64,
}

/// The bridge's default human-gate budget (review R5). OneMind clamps its
/// advertised approval window under this minus a margin, and the vendor
/// MCP request budget (claude launch) exceeds this plus the effect
/// timeout — one coherent, compatible deadline chain.
pub const BROKER_AUTHORIZE_BUDGET_MS: i64 = 360_000;

impl Default for BrokerLimits {
    fn default() -> Self {
        BrokerLimits {
            max_calls: 64,
            max_args_bytes: 256 * 1024,
            max_result_bytes: 1024 * 1024,
            call_timeout_ms: 120_000,
            authorize_timeout_ms: BROKER_AUTHORIZE_BUDGET_MS,
        }
    }
}

#[derive(Default)]
struct Live {
    calls: usize,
    revoked: bool,
}

/// Identity of a PROVEN-redelivery candidate (review R7, AGENTKIT-115
/// lane): the per-session broker identity plus the TYPED JSON-RPC request
/// id. One `BrokerBackend` (and therefore one memo map) is shared by
/// EVERY MCP session connected to the attempt endpoint — the transport
/// mints a fresh `actor_id` per initialized session — so the request id
/// alone is NOT a session-private key: two concurrent sessions restating
/// id `1` would otherwise replay each other's tool results. Scoping the
/// memo by `actor_id` keeps redelivery proof meaningful (same session,
/// same typed id), and keeping the id typed (numeric `1` ≠ string `"1"`)
/// keeps provably distinct calls from fusing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct MemoKey {
    /// Session identity as observed at ingress (`SessionScope::actor_id`).
    actor_id: String,
    /// JSON-RPC id, stored typed — never stringified.
    request_id: RpcId,
}

/// The attempt-scoped MCP backend. Cheap to share; revocation is global.
pub struct BrokerBackend {
    task_id: String,
    attempt_id: String,
    server_name: String,
    catalog: Vec<CatalogTool>,
    /// Exact allowed names (server-local), frozen.
    allowed: Vec<String>,
    limits: BrokerLimits,
    authority: Arc<dyn BrokerAuthority>,
    live: Mutex<Live>,
    /// In-flight gauge: calls currently parked in the authority (approval
    /// waits, effects). Unlike `live.calls` (a lifetime quota, never
    /// decremented) this returns to zero as calls settle — the host reads
    /// it to distinguish broker-bound stdout silence from inference
    /// silence (review §2.9).
    in_flight: std::sync::atomic::AtomicUsize,
    /// Cumulative milliseconds spent inside `BrokerAuthority::authorize`
    /// (human approval waits; the effect path is separately bounded by
    /// `call_timeout_ms`). The stream loop CREDITS this against the turn
    /// deadline: a parked approval is a human decision, not inference
    /// time, and the consumer's own approval timeout bounds the wait
    /// (review §2.9: an approval answered inside the displayed deadline
    /// succeeds).
    parked_ms: std::sync::atomic::AtomicU64,
    /// Starts of the CURRENTLY pending authorization gates (empty when no
    /// call is parked). Credit is LIVE: a human who has not answered yet
    /// must already be exempt from the turn deadline, not reimbursed only
    /// after they answer. Live credit measures from the OLDEST active
    /// park; completed parks move into `parked_ms`.
    park_gate: Mutex<Vec<std::time::Instant>>,
    /// Proven-redelivery memo: a call whose transport RESTATES the same
    /// typed JSON-RPC request id ON THE SAME SESSION replays the first
    /// outcome instead of executing twice (AGENTKIT-115: only the exact
    /// request id proves redelivery; identical arguments never do).
    /// Keyed by [`MemoKey`] — session identity + typed id — because this
    /// map lives on the per-attempt backend that all connected sessions
    /// share (review R7).
    memos: Mutex<HashMap<MemoKey, CallToolResult>>,
    /// In-flight counterpart of `memos`: an id restated while its first
    /// execution is still running is refused, not doubled. Same
    /// session-scoped typed key.
    inflight: Mutex<HashSet<MemoKey>>,
    cancellation: CancellationToken,
}

impl BrokerBackend {
    pub fn new(
        task_id: impl Into<String>,
        attempt_id: impl Into<String>,
        server_name: &str,
        catalog: Vec<BrokerToolDefinition>,
        limits: BrokerLimits,
        authority: Arc<dyn BrokerAuthority>,
        cancellation: CancellationToken,
    ) -> Result<Arc<Self>, BrokerDenial> {
        validate_server_name(server_name)?;
        let mut defs = Vec::with_capacity(catalog.len());
        let mut allowed = Vec::with_capacity(catalog.len());
        for entry in catalog {
            validate_tool_name(&entry.name)?;
            if entry.input_schema.get("type").and_then(Value::as_str) != Some("object") {
                return Err(BrokerDenial {
                    code: "broker_catalog".into(),
                    message: format!("tool {} schema must be an object type", entry.name),
                });
            }
            allowed.push(entry.name.clone());
            defs.push(entry);
        }
        if allowed.is_empty() {
            return Err(BrokerDenial {
                code: "broker_catalog".into(),
                message: "broker catalog is empty".into(),
            });
        }
        let tools: Vec<CatalogTool> = defs
            .into_iter()
            .map(|entry| CatalogTool {
                definition: Tool::new(
                    entry.name,
                    entry.description,
                    std::sync::Arc::new(
                        entry.input_schema.as_object().cloned().unwrap_or_default(),
                    ),
                ),
                writes: entry.writes,
            })
            .collect();
        Ok(Arc::new(BrokerBackend {
            task_id: task_id.into(),
            attempt_id: attempt_id.into(),
            server_name: server_name.into(),
            catalog: tools,
            allowed,
            limits,
            authority,
            live: Mutex::new(Live::default()),
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            parked_ms: std::sync::atomic::AtomicU64::new(0),
            park_gate: Mutex::new(Vec::new()),
            memos: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
            cancellation,
        }))
    }

    /// Exact CLI-visible tool names (`mcp__<server>__<name>`), frozen.
    pub fn cli_tool_names(&self) -> Vec<String> {
        self.allowed
            .iter()
            .map(|name| format!("mcp__{}__{}", self.server_name, name))
            .collect()
    }

    pub fn has_writes(&self) -> bool {
        self.catalog.iter().any(|entry| entry.writes)
    }

    /// Immediate revocation for settlement/cancellation/lease loss.
    pub fn revoke(&self) {
        self.live.lock().expect("broker lock").revoked = true;
        self.cancellation.cancel();
    }

    /// Calls currently parked in the authority (approval waits, effects):
    /// the host consults this to tell a legitimate broker-bound silence
    /// from inference silence (review §2.9).
    #[must_use]
    pub fn live_calls(&self) -> usize {
        self.in_flight.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Cumulative ms the authority spent (and is still spending) in
    /// `authorize` (approval parks); the stream loop credits this against
    /// the turn deadline.
    #[must_use]
    pub fn parked_ms(&self) -> u64 {
        let done = self.parked_ms.load(std::sync::atomic::Ordering::Relaxed);
        let live = self
            .park_gate
            .lock()
            .expect("park gate lock")
            .iter()
            .map(|start| start.elapsed().as_millis() as u64)
            .max()
            .unwrap_or(0);
        done.saturating_add(live)
    }

    fn denied(&self, code: &str, message: impl Into<String>) -> CallToolResult {
        let message = message.into();
        let denial = json!({"denied": true, "code": code, "message": message});
        self.authority.record(
            &BrokerInvocation {
                task_id: self.task_id.clone(),
                attempt_id: self.attempt_id.clone(),
                server_name: self.server_name.clone(),
                generation: 0,
                actor_id: String::new(),
                tool_name: String::new(),
                invocation_id: String::new(),
                request_id: None,
            },
            &denial,
        );
        CallToolResult::error(vec![ContentBlock::text(denial.to_string())])
    }

    /// Authorization + execution once the call passed mechanical bounds.
    /// Every terminal outcome is recorded durably by the authority under
    /// the exact invocation identity.
    async fn dispatch_authorized(
        &self,
        invocation: &BrokerInvocation,
        arguments: Value,
    ) -> CallToolResult {
        if let Err(denial) = {
            // Timing the human gate: approval parks are credited (LIVE,
            // while unanswered) against the turn deadline by the stream
            // loop — a human decision is not inference time. Only
            // authorize is timed; the effect path below stays charged and
            // call_timeout-bound. RAII (review R5): if this future is
            // ABANDONED mid-park (outer request cancelled, authorize
            // deadline fired), the gate still closes and the gauge still
            // returns — no leaked `live_calls` and no park whose credit
            // keeps growing after the call is gone. The gate is also
            // DEADLINED: a consumer that never answers cannot pin the
            // attempt open forever.
            let _park = ParkGuard::new(self);
            let outcome = tokio::time::timeout(
                Duration::from_millis(self.limits.authorize_timeout_ms.max(1) as u64),
                self.authority.authorize(invocation, &arguments),
            )
            .await;
            match outcome {
                Err(_) => Err(BrokerDenial {
                    code: "approval_timeout".into(),
                    message: format!(
                        "no authority decision arrived within {} ms",
                        self.limits.authorize_timeout_ms
                    ),
                }),
                Ok(denial) => denial,
            }
        } {
            let payload = json!({"denied": true, "code": denial.code, "message": denial.message});
            self.authority.record(invocation, &payload);
            return CallToolResult::error(vec![ContentBlock::text(payload.to_string())]);
        }
        let executed = tokio::time::timeout(
            Duration::from_millis(self.limits.call_timeout_ms.max(1) as u64),
            self.authority.execute(invocation, arguments),
        )
        .await;
        match executed {
            Err(_) => {
                let payload = json!({
                    "denied": true, "code": "call_timeout",
                    "message": format!("call exceeded {} ms", self.limits.call_timeout_ms),
                });
                self.authority.record(invocation, &payload);
                CallToolResult::error(vec![ContentBlock::text(payload.to_string())])
            }
            Ok(Err(denial)) => {
                let payload =
                    json!({"denied": true, "code": denial.code, "message": denial.message});
                self.authority.record(invocation, &payload);
                CallToolResult::error(vec![ContentBlock::text(payload.to_string())])
            }
            Ok(Ok(result)) => {
                let mut parts = Vec::new();
                for part in &result.content {
                    parts.push(ContentBlock::text(part.to_string()));
                }
                if parts.is_empty() {
                    parts.push(ContentBlock::text("{}".to_owned()));
                }
                let serialized = serde_json::to_string(&result.content)
                    .unwrap_or_default()
                    .len();
                if serialized > self.limits.max_result_bytes {
                    // Never silently truncate: the model sees the bound.
                    let payload = json!({
                        "denied": true, "code": "quota_result",
                        "message": format!("result exceeds {} bytes", self.limits.max_result_bytes),
                    });
                    self.authority.record(invocation, &payload);
                    return CallToolResult::error(vec![ContentBlock::text(payload.to_string())]);
                }
                let outcome = json!({
                    "executed": true,
                    "tool": invocation.tool_name,
                    "invocationId": invocation.invocation_id,
                    "content": result.content,
                    "structured": result.structured,
                });
                self.authority.record(invocation, &outcome);
                let mut response = CallToolResult::success(parts);
                response.structured_content = result.structured;
                response.is_error = Some(false);
                response
            }
        }
    }
}

/// RAII counterpart of the `in_flight` gauge (review R5). The increment
/// is part of construction — a constructed guard always has a matching
/// decrement, and a future abandoned mid-flight cannot strand the gauge.
struct InFlightGuard<'a>(&'a BrokerBackend);

impl<'a> InFlightGuard<'a> {
    fn new(backend: &'a BrokerBackend) -> Self {
        backend
            .in_flight
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(backend)
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0
            .in_flight
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// RAII counterpart of an `authorize` park (review R5): the gate opens on
/// creation (live deadline credit starts) and CLOSES on drop — whether
/// authorize answered, denied, timed out, or the whole request future was
/// abandoned. Parked time accrued so far moves into `parked_ms`; nothing
/// keeps growing once the call is gone.
struct ParkGuard<'a> {
    backend: &'a BrokerBackend,
    start: std::time::Instant,
}

impl<'a> ParkGuard<'a> {
    fn new(backend: &'a BrokerBackend) -> Self {
        let start = std::time::Instant::now();
        backend
            .park_gate
            .lock()
            .expect("park gate lock")
            .push(start);
        Self { backend, start }
    }
}

impl Drop for ParkGuard<'_> {
    fn drop(&mut self) {
        {
            let mut slots = self.backend.park_gate.lock().expect("park gate lock");
            if let Some(position) = slots.iter().position(|gate| *gate == self.start) {
                slots.swap_remove(position);
            }
        }
        self.backend.parked_ms.fetch_add(
            self.start.elapsed().as_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

#[async_trait]
impl Backend for BrokerBackend {
    async fn list_tools(&self, _scope: &SessionScope) -> Result<Vec<CatalogTool>, ErrorData> {
        if self.live.lock().expect("broker lock").revoked
            || self.authority.attempt_generation().is_none()
        {
            return Err(ErrorData::internal_error("broker revoked", None));
        }
        Ok(self.catalog.clone())
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: JsonObject,
        scope: &SessionScope,
    ) -> Result<CallToolResult, ErrorData> {
        // Attempt liveness first: a settled attempt answers nothing.
        let Some(generation) = self.authority.attempt_generation() else {
            return Ok(self.denied("attempt_not_live", "attempt no longer live"));
        };
        {
            let live = self.live.lock().expect("broker lock");
            if live.revoked {
                return Ok(self.denied("revoked", "broker revoked"));
            }
        }
        if self.cancellation.is_cancelled() || scope.cancellation.is_cancelled() {
            return Ok(self.denied("cancelled", "attempt cancelled"));
        }
        if !self.allowed.iter().any(|allowed| allowed == name) {
            return Ok(self.denied("unknown_tool", format!("unknown tool {name}")));
        }
        let arguments = Value::Object(arguments);
        if arguments.to_string().len() > self.limits.max_args_bytes {
            return Ok(self.denied(
                "quota_args",
                format!("arguments exceed {} bytes", self.limits.max_args_bytes),
            ));
        }
        // Proven redelivery BEFORE any budget spend: the same typed
        // JSON-RPC request id restated ON THE SAME SESSION replays the
        // recorded outcome — it is the same call, not a second one.
        // Another session, or a different id TYPE (numeric `1` is not
        // string `"1"`), or no id at all, is an independent call even for
        // byte-identical arguments (AGENTKIT-115; review R7: the memo key
        // is session identity + typed id, never a stringified id alone).
        let request_id = scope.request_id.clone();
        let memo_key = request_id.as_ref().map(|request_id| MemoKey {
            actor_id: scope.actor_id.clone(),
            request_id: request_id.clone(),
        });
        if let Some(key) = &memo_key {
            if let Some(replay) = self.memos.lock().expect("broker memos").get(key).cloned() {
                return Ok(replay);
            }
            if !self
                .inflight
                .lock()
                .expect("broker inflight")
                .insert(key.clone())
            {
                return Ok(self.denied(
                    "duplicate_in_progress",
                    "the identical request is still executing; retry after it settles",
                ));
            }
        }
        // Count the call before authorize: approvals that park also spend
        // the attempt's CALL QUOTA (no infinite approval loops), even
        // though their wall time is credited against the turn deadline.
        {
            let mut live = self.live.lock().expect("broker lock");
            live.calls += 1;
            if live.calls > self.limits.max_calls {
                if let Some(key) = &memo_key {
                    self.inflight.lock().expect("broker inflight").remove(key);
                }
                return Ok(self.denied(
                    "quota_calls",
                    format!("attempt call budget {} exhausted", self.limits.max_calls),
                ));
            }
        }
        // Unique per-dispatch identity, minted at ingress: approvals,
        // effects and receipts key on THIS invocation, never on arguments.
        let invocation = BrokerInvocation {
            task_id: self.task_id.clone(),
            attempt_id: self.attempt_id.clone(),
            server_name: self.server_name.clone(),
            generation,
            actor_id: scope.actor_id.clone(),
            tool_name: name.to_owned(),
            invocation_id: format!("inv_{}", uuid::Uuid::new_v4().simple()),
            request_id: request_id.clone(),
        };
        let outcome = {
            // RAII (review R5): the gauge returns to zero even if the
            // dispatch future is abandoned mid-flight — an abandoned
            // request must never leave `live_calls()` lying about a call
            // that no longer exists.
            let _in_flight = InFlightGuard::new(self);
            self.dispatch_authorized(&invocation, arguments).await
        };
        if let Some(key) = &memo_key {
            self.inflight.lock().expect("broker inflight").remove(key);
            self.memos
                .lock()
                .expect("broker memos")
                .insert(key.clone(), outcome.clone());
        }
        Ok(outcome)
    }

    fn revoke_actor(&self, _actor_id: &str) {
        // One attempt, one principal: any revocation revokes the attempt.
        self.revoke();
    }
}

fn validate_server_name(name: &str) -> Result<(), BrokerDenial> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(BrokerDenial {
            code: "broker_catalog".into(),
            message: format!("invalid MCP server name {name:?}"),
        })
    }
}

fn validate_tool_name(name: &str) -> Result<(), BrokerDenial> {
    let ok = !name.is_empty()
        && name.len() <= 96
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(BrokerDenial {
            code: "broker_catalog".into(),
            message: format!("invalid tool name {name:?}"),
        })
    }
}

/// A live broker endpoint for exactly one attempt.
pub struct AttemptBroker {
    backend: Arc<BrokerBackend>,
    token: String,
    url: String,
    server: McpServer,
    serve: tokio::task::JoinHandle<()>,
    cancellation: CancellationToken,
}

impl AttemptBroker {
    /// Bind a loopback endpoint and serve the frozen catalog. The token is
    /// returned to the driver ONLY through [`AttemptBroker::spec`] (private
    /// 0600 per-run file; never argv).
    pub async fn start(
        task_id: &str,
        attempt_id: &str,
        server_name: &str,
        catalog: Vec<BrokerToolDefinition>,
        limits: BrokerLimits,
        authority: Arc<dyn BrokerAuthority>,
    ) -> Result<Arc<Self>, String> {
        let cancellation = CancellationToken::new();
        let backend = BrokerBackend::new(
            task_id,
            attempt_id,
            server_name,
            catalog,
            limits,
            authority,
            cancellation.clone(),
        )
        .map_err(|denial| format!("{}: {}", denial.code, denial.message))?;
        let token = format!(
            "brk-{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let auth = BearerAuth::new(
            &token,
            json!({"brokerTask": task_id, "brokerAttempt": attempt_id}),
        )
        .map_err(|e| format!("broker token: {e}"))?;
        let mut options = ServerOptions {
            writes_enabled: backend.has_writes(),
            max_request_bytes: limits.max_args_bytes.saturating_mul(2).max(64 * 1024),
            max_call_duration: Duration::from_millis(limits.call_timeout_ms.max(1) as u64),
            ..ServerOptions::default()
        };
        options.session_idle_ttl = Duration::from_secs(3600);
        let server = McpServer::new(backend.clone(), Arc::new(auth), options)
            .map_err(|e| format!("broker server: {e}"))?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("broker bind: {e}"))?;
        let url = format!(
            "http://{}/mcp",
            listener
                .local_addr()
                .map_err(|e| format!("broker addr: {e}"))?
        );
        let router = server.router();
        let shutdown = cancellation.clone();
        let serve = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move { shutdown.cancelled().await })
                .await;
        });
        Ok(Arc::new(AttemptBroker {
            backend,
            token,
            url,
            server,
            serve,
            cancellation,
        }))
    }

    /// The endpoint spec to hand the driver for `plan_turn`.
    pub fn spec(&self) -> agentkit_native::McpEndpointSpec {
        agentkit_native::McpEndpointSpec {
            server_name: self.backend.server_name.clone(),
            broker_tool_names: self.backend.cli_tool_names(),
            stdio_command: None,
            http_url: Some(self.url.clone()),
            auth_token: Some(self.token.clone()),
        }
    }

    pub fn backend(&self) -> &Arc<BrokerBackend> {
        &self.backend
    }

    /// Calls currently parked in the authority (approval waits, effects).
    /// While nonzero, vendor stdout silence is LEGITIMATE — the CLI is
    /// waiting on this broker, not thinking — so the host's idle budget
    /// must not preempt such a wait (review §2.9: a human decision is not
    /// inference silence; the turn deadline and the broker call budget
    /// still bound it).
    #[must_use]
    pub fn live_calls(&self) -> usize {
        self.backend.live_calls()
    }

    /// Cumulative ms parked behind approval gates; the stream loop credits
    /// this against the wall-clock turn deadline (review §2.9).
    #[must_use]
    pub fn parked_ms(&self) -> u64 {
        self.backend.parked_ms()
    }

    /// Stop serving and revoke every token; bounded, idempotent.
    pub async fn shutdown(&self) {
        self.backend.revoke();
        self.server.close().await;
        self.cancellation.cancel();
        self.serve.abort();
    }
}

/// Dropped without an explicit shutdown (aborted execution future, failed
/// settlement): the broker must stop authenticating and stop serving
/// IMMEDIATELY — a detached HTTP endpoint is a live tool door after the
/// attempt that owned it is gone. Aborting the serve task is synchronous
/// and bounded; `server.close()` remains the graceful path.
impl Drop for AttemptBroker {
    fn drop(&mut self) {
        self.backend.revoke();
        self.cancellation.cancel();
        self.serve.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Review round-3 F4: the R5 deadline chain — human approval window ≤
    /// authorize budget − margin, and the outer vendor MCP request budget
    /// strictly above (authorize budget + effect timeout) — was tied only
    /// by comments across crates. This test fails mechanically if anyone
    /// edits one constant and re-opens the drift R5 closed.
    #[test]
    fn deadline_chain_stays_compatible_by_construction() {
        // OneMind's approvalTimeoutMs default and its clamp margin
        // (native_chat/broker.rs clamps the human window to
        // BROKER_AUTHORIZE_BUDGET_MS − 30 s).
        const HUMAN_APPROVAL_WINDOW_MS: i64 = 300_000;
        const CLAMP_MARGIN_MS: i64 = 30_000;
        let call_timeout = BrokerLimits::default().call_timeout_ms;
        const {
            assert!(
                BROKER_AUTHORIZE_BUDGET_MS
                    >= HUMAN_APPROVAL_WINDOW_MS + CLAMP_MARGIN_MS,
                "authorize budget must cover the full human approval window plus margin"
            );
        }
        assert!(
            agentkit_claude::launch::BROKER_MCP_REQUEST_BUDGET_MS
                > BROKER_AUTHORIZE_BUDGET_MS + call_timeout,
            "the outer MCP request budget must exceed approval parking plus the effect timeout: \
             {} !> {BROKER_AUTHORIZE_BUDGET_MS} + {call_timeout}",
            agentkit_claude::launch::BROKER_MCP_REQUEST_BUDGET_MS
        );
    }

    struct ScriptedAuthority {
        live: bool,
        authorize_result: Option<BrokerDenial>,
        execute_result: Option<BrokerDenial>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl BrokerAuthority for ScriptedAuthority {
        fn attempt_generation(&self) -> Option<u64> {
            if self.live { Some(7) } else { None }
        }
        async fn authorize(
            &self,
            _invocation: &BrokerInvocation,
            _arguments: &Value,
        ) -> Result<(), BrokerDenial> {
            match &self.authorize_result {
                Some(denial) => Err(BrokerDenial {
                    code: denial.code.clone(),
                    message: denial.message.clone(),
                }),
                None => Ok(()),
            }
        }
        async fn execute(
            &self,
            _invocation: &BrokerInvocation,
            _arguments: Value,
        ) -> Result<BrokerExecuted, BrokerDenial> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.execute_result {
                Some(denial) => Err(BrokerDenial {
                    code: denial.code.clone(),
                    message: denial.message.clone(),
                }),
                None => Ok(BrokerExecuted {
                    content: vec![json!({"note": "found"})],
                    structured: Some(json!({"id": 1})),
                }),
            }
        }
    }

    fn catalog() -> Vec<BrokerToolDefinition> {
        vec![
            BrokerToolDefinition {
                name: "search_notes".into(),
                description: "Search notes".into(),
                input_schema: json!({"type":"object","properties":{"q":{"type":"string"}}}),
                writes: false,
            },
            BrokerToolDefinition {
                name: "propose_note".into(),
                description: "Propose a note".into(),
                input_schema: json!({"type":"object","properties":{}}),
                writes: true,
            },
        ]
    }

    fn authority(live: bool) -> Arc<ScriptedAuthority> {
        Arc::new(ScriptedAuthority {
            live,
            authorize_result: None,
            execute_result: None,
            calls: AtomicUsize::new(0),
        })
    }

    fn scope() -> SessionScope {
        SessionScope {
            principal: json!({"attempt":"att-1"}),
            chat_id: None,
            actor_id: "vendor-session-1".into(),
            request_id: None,
            cancellation: CancellationToken::new(),
        }
    }

    fn scope_with(actor: &str, request_id: Option<RpcId>) -> SessionScope {
        SessionScope {
            actor_id: actor.into(),
            request_id,
            ..scope()
        }
    }

    /// Distinguishes every EXECUTION by a sequence number baked into the
    /// result: a memoized replay necessarily carries the ORIGINAL seq, so
    /// an illegitimate cross-session or cross-type memo hit is directly
    /// observable in the returned content, not merely inferred from a
    /// counter (review R7).
    struct SequencingAuthority {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl BrokerAuthority for SequencingAuthority {
        fn attempt_generation(&self) -> Option<u64> {
            Some(7)
        }
        async fn authorize(
            &self,
            _invocation: &BrokerInvocation,
            _arguments: &Value,
        ) -> Result<(), BrokerDenial> {
            Ok(())
        }
        async fn execute(
            &self,
            _invocation: &BrokerInvocation,
            _arguments: Value,
        ) -> Result<BrokerExecuted, BrokerDenial> {
            let seq = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(BrokerExecuted {
                content: vec![json!({"note": "found", "seq": seq})],
                structured: Some(json!({"seq": seq})),
            })
        }
    }

    async fn sequenced_backend() -> (Arc<BrokerBackend>, Arc<SequencingAuthority>) {
        let authority = Arc::new(SequencingAuthority {
            calls: AtomicUsize::new(0),
        });
        let backend = BrokerBackend::new(
            "task-1",
            "att-1",
            "onemind",
            catalog(),
            BrokerLimits::default(),
            authority.clone(),
            CancellationToken::new(),
        )
        .unwrap();
        (backend, authority)
    }

    /// Review R7 (a): numeric `1` and string `"1"` are DIFFERENT JSON-RPC
    /// ids. A stringify-then-key memo once fused them; the typed key must
    /// not — restating the other type is an independent call.
    #[tokio::test]
    async fn memo_never_collapses_numeric_and_string_request_ids() {
        let (backend, authority) = backend(true).await;
        let mut args = JsonObject::new();
        args.insert("q".into(), json!("hello"));
        for id in [RpcId::Number(1), RpcId::String("1".to_owned())] {
            let result = backend
                .call_tool(
                    "search_notes",
                    args.clone(),
                    &scope_with("vendor-session-1", Some(id)),
                )
                .await
                .unwrap();
            assert_eq!(denial_of(&result), None);
        }
        assert_eq!(
            authority.calls.load(Ordering::SeqCst),
            2,
            "string \"1\" must not replay numeric 1's memoized result"
        );
    }

    /// Review R7 (b): the memo lives on the per-attempt backend shared by
    /// EVERY connected session, so a second session restating the same
    /// typed id is a NEW call — it must execute its own effect, never
    /// replay the first session's result.
    #[tokio::test]
    async fn memo_is_scoped_to_the_session_that_earned_it() {
        let (backend, authority) = sequenced_backend().await;
        let mut args = JsonObject::new();
        args.insert("q".into(), json!("hello"));
        let first = backend
            .call_tool(
                "search_notes",
                args.clone(),
                &scope_with("vendor-session-1", Some(RpcId::Number(7))),
            )
            .await
            .unwrap();
        let second = backend
            .call_tool(
                "search_notes",
                args,
                &scope_with("vendor-session-2", Some(RpcId::Number(7))),
            )
            .await
            .unwrap();
        assert_eq!(denial_of(&first), None);
        assert_eq!(denial_of(&second), None);
        assert_eq!(
            authority.calls.load(Ordering::SeqCst),
            2,
            "another session's id restatement is not a redelivery"
        );
        // The second session observed its OWN execution (seq 2), not a
        // replay of the first session's effect (seq 1).
        assert!(
            second.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("\"seq\":2"),
            "second session must never see the first session's memoized result"
        );
    }

    /// Review R7 (c): the widened key must not weaken AGENTKIT-115
    /// idempotency — same session, same typed id, same arguments replays
    /// the memoized outcome byte-for-byte and executes exactly once.
    #[tokio::test]
    async fn same_session_same_typed_id_replays_exactly_once_executed() {
        let (backend, authority) = sequenced_backend().await;
        let mut args = JsonObject::new();
        args.insert("q".into(), json!("hello"));
        let first = backend
            .call_tool(
                "search_notes",
                args.clone(),
                &scope_with("vendor-session-1", Some(RpcId::Number(7))),
            )
            .await
            .unwrap();
        let replay = backend
            .call_tool(
                "search_notes",
                args,
                &scope_with("vendor-session-1", Some(RpcId::Number(7))),
            )
            .await
            .unwrap();
        assert_eq!(denial_of(&first), None);
        assert_eq!(
            authority.calls.load(Ordering::SeqCst),
            1,
            "the proven redelivery executed nothing"
        );
        assert_eq!(
            serde_json::to_string(&replay).unwrap(),
            serde_json::to_string(&first).unwrap(),
            "the redelivery replays the memoized outcome exactly"
        );
    }

    async fn backend(live: bool) -> (Arc<BrokerBackend>, Arc<ScriptedAuthority>) {
        let authority = authority(live);
        let backend = BrokerBackend::new(
            "task-1",
            "att-1",
            "onemind",
            catalog(),
            BrokerLimits::default(),
            authority.clone(),
            CancellationToken::new(),
        )
        .unwrap();
        (backend, authority)
    }

    fn denial_of(result: &CallToolResult) -> Option<Value> {
        if result.is_error == Some(true) {
            result
                .content
                .first()
                .and_then(|block| serde_json::from_str(&block.as_text()?.text.clone()).ok())
        } else {
            None
        }
    }

    #[tokio::test]
    async fn catalog_is_frozen_and_named_for_the_cli() {
        let (backend, _) = backend(true).await;
        let tools = backend.list_tools(&scope()).await.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(
            backend.cli_tool_names(),
            vec![
                "mcp__onemind__search_notes".to_string(),
                "mcp__onemind__propose_note".to_string(),
            ]
        );
        assert!(backend.has_writes());
    }

    #[tokio::test]
    async fn unknown_tool_never_reaches_the_authority() {
        let (backend, authority) = backend(true).await;
        let result = backend
            .call_tool("nope", Default::default(), &scope())
            .await
            .unwrap();
        assert_eq!(denial_of(&result).unwrap()["code"], "unknown_tool");
        assert_eq!(authority.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn settled_attempt_is_refused_everywhere() {
        let (backend, authority) = backend(false).await;
        assert!(
            backend
                .list_tools(&scope())
                .await
                .is_err_and(|e| e.message.contains("revoked"))
        );
        let result = backend
            .call_tool("search_notes", Default::default(), &scope())
            .await
            .unwrap();
        assert_eq!(denial_of(&result).unwrap()["code"], "attempt_not_live");
        assert_eq!(authority.calls.load(Ordering::SeqCst), 0);
    }

    /// Review §2.9 support: the host reads `live_calls()` to keep a turn
    /// alive while a call is parked in the authority (approval wait or
    /// effect). The gauge must rise exactly while parked and fall to zero
    /// as the call settles.
    #[tokio::test]
    async fn in_flight_gauge_tracks_parked_calls() {
        struct ParkingAuthority {
            release: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
            entered: tokio::sync::Notify,
        }
        #[async_trait]
        impl BrokerAuthority for ParkingAuthority {
            fn attempt_generation(&self) -> Option<u64> {
                Some(1)
            }
            async fn authorize(
                &self,
                _invocation: &BrokerInvocation,
                _arguments: &Value,
            ) -> Result<(), BrokerDenial> {
                self.entered.notify_waiters();
                let rx = { self.release.lock().expect("gate").take() };
                if let Some(rx) = rx {
                    let _ = rx.await;
                }
                Ok(())
            }
            async fn execute(
                &self,
                _invocation: &BrokerInvocation,
                _arguments: Value,
            ) -> Result<BrokerExecuted, BrokerDenial> {
                Ok(BrokerExecuted {
                    content: vec![],
                    structured: None,
                })
            }
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let authority = Arc::new(ParkingAuthority {
            release: std::sync::Mutex::new(Some(rx)),
            entered: tokio::sync::Notify::new(),
        });
        let backend = BrokerBackend::new(
            "task-1",
            "att-1",
            "onemind",
            catalog(),
            BrokerLimits::default(),
            authority.clone(),
            CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(backend.live_calls(), 0);
        let entered = authority.entered.notified();
        let calling = {
            let backend = Arc::clone(&backend);
            tokio::spawn(async move {
                backend
                    .call_tool("search_notes", Default::default(), &scope())
                    .await
            })
        };
        entered.await;
        assert_eq!(
            backend.live_calls(),
            1,
            "a call parked on approval is broker-bound silence, not idle"
        );
        // Review §2.9: the credit is LIVE — the human has not answered
        // yet and the turn deadline must already be standing still.
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            backend.parked_ms() >= 10,
            "a parked approval accrues turn-deadline credit while unanswered, got {}",
            backend.parked_ms()
        );
        tx.send(()).expect("release the parked call");
        let result = calling.await.unwrap().unwrap();
        assert!(denial_of(&result).is_none(), "the call settled normally");
        assert_eq!(
            backend.live_calls(),
            0,
            "the gauge returns to zero as the call settles"
        );
        // Settled: the wait is remembered (still credited) but no longer
        // grows — unbounded retroactive credit would defeat the deadline.
        let settled_credit = backend.parked_ms();
        assert!(settled_credit >= 10);
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(
            backend.parked_ms(),
            settled_credit,
            "credit stops accruing once the call settles"
        );
    }

    /// Review R5: abandoning a parked call (outer request cancelled) must
    /// NOT leak the in-flight gauge or keep growing the approval-pause
    /// credit — RAII closes both when the dispatch future drops.
    #[tokio::test]
    async fn abandoned_park_leaks_no_gauge_or_credit() {
        struct NeverAnswers {
            entered: tokio::sync::Notify,
        }
        #[async_trait]
        impl BrokerAuthority for NeverAnswers {
            fn attempt_generation(&self) -> Option<u64> {
                Some(1)
            }
            async fn authorize(
                &self,
                _invocation: &BrokerInvocation,
                _arguments: &Value,
            ) -> Result<(), BrokerDenial> {
                self.entered.notify_waiters();
                std::future::pending::<()>().await;
                Ok(())
            }
            async fn execute(
                &self,
                _invocation: &BrokerInvocation,
                _arguments: Value,
            ) -> Result<BrokerExecuted, BrokerDenial> {
                unreachable!("authorize never answers")
            }
        }
        let authority = Arc::new(NeverAnswers {
            entered: tokio::sync::Notify::new(),
        });
        let backend = BrokerBackend::new(
            "task-1",
            "att-1",
            "onemind",
            catalog(),
            // The gate would never answer; this test abandons, not times out.
            BrokerLimits {
                authorize_timeout_ms: 60_000,
                ..BrokerLimits::default()
            },
            authority.clone(),
            CancellationToken::new(),
        )
        .unwrap();
        let entered = authority.entered.notified();
        let calling = {
            let backend = Arc::clone(&backend);
            tokio::spawn(async move {
                backend
                    .call_tool("search_notes", Default::default(), &scope())
                    .await
            })
        };
        entered.await;
        assert_eq!(backend.live_calls(), 1, "parked while awaited");
        calling.abort();
        let _ = calling.await;
        assert_eq!(
            backend.live_calls(),
            0,
            "an abandoned dispatch must not leave the gauge lying"
        );
        let frozen_credit = backend.parked_ms();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(
            backend.parked_ms(),
            frozen_credit,
            "pause credit stops the instant the call is gone"
        );
    }

    /// Review R5: a consumer that never answers cannot pin the attempt
    /// open — the human gate is DEADLINED and denies in band.
    #[tokio::test]
    async fn unanswered_authorize_hits_the_gate_deadline() {
        struct NeverAnswers {
            entered: tokio::sync::Notify,
        }
        #[async_trait]
        impl BrokerAuthority for NeverAnswers {
            fn attempt_generation(&self) -> Option<u64> {
                Some(1)
            }
            async fn authorize(
                &self,
                _invocation: &BrokerInvocation,
                _arguments: &Value,
            ) -> Result<(), BrokerDenial> {
                self.entered.notify_waiters();
                std::future::pending::<()>().await;
                Ok(())
            }
            async fn execute(
                &self,
                _invocation: &BrokerInvocation,
                _arguments: Value,
            ) -> Result<BrokerExecuted, BrokerDenial> {
                unreachable!("the gate times out first")
            }
        }
        let authority = Arc::new(NeverAnswers {
            entered: tokio::sync::Notify::new(),
        });
        let backend = BrokerBackend::new(
            "task-1",
            "att-1",
            "onemind",
            catalog(),
            BrokerLimits {
                authorize_timeout_ms: 30,
                ..BrokerLimits::default()
            },
            authority.clone(),
            CancellationToken::new(),
        )
        .unwrap();
        let result = backend
            .call_tool("search_notes", Default::default(), &scope())
            .await
            .unwrap();
        let denial = denial_of(&result).expect("the gate deadline denies in band");
        assert_eq!(denial["code"], "approval_timeout");
        assert_eq!(backend.live_calls(), 0);
    }

    #[tokio::test]
    async fn denial_is_in_band_and_recorded() {
        let authority = Arc::new(ScriptedAuthority {
            live: true,
            authorize_result: Some(BrokerDenial {
                code: "approval_denied".into(),
                message: "human denied".into(),
            }),
            execute_result: None,
            calls: AtomicUsize::new(0),
        });
        let backend = BrokerBackend::new(
            "task-1",
            "att-1",
            "onemind",
            catalog(),
            BrokerLimits::default(),
            authority.clone(),
            CancellationToken::new(),
        )
        .unwrap();
        let result = backend
            .call_tool("search_notes", Default::default(), &scope())
            .await
            .unwrap();
        let denial = denial_of(&result).unwrap();
        assert_eq!(denial["code"], "approval_denied");
        assert_eq!(authority.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn execution_returns_content_and_counts() {
        let (backend, authority) = backend(true).await;
        let mut args = JsonObject::new();
        args.insert("q".into(), json!("hello"));
        let result = backend
            .call_tool("search_notes", args, &scope())
            .await
            .unwrap();
        assert_eq!(denial_of(&result), None);
        assert!(result.content[0].as_text().unwrap().text.contains("found"));
        assert_eq!(result.structured_content, Some(json!({"id": 1})));
        assert_eq!(authority.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn call_budget_is_bounded_and_visible_in_band() {
        let authority = authority(true);
        let backend = BrokerBackend::new(
            "task-1",
            "att-1",
            "onemind",
            catalog(),
            BrokerLimits {
                max_calls: 2,
                ..BrokerLimits::default()
            },
            authority.clone(),
            CancellationToken::new(),
        )
        .unwrap();
        for _ in 0..2 {
            let result = backend
                .call_tool("search_notes", Default::default(), &scope())
                .await
                .unwrap();
            assert_eq!(denial_of(&result), None);
        }
        let result = backend
            .call_tool("search_notes", Default::default(), &scope())
            .await
            .unwrap();
        assert_eq!(denial_of(&result).unwrap()["code"], "quota_calls");
        // The over-budget call never executed.
        assert_eq!(authority.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn argument_bound_fails_closed() {
        let (backend, authority) = backend(true).await;
        let mut args = JsonObject::new();
        args.insert("q".into(), json!("x".repeat(300 * 1024)));
        let result = backend
            .call_tool("search_notes", args, &scope())
            .await
            .unwrap();
        assert_eq!(denial_of(&result).unwrap()["code"], "quota_args");
        assert_eq!(authority.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn result_bound_is_reported_not_truncated() {
        struct Huge;
        #[async_trait]
        impl BrokerAuthority for Huge {
            fn attempt_generation(&self) -> Option<u64> {
                Some(1)
            }
            async fn authorize(&self, _: &BrokerInvocation, _: &Value) -> Result<(), BrokerDenial> {
                Ok(())
            }
            async fn execute(
                &self,
                _: &BrokerInvocation,
                _: Value,
            ) -> Result<BrokerExecuted, BrokerDenial> {
                Ok(BrokerExecuted {
                    content: vec![json!("y".repeat(8 * 1024))],
                    structured: None,
                })
            }
        }
        let backend = BrokerBackend::new(
            "t",
            "a",
            "s",
            catalog(),
            BrokerLimits {
                max_result_bytes: 1024,
                ..BrokerLimits::default()
            },
            Arc::new(Huge),
            CancellationToken::new(),
        )
        .unwrap();
        let result = backend
            .call_tool("search_notes", Default::default(), &scope())
            .await
            .unwrap();
        assert_eq!(denial_of(&result).unwrap()["code"], "quota_result");
    }

    #[tokio::test]
    async fn cancellation_stops_calls() {
        let cancellation = CancellationToken::new();
        let backend = BrokerBackend::new(
            "t",
            "a",
            "s",
            catalog(),
            BrokerLimits::default(),
            authority(true),
            cancellation.clone(),
        )
        .unwrap();
        cancellation.cancel();
        let result = backend
            .call_tool("search_notes", Default::default(), &scope())
            .await
            .unwrap();
        assert_eq!(denial_of(&result).unwrap()["code"], "cancelled");
    }

    #[tokio::test]
    async fn revoke_stops_list_and_calls_immediately() {
        let (backend, _) = backend(true).await;
        backend.revoke();
        assert!(backend.list_tools(&scope()).await.is_err());
        let result = backend
            .call_tool("search_notes", Default::default(), &scope())
            .await
            .unwrap();
        assert_eq!(denial_of(&result).unwrap()["code"], "revoked");
    }

    #[tokio::test]
    async fn catalog_validation_refuses_junk() {
        let cancellation = CancellationToken::new();
        // Empty server name.
        assert!(
            BrokerBackend::new(
                "t",
                "a",
                "",
                catalog(),
                BrokerLimits::default(),
                authority(true),
                cancellation.clone(),
            )
            .is_err_and(|d| d.code == "broker_catalog")
        );
        // Bad tool name.
        let mut junk = catalog();
        junk[0].name = "bad name!".into();
        assert!(
            BrokerBackend::new(
                "t",
                "a",
                "s",
                junk,
                BrokerLimits::default(),
                authority(true),
                cancellation.clone(),
            )
            .is_err_and(|d| d.code == "broker_catalog")
        );
        // Non-object schema.
        let mut junk = catalog();
        junk[0].input_schema = json!([]);
        assert!(
            BrokerBackend::new(
                "t",
                "a",
                "s",
                junk,
                BrokerLimits::default(),
                authority(true),
                cancellation.clone(),
            )
            .is_err_and(|d| d.code == "broker_catalog")
        );
        // Empty catalog.
        assert!(
            BrokerBackend::new(
                "t",
                "a",
                "s",
                vec![],
                BrokerLimits::default(),
                authority(true),
                cancellation,
            )
            .is_err_and(|d| d.code == "broker_catalog")
        );
    }

    #[tokio::test]
    async fn endpoint_binds_loopback_and_spec_carries_private_token() {
        let broker = AttemptBroker::start(
            "task-1",
            "att-1",
            "onemind",
            catalog(),
            BrokerLimits::default(),
            authority(true),
        )
        .await
        .unwrap();
        let spec = broker.spec();
        assert_eq!(spec.server_name, "onemind");
        let url = spec.http_url.clone().unwrap();
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        assert!(spec.stdio_command.is_none());
        let token = spec.auth_token.clone().unwrap();
        assert!(token.starts_with("brk-") && token.len() > 40);
        assert_eq!(
            spec.broker_tool_names,
            vec![
                "mcp__onemind__search_notes".to_string(),
                "mcp__onemind__propose_note".to_string()
            ]
        );
        broker.shutdown().await;
        // Idempotent second shutdown; calls after shutdown are refused.
        broker.shutdown().await;
        let result = broker
            .backend()
            .call_tool("search_notes", Default::default(), &scope())
            .await
            .unwrap();
        assert_eq!(denial_of(&result).unwrap()["code"], "revoked");
    }
}
