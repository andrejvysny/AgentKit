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
//! * loss, tokens stop authenticating and live calls are cancelled;
//! * an abandoned dispatch cleans up panic-safely (AGENTKIT-140): the
//!   reservation guard publishes uncertainty IN MEMORY (uncertain memo +
//!   authority record) BEFORE any optional async work, schedules the
//!   durable `uncertain` refinement only through a checked live handle
//!   (`tokio::runtime::Handle::try_current` at drop), and NEVER
//!   constructs or blocks a runtime in Drop. With no runtime able to run
//!   the refinement — a pending future dropped after `block_on` returned
//!   or after runtime shutdown — the already-acknowledged `effectStarted`
//!   row is the recovery fallback. See `ReservationGuard` for the full
//!   Drop contract.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use agentkit_native::canonical_digest;

use agentkit_mcp::ErrorData;
use agentkit_mcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use agentkit_mcp::{
    Backend, BearerAuth, CatalogTool, McpServer, RpcId, ServerOptions, SessionScope,
};

// Broker contracts now live in `agentkit-native` (phase N1); re-exported so
// existing paths keep working.
use agentkit_native::AttemptBrokerHandle;
pub use agentkit_native::{BROKER_AUTHORIZE_BUDGET_MS, BrokerLimits, BrokerToolDefinition};

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

    /// Acknowledged durable lifecycle transition. An unfinished effectStarted
    /// record is uncertainty evidence after a crash; terminal settlement may
    /// refine it only for this exact invocation and argument identity.
    async fn record_lifecycle(
        &self,
        _invocation: &BrokerInvocation,
        _phase: &str,
        _outcome: &Value,
    ) -> Result<(), BrokerDenial> {
        Err(BrokerDenial {
            code: "durability_unavailable".into(),
            message: "authority has no durable lifecycle journal".into(),
        })
    }

    /// Durable bookkeeping hook for every terminal call outcome.
    fn record(&self, _invocation: &BrokerInvocation, _outcome: &Value) {}
}

/// Headroom the broker adds to the SERVER-side per-call deadline on top of
/// (authorize budget + effect timeout) (review R4 F2). The `agentkit-mcp`
/// handler wraps the ENTIRE `tools/call` — human approval park included —
/// in this one deadline, so deriving it from the effect timeout alone cut
/// approvals off early. With this overhead the installed server deadline
/// still sits strictly below the vendor MCP request budget
/// (`agentkit_claude::launch::BROKER_MCP_REQUEST_BUDGET_MS`), so the
/// vendor is always the first to give up — mechanically guarded by
/// `deadline_chain_stays_compatible_by_construction`.
pub const BROKER_SERVER_DEADLINE_OVERHEAD_MS: i64 = 30_000;

/// The per-call deadline the broker INSTALLED on its MCP server, derived
/// from the deadline chain: authorize budget + effect timeout + bounded
/// overhead (review R4 F2). Inner bounds stay meaningful — `authorize` is
/// still bounded by `authorize_timeout_ms` and `execute` by
/// `call_timeout_ms` inside `dispatch_authorized`; this outer bound only
/// guarantees neither is cut off from outside.
fn broker_call_deadline(limits: &BrokerLimits) -> Duration {
    Duration::from_millis(
        limits
            .authorize_timeout_ms
            .max(1)
            .saturating_add(limits.call_timeout_ms.max(1))
            .saturating_add(BROKER_SERVER_DEADLINE_OVERHEAD_MS)
            .max(1) as u64,
    )
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

/// A recorded replayable outcome (review R4 F7c). The memoized result is
/// only served to a restatement that proves it is the SAME call: same tool
/// name and same canonical digest of the arguments. A proven-redelivery
/// key restated with different content is a different call wearing an
/// old badge — it never replays this result.
struct Memo {
    result: Option<CallToolResult>,
    tool: String,
    digest: String,
}

/// Union bookkeeping for approval parks (review R4 F7b). `active` is the
/// count of currently open gates; `base` is the wall-clock start of the
/// current UNION interval, set on the 0→1 transition and flushed into
/// `parked_ms` on the 1→0 transition. Overlapping parks therefore credit
/// the union of the wait, never the sum of per-call waits.
#[derive(Default)]
struct ParkState {
    active: usize,
    base: Option<std::time::Instant>,
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
    /// Starts of the CURRENTLY pending authorization gates (review R4 F7b:
    /// UNION accounting, not per-call accumulation). `active` counts the
    /// gates currently open; `base` is set on the 0→1 transition and the
    /// interval is added to `parked_ms` on every 1→0 transition, so two
    /// concurrently parked calls credit the wall-clock UNION of their
    /// waits, never the doubled sum. Credit is LIVE: a human who has not
    /// answered yet must already be exempt from the turn deadline, not
    /// reimbursed only after they answer.
    park_gate: Mutex<ParkState>,
    /// Proven-redelivery memo: a call whose transport RESTATES the same
    /// typed JSON-RPC request id ON THE SAME SESSION replays the first
    /// outcome instead of executing twice (AGENTKIT-115: only the exact
    /// request id proves redelivery; identical arguments never do).
    /// Keyed by [`MemoKey`] — session identity + typed id — because this
    /// map lives on the per-attempt backend that all connected sessions
    /// share (review R7). Each memo also carries the tool name and the
    /// canonical digest of the arguments that EARNED it (review R4 F7c):
    /// a restated id whose content differs is not the same call and never
    /// replays a result it did not produce.
    memos: Mutex<HashMap<MemoKey, Memo>>,
    cancellation: CancellationToken,
    /// Durable refinements scheduled by abandoned dispatches. Only ever
    /// pushed through a checked live handle (see the Drop contract on
    /// [`ReservationGuard`], AGENTKIT-140); drained at shutdown.
    settlements: Mutex<Vec<tokio::task::JoinHandle<()>>>,
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
        if limits.max_result_bytes < 100 {
            return Err(BrokerDenial {
                code: "broker_limits".into(),
                message: "result limit must allow a 100-byte error envelope".into(),
            });
        }
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
            park_gate: Mutex::new(ParkState::default()),
            memos: Mutex::new(HashMap::new()),
            settlements: Mutex::new(Vec::new()),
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
    /// the turn deadline. UNION time (review R4 F7b): the wall-clock
    /// duration over which at least one gate was open, never the sum of
    /// per-call waits.
    #[must_use]
    pub fn parked_ms(&self) -> u64 {
        let done = self.parked_ms.load(std::sync::atomic::Ordering::Relaxed);
        let live = self
            .park_gate
            .lock()
            .expect("park gate lock")
            .base
            .map(|base| base.elapsed().as_millis() as u64)
            .unwrap_or(0);
        done.saturating_add(live)
    }

    async fn drain_settlements(&self, budget: Duration) {
        let settlements =
            std::mem::take(&mut *self.settlements.lock().expect("broker settlements"));
        // One aggregate deadline bounds shutdown regardless of call count.
        // Dropping a join handle detaches its writer; durable effect-start
        // evidence remains if the process exits before settlement finishes.
        let _ = tokio::time::timeout(budget, async move {
            for settlement in settlements {
                let _ = settlement.await;
            }
        })
        .await;
    }

    fn error_result(&self, payload: &Value) -> CallToolResult {
        let result = CallToolResult::error(vec![ContentBlock::text(payload.to_string())]);
        if serde_json::to_vec(&result).map_or(usize::MAX, |bytes| bytes.len())
            <= self.limits.max_result_bytes
        {
            return result;
        }
        let code = payload
            .get("code")
            .and_then(Value::as_str)
            .filter(|code| code.len() <= 16)
            .unwrap_or("quota_result");
        CallToolResult::error(vec![ContentBlock::text(code)])
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
        self.error_result(&denial)
    }

    /// Authorization + execution once the call passed mechanical bounds.
    /// Every terminal outcome is recorded durably by the authority under
    /// the exact invocation identity. `effect_started` flips immediately
    /// before the effect is entered (review R4 F7a): an abandoned dispatch
    /// whose flag is still `false` provably never reached `execute`, so
    /// its replay reservation may be released; one whose flag is `true`
    /// must be remembered as UNCERTAIN, never silently re-executed.
    async fn dispatch_authorized(
        &self,
        invocation: &BrokerInvocation,
        arguments: Value,
        effect_started: &std::sync::atomic::AtomicBool,
        effect_uncertain: &std::sync::atomic::AtomicBool,
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
            return self.error_result(&payload);
        }
        // The effect MAY have started from this line onward: set the flag
        // BEFORE awaiting so an abandonment at any later point is visible
        // to the reservation guard (review R4 F7a).
        let start = json!({"contentHash": canonical_digest(&arguments)});
        if let Err(denial) = self
            .authority
            .record_lifecycle(invocation, "effectStarted", &start)
            .await
        {
            return self.denied(&denial.code, denial.message);
        }
        effect_started.store(true, std::sync::atomic::Ordering::SeqCst);
        let executed = tokio::time::timeout(
            Duration::from_millis(self.limits.call_timeout_ms.max(1) as u64),
            self.authority.execute(invocation, arguments),
        )
        .await;
        match executed {
            Err(_) => {
                effect_uncertain.store(true, std::sync::atomic::Ordering::SeqCst);
                let payload = json!({
                    "uncertain": true, "code": "effect_uncertain",
                    "message": format!("call exceeded {} ms", self.limits.call_timeout_ms),
                });
                self.authority.record(invocation, &payload);
                self.error_result(&payload)
            }
            Ok(Err(denial)) => {
                let payload =
                    json!({"denied": true, "code": denial.code, "message": denial.message});
                self.authority.record(invocation, &payload);
                self.error_result(&payload)
            }
            Ok(Ok(result)) => {
                let mut parts = Vec::new();
                for part in &result.content {
                    // Authorities return MCP-shaped parts
                    // `{"type":"text","text":"..."}`; forwarding the part's
                    // JSON text would hand the model escaped JSON inside
                    // JSON. Unwrap text parts (and bare strings); anything
                    // else keeps the JSON-serialised fallback.
                    let text = match part {
                        Value::String(text) => text.clone(),
                        Value::Object(object)
                            if object.get("type").and_then(Value::as_str) == Some("text") =>
                        {
                            match object.get("text").and_then(Value::as_str) {
                                Some(text) => text.to_owned(),
                                None => part.to_string(),
                            }
                        }
                        other => other.to_string(),
                    };
                    parts.push(ContentBlock::text(text));
                }
                if parts.is_empty() {
                    parts.push(ContentBlock::text("{}".to_owned()));
                }
                let mut response = CallToolResult::success(parts);
                response.structured_content = result.structured.clone();
                response.is_error = Some(false);
                let outcome = json!({
                    "executed": true, "tool": invocation.tool_name,
                    "invocationId": invocation.invocation_id,
                    "content": result.content, "structured": result.structured,
                });
                let serialized = serde_json::to_vec(&response)
                    .map_or(usize::MAX, |v| v.len())
                    .max(serde_json::to_vec(&outcome).map_or(usize::MAX, |v| v.len()));
                if serialized > self.limits.max_result_bytes {
                    // Never silently truncate: the model sees the bound.
                    let payload = json!({
                        "denied": true, "code": "quota_result",
                        "message": format!("result exceeds {} bytes", self.limits.max_result_bytes),
                    });
                    self.authority.record(invocation, &payload);
                    return self.error_result(&payload);
                }
                self.authority.record(invocation, &outcome);
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

/// RAII counterpart of an `authorize` park (review R5; union-accounted per
/// review R4 F7b): the gate opens on creation (live deadline credit starts
/// on the 0→1 transition) and CLOSES on drop — whether authorize answered,
/// denied, timed out, or the whole request future was abandoned. On the
/// 1→0 transition the UNION interval accrues to `parked_ms`; a nested
/// (overlapping) park adds nothing on open and nothing on close, so two
/// concurrently parked calls are never double-credited.
struct ParkGuard<'a> {
    backend: &'a BrokerBackend,
}

impl<'a> ParkGuard<'a> {
    fn new(backend: &'a BrokerBackend) -> Self {
        let mut gate = backend.park_gate.lock().expect("park gate lock");
        if gate.active == 0 {
            gate.base = Some(std::time::Instant::now());
        }
        gate.active += 1;
        Self { backend }
    }
}

impl Drop for ParkGuard<'_> {
    fn drop(&mut self) {
        let mut gate = self.backend.park_gate.lock().expect("park gate lock");
        gate.active = gate.active.saturating_sub(1);
        if gate.active == 0
            && let Some(base) = gate.base.take()
        {
            self.backend.parked_ms.fetch_add(
                base.elapsed().as_millis() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }
}

/// RAII counterpart of a proven-redelivery reservation (review R4 F7a).
/// The normal path disarms the guard after recording the memo; if the
/// dispatch future is ABANDONED instead (outer request cancelled while the
/// call parked on authorization), the guard makes cleanup
/// cancellation-safe:
/// * the effect was NOT entered → the inflight reservation is released, so
///   a legitimate redelivery of the same key is not frozen out with
///   `duplicate_in_progress` for the life of the attempt;
/// * the effect MAY have started → the guard publishes UNCERTAINTY in
///   memory first (authority bookkeeping + an uncertain memo that denies
///   any redelivery with `effect_uncertain`), and only then schedules the
///   durable refinement that writes the `uncertain` lifecycle record.
///
/// The Drop contract (AGENTKIT-140) — code-coupled with `drop` below:
/// * in-memory uncertainty is published BEFORE any optional async work,
///   so redelivery safety holds even when no runtime can refine durably;
/// * the durable refinement is scheduled ONLY through a checked live
///   handle (`tokio::runtime::Handle::try_current` at drop). Outside an
///   entered runtime — after `block_on` returned, or after the runtime
///   was shut down, i.e. exactly when the host drops a pending future —
///   it is skipped and the already-acknowledged `effectStarted` row
///   remains as the recovery fallback;
/// * Drop NEVER constructs or blocks a replacement runtime and never
///   panics on runtime state (the unguarded `tokio::spawn` this replaced
///   panicked 'there is no reactor running' and skipped the memo);
/// * a disarmed guard (conclusive terminal receipt already recorded)
///   drops as a no-op, so a terminal receipt always wins over a racing
///   cleanup, whichever of the two runs first.
struct ReservationGuard<'a> {
    backend: &'a BrokerBackend,
    key: Option<MemoKey>,
    effect_started: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tool: String,
    digest: String,
    invocation: BrokerInvocation,
    armed: bool,
}

impl<'a> ReservationGuard<'a> {
    fn new(
        backend: &'a BrokerBackend,
        key: Option<MemoKey>,
        effect_started: std::sync::Arc<std::sync::atomic::AtomicBool>,
        tool: String,
        digest: String,
        invocation: BrokerInvocation,
    ) -> Self {
        Self {
            backend,
            key,
            effect_started,
            tool,
            digest,
            invocation,
            armed: true,
        }
    }
    /// The dispatch settled normally: the caller records the real memo.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let key = self.key.take();
        if self
            .effect_started
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            let outcome = json!({
                "denied": true, "code": "effect_uncertain",
                "message": "the effect of this call may have started before its \
                            request was abandoned; its outcome is uncertain and \
                            it will never be re-executed automatically",
            });
            self.backend.authority.record(&self.invocation, &outcome);
            // AGENTKIT-140: publish in-memory uncertainty FIRST — the
            // authority bookkeeping just above and the uncertain memo
            // below exist even when no runtime can run the durable
            // refinement, so a redelivery of this key is denial-safe
            // unconditionally.
            if let Some(key) = key {
                self.backend.memos.lock().expect("broker memos").insert(
                    key,
                    Memo {
                        result: Some(self.backend.error_result(&outcome)),
                        tool: std::mem::take(&mut self.tool),
                        digest: std::mem::take(&mut self.digest),
                    },
                );
            }
            // The durable refinement is OPTIONAL and runs only through a
            // checked live handle: `try_current` fails once the future is
            // dropped outside an entered runtime (after `block_on`
            // returned, or after the runtime was shut down), and the
            // acknowledged `effectStarted` row is then the recovery
            // fallback. Drop NEVER constructs or blocks a replacement
            // runtime — an unguarded `tokio::spawn` here panicked 'there
            // is no reactor running' and skipped the memo (AGENTKIT-140).
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let authority = self.backend.authority.clone();
                let invocation = self.invocation.clone();
                let durable = outcome.clone();
                let settlement = runtime.spawn(async move {
                    let _ = authority
                        .record_lifecycle(&invocation, "uncertain", &durable)
                        .await;
                });
                self.backend
                    .settlements
                    .lock()
                    .expect("broker settlements")
                    .push(settlement);
            }
        } else if let Some(key) = key {
            self.backend
                .memos
                .lock()
                .expect("broker requests")
                .remove(&key);
        }
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
        // Canonical content identity of THIS request (review R4 F7c): the
        // memo only replays to a restatement that proves the same tool
        // and the same arguments.
        let digest = canonical_digest(&arguments);
        if let Some(key) = &memo_key {
            let mut requests = self.memos.lock().expect("broker requests");
            if let Some(memo) = requests.get(key) {
                if memo.tool != name || memo.digest != digest {
                    return Ok(self.denied("replay_conflict", "request identity changed"));
                }
                return Ok(memo.result.clone().unwrap_or_else(|| {
                    self.denied("duplicate_in_progress", "request is still executing")
                }));
            }
            requests.insert(
                key.clone(),
                Memo {
                    result: None,
                    tool: name.to_owned(),
                    digest: digest.clone(),
                },
            );
        }
        // Count the call before authorize: approvals that park also spend
        // the attempt's CALL QUOTA (no infinite approval loops), even
        // though their wall time is credited against the turn deadline.
        {
            let mut live = self.live.lock().expect("broker lock");
            live.calls += 1;
            if live.calls > self.limits.max_calls {
                if let Some(key) = &memo_key {
                    self.memos.lock().expect("broker requests").remove(key);
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
        let effect_started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let effect_uncertain = std::sync::atomic::AtomicBool::new(false);
        let outcome = {
            // RAII (review R5): the gauge returns to zero even if the
            // dispatch future is abandoned mid-flight — an abandoned
            // request must never leave `live_calls()` lying about a call
            // that no longer exists. The reservation guard (review R4 F7a)
            // makes the replay bookkeeping equally abandonment-safe.
            let _in_flight = InFlightGuard::new(self);
            let mut reservation = ReservationGuard::new(
                self,
                memo_key.clone(),
                effect_started.clone(),
                name.to_owned(),
                digest.clone(),
                invocation.clone(),
            );
            let outcome = self
                .dispatch_authorized(&invocation, arguments, &effect_started, &effect_uncertain)
                .await;
            // Response compaction must not erase whether execution was
            // entered: a quota error may follow a successful mutation.
            let payload = json!({
                "response": outcome,
                "effectStarted": effect_started.load(std::sync::atomic::Ordering::SeqCst),
                "effectUncertain": effect_uncertain.load(std::sync::atomic::Ordering::SeqCst),
            });
            let phase = if effect_uncertain.load(std::sync::atomic::Ordering::SeqCst) {
                "uncertain"
            } else {
                "terminal"
            };
            if self
                .authority
                .record_lifecycle(&invocation, phase, &payload)
                .await
                .is_err()
            {
                return Ok(self.denied(
                    "effect_uncertain",
                    "terminal receipt could not be persisted",
                ));
            }
            reservation.disarm();
            outcome
        };
        if let Some(key) = &memo_key {
            self.memos.lock().expect("broker memos").insert(
                key.clone(),
                Memo {
                    result: Some(outcome.clone()),
                    tool: name.to_owned(),
                    digest,
                },
            );
        }
        Ok(outcome)
    }

    fn revoke_actor(&self, _actor_id: &str) {
        // Deliberately NOT an attempt-wide revocation. The mcp layer calls
        // this whenever a per-request or per-session scope lease is dropped,
        // and that happens on perfectly healthy paths: the real CLI's first
        // request is a `server/discover` probe that is rejected (400), and a
        // session may expire or be deleted. Revoking the attempt here would
        // cancel the listener's shutdown token and make the CLI's follow-up
        // `initialize` fail with ECONNREFUSED. Session-scoped revocation is
        // already handled by the mcp layer (`ScopeLease::revoke` cancels that
        // session's `scope.cancellation`, which `call_tool` checks).
        // Attempt-wide revocation is driven by the host only: `revoke`,
        // `AttemptBroker::shutdown` and `Drop`.
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
    /// The per-call deadline actually INSTALLED on the MCP server
    /// (review R4 F2); observable so assembly tests can prove the
    /// installed configuration, not just the constants.
    call_deadline: Duration,
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
        // Review R4 F2: the mcp handler applies ONE deadline to the whole
        // tools/call — human authorization park included — so the
        // installed server deadline is derived from the full chain
        // (authorize budget + effect timeout + bounded overhead), never
        // from the effect timeout alone. The inner bounds inside
        // `dispatch_authorized` still bound each phase individually;
        // this only stops the outer transport from cutting approval short.
        let call_deadline = broker_call_deadline(&limits);
        let mut options = ServerOptions {
            writes_enabled: backend.has_writes(),
            max_request_bytes: limits.max_args_bytes.saturating_mul(2).max(64 * 1024),
            max_call_duration: call_deadline,
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
        // Review R4 F1: the endpoint ADVERTISES `/mcp`, so `/mcp` must be
        // exactly what the transport serves — the same authenticated
        // handler is also kept on `/` for the existing mcp-suite clients.
        let router = server.router().merge(server.router_at("/mcp"));
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
            call_deadline,
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

    /// The per-call deadline INSTALLED on the broker's MCP server
    /// (review R4 F2): authorize budget + effect timeout + bounded
    /// overhead. Assembly tests assert against this, not just constants.
    #[must_use]
    pub fn server_call_deadline(&self) -> Duration {
        self.call_deadline
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
        self.backend.drain_settlements(Duration::from_secs(5)).await;
        self.cancellation.cancel();
        self.serve.abort();
    }
}

#[async_trait]
impl AttemptBrokerHandle for AttemptBroker {
    fn spec(&self) -> agentkit_native::McpEndpointSpec {
        AttemptBroker::spec(self)
    }
    fn revoke(&self) {
        self.backend().revoke();
    }
    fn live_calls(&self) -> usize {
        AttemptBroker::live_calls(self)
    }
    fn parked_ms(&self) -> u64 {
        AttemptBroker::parked_ms(self)
    }
    async fn shutdown(&self) {
        AttemptBroker::shutdown(self).await;
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
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Review round-3 F4; extended review R4 F2: the R5 deadline chain —
    /// human approval window ≤ authorize budget − margin, and the outer
    /// vendor MCP request budget strictly above (authorize budget +
    /// effect timeout + the broker's installed-server overhead) — was tied
    /// only by comments across crates. This test fails mechanically if
    /// anyone edits one constant and re-opens the drift R5 closed. It
    /// covers the CONSTANTS only: the deadline the broker actually
    /// INSTALLS on its MCP server is proven by the assembly test
    /// `approval_longer_than_effect_timeout_serves_through_the_server`,
    /// which starts a real broker and calls it over the advertised URL.
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
                BROKER_AUTHORIZE_BUDGET_MS >= HUMAN_APPROVAL_WINDOW_MS + CLAMP_MARGIN_MS,
                "authorize budget must cover the full human approval window plus margin"
            );
        }
        assert!(
            agentkit_claude::launch::BROKER_MCP_REQUEST_BUDGET_MS
                > BROKER_AUTHORIZE_BUDGET_MS + call_timeout + BROKER_SERVER_DEADLINE_OVERHEAD_MS,
            "the outer MCP request budget must exceed approval parking, the effect \
             timeout and the installed server-deadline overhead: {} !> \
             {BROKER_AUTHORIZE_BUDGET_MS} + {call_timeout} + \
             {BROKER_SERVER_DEADLINE_OVERHEAD_MS}",
            agentkit_claude::launch::BROKER_MCP_REQUEST_BUDGET_MS
        );
        // The value the broker actually installs at defaults, so the
        // chain above covers the REAL installed deadline, not a
        // hypothetical smaller one.
        assert_eq!(
            broker_call_deadline(&BrokerLimits::default()),
            Duration::from_millis(
                (BROKER_AUTHORIZE_BUDGET_MS + call_timeout + BROKER_SERVER_DEADLINE_OVERHEAD_MS)
                    as u64
            )
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
        async fn record_lifecycle(
            &self,
            _: &BrokerInvocation,
            _: &str,
            _: &Value,
        ) -> Result<(), BrokerDenial> {
            Ok(())
        }

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
                    content: vec![json!({"type": "text", "text": "found it"})],
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
        async fn record_lifecycle(
            &self,
            _: &BrokerInvocation,
            _: &str,
            _: &Value,
        ) -> Result<(), BrokerDenial> {
            Ok(())
        }

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
            async fn record_lifecycle(
                &self,
                _: &BrokerInvocation,
                _: &str,
                _: &Value,
            ) -> Result<(), BrokerDenial> {
                Ok(())
            }

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
            async fn record_lifecycle(
                &self,
                _: &BrokerInvocation,
                _: &str,
                _: &Value,
            ) -> Result<(), BrokerDenial> {
                Ok(())
            }

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
            async fn record_lifecycle(
                &self,
                _: &BrokerInvocation,
                _: &str,
                _: &Value,
            ) -> Result<(), BrokerDenial> {
                Ok(())
            }

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
        // The authority's `{"type":"text","text":..}` part reaches the
        // client as PLAIN text, not as escaped JSON (Fix D).
        assert_eq!(result.content[0].as_text().unwrap().text, "found it");
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
        struct Huge(Arc<Mutex<Vec<Value>>>);
        #[async_trait]
        impl BrokerAuthority for Huge {
            async fn record_lifecycle(
                &self,
                _: &BrokerInvocation,
                phase: &str,
                outcome: &Value,
            ) -> Result<(), BrokerDenial> {
                self.0
                    .lock()
                    .unwrap()
                    .push(json!({"phase": phase, "outcome": outcome}));
                Ok(())
            }

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
                    content: vec![json!("ok")],
                    structured: Some(json!({"large": "y".repeat(16 * 1024)})),
                })
            }
        }
        let records = Arc::new(Mutex::new(Vec::new()));
        let backend = BrokerBackend::new(
            "t",
            "a",
            "s",
            catalog(),
            BrokerLimits {
                max_result_bytes: 100,
                ..BrokerLimits::default()
            },
            Arc::new(Huge(records.clone())),
            CancellationToken::new(),
        )
        .unwrap();
        let result = backend
            .call_tool("search_notes", Default::default(), &scope())
            .await
            .unwrap();
        assert!(
            serde_json::to_string(&result)
                .unwrap()
                .contains("quota_result")
        );
        assert!(
            serde_json::to_vec(&result).unwrap().len() <= 100,
            "{}",
            serde_json::to_string(&result).unwrap()
        );
        let receipts = records.lock().unwrap();
        let terminal = receipts.last().unwrap();
        assert_eq!(terminal["phase"], "terminal");
        assert_eq!(terminal["outcome"]["effectStarted"], true);
        assert_eq!(terminal["outcome"]["effectUncertain"], false);
        assert!(
            !terminal.to_string().contains(&"y".repeat(100)),
            "oversized output must not reach durable receipt"
        );
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

    /// Raw JSON-RPC POST mirroring the mcp suite's own HTTP tests: bearer
    /// auth, dual accept, protocol-version header, session-id capture,
    /// and JSON-or-SSE body parsing.
    async fn rpc(
        client: &reqwest::Client,
        url: &str,
        token: &str,
        session: Option<&str>,
        payload: Value,
    ) -> (reqwest::StatusCode, Option<String>, Value) {
        let mut request = client
            .post(url)
            .bearer_auth(token)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .json(&payload);
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let session = response
            .headers()
            .get("mcp-session-id")
            .map(|value| value.to_str().unwrap().to_owned());
        let text = response.text().await.unwrap();
        let body = serde_json::from_str(&text)
            .ok()
            .or_else(|| {
                text.lines()
                    .filter_map(|line| {
                        line.strip_prefix("data:")
                            .and_then(|data| serde_json::from_str(data.trim()).ok())
                    })
                    .next_back()
            })
            .unwrap_or(Value::Null);
        (status, session, body)
    }

    /// Full streamable-HTTP handshake: initialize → notifications/initialized.
    async fn initialize(client: &reqwest::Client, url: &str, token: &str) -> String {
        let (status, session, body) = rpc(
            client,
            url,
            token,
            None,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-11-25","capabilities":{},
                "clientInfo":{"name":"r4-test","version":"1"}}}),
        )
        .await;
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "the ADVERTISED URL must serve initialize, not 404 (review R4 F1): {body}"
        );
        assert_eq!(body["result"]["protocolVersion"], "2025-11-25");
        let session = session.expect("initialize mints a session");
        let (status, _, _) = rpc(
            client,
            url,
            token,
            Some(&session),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;
        assert!(status.is_success(), "initialized notification: {status}");
        session
    }

    /// Review R4 F1 (reproduced via real loopback HTTP): the URL the
    /// broker ADVERTISES through `spec()` must be the URL the transport
    /// actually serves — a full initialize → tools/list → tools/call
    /// round trip against `broker.spec().http_url` with
    /// `broker.spec().auth_token`, no path fix-ups by the caller.
    #[tokio::test]
    async fn advertised_endpoint_serves_initialize_list_and_call() {
        let authority = authority(true);
        let broker = AttemptBroker::start(
            "task-1",
            "att-1",
            "onemind",
            catalog(),
            BrokerLimits::default(),
            authority.clone(),
        )
        .await
        .unwrap();
        let spec = broker.spec();
        let url = spec.http_url.clone().expect("http endpoint advertised");
        let token = spec.auth_token.clone().expect("private token");
        assert!(url.ends_with("/mcp"), "advertised: {url}");
        let client = reqwest::Client::new();
        let session = initialize(&client, &url, &token).await;
        let (status, _, body) = rpc(
            &client,
            &url,
            &token,
            Some(&session),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
        assert!(status.is_success(), "tools/list: {status} {body}");
        let names: Vec<&str> = body["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name"))
            .collect();
        assert!(
            names.contains(&"search_notes") && names.contains(&"propose_note"),
            "the frozen catalog must be listed: {names:?}"
        );
        let (status, _, body) = rpc(
            &client,
            &url,
            &token,
            Some(&session),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"search_notes","arguments":{"q":"hello"}}}),
        )
        .await;
        assert!(status.is_success(), "tools/call: {status} {body}");
        assert_ne!(body["result"]["isError"], Value::Bool(true), "{body}");
        assert!(
            body["result"]["content"][0]["text"].as_str() == Some("found it"),
            "the scripted authority's execution must travel back: {body}"
        );
        assert_eq!(authority.calls.load(Ordering::SeqCst), 1);
        broker.shutdown().await;
    }

    /// `tools/list` + `tools/call` on an initialized session; asserts the
    /// call executed and returned the authority's plain text.
    async fn list_and_call(client: &reqwest::Client, url: &str, token: &str, session: &str) {
        let (status, _, body) = rpc(
            client,
            url,
            token,
            Some(session),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
        assert!(status.is_success(), "tools/list: {status} {body}");
        assert!(
            body["result"]["tools"]
                .as_array()
                .is_some_and(|t| !t.is_empty())
        );
        let (status, _, body) = rpc(
            client,
            url,
            token,
            Some(session),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"search_notes","arguments":{"q":"hello"}}}),
        )
        .await;
        assert!(status.is_success(), "tools/call: {status} {body}");
        assert_ne!(body["result"]["isError"], Value::Bool(true), "{body}");
        assert_eq!(body["result"]["content"][0]["text"], "found it", "{body}");
    }

    /// Fix A (reproduced live): the real CLI's FIRST request is a
    /// `server/discover` probe with no session. It is rejected (4xx) and its
    /// per-request scope lease is dropped, which must NOT revoke the attempt
    /// or close the listener: the follow-up `initialize` must still succeed.
    #[tokio::test]
    async fn rejected_discover_probe_does_not_kill_the_broker() {
        let authority = authority(true);
        let broker = AttemptBroker::start(
            "task-1",
            "att-1",
            "onemind",
            catalog(),
            BrokerLimits::default(),
            authority.clone(),
        )
        .await
        .unwrap();
        let spec = broker.spec();
        let url = spec.http_url.clone().unwrap();
        let token = spec.auth_token.clone().unwrap();
        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .bearer_auth(&token)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-method", "server/discover")
            .header("mcp-protocol-version", "2026-07-28")
            .json(&json!({"jsonrpc":"2.0","id":"server-discover-probe-1",
                "method":"server/discover","params":{"_meta":{}}}))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_client_error(),
            "the pre-session probe is rejected: {}",
            response.status()
        );
        let session = initialize(&client, &url, &token).await;
        list_and_call(&client, &url, &token, &session).await;
        assert_eq!(authority.calls.load(Ordering::SeqCst), 1);
        broker.shutdown().await;
    }

    /// Fix A: ending one session (HTTP DELETE) is session-scoped; a second,
    /// new session on the same broker still initializes and calls tools.
    #[tokio::test]
    async fn ended_session_does_not_stop_a_new_session() {
        let authority = authority(true);
        let broker = AttemptBroker::start(
            "task-1",
            "att-1",
            "onemind",
            catalog(),
            BrokerLimits::default(),
            authority.clone(),
        )
        .await
        .unwrap();
        let spec = broker.spec();
        let url = spec.http_url.clone().unwrap();
        let token = spec.auth_token.clone().unwrap();
        let client = reqwest::Client::new();
        let first = initialize(&client, &url, &token).await;
        let response = client
            .delete(&url)
            .bearer_auth(&token)
            .header("mcp-protocol-version", "2025-11-25")
            .header("mcp-session-id", &first)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        let second = initialize(&client, &url, &token).await;
        assert_ne!(first, second);
        list_and_call(&client, &url, &token, &second).await;
        assert_eq!(authority.calls.load(Ordering::SeqCst), 1);
        broker.shutdown().await;
    }

    /// Fix A: the host-driven `revoke()` still stops everything over HTTP,
    /// and `shutdown()` closes the listener.
    #[tokio::test]
    async fn host_revoke_and_shutdown_still_stop_everything() {
        let authority = authority(true);
        let broker = AttemptBroker::start(
            "task-1",
            "att-1",
            "onemind",
            catalog(),
            BrokerLimits::default(),
            authority.clone(),
        )
        .await
        .unwrap();
        let spec = broker.spec();
        let url = spec.http_url.clone().unwrap();
        let token = spec.auth_token.clone().unwrap();
        let client = reqwest::Client::new();
        let session = initialize(&client, &url, &token).await;
        broker.backend().revoke();
        // Host revocation also closes the listener (graceful shutdown), so
        // the call is either refused in-band or fails at the transport; in
        // no case does it reach the authority.
        let attempt = client
            .post(&url)
            .bearer_auth(&token)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .header("mcp-session-id", &session)
            .json(
                &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"search_notes","arguments":{"q":"hello"}}}),
            )
            .send()
            .await;
        if let Ok(response) = attempt {
            let text = response.text().await.unwrap_or_default();
            assert!(!text.contains("found it"), "revoked broker served: {text}");
        }
        assert_eq!(authority.calls.load(Ordering::SeqCst), 0);
        broker.shutdown().await;
        let refused = client
            .post(&url)
            .bearer_auth(&token)
            .json(&json!({}))
            .send()
            .await;
        assert!(refused.is_err(), "the listener is closed after shutdown");
    }

    /// Review R4 F2 (assembly): with a TINY effect timeout, an approval
    /// that takes longer than the effect timeout must still succeed
    /// THROUGH the real installed server over the advertised URL — the
    /// outer per-call deadline is derived from the whole chain, not from
    /// `call_timeout_ms`. Under the old wiring (installed deadline =
    /// effect timeout) this same call died at ~50 ms while the human was
    /// still deciding.
    #[tokio::test]
    async fn approval_longer_than_effect_timeout_serves_through_the_server() {
        struct SlowApproval {
            authorize_ms: u64,
        }
        #[async_trait]
        impl BrokerAuthority for SlowApproval {
            async fn record_lifecycle(
                &self,
                _: &BrokerInvocation,
                _: &str,
                _: &Value,
            ) -> Result<(), BrokerDenial> {
                Ok(())
            }

            fn attempt_generation(&self) -> Option<u64> {
                Some(3)
            }
            async fn authorize(
                &self,
                _invocation: &BrokerInvocation,
                _arguments: &Value,
            ) -> Result<(), BrokerDenial> {
                // The human decides LONGER than the 50 ms effect timeout,
                // well inside the authorize budget.
                tokio::time::sleep(Duration::from_millis(self.authorize_ms)).await;
                Ok(())
            }
            async fn execute(
                &self,
                _invocation: &BrokerInvocation,
                _arguments: Value,
            ) -> Result<BrokerExecuted, BrokerDenial> {
                Ok(BrokerExecuted {
                    content: vec![json!({"approved": true})],
                    structured: None,
                })
            }
        }
        let limits = BrokerLimits {
            call_timeout_ms: 50,
            authorize_timeout_ms: 1_000,
            ..BrokerLimits::default()
        };
        let broker = AttemptBroker::start(
            "task-1",
            "att-1",
            "onemind",
            catalog(),
            limits,
            Arc::new(SlowApproval { authorize_ms: 120 }),
        )
        .await
        .unwrap();
        // The INSTALLED configuration, not just the constants: the
        // per-call deadline must cover authorize + effect (the OLD value,
        // the bare 50 ms effect timeout, would fail the call below).
        assert!(
            broker.server_call_deadline()
                >= Duration::from_millis(
                    (limits.authorize_timeout_ms + limits.call_timeout_ms) as u64
                ),
            "installed deadline {:?} must be >= authorize budget + effect timeout",
            broker.server_call_deadline()
        );
        let spec = broker.spec();
        let url = spec.http_url.clone().unwrap();
        let token = spec.auth_token.clone().unwrap();
        let client = reqwest::Client::new();
        let session = initialize(&client, &url, &token).await;
        let (status, _, body) = rpc(
            &client,
            &url,
            &token,
            Some(&session),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"search_notes","arguments":{"q":"hello"}}}),
        )
        .await;
        assert!(
            status.is_success() && body["result"]["isError"] != Value::Bool(true),
            "an approval slower than the effect timeout must succeed: {status} {body}"
        );
        assert!(
            body["result"]["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("approved")),
            "{body}"
        );
        broker.shutdown().await;
    }

    /// Parks EVERY caller in `authorize` (and optionally `execute`) on a
    /// shared watch flag until it flips — genuinely concurrent parks, as
    /// review R4 F7b requires. Counts only COMPLETED executions so an
    /// abandoned effect is observably never re-run.
    struct GateAuthority {
        authorize_gate: Option<tokio::sync::watch::Receiver<bool>>,
        execute_gate: Option<tokio::sync::watch::Receiver<bool>>,
        in_authorize: tokio::sync::Notify,
        in_execute: tokio::sync::Notify,
        executions: AtomicUsize,
        records: Mutex<Vec<Value>>,
    }

    async fn wait_gate(
        gate: &Option<tokio::sync::watch::Receiver<bool>>,
        entered: &tokio::sync::Notify,
    ) {
        entered.notify_waiters();
        if let Some(gate) = gate {
            let mut gate = gate.clone();
            while !*gate.borrow_and_update() && gate.changed().await.is_ok() {}
        }
    }

    #[async_trait]
    impl BrokerAuthority for GateAuthority {
        async fn record_lifecycle(
            &self,
            _: &BrokerInvocation,
            _phase: &str,
            _outcome: &Value,
        ) -> Result<(), BrokerDenial> {
            self.records
                .lock()
                .unwrap()
                .push(json!({"phase": _phase, "outcome": _outcome}));
            Ok(())
        }

        fn record(&self, _: &BrokerInvocation, outcome: &Value) {
            self.records.lock().unwrap().push(outcome.clone());
        }
        fn attempt_generation(&self) -> Option<u64> {
            Some(9)
        }
        async fn authorize(
            &self,
            _invocation: &BrokerInvocation,
            _arguments: &Value,
        ) -> Result<(), BrokerDenial> {
            wait_gate(&self.authorize_gate, &self.in_authorize).await;
            Ok(())
        }
        async fn execute(
            &self,
            _invocation: &BrokerInvocation,
            _arguments: Value,
        ) -> Result<BrokerExecuted, BrokerDenial> {
            wait_gate(&self.execute_gate, &self.in_execute).await;
            self.executions.fetch_add(1, Ordering::SeqCst);
            Ok(BrokerExecuted {
                content: vec![json!({"done": true})],
                structured: None,
            })
        }
    }

    fn gate_backend(
        authorize_gate: Option<tokio::sync::watch::Receiver<bool>>,
        execute_gate: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> (Arc<BrokerBackend>, Arc<GateAuthority>) {
        let authority = Arc::new(GateAuthority {
            authorize_gate,
            execute_gate,
            in_authorize: tokio::sync::Notify::new(),
            in_execute: tokio::sync::Notify::new(),
            executions: AtomicUsize::new(0),
            records: Mutex::new(Vec::new()),
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

    fn gated_call(
        backend: &Arc<BrokerBackend>,
        actor: &str,
        request_id: Option<RpcId>,
        query: &str,
    ) -> tokio::task::JoinHandle<CallToolResult> {
        let mut args = JsonObject::new();
        args.insert("q".into(), json!(query));
        let backend = Arc::clone(backend);
        let actor = actor.to_owned();
        tokio::spawn(async move {
            backend
                .call_tool("search_notes", args, &scope_with(&actor, request_id))
                .await
                .unwrap()
        })
    }

    /// Review R4 F7(a) pre-effect: cancelling a call parked in
    /// authorization must RELEASE its replay reservation — a legitimate
    /// redelivery of the same request key then executes normally instead
    /// of being frozen out with `duplicate_in_progress` forever.
    #[tokio::test]
    async fn cancelled_authorization_releases_the_replay_reservation() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let (backend, authority) = gate_backend(Some(rx), None);
        let calling = gated_call(
            &backend,
            "vendor-session-1",
            Some(RpcId::Number(1)),
            "hello",
        );
        authority.in_authorize.notified().await;
        assert_eq!(backend.live_calls(), 1, "parked on approval");
        calling.abort();
        let _ = calling.await;
        assert_eq!(backend.live_calls(), 0);
        let _ = tx.send(true);
        // Same session, same typed id, same arguments: the abandoned call
        // never reached the effect, so this restatement is a fresh call.
        let redelivered = gated_call(
            &backend,
            "vendor-session-1",
            Some(RpcId::Number(1)),
            "hello",
        );
        let result = redelivered.await.unwrap();
        assert_eq!(
            denial_of(&result),
            None,
            "a released reservation must not refuse the redelivery: {result:?}"
        );
        assert_eq!(authority.executions.load(Ordering::SeqCst), 1);
    }

    /// Review R4 F7(a) post-effect: abandoning a call whose effect MAY
    /// have started must leave a durable UNCERTAIN reservation — the
    /// redelivery gets an explicit uncertain denial, never a silent
    /// `duplicate_in_progress`, and never a second execution.
    #[tokio::test]
    async fn abandoned_effect_reports_uncertainty_and_never_reexecutes() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let (backend, authority) = gate_backend(None, Some(rx));
        let calling = gated_call(
            &backend,
            "vendor-session-1",
            Some(RpcId::Number(2)),
            "hello",
        );
        authority.in_execute.notified().await;
        calling.abort();
        let _ = calling.await;
        assert!(
            authority
                .records
                .lock()
                .unwrap()
                .iter()
                .any(|record| record["code"] == "effect_uncertain")
        );
        drop(tx);
        let redelivered = gated_call(
            &backend,
            "vendor-session-1",
            Some(RpcId::Number(2)),
            "hello",
        );
        let result = redelivered.await.unwrap();
        let denial = denial_of(&result).expect("the redelivery reports uncertainty");
        assert_eq!(denial["code"], "effect_uncertain");
        assert_eq!(
            authority.executions.load(Ordering::SeqCst),
            0,
            "the uncertain effect is never re-executed"
        );
    }

    /// AGENTKIT-140 reproduction shape: drive a freshly created
    /// `call_tool` future until the authority is parked INSIDE `execute`
    /// (the acknowledged `effectStarted` row exists, the effect may or may
    /// not have run), then RETURN from `block_on` leaving the future
    /// pending. The caller drops the future afterwards — outside any
    /// entered runtime, exactly as the post-merge review did on master
    /// 755df64, where the guard's unguarded `tokio::spawn` panicked
    /// 'there is no reactor running' and skipped the uncertain memo.
    fn drive_to_pending_execute<'a>(
        backend: &'a Arc<BrokerBackend>,
        authority: &Arc<GateAuthority>,
        runtime: &tokio::runtime::Runtime,
        scope: &'a SessionScope,
    ) -> Pin<Box<dyn Future<Output = Result<CallToolResult, ErrorData>> + Send + 'a>> {
        use std::task::Poll;
        let mut args = JsonObject::new();
        args.insert("q".into(), json!("hello"));
        let mut calling = backend.call_tool("search_notes", args, scope);
        // Register the execute-entry waiter BEFORE the first poll: the
        // whole prefix up to the execute park runs inside that first
        // poll, and `wait_gate` fires `notify_waiters` as it parks.
        let entered = authority.in_execute.notified();
        let mut entered = std::pin::pin!(entered);
        runtime.block_on(async {
            std::future::poll_fn(|cx| {
                let entered = entered.as_mut().poll(cx);
                match calling.as_mut().poll(cx) {
                    Poll::Ready(_) => panic!("the call must still be gated inside execute"),
                    Poll::Pending => entered,
                }
            })
            .await;
        });
        calling
    }

    /// The shared post-drop assertions (AGENTKIT-140): the cleanup ran
    /// with NO entered runtime, so it must have published uncertainty in
    /// memory (authority record + uncertain memo) WITHOUT any durable
    /// refinement running — the acknowledged `effectStarted` row is what
    /// remains as the recovery evidence.
    fn assert_uncertain_without_refinement(
        backend: &Arc<BrokerBackend>,
        authority: &Arc<GateAuthority>,
        request_id: RpcId,
    ) {
        let records = authority.records.lock().unwrap();
        assert!(
            records.iter().any(|r| r["phase"] == "effectStarted"),
            "durable start evidence survives: {records:?}"
        );
        assert!(
            records.iter().any(|r| r["code"] == "effect_uncertain"),
            "the in-memory uncertain bookkeeping was published: {records:?}"
        );
        assert!(
            !records.iter().any(|r| r["phase"] == "uncertain"),
            "no runtime was entered, so no durable refinement ran: {records:?}"
        );
        drop(records);
        assert!(
            backend.settlements.lock().unwrap().is_empty(),
            "no settlement was scheduled without a checked live handle"
        );
        let key = MemoKey {
            actor_id: "vendor-session-1".into(),
            request_id,
        };
        let memos = backend.memos.lock().unwrap();
        let memo = memos.get(&key).expect("the dropped effect left a memo");
        let result = memo
            .result
            .as_ref()
            .expect("the memo is settled as uncertain, not in-flight");
        let denial = denial_of(result).expect("the memo denies redelivery");
        assert_eq!(denial["code"], "effect_uncertain");
    }

    /// AGENTKIT-140 (acceptance 1/3/4): a call whose effect is pending is
    /// dropped AFTER `block_on` returned — the runtime is alive but not
    /// entered. Cleanup must not panic, must publish in-memory
    /// uncertainty unconditionally, must skip the durable refinement
    /// (checked-handle spawn), and a replay of the same invocation in a
    /// NEW runtime can never execute the abandoned effect again.
    #[test]
    fn dropped_effect_outside_entered_runtime_never_panics_and_never_reexecutes() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let (backend, authority) = gate_backend(None, Some(rx));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let scope = scope_with("vendor-session-1", Some(RpcId::Number(31)));
        let calling = drive_to_pending_execute(&backend, &authority, &runtime, &scope);
        // block_on has returned: no entered runtime. The old code panicked
        // here in `tokio::spawn`; now Drop publishes uncertainty in memory
        // and skips the refinement.
        drop(calling);
        assert_uncertain_without_refinement(&backend, &authority, RpcId::Number(31));
        // Replay in a fresh runtime: the in-memory uncertain memo denies
        // any re-execution of the abandoned effect.
        let replay_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let replay = replay_runtime
            .block_on(async {
                let mut args = JsonObject::new();
                args.insert("q".into(), json!("hello"));
                backend.call_tool("search_notes", args, &scope).await
            })
            .unwrap();
        let denial = denial_of(&replay).expect("the replay is denied with uncertainty");
        assert_eq!(denial["code"], "effect_uncertain");
        assert_eq!(
            authority.executions.load(Ordering::SeqCst),
            0,
            "the abandoned effect is never re-executed by a replay"
        );
        drop(tx);
    }

    /// AGENTKIT-140 (acceptance 1/4): the harsher ordering — drop the
    /// Tokio runtime FIRST, then the pending future. Cleanup still must
    /// not panic, still publishes in-memory uncertainty, and the
    /// acknowledged `effectStarted` row remains as the durable evidence.
    #[test]
    fn dropped_effect_after_runtime_shutdown_never_panics() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let (backend, authority) = gate_backend(None, Some(rx));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let scope = scope_with("vendor-session-1", Some(RpcId::Number(32)));
        let calling = drive_to_pending_execute(&backend, &authority, &runtime, &scope);
        drop(runtime);
        // No runtime exists at all anymore: the checked-handle refinement
        // is skipped; the effectStarted row is the recovery fallback.
        drop(calling);
        assert_uncertain_without_refinement(&backend, &authority, RpcId::Number(32));
        drop(tx);
    }

    /// AGENTKIT-140 (acceptance 2; drop-before-receipt ordering of the
    /// racing cleanup): cancellation INSIDE a live runtime must attempt
    /// the durable uncertain settlement through the checked handle AND
    /// leave the in-memory uncertain result while the broker lives.
    #[tokio::test]
    async fn cancelled_effect_settles_durably_and_keeps_uncertain_memo() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let (backend, authority) = gate_backend(None, Some(rx));
        let calling = gated_call(
            &backend,
            "vendor-session-1",
            Some(RpcId::Number(33)),
            "hello",
        );
        authority.in_execute.notified().await;
        calling.abort();
        let _ = calling.await;
        // Cleanup ran inside a live runtime: the refinement was scheduled
        // through the checked handle and settles when drained.
        assert_eq!(
            backend.settlements.lock().unwrap().len(),
            1,
            "a live runtime receives the durable refinement"
        );
        backend.drain_settlements(Duration::from_secs(5)).await;
        {
            let records = authority.records.lock().unwrap();
            assert!(
                records.iter().any(|r| r["phase"] == "uncertain"),
                "the durable uncertain settlement ran: {records:?}"
            );
            assert!(records.iter().any(|r| r["phase"] == "effectStarted"));
            assert!(records.iter().any(|r| r["code"] == "effect_uncertain"));
            assert!(
                !records.iter().any(|r| r["phase"] == "terminal"),
                "cleanup ran BEFORE any receipt: no terminal record exists"
            );
        }
        // The in-memory uncertain result is observable while the broker
        // lives, and denies the replay without a second execution.
        let key = MemoKey {
            actor_id: "vendor-session-1".into(),
            request_id: RpcId::Number(33),
        };
        {
            let memos = backend.memos.lock().unwrap();
            let memo = memos.get(&key).expect("uncertain memo while broker lives");
            let result = memo
                .result
                .as_ref()
                .expect("the memo carries the uncertain result");
            assert_eq!(
                denial_of(result).expect("denial")["code"],
                "effect_uncertain"
            );
        }
        drop(tx);
        let replay = gated_call(
            &backend,
            "vendor-session-1",
            Some(RpcId::Number(33)),
            "hello",
        )
        .await
        .unwrap();
        assert_eq!(
            denial_of(&replay).expect("the replay is denied with uncertainty")["code"],
            "effect_uncertain"
        );
        assert_eq!(authority.executions.load(Ordering::SeqCst), 0);
    }

    /// AGENTKIT-140 (acceptance 5, receipt-before-drop ordering): once the
    /// conclusive terminal receipt is recorded the guard is disarmed, so
    /// the cleanup drop that follows is a no-op — no uncertainty is
    /// published anywhere, and the memo replays the REAL result without a
    /// second execution.
    #[tokio::test]
    async fn conclusive_terminal_receipt_wins_over_the_cleanup_drop() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let (backend, authority) = gate_backend(None, Some(rx));
        let calling = gated_call(
            &backend,
            "vendor-session-1",
            Some(RpcId::Number(34)),
            "hello",
        );
        authority.in_execute.notified().await;
        tx.send(true).unwrap();
        let result = calling.await.unwrap();
        assert_eq!(denial_of(&result), None, "the call completed: {result:?}");
        {
            let records = authority.records.lock().unwrap();
            assert!(records.iter().any(|r| r["phase"] == "terminal"));
            assert!(records.iter().any(|r| r["phase"] == "effectStarted"));
            assert!(
                !records.iter().any(|r| r["phase"] == "uncertain"),
                "a disarmed guard must not publish uncertainty: {records:?}"
            );
            assert!(
                !records.iter().any(|r| r["code"] == "effect_uncertain"),
                "a disarmed guard must not record uncertain bookkeeping: {records:?}"
            );
        }
        assert!(
            backend.settlements.lock().unwrap().is_empty(),
            "a completed call schedules no uncertain refinement"
        );
        let key = MemoKey {
            actor_id: "vendor-session-1".into(),
            request_id: RpcId::Number(34),
        };
        {
            let memos = backend.memos.lock().unwrap();
            let memo = memos.get(&key).expect("terminal memo");
            let result = memo
                .result
                .as_ref()
                .expect("the terminal path settled the memo");
            assert_eq!(denial_of(result), None, "the memo holds the REAL result");
        }
        // The replay gets the conclusive receipt, and never re-executes.
        let replay = gated_call(
            &backend,
            "vendor-session-1",
            Some(RpcId::Number(34)),
            "hello",
        )
        .await
        .unwrap();
        assert_eq!(denial_of(&replay), None, "replay serves the receipt");
        assert_eq!(authority.executions.load(Ordering::SeqCst), 1);
    }

    /// Review R4 F7(b): two calls parked in authorization OVERLAPPING in
    /// time must be credited the WALL-CLOCK UNION of their waits, never
    /// the sum of the two parks.
    #[tokio::test]
    async fn overlapping_parks_are_credited_as_a_union() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let (backend, authority) = gate_backend(Some(rx), None);
        let first_notified = authority.in_authorize.notified();
        let first = {
            let backend = Arc::clone(&backend);
            tokio::spawn(async move {
                backend
                    .call_tool("search_notes", Default::default(), &scope())
                    .await
                    .unwrap()
            })
        };
        first_notified.await;
        let second_notified = authority.in_authorize.notified();
        let second = {
            let backend = Arc::clone(&backend);
            tokio::spawn(async move {
                backend
                    .call_tool(
                        "search_notes",
                        Default::default(),
                        &scope_with("vendor-session-2", None),
                    )
                    .await
                    .unwrap()
            })
        };
        second_notified.await;
        assert_eq!(backend.live_calls(), 2, "both parked together");
        // From here both gates are open simultaneously: the union of the
        // wait IS the wall clock from the first park to now.
        let union_start = std::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let parked = backend.parked_ms();
        let wall = union_start.elapsed().as_millis() as u64;
        // The old per-call accumulation credited ~2x the wall duration;
        // union accounting must track the wall clock, never exceed it by
        // more than the slack between the two readings.
        assert!(
            parked >= 60,
            "the live union must be crediting the ongoing wait: {parked}"
        );
        assert!(
            parked <= wall + 60,
            "two overlapping parks must not double-credit: parked {parked}, wall {wall}"
        );
        let _ = tx.send(true);
        let first = first.await.unwrap();
        let second = second.await.unwrap();
        assert_eq!(denial_of(&first), None);
        assert_eq!(denial_of(&second), None);
        let settled = backend.parked_ms();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(backend.parked_ms(), settled, "credit stops when both leave");
    }

    /// Review R4 F7(c): a session-scoped typed request id restated with
    /// CHANGED arguments must never receive the first request's cached
    /// result — it fails closed with a stable refusal and executes
    /// nothing.
    #[tokio::test]
    async fn replay_never_serves_a_restated_request_with_different_content() {
        let (backend, authority) = sequenced_backend().await;
        let mut args = JsonObject::new();
        args.insert("q".into(), json!("A"));
        let first = backend
            .call_tool(
                "search_notes",
                args.clone(),
                &scope_with("vendor-session-1", Some(RpcId::Number(4))),
            )
            .await
            .unwrap();
        assert_eq!(denial_of(&first), None);
        assert!(
            first.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("\"seq\":1")
        );
        let mut changed = JsonObject::new();
        changed.insert("q".into(), json!("B"));
        let restated = backend
            .call_tool(
                "search_notes",
                changed,
                &scope_with("vendor-session-1", Some(RpcId::Number(4))),
            )
            .await
            .unwrap();
        let denial = denial_of(&restated).expect("fail-closed refusal, not a replay");
        assert_eq!(denial["code"], "replay_conflict");
        assert!(
            !restated.content[0].as_text().unwrap().text.contains("seq"),
            "the first request's cached result must never travel"
        );
        // The faithful restatement of A still replays exactly once; the
        // mismatching one executed nothing new.
        let replay = backend
            .call_tool(
                "search_notes",
                args,
                &scope_with("vendor-session-1", Some(RpcId::Number(4))),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_string(&replay).unwrap(),
            serde_json::to_string(&first).unwrap()
        );
        assert_eq!(authority.calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn thirty_two_simultaneous_redeliveries_execute_once() {
        let authority = Arc::new(SequencingAuthority {
            calls: AtomicUsize::new(0),
        });
        let backend = BrokerBackend::new(
            "task",
            "attempt",
            "broker",
            catalog(),
            BrokerLimits::default(),
            authority.clone(),
            CancellationToken::new(),
        )
        .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(32));
        let mut callers = Vec::new();
        for _ in 0..32 {
            let backend = backend.clone();
            let barrier = barrier.clone();
            callers.push(tokio::spawn(async move {
                barrier.wait().await;
                backend
                    .call_tool(
                        "search_notes",
                        JsonObject::new(),
                        &scope_with("session", Some(RpcId::Number(42))),
                    )
                    .await
                    .unwrap()
            }));
        }
        for caller in callers {
            let _ = caller.await.unwrap();
        }
        assert_eq!(authority.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pending_request_also_checks_argument_identity() {
        let (release, gate) = tokio::sync::watch::channel(false);
        let (backend, authority) = gate_backend(None, Some(gate));
        let calling = gated_call(&backend, "session", Some(RpcId::Number(3)), "original");
        authority.in_execute.notified().await;
        let conflict = gated_call(&backend, "session", Some(RpcId::Number(3)), "changed")
            .await
            .unwrap();
        assert_eq!(denial_of(&conflict).unwrap()["code"], "replay_conflict");
        release.send(true).unwrap();
        calling.await.unwrap();
        assert_eq!(authority.executions.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn abandoned_effect_replay_respects_small_result_limit() {
        let (_release, gate) = tokio::sync::watch::channel(false);
        let (mut backend, authority) = gate_backend(None, Some(gate));
        Arc::get_mut(&mut backend).unwrap().limits.max_result_bytes = 100;
        let calling = gated_call(&backend, "session", Some(RpcId::Number(17)), "original");
        authority.in_execute.notified().await;
        calling.abort();
        let _ = calling.await;
        let replay = gated_call(&backend, "session", Some(RpcId::Number(17)), "original")
            .await
            .unwrap();
        let wire = serde_json::to_string(&replay).unwrap();
        assert!(wire.len() <= 100, "{wire}");
        assert!(wire.contains("effect_uncertain"), "{wire}");
        assert_eq!(authority.executions.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn settlement_drain_has_one_deadline_and_keeps_writers_alive() {
        let (backend, _) = backend(true).await;
        let (release, gate) = tokio::sync::watch::channel(false);
        let settled = Arc::new(AtomicUsize::new(0));
        for _ in 0..10 {
            let mut gate = gate.clone();
            let settled = settled.clone();
            let writer = tokio::spawn(async move {
                while !*gate.borrow_and_update() {
                    gate.changed().await.unwrap();
                }
                settled.fetch_add(1, Ordering::SeqCst);
            });
            backend.settlements.lock().unwrap().push(writer);
        }
        let start = std::time::Instant::now();
        backend.drain_settlements(Duration::from_millis(20)).await;
        assert!(
            start.elapsed() < Duration::from_millis(150),
            "each writer must not receive its own timeout"
        );
        release.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while settled.load(Ordering::SeqCst) != 10 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
