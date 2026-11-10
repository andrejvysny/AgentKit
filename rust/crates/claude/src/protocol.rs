//! Public structured-stream adapter and event normalization (AGENTKIT-111).
//!
//! Decodes official Claude Code `--output-format stream-json` NDJSON frames
//! into [`NativeEvent`]s and owns the single authoritative terminal-outcome
//! decision table (AGENTKIT-110/111/116).
//!
//! Forward compatibility: the real CLI emits dozens of `system` subtypes
//! and several top-level frame types this adapter does not model (e.g.
//! `thinking_tokens`, `notification`, `session_state_changed`,
//! `auth_status`, `keep_alive`), intermittently and at high rate. Unknown
//! `system` subtypes, unknown top-level frame types, unknown assistant
//! content block types and unknown `result` subtypes are therefore
//! TOLERATED: they produce no events (never a user-visible warning), are
//! counted in a bounded diagnostic record
//! ([`ClaudeStreamAdapter::ignored_frames`]), and an unknown `result`
//! subtype is a root result that [`classify_result`]/`terminal_core` map to
//! `Failed` (never `Completed`). Genuinely correctness-affecting
//! malformations still fail explicitly with [`ProtocolViolation`]:
//! undecodable/malformed JSON, a frame without a string `type`, a `system`
//! frame without string `subtype`, `init` without `session_id`, a `result`
//! missing `subtype`/`session_id`/boolean `is_error`, a conflicting second
//! root result, `tool_use` without `id`/`name`, `text` without string
//! `text`, and malformed `stream_event` envelopes.
//!
//! Vendor frame shapes accepted here (official docs, qualified versions):
//! * `{"type":"system","subtype":"init","session_id":S,"model":S,"tools":[..],
//!    "mcp_servers":[..],"capabilities":[S]?,"claude_code_version":S?}`
//! * `{"type":"assistant","message":{"content":[{"type":"text","text":S} |
//!    {"type":"tool_use","id":S,"name":S,"input":V} |
//!    {"type":"thinking","thinking":S} |
//!    {"type":"redacted_thinking","data":V}]},"session_id":S,
//!    "parent_tool_use_id":S|null}` — a `redacted_thinking` block is the
//!   public SDK's withheld-reasoning block: it normalizes to an opaque
//!   marker ([`ClaudeStreamAdapter::redacted_thinking_markers`]), never to
//!   reasoning or visible text, and never stops its sibling blocks
//!   (review round-4 F9)
//! * `{"type":"user","message":{"content":[{"type":"tool_result",
//!    "tool_use_id":S,..}]},"session_id":S,"parent_tool_use_id":S?}`
//! * `{"type":"stream_event","event":{"type":"content_block_delta",
//!    "delta":{"type":"text_delta","text":S}},...}` with partial messages
//! * `{"type":"system","subtype":"api_retry","attempt":N,"max_retries":N,
//!    "error":S|null,...}`
//! * `{"type":"system","subtype":"permission_denied","tool_name":S?}`
//! * `{"type":"system","subtype":"status","status":S|null}`
//! * `{"type":"system","subtype":"compact_boundary","trigger":S?}`
//! * `{"type":"system","subtype":"model_refusal_no_fallback",...}` — the
//!   documented refusal-without-fallback notification: informational only
//!   ([`NativeEvent::Warning`]); the root `result` frame stays the sole
//!   terminal authority (review round-4 F9)
//! * `{"type":"rate_limit_event","rate_limit_info":{"status":S,
//!    "resetsAt":N?,"utilization":F?,"rateLimitType":S?},"session_id":S?}`
//!   — the public Agent SDK shape (`SDKRateLimitEvent`); the older
//!   `rate_limit` envelope and string-encoded `resetsAt` are tolerated,
//!   numeric reset timestamps are preserved verbatim
//! * `{"type":"tool_progress","tool_use_id":S,"tool_name":S,
//!    "elapsed_time_seconds":F?,"parent_tool_use_id":S|null,
//!    "session_id":S?}` — documented tool-execution heartbeat (including
//!   the ~30 s foreground heartbeat); progress metadata, never terminal
//! * `{"type":"result","subtype":"success"|"error_max_turns"|
//!    "error_during_execution","is_error":B,"result":S|null,"session_id":S,
//!    "usage":{...}|null,"total_cost_usd":F|null,"permission_denials":[..]?}`
//!
//! Tool frames are DATA ONLY ([`NativeEvent::ToolObservation`]): nothing
//! here can dispatch a host tool call; only the governed MCP bridge
//! (AGENTKIT-114) executes application tools.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use agentkit_native::{
    BillingInclusion, FailureClass, FinishDisposition, McpServerStatus, NativeEvent, NativeRefusal,
    NativeTerminalOutcome, NativeUsage, ProcessExit, ProtocolViolation, StreamFrameAdapter,
    TokenSet, UsageProvenance, VendorToolPhase,
};

/// Minimum official CLI version this adapter's protocol handling is
/// qualified against: `(2, 1, 259)`.
///
/// Rationale (AGENTKIT-109/111): 2.1.259 is the oldest build where the
/// qualification sweep observed the full set of stream contract
/// prerequisites used here — a root `result` frame that always carries
/// `session_id`, `--permission-prompts none` honored without the private
/// stdio control protocol, `--include-partial-messages` `stream_event`
/// deltas with the documented `content_block_delta`/`text_delta` shape, and
/// `api_retry` system frames with numeric `attempt`/`max_retries`. Below
/// this floor the support matrix marks the driver unqualified
/// ([`crate::discovery::ClaudeSupportMatrix`]) and admission refuses the
/// native target rather than guessing at pre-qualification stream shapes.
pub const QUALIFIED_MIN_VERSION: (u32, u32, u32) = (2, 1, 259);

/// The root `result` frame facts the adapter alone possesses; consumed by
/// the terminal-outcome decision table (AGENTKIT-111/116).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultEvidence {
    /// `"success"`, `"error_max_turns"`, `"error_during_execution"` or
    /// `"deferred"`.
    pub subtype: String,
    pub is_error: bool,
    pub result_text: Option<String>,
    pub session_id: String,
    /// Per-turn usage only; `cumulative` stays `None` (the vendor publishes
    /// no cumulative counters in this profile). `None` when the vendor sent
    /// `usage: null` — unknown, never zero.
    pub usage: Option<NativeUsage>,
    pub permission_denials: Vec<String>,
    /// Vendor `errors[]` details from an error result, preserved bounded
    /// and redacted (strings kept as-is; object entries reduced to their
    /// message-ish field or raw JSON text; first 8 kept with a visible
    /// omission entry, each clipped to 300 chars, credential-shaped runs
    /// replaced). Never dropped in favour of the bare subtype
    /// (AGENTKIT-111 diagnostics, R10).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    /// Model identity observed in the `init` frame.
    pub observed_model: Option<String>,
}

/// Why the vendor ended via a deferred/child result path (typed unsupported;
/// native deferral is excluded from this profile, AGENTKIT-116).
#[derive(Clone, Debug)]
enum TerminalSignal {
    /// A valid root `result` frame.
    Root(Box<ResultEvidence>),
    /// A child-scoped `result` (`parent_tool_use_id` non-null): the root
    /// turn's work was deferred into a child stream.
    Deferred(String),
}

/// One root-visible text segment (AGENTKIT-111 segment model): accumulated
/// `text_delta`s until a complete block from the same message supersedes
/// them; then the authoritative complete text.
#[derive(Clone, Debug)]
struct TextSegment {
    text: String,
    /// True once a complete root text block owns this segment's text.
    complete: bool,
    /// Vendor message id when observed (`message.id` /
    /// `event.message.id`); never guessed.
    message_id: Option<String>,
}

/// Incremental decoder from framed stdout lines to normalized events.
///
/// Text accumulation rule (AGENTKIT-111, segment model): the user-visible
/// text sequence per turn is the **complete root segments joined with
/// `\n\n`** once any complete block arrived (multiple text blocks of one
/// message and multiple messages per turn all appear, never overwritten),
/// else the **concatenated root `text_delta`s**. A complete block supersedes
/// the still-incomplete deltas of its own message without duplication;
/// child-scoped frames (`parent_tool_use_id` non-null) never touch the root
/// accumulator at all.
#[derive(Clone, Debug, Default)]
pub struct ClaudeStreamAdapter {
    /// Ordered root text segments: deltas superseded in place, complete
    /// blocks appended.
    segments: Vec<TextSegment>,
    /// Last root message id observed on a stream/assistant frame; lets a
    /// complete block recognize its own segment without guessing.
    current_message_id: Option<String>,
    /// Model identity from the most recent `init` frame.
    init_model: Option<String>,
    /// Parsed root result facts, set once by the first root `result` frame.
    root_result: Option<ResultEvidence>,
    /// Reason captured when a child-scoped `result` marker was seen.
    deferred: Option<String>,
    /// Last non-`stream_event` raw line, for idempotent duplicate-frame
    /// tolerance (a byte-identical repeat is a no-op).
    last_frame: Option<String>,
    /// Opaque markers for redacted-thinking blocks (review round-4 F9):
    /// the vendor withholds the reasoning content, so only the block's own
    /// opaque payload is retained, never rendered as reasoning or
    /// user-visible text and never reconstructed. Bounded like every other
    /// retained vendor payload; the dropped counter records what the cap
    /// discarded.
    redacted_thinking: Vec<Value>,
    redacted_thinking_dropped: usize,
    /// Bounded diagnostic record of tolerated-but-unmodelled vendor shapes:
    /// distinct `"system:<subtype>"` / `"frame:<type>"` / `"block:<type>"`
    /// keys with occurrence counts, capped at [`MAX_IGNORED_KEYS`].
    ignored: std::collections::BTreeMap<String, u64>,
    /// Occurrences whose key did not fit under the distinct-key cap.
    ignored_overflow: u64,
    /// `tool_use_id -> tool name` from `Requested` observations, so a
    /// `tool_result` (which carries no name) can be labelled. FIFO-bounded
    /// at [`MAX_TOOL_NAMES`]; root and child scopes alike.
    tool_names: std::collections::VecDeque<(String, String)>,
}

impl ClaudeStreamAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Concatenated visible root text: all complete segment texts joined
    /// with `\n\n` once any complete root block arrived (multiple blocks
    /// and messages all appear, never overwritten), else the accumulated
    /// root deltas; `None` when no root text was seen at all. Deltas that
    /// arrive **after** the last complete block are the in-flight partial
    /// text of the next message and are appended after the completed text
    /// with the same `\n\n` joiner — an interrupted turn must not lose the
    /// answer being streamed (partial + completed retained, never
    /// duplicated).
    pub fn partial_text(&self) -> Option<String> {
        let last_complete = self.segments.iter().rposition(|s| s.complete);
        if let Some(last_complete) = last_complete {
            let complete: Vec<&str> = self.segments[..=last_complete]
                .iter()
                .filter(|s| s.complete)
                .map(|s| s.text.as_str())
                .collect();
            let tail: String = self.segments[last_complete + 1..]
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .concat();
            let joined = complete.join("\n\n");
            return Some(if tail.is_empty() {
                joined
            } else {
                format!("{joined}\n\n{tail}")
            });
        }
        let deltas: Vec<&str> = self.segments.iter().map(|s| s.text.as_str()).collect();
        let joined = deltas.concat();
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    }

    /// Record a complete root text block under the segment model (C4): it
    /// supersedes the last segment only while that segment still holds
    /// unpromoted deltas of the same message (identity matched where known
    /// on either side); anything else appends a new complete segment, so
    /// multiple blocks of one message and multiple messages per turn all
    /// appear, never overwritten.
    fn record_complete(&mut self, text: String, message_id: Option<String>) {
        let supersedes_last = match self.segments.last() {
            Some(last) if !last.complete => {
                last.message_id.is_none() || message_id.is_none() || last.message_id == message_id
            }
            _ => false,
        };
        if supersedes_last {
            let last = self.segments.last_mut().expect("checked above");
            last.text = text;
            last.complete = true;
            last.message_id = message_id;
        } else {
            self.segments.push(TextSegment {
                text,
                complete: true,
                message_id,
            });
        }
    }

    /// Append a root `text_delta`: into the last segment while it still
    /// holds deltas, else as a new segment.
    fn record_delta(&mut self, text: &str) {
        match self.segments.last_mut() {
            Some(last) if !last.complete => {
                last.text.push_str(text);
                if last.message_id.is_none() {
                    last.message_id = self.current_message_id.clone();
                }
            }
            _ => self.segments.push(TextSegment {
                text: text.to_owned(),
                complete: false,
                message_id: self.current_message_id.clone(),
            }),
        }
    }

    /// Whether a valid root `result` frame was observed (exit alone can
    /// never produce `Completed`, AGENTKIT-107 §3.5).
    pub fn saw_root_result(&self) -> bool {
        self.root_result.is_some()
    }

    /// Root `result` frame facts, once observed.
    pub fn result_evidence(&self) -> Option<ResultEvidence> {
        self.root_result.clone()
    }

    /// Tolerated-but-unmodelled vendor shapes seen so far: distinct
    /// `"system:<subtype>"`, `"frame:<type>"` and `"block:<type>"` keys with
    /// occurrence counts (at most [`MAX_IGNORED_KEYS`] distinct keys; see
    /// [`Self::ignored_overflow`]). Diagnostics only — never user-visible.
    pub fn ignored_frames(&self) -> &std::collections::BTreeMap<String, u64> {
        &self.ignored
    }

    /// Occurrences of tolerated shapes whose distinct key was dropped
    /// because the diagnostic record was full.
    pub fn ignored_overflow(&self) -> u64 {
        self.ignored_overflow
    }

    fn note_ignored(&mut self, key: String) {
        if let Some(count) = self.ignored.get_mut(&key) {
            *count += 1;
        } else if self.ignored.len() < MAX_IGNORED_KEYS {
            self.ignored.insert(key, 1);
        } else {
            self.ignored_overflow += 1;
        }
    }

    fn remember_tool_name(&mut self, id: &str, name: &str) {
        if self.tool_names.iter().any(|(known, _)| known == id) {
            return;
        }
        if self.tool_names.len() >= MAX_TOOL_NAMES {
            self.tool_names.pop_front();
        }
        self.tool_names.push_back((id.to_owned(), name.to_owned()));
    }

    /// Opaque retained markers for withheld redacted-thinking blocks
    /// (review round-4 F9): the vendor's block payload kept verbatim as an
    /// opaque value. Never decoded into reasoning or user-visible text; no
    /// field of the block is required to exist.
    pub fn redacted_thinking_markers(&self) -> &[Value] {
        &self.redacted_thinking
    }

    /// How many redacted-thinking markers exceeded the retention cap: a
    /// nonzero count means the marker list is a prefix, never the whole
    /// turn (unknown is never presented as zero).
    pub fn redacted_thinking_dropped(&self) -> usize {
        self.redacted_thinking_dropped
    }

    /// Normalize one withheld redacted-thinking block to an opaque marker
    /// (review round-4 F9): retained verbatim as an opaque value with NO
    /// field requirements (the opaque payload is never required to be
    /// present, parseable, or well-typed), and never routed into the text
    /// accumulator or the reasoning channel.
    fn record_redacted_thinking(&mut self, block: &Value) {
        if self.redacted_thinking.len() < MAX_REDACTED_MARKERS {
            self.redacted_thinking.push(block.clone());
        } else {
            self.redacted_thinking_dropped += 1;
        }
    }
}

impl StreamFrameAdapter for ClaudeStreamAdapter {
    fn push_line(&mut self, line: &[u8]) -> Result<Vec<NativeEvent>, ProtocolViolation> {
        let text = std::str::from_utf8(line)
            .map_err(|e| violation("invalid_utf8", format!("frame is not valid UTF-8: {e}")))?;
        if text.trim().is_empty() {
            // Blank keep-alive lines carry no information; tolerated.
            return Ok(Vec::new());
        }
        let frame: Value = serde_json::from_str(text)
            .map_err(|e| violation("malformed_json", format!("frame is not valid JSON: {e}")))?;
        let kind = frame.get("type").and_then(Value::as_str);
        // Idempotent duplicate tolerance for complete frames: a byte-
        // identical repeat of the previous non-delta frame is a no-op.
        // Deltas are excluded because repeated identical delta tokens are
        // legitimate and must concatenate.
        if kind != Some("stream_event") && self.last_frame.as_deref() == Some(text) {
            return Ok(Vec::new());
        }
        let events = self.dispatch(kind, &frame)?;
        if kind != Some("stream_event") {
            self.last_frame = Some(text.to_owned());
        }
        Ok(events)
    }

    fn finish(&mut self) -> FinishDisposition {
        FinishDisposition::NoResult {
            partial_text: self.partial_text(),
        }
    }

    /// Authoritative full decision table (AGENTKIT-110/111/116).
    fn terminal_outcome(&self, exit: &ProcessExit) -> NativeTerminalOutcome {
        let signal = if let Some(reason) = &self.deferred {
            Some(TerminalSignal::Deferred(reason.clone()))
        } else {
            self.root_result
                .clone()
                .map(|ev| TerminalSignal::Root(Box::new(ev)))
        };
        let partial = self.partial_text();
        terminal_core(exit, signal.as_ref(), partial.as_deref(), &[])
    }
}

impl ClaudeStreamAdapter {
    fn dispatch(
        &mut self,
        kind: Option<&str>,
        frame: &Value,
    ) -> Result<Vec<NativeEvent>, ProtocolViolation> {
        match kind {
            Some("system") => self.on_system(frame),
            Some("assistant") => self.on_assistant(frame),
            Some("user") => self.on_user(frame),
            Some("stream_event") => self.on_stream_event(frame),
            Some("result") => self.on_result(frame),
            Some("rate_limit_event") => self.on_rate_limit_event(frame),
            // Documented tool-execution heartbeat (foreground heartbeats
            // arrive every ~30 s while a tool runs): progress metadata,
            // never terminal. Validated loosely — an unusable heartbeat
            // must not kill an otherwise healthy tool turn (AGENTKIT-111).
            Some("tool_progress") => {
                parent_id(frame)?;
                Ok(Vec::new())
            }
            // Qualified additive metadata (ignored): plugin-install and
            // prompt-suggestion frames.
            Some("plugin_install") | Some("prompt_suggestion") => Ok(Vec::new()),
            // Any other top-level type (`auth_status`, `tool_use_summary`,
            // `keep_alive`, ...) is tolerated and counted, never fatal.
            Some(other) => {
                self.note_ignored(format!("frame:{other}"));
                Ok(Vec::new())
            }
            None => Err(violation(
                "missing_frame_type",
                "frame has no string 'type' field",
            )),
        }
    }

    fn on_system(&mut self, frame: &Value) -> Result<Vec<NativeEvent>, ProtocolViolation> {
        let subtype = frame
            .get("subtype")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                violation(
                    "system_missing_subtype",
                    "system frame without string subtype",
                )
            })?;
        match subtype {
            "init" => self.on_init(frame),
            "api_retry" => on_api_retry(frame),
            // Documented progress metadata: the vendor's current stream
            // status (`"compacting"` or null = cleared) and the compaction
            // boundary marker. Never terminal-affecting (AGENTKIT-111).
            "status" => {
                let status = opt_str(frame, "status", "system_status_type")?.map(str::to_owned);
                Ok(vec![NativeEvent::Status { status }])
            }
            "compact_boundary" => {
                let trigger =
                    opt_str(frame, "trigger", "compact_boundary_trigger_type")?.map(str::to_owned);
                Ok(vec![NativeEvent::CompactBoundary { trigger }])
            }
            "permission_denied" => {
                let tool_name = frame
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                Ok(vec![NativeEvent::PermissionDenied { tool_name }])
            }
            // Documented refusal-without-fallback notification (review
            // round-4 F9): the vendor says this model refused and no
            // fallback was applied. Informational progress, shaped like the
            // other non-fatal vendor notices: it never terminates the
            // stream and never becomes a terminal outcome — the root
            // `result` frame stays the sole authority (AGENTKIT-111/116).
            "model_refusal_no_fallback" => Ok(vec![NativeEvent::Warning {
                code: "model_refusal_no_fallback".to_owned(),
                message: "vendor reported a model refusal with no fallback model; \
                          the root result frame remains the terminal authority"
                    .to_owned(),
            }]),
            // Hook lifecycle, plugin-install and tool-progress system
            // frames are additive progress metadata; correctness of the
            // turn does not depend on them (AGENTKIT-111 tolerance list).
            "hook_started" | "hook_progress" | "hook_response" | "plugin_install"
            | "tool_progress" => Ok(Vec::new()),
            // Every other subtype (`thinking_tokens`, `notification`,
            // `session_state_changed`, `turn_duration`, ...) is
            // forward-compatible progress metadata: tolerated silently (no
            // Warning: these can arrive many times per second) and
            // counted for diagnostics.
            other => {
                self.note_ignored(format!("system:{other}"));
                Ok(Vec::new())
            }
        }
    }

    fn on_init(&mut self, frame: &Value) -> Result<Vec<NativeEvent>, ProtocolViolation> {
        let session_id = required_str(frame, "session_id")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| violation("init_missing_session_id", "init frame without session_id"))?
            .to_owned();
        let observed_model = opt_str(frame, "model", "init_model_type")?.map(str::to_owned);
        // Effort is claimed only when the init frame itself reports it.
        let observed_effort = opt_str(frame, "effort", "init_effort_type")?.map(str::to_owned);
        let observed_version =
            opt_str(frame, "claude_code_version", "init_version_type")?.map(str::to_owned);
        let capabilities = frame
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        // MCP server connection statuses, parsed tolerantly: malformed
        // entries are skipped, absent means none reported.
        let mcp_servers = frame
            .get("mcp_servers")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        Some(McpServerStatus {
                            name: item.get("name")?.as_str()?.to_owned(),
                            status: item.get("status")?.as_str()?.to_owned(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.init_model = observed_model.clone();
        Ok(vec![NativeEvent::Init {
            session_id,
            observed_model,
            observed_effort,
            observed_version,
            capabilities,
            mcp_servers,
        }])
    }

    fn on_assistant(&mut self, frame: &Value) -> Result<Vec<NativeEvent>, ProtocolViolation> {
        let parent = parent_id(frame)?;
        let content = content_blocks(frame, "assistant")?;
        let mut events = Vec::new();
        for block in content {
            let block_type = block
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| violation("block_missing_type", "assistant block without type"))?;
            match block_type {
                "text" => {
                    let text = required_str(block, "text")
                        .ok_or_else(|| {
                            violation("text_block_missing_text", "text block without text")
                        })?
                        .to_owned();
                    // Child-scoped text belongs to the vendor's substream:
                    // never touches the root accumulator and never surfaces
                    // as root user-visible text.
                    if parent.is_none() {
                        // The complete block supersedes the still-incomplete
                        // deltas of its own message; earlier complete text
                        // is accumulated, never overwritten.
                        let message_id = frame
                            .get("message")
                            .and_then(|m| m.get("id"))
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        if message_id.is_some() {
                            self.current_message_id = message_id.clone();
                        }
                        self.record_complete(text.clone(), message_id);
                        events.push(NativeEvent::MessageCompleted { text });
                    }
                }
                "tool_use" => {
                    let id = required_str(block, "id")
                        .ok_or_else(|| {
                            violation("tool_use_missing_id", "tool_use block without id")
                        })?
                        .to_owned();
                    let name = required_str(block, "name")
                        .ok_or_else(|| {
                            violation("tool_use_missing_name", "tool_use block without name")
                        })?
                        .to_owned();
                    let input = block.get("input").filter(|v| !v.is_null()).cloned();
                    self.remember_tool_name(&id, &name);
                    events.push(NativeEvent::ToolObservation {
                        vendor_tool_use_id: id,
                        name,
                        phase: VendorToolPhase::Requested,
                        input,
                        parent_tool_use_id: parent.clone(),
                        is_error: None,
                        result: None,
                    });
                }
                "thinking" => {
                    let text = required_str(block, "thinking")
                        .ok_or_else(|| {
                            violation("thinking_missing_text", "thinking block without text")
                        })?
                        .to_owned();
                    if parent.is_none() {
                        events.push(NativeEvent::ReasoningDelta { text });
                    }
                }
                // Public SDK withheld-reasoning block (review round-4 F9):
                // accepted as an opaque marker only. No field of the block
                // is required (the opaque payload may be absent or
                // malformed), nothing is reconstructed or displayed as
                // reasoning, and sibling blocks keep processing normally,
                // so the visible answer after it still surfaces.
                "redacted_thinking" => {
                    if parent.is_none() {
                        self.record_redacted_thinking(block);
                    }
                }
                // Unknown block types are skipped (counted); siblings still
                // process.
                other => {
                    self.note_ignored(format!("block:{other}"));
                }
            }
        }
        Ok(events)
    }

    fn on_user(&mut self, frame: &Value) -> Result<Vec<NativeEvent>, ProtocolViolation> {
        let parent = parent_id(frame)?;
        let message = frame
            .get("message")
            .ok_or_else(|| violation("user_missing_message", "user frame without message"))?;
        if message.get("content").is_some_and(Value::is_string) {
            // Plain-string content is the echo of our own input frame.
            return Ok(Vec::new());
        }
        let content = message
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                violation("user_missing_content", "user message without content array")
            })?;
        let mut events = Vec::new();
        for block in content {
            // Text echoes and non-structural blocks (images etc. from a
            // resumed session) never affect root correctness: ignored.
            if let Some("tool_result") = block.get("type").and_then(Value::as_str) {
                let tool_use_id = required_str(block, "tool_use_id").ok_or_else(|| {
                    violation(
                        "tool_result_missing_id",
                        "tool_result block without tool_use_id",
                    )
                })?;
                // The block carries no name; resolve it from the matching
                // `tool_use` (unknown id keeps the empty string).
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        self.tool_names
                            .iter()
                            .find(|(id, _)| id == tool_use_id)
                            .map(|(_, name)| name.clone())
                    })
                    .unwrap_or_default();
                // Absent `is_error` means the call succeeded.
                let is_error = block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                events.push(NativeEvent::ToolObservation {
                    vendor_tool_use_id: tool_use_id.to_owned(),
                    name,
                    phase: VendorToolPhase::Result,
                    input: None,
                    parent_tool_use_id: parent.clone(),
                    is_error: Some(is_error),
                    result: tool_result_text(block),
                });
            }
        }
        Ok(events)
    }

    fn on_stream_event(&mut self, frame: &Value) -> Result<Vec<NativeEvent>, ProtocolViolation> {
        // Child-scoped deltas belong to the vendor's substream: they must
        // never pollute the root accumulator (AGENTKIT-111).
        if parent_id(frame)?.is_some() {
            return Ok(Vec::new());
        }
        let event = frame
            .get("event")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                violation(
                    "stream_event_missing_event",
                    "stream_event without event object",
                )
            })?;
        // Message identity from `message_start` / partial message envelopes
        // when the vendor carries it; never guessed.
        if let Some(id) = event
            .get("message")
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
        {
            self.current_message_id = Some(id.to_owned());
        }
        let Some(event_type) = event.get("type").and_then(Value::as_str) else {
            return Err(violation(
                "stream_event_missing_type",
                "stream_event event without string type",
            ));
        };
        if event_type != "content_block_delta" {
            // message_start / content_block_start / stop / message_delta /
            // message_stop: framing metadata, ignored.
            return Ok(Vec::new());
        }
        let delta = event
            .get("delta")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                violation("delta_missing_object", "content_block_delta without delta")
            })?;
        let delta_type = delta.get("type").and_then(Value::as_str).ok_or_else(|| {
            violation(
                "delta_missing_type",
                "content_block_delta without delta type",
            )
        })?;
        if delta_type != "text_delta" {
            // thinking_delta / input_json_delta / signature_delta and any
            // future delta kinds: the authoritative complete frames carry
            // this content; ignored.
            return Ok(Vec::new());
        }
        let text = delta.get("text").and_then(Value::as_str).ok_or_else(|| {
            violation("text_delta_missing_text", "text_delta without string text")
        })?;
        self.record_delta(text);
        Ok(vec![NativeEvent::TextDelta {
            text: text.to_owned(),
        }])
    }

    /// Top-level `rate_limit_event` frame: a progress signal, never
    /// terminal-affecting (AGENTKIT-111). The public Agent SDK shape
    /// carries a `rate_limit_info` object (`status` string, numeric
    /// `resetsAt` Unix seconds, `utilization`, `rateLimitType`); the older
    /// `rate_limit` envelope is accepted as an alias and a string-encoded
    /// numeric `resetsAt` is preserved as the same value. Strict where the
    /// frame identifies the signal (envelope object, string `status`);
    /// malformed progress VALUES (non-numeric reset/utilization) are
    /// surfaced as a non-fatal [`NativeEvent::Warning`] instead of
    /// terminating an otherwise healthy turn — progress metadata must
    /// never be terminal-affecting.
    fn on_rate_limit_event(&self, frame: &Value) -> Result<Vec<NativeEvent>, ProtocolViolation> {
        let rate = frame
            .get("rate_limit_info")
            .or_else(|| frame.get("rate_limit"))
            .filter(|r| r.is_object())
            .ok_or_else(|| {
                violation(
                    "rate_limit_shape",
                    "rate_limit_event without rate_limit_info object",
                )
            })?;
        let status = required_str(rate, "status")
            .ok_or_else(|| violation("rate_limit_shape", "rate_limit without string status"))?;
        let mut events = Vec::new();
        let mut malformed = |key: &str, detail: String| {
            events.push(NativeEvent::Warning {
                code: "rate_limit_shape".to_owned(),
                message: format!("rate_limit {key} unusable, got {detail}"),
            });
        };
        let resets_at = match rate.get("resetsAt").or_else(|| rate.get("resets_at")) {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => match n.as_f64() {
                Some(v) => Some(v),
                None => {
                    malformed("resetsAt", n.to_string());
                    None
                }
            },
            Some(Value::String(s)) => match s.trim().parse::<f64>() {
                Ok(v) => Some(v),
                Err(_) => {
                    malformed("resetsAt", format!("{s:?}"));
                    None
                }
            },
            Some(other) => {
                malformed("resetsAt", other.to_string());
                None
            }
        };
        let utilization = match rate.get("utilization") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => match n.as_f64() {
                Some(v) => Some(v),
                None => {
                    malformed("utilization", n.to_string());
                    None
                }
            },
            Some(other) => {
                malformed("utilization", other.to_string());
                None
            }
        };
        let rate_limit_type = match rate
            .get("rateLimitType")
            .or_else(|| rate.get("rate_limit_type"))
        {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(other) => {
                malformed("rateLimitType", other.to_string());
                None
            }
        };
        events.insert(
            0,
            NativeEvent::RateLimit {
                status: status.to_owned(),
                resets_at,
                utilization,
                rate_limit_type,
            },
        );
        Ok(events)
    }

    fn on_result(&mut self, frame: &Value) -> Result<Vec<NativeEvent>, ProtocolViolation> {
        let subtype = required_str(frame, "subtype")
            .ok_or_else(|| violation("result_missing_subtype", "result frame without subtype"))?
            .to_owned();
        // Unknown result subtypes are accepted as root results when the
        // frame is otherwise valid: `terminal_core` maps every non-`success`
        // subtype to `Failed`, the safe direction.
        let session_id = required_str(frame, "session_id")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                violation(
                    "result_missing_session_id",
                    "result frame without session_id",
                )
            })?
            .to_owned();
        let is_error = frame
            .get("is_error")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                violation(
                    "result_missing_is_error",
                    "result frame without boolean is_error",
                )
            })?;
        let result_text = frame
            .get("result")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let usage = result_usage(frame);
        let permission_denials = result_denials(frame);
        let errors = result_errors(frame);
        // Child-scoped result: the vendor deferred work into a child
        // stream; never a root completion (AGENTKIT-116).
        if let Some(parent) = parent_id(frame)? {
            if self.deferred.is_none() {
                self.deferred = Some(format!(
                    "vendor emitted a child-scoped result (parent_tool_use_id '{parent}', subtype '{subtype}'): native deferral is excluded from this profile"
                ));
            }
            return Ok(Vec::new());
        }
        if let Some(existing) = &self.root_result {
            // A second, different root result contradicts the first: fail
            // explicitly rather than pick one.
            if existing.subtype != subtype
                || existing.is_error != is_error
                || existing.result_text != result_text
                || existing.session_id != session_id
                || existing.errors != errors
            {
                return Err(violation(
                    "conflicting_root_result",
                    "second root result frame contradicts the first",
                ));
            }
            return Ok(Vec::new());
        }
        let evidence = ResultEvidence {
            subtype,
            is_error,
            result_text,
            session_id,
            usage: usage.clone(),
            permission_denials: permission_denials.clone(),
            errors,
            observed_model: self.init_model.clone(),
        };
        self.root_result = Some(evidence);
        let mut events = Vec::new();
        // The result frame is the vendor's single usage statement for the
        // turn: one Usage event, per-turn scope only.
        events.push(NativeEvent::Usage(usage.unwrap_or_default()));
        for tool_name in permission_denials {
            events.push(NativeEvent::PermissionDenied { tool_name });
        }
        Ok(events)
    }
}

fn on_api_retry(frame: &Value) -> Result<Vec<NativeEvent>, ProtocolViolation> {
    let attempt = frame
        .get("attempt")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| {
            violation(
                "api_retry_missing_attempt",
                "api_retry without numeric attempt",
            )
        })?;
    let max_retries = frame
        .get("max_retries")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| {
            violation(
                "api_retry_missing_max_retries",
                "api_retry without numeric max_retries",
            )
        })?;
    let error = opt_str(frame, "error", "api_retry_error_type")?;
    Ok(vec![NativeEvent::Retry {
        attempt,
        max_retries,
        error_class: retry_error_class(error),
    }])
}

/// Map `api_retry` error categories to an actionable class. API-category
/// retries (rate limit, overloaded) are progress information only: the
/// vendor is still retrying, so no failure class is claimed.
fn retry_error_class(error: Option<&str>) -> Option<FailureClass> {
    match error {
        Some("authentication_failed") => Some(FailureClass::AuthExpired),
        Some("billing_error") => Some(FailureClass::BillingBlocked),
        _ => None,
    }
}

/// Usage from a `result` frame: per-turn scope only, `cumulative` stays
/// `None` unless a future cumulative field is present; cost is an estimate
/// provenance, and billing inclusion is never inferred (AGENTKIT-107 §4).
fn result_usage(frame: &Value) -> Option<NativeUsage> {
    let cost = frame.get("total_cost_usd").and_then(Value::as_f64);
    let usage = frame.get("usage").filter(|u| !u.is_null())?;
    let tokens = TokenSet {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        cache_read_tokens: usage.get("cache_read_input_tokens").and_then(Value::as_u64),
        cache_write_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64),
    };
    Some(NativeUsage {
        per_turn: Some(tokens),
        cumulative: cumulative_tokens(frame),
        api_equivalent_estimate_usd: cost,
        estimate_provenance: if cost.is_some() {
            UsageProvenance::Estimated
        } else {
            UsageProvenance::Unknown
        },
        billing_inclusion: BillingInclusion::Unknown,
    })
}

/// Reserved for a future vendor cumulative counter field
/// (`cumulativeUsage`); the qualified protocol publishes none, so this is
/// always `None` today (per-turn vs cumulative stays explicit).
fn cumulative_tokens(_frame: &Value) -> Option<TokenSet> {
    None
}

/// Collect permission denial names from `permission_denials`: entries are
/// either plain strings or objects carrying `tool_name`/`toolName`; other
/// shapes contribute no name and are skipped.
fn result_denials(frame: &Value) -> Vec<String> {
    frame
        .get("permission_denials")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item {
                    Value::String(s) => Some(s.clone()),
                    Value::Object(_) => item
                        .get("tool_name")
                        .or_else(|| item.get("toolName"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Cap on retained vendor `errors[]` diagnostics: the first
/// [`MAX_ERROR_DETAILS`] usable entries, each clipped to
/// [`MAX_ERROR_DETAIL_CHARS`] characters, plus an honest omission entry
/// when rows were dropped. These strings become user-visible failure text
/// and durable evidence — unbounded vendor payloads are neither displayable
/// nor storable, and credential-shaped substrings are redacted before
/// retention (R10: details survive, bounded and redacted, never flattened
/// into the bare subtype).
const MAX_ERROR_DETAILS: usize = 8;
const MAX_ERROR_DETAIL_CHARS: usize = 300;

/// Cap on distinct tolerated-shape keys in the diagnostic record.
const MAX_IGNORED_KEYS: usize = 32;
/// Cap on remembered `tool_use_id -> name` pairs.
const MAX_TOOL_NAMES: usize = 256;
/// Cap on the textual tool result carried by a `ToolObservation`.
const MAX_TOOL_RESULT_BYTES: usize = 16 * 1024;
/// Appended when a tool result was clipped.
const TOOL_RESULT_TRUNCATION_MARKER: &str = "\n[... tool result truncated ...]";

/// Textual content of a `tool_result` block: a string `content` as is, or
/// the `\n`-joined `text` of the `{"type":"text"}` parts of an array
/// `content` (non-text parts skipped). Clipped to 16 KiB on a char
/// boundary with a visible marker; `None` when there is no text.
fn tool_result_text(block: &Value) -> Option<String> {
    let text = match block.get("content")? {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect();
            if texts.is_empty() {
                return None;
            }
            texts.join("\n")
        }
        _ => return None,
    };
    if text.len() <= MAX_TOOL_RESULT_BYTES {
        return Some(text);
    }
    let mut end = MAX_TOOL_RESULT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}{TOOL_RESULT_TRUNCATION_MARKER}", &text[..end]))
}

/// Cap on retained redacted-thinking opaque markers per turn (review
/// round-4 F9): the markers are durable evidence about withheld content,
/// and an unbounded vendor payload is neither storable nor displayable.
const MAX_REDACTED_MARKERS: usize = 32;

/// Collect vendor `errors[]` details from an error `result` frame. String
/// entries are kept as-is; object entries contribute their most
/// message-like field (`message`/`detail`/`error`/`type`, else the raw
/// JSON text) so diagnostics survive to the user-visible failure. The
/// result is bounded ([`MAX_ERROR_DETAILS`] / [`MAX_ERROR_DETAIL_CHARS`],
/// with a visible omission entry) and secret-redacted
/// ([`redact_secret_ish`]).
fn result_errors(frame: &Value) -> Vec<String> {
    let Some(items) = frame.get("errors").and_then(Value::as_array) else {
        return Vec::new();
    };
    let collected: Vec<String> = items
        .iter()
        .filter_map(|item| match item {
            Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
            Value::Object(_) => ["message", "detail", "error", "type"]
                .iter()
                .find_map(|key| item.get(*key).and_then(Value::as_str))
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .or_else(|| Some(item.to_string())),
            _ => None,
        })
        .collect();
    let mut details: Vec<String> = collected
        .iter()
        .take(MAX_ERROR_DETAILS)
        .map(|text| bounded_error_detail(text))
        .collect();
    let omitted = collected.len().saturating_sub(MAX_ERROR_DETAILS);
    if omitted > 0 {
        details.push(format!(
            "… {omitted} further vendor error detail(s) omitted"
        ));
    }
    details
}

/// Clip one vendor error detail to the character cap (never splitting a
/// character) and redact credential-shaped substrings from it.
fn bounded_error_detail(text: &str) -> String {
    let count = text.chars().count();
    let clipped: String = text.chars().take(MAX_ERROR_DETAIL_CHARS).collect();
    let clipped = if count > MAX_ERROR_DETAIL_CHARS {
        format!("{clipped}…")
    } else {
        clipped
    };
    redact_secret_ish(&clipped)
}

/// The character set of a credential run: base64-ish plus token punctuation.
fn is_secret_run_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'_' | b'-' | b'+' | b'/' | b'=' | b'.' | b'~' | b'%' | b':'
        )
}

fn secret_run_end(bytes: &[u8], from: usize) -> usize {
    let mut end = from;
    while end < bytes.len() && is_secret_run_byte(bytes[end]) {
        end += 1;
    }
    end
}

/// Redact credential-shaped substrings from vendor-supplied text before it
/// becomes durable/user-visible. Two shapes, bounded string scans only
/// (no regexes, O(len) per marker). Token prefixes — `sk-…`, `ghp_…`,
/// `github_pat_…`, `AIza…` — replace the whole marker+run when the run is
/// credibly long (≥ 6). Secret labels — `Bearer …`, `x-api-key`, `api_key`,
/// `apikey`, `api-key`, `access_token`, `authorization` — replace only the
/// run after the label's separators, and only when credibly long (≥ 8).
/// A marker never starts inside a larger word (the preceding byte must not
/// be a run byte), so prose like "task-force" survives intact.
fn redact_secret_ish(text: &str) -> String {
    const REDACTION: &str = "[redacted]";
    const PREFIX_TOKENS: [&str; 4] = ["sk-", "ghp_", "github_pat_", "aiza"];
    const SECRET_LABELS: [&str; 7] = [
        "bearer",
        "x-api-key",
        "api_key",
        "apikey",
        "api-key",
        "access_token",
        "authorization",
    ];
    let bytes = text.as_bytes();
    let lower: Vec<u8> = bytes.iter().map(u8::to_ascii_lowercase).collect();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    'scan: while index < bytes.len() {
        let word_bounded = index == 0 || !is_secret_run_byte(bytes[index - 1]);
        if word_bounded {
            for marker in PREFIX_TOKENS {
                if !lower[index..].starts_with(marker.as_bytes()) {
                    continue;
                }
                let run_start = index + marker.len();
                let run_end = secret_run_end(bytes, run_start);
                if run_end - run_start >= 6 {
                    out.extend_from_slice(REDACTION.as_bytes());
                    index = run_end;
                    continue 'scan;
                }
            }
            for label in SECRET_LABELS {
                if !lower[index..].starts_with(label.as_bytes()) {
                    continue;
                }
                let after_label = index + label.len();
                let mut run_start = after_label;
                while run_start < lower.len()
                    && matches!(lower[run_start], b' ' | b'\t' | b':' | b'=' | b'"' | b'\'')
                {
                    run_start += 1;
                }
                // The value must be separated from the label ("authorization
                // header" is prose; only a separated value is a secret).
                if run_start == after_label {
                    continue;
                }
                let run_end = secret_run_end(bytes, run_start);
                if run_end - run_start >= 8 {
                    out.extend_from_slice(&bytes[index..run_start]);
                    out.extend_from_slice(REDACTION.as_bytes());
                    index = run_end;
                    continue 'scan;
                }
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

/// `parent_tool_use_id`: absent or null = root; string = child identity.
fn parent_id(frame: &Value) -> Result<Option<String>, ProtocolViolation> {
    match frame.get("parent_tool_use_id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(violation(
            "parent_tool_use_id_type",
            format!("parent_tool_use_id must be string or null, got {other}"),
        )),
    }
}

fn content_blocks<'a>(frame: &'a Value, kind: &str) -> Result<&'a Vec<Value>, ProtocolViolation> {
    frame
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            violation(
                "assistant_missing_content",
                format!("{kind} frame without message.content array"),
            )
        })
}

fn required_str<'a>(frame: &'a Value, key: &str) -> Option<&'a str> {
    frame.get(key).and_then(Value::as_str)
}

/// Known string field with strict typing: present-but-not-string (nor
/// null) on a correctness-relevant field is a violation, absent/null is
/// `None`.
fn opt_str<'a>(
    frame: &'a Value,
    key: &str,
    code: &str,
) -> Result<Option<&'a str>, ProtocolViolation> {
    match frame.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(violation(
            code,
            format!("field '{key}' must be string or null, got {other}"),
        )),
    }
}

fn violation(code: &str, message: impl Into<String>) -> ProtocolViolation {
    ProtocolViolation {
        code: code.to_owned(),
        message: message.into(),
    }
}

/// Encode the admitted turn input as one NDJSON stdin frame:
/// `{"type":"user","client_composed":true,"message":{"role":"user",
/// "content":text}}` + `\n`. `client_composed` is the literal Anthropic
/// wire field (snake_case) marking the frame as host-composed literal
/// input. Content newlines are JSON-escaped by the serializer, so the
/// frame is always exactly one NDJSON line (AGENTKIT-110/113).
pub fn claude_input_frame(text: &str) -> Result<Vec<u8>, NativeRefusal> {
    if text.trim().is_empty() {
        return Err(NativeRefusal::new(
            "empty_input",
            "turn input text must not be empty or whitespace-only",
            false,
        ));
    }
    let frame = serde_json::json!({
        "type": "user",
        "client_composed": true,
        "message": { "role": "user", "content": text },
    });
    let mut bytes = serde_json::to_vec(&frame).map_err(|e| {
        NativeRefusal::new(
            "input_encoding",
            format!("input frame not serializable: {e}"),
            false,
        )
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Stateless terminal mapping for the **no-result** exit rows only (used by
/// the backend fallback and the executor's generic path). It never
/// produces `Completed`: root-result rows live on
/// [`ClaudeStreamAdapter::terminal_outcome`] and
/// [`claude_terminal_outcome_with_result`] (AGENTKIT-107 §3.5).
pub fn claude_terminal_outcome(
    exit: &ProcessExit,
    observed: &[NativeEvent],
    partial_text: Option<&str>,
) -> NativeTerminalOutcome {
    terminal_core(exit, None, partial_text, observed)
}

/// Full decision table pinned for the conformance harness (AGENTKIT-116):
/// root-result rows first, then the exit rows shared with
/// [`claude_terminal_outcome`].
pub fn claude_terminal_outcome_with_result(
    exit: &ProcessExit,
    result: Option<&ResultEvidence>,
    observed: &[NativeEvent],
    partial_text: Option<&str>,
) -> NativeTerminalOutcome {
    let signal = result.cloned().map(|ev| TerminalSignal::Root(Box::new(ev)));
    terminal_core(exit, signal.as_ref(), partial_text, observed)
}

fn terminal_core(
    exit: &ProcessExit,
    signal: Option<&TerminalSignal>,
    partial_text: Option<&str>,
    observed: &[NativeEvent],
) -> NativeTerminalOutcome {
    let partial = partial_text.map(str::to_owned);
    match signal {
        Some(TerminalSignal::Deferred(reason)) => NativeTerminalOutcome::DeferredBlocked {
            reason: reason.clone(),
            partial_text: partial,
        },
        Some(TerminalSignal::Root(ev)) => {
            if ev.subtype == "deferred" {
                return NativeTerminalOutcome::DeferredBlocked {
                    reason: "vendor root result subtype is 'deferred': native deferral is excluded from this profile".to_owned(),
                    partial_text: partial,
                };
            }
            if ev.subtype == "success" && !ev.is_error {
                return NativeTerminalOutcome::Completed {
                    text: ev
                        .result_text
                        .clone()
                        .or_else(|| partial.clone())
                        .unwrap_or_default(),
                    session_id: ev.session_id.clone(),
                    usage: ev.usage.clone(),
                    permission_denials: ev.permission_denials.clone(),
                    observed_model: ev.observed_model.clone(),
                    observed_effort: None,
                };
            }
            let mut message = ev
                .result_text
                .clone()
                .unwrap_or_else(|| format!("vendor result subtype '{}'", ev.subtype));
            if !ev.errors.is_empty() {
                // Vendor error details survive into the failure: appended
                // unless the result text already carries them verbatim.
                let detail = ev.errors.join("; ");
                if !message.contains(&detail) {
                    message = if message.trim().is_empty() {
                        detail
                    } else {
                        format!("{message} — vendor errors: {detail}")
                    };
                }
            }
            NativeTerminalOutcome::Failed {
                class: classify_result(ev),
                message,
                partial_text: partial,
            }
        }
        None => exit_rows(exit, partial, observed),
    }
}

fn exit_rows(
    exit: &ProcessExit,
    partial: Option<String>,
    observed: &[NativeEvent],
) -> NativeTerminalOutcome {
    let denied = observed
        .iter()
        .filter(|e| matches!(e, NativeEvent::PermissionDenied { .. }))
        .count();
    // A user Stop (host-requested interrupt) with no root result leaves the
    // turn unfinished. Live, `claude -p` answers SIGINT with a clean exit 0
    // and no result frame, so exit 0 / 130 / 143 or signal 2 / 15 / 9 after
    // a requested interrupt are all `Interrupted`. Without a requested
    // interrupt the same exits keep their rows below (AGENTKIT-116).
    if exit.after_interrupt
        && (matches!(exit.exit_code, Some(0 | 130 | 143))
            || matches!(exit.signal, Some(2 | 15 | 9)))
    {
        return NativeTerminalOutcome::Interrupted {
            partial_text: partial,
        };
    }
    if exit.exit_code == Some(0) && exit.signal.is_none() {
        return NativeTerminalOutcome::Uncertain {
            class: FailureClass::ExitWithoutResult,
            detail: "clean exit without root result".to_owned(),
            partial_text: partial,
        };
    }
    if exit.exit_code == Some(143) && !exit.after_interrupt {
        return NativeTerminalOutcome::Uncertain {
            class: FailureClass::ProcessLost,
            detail: "SIGTERM from outside without result".to_owned(),
            partial_text: partial,
        };
    }
    let class = if exit.signal.is_some() {
        FailureClass::ProcessLost
    } else {
        FailureClass::ExitWithoutResult
    };
    NativeTerminalOutcome::Failed {
        class,
        message: format!(
            "vendor process exited without a root result (exit_code={:?}, signal={:?}{})",
            exit.exit_code,
            exit.signal,
            if denied > 0 {
                format!(", {denied} permission denial(s) observed")
            } else {
                String::new()
            }
        ),
        partial_text: partial,
    }
}

/// Classify an error result (AGENTKIT-116). String matching is on the
/// vendor's human-readable result text; anything unrecognized stays
/// `Internal` rather than guessing an actionable family.
fn classify_result(ev: &ResultEvidence) -> FailureClass {
    if ev.subtype == "error_max_turns" {
        return FailureClass::TurnDeadline;
    }
    let haystack = format!(
        "{} {} {}",
        ev.subtype,
        ev.result_text.clone().unwrap_or_default(),
        ev.errors.join(" ")
    )
    .to_lowercase();
    if haystack.contains("expired") {
        FailureClass::AuthExpired
    } else if haystack.contains("login")
        || haystack.contains("log in")
        || haystack.contains("oauth")
    {
        FailureClass::NotSignedIn
    } else if haystack.contains("credit")
        || haystack.contains("quota")
        || haystack.contains("rate limit")
        || haystack.contains("usage limit")
        || haystack.contains("limit reached")
        || haystack.contains("out of extra usage")
        || haystack.contains("hit your limit")
    {
        FailureClass::QuotaExhausted
    } else if haystack.contains("model")
        && haystack.contains("not")
        && (haystack.contains("access") || haystack.contains("allowed"))
    {
        FailureClass::ModelNotEntitled
    } else {
        FailureClass::Internal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"sess-1","model":"claude-sonnet-4-5","tools":["Read"],"mcp_servers":[{"name":"agentkit","status":"connected"}],"claude_code_version":"2.1.310"}"#;
    const INIT_WITH_EFFORT: &str = r#"{"type":"system","subtype":"init","session_id":"sess-1","model":"claude-sonnet-4-5","effort":"high","capabilities":["streaming-tools"]}"#;
    const DELTA_HEL: &str = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}},"session_id":"sess-1"}"#;
    const DELTA_LO: &str = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}},"session_id":"sess-1"}"#;
    const MSG_HELLO: &str = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]},"session_id":"sess-1","parent_tool_use_id":null}"#;
    const RESULT_OK: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"Hello","session_id":"sess-1","usage":{"input_tokens":11,"output_tokens":5,"cache_read_input_tokens":3,"cache_creation_input_tokens":2},"total_cost_usd":0.004,"permission_denials":[]}"#;
    const RESULT_OK_NULL: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":null,"session_id":"sess-1","usage":null,"total_cost_usd":null}"#;
    const TOOL_USE: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"x"}}]},"session_id":"sess-1","parent_tool_use_id":null}"#;
    const TOOL_RESULT: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"data"}]},"session_id":"sess-1","parent_tool_use_id":null}"#;
    const CHILD_TOOL_RESULT: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_9","content":"child data"}]},"session_id":"sess-1","parent_tool_use_id":"toolu_parent"}"#;

    fn exit_with(code: Option<i32>, signal: Option<i32>, after_interrupt: bool) -> ProcessExit {
        ProcessExit {
            pid: 4242,
            generation: 1,
            exit_code: code,
            signal,
            after_interrupt,
        }
    }

    fn push(adapter: &mut ClaudeStreamAdapter, line: &str) -> Vec<NativeEvent> {
        adapter
            .push_line(line.as_bytes())
            .expect("fixture line accepted")
    }

    /// Feed a canonical successful session, returning the primed adapter.
    fn successful_adapter() -> ClaudeStreamAdapter {
        let mut adapter = ClaudeStreamAdapter::new();
        for line in [INIT, DELTA_HEL, DELTA_LO, MSG_HELLO, RESULT_OK] {
            push(&mut adapter, line);
        }
        adapter
    }

    fn result_line(subtype: &str, is_error: bool, result: Option<&str>) -> String {
        format!(
            r#"{{"type":"result","subtype":{subtype:?},"is_error":{is_error},"result":{},"session_id":"sess-1"}}"#,
            match result {
                Some(t) => format!("{t:?}"),
                None => "null".to_owned(),
            }
        )
    }

    #[test]
    fn init_normalizes_identity_and_claims_effort_only_when_reported() {
        let mut adapter = ClaudeStreamAdapter::new();
        let events = push(&mut adapter, INIT);
        assert_eq!(
            events,
            vec![NativeEvent::Init {
                session_id: "sess-1".into(),
                observed_model: Some("claude-sonnet-4-5".into()),
                observed_effort: None,
                observed_version: Some("2.1.310".into()),
                capabilities: vec![],
                mcp_servers: vec![McpServerStatus {
                    name: "agentkit".into(),
                    status: "connected".into()
                }],
            }]
        );
        let mut with_effort = ClaudeStreamAdapter::new();
        let events = push(&mut with_effort, INIT_WITH_EFFORT);
        match &events[0] {
            NativeEvent::Init {
                observed_effort,
                capabilities,
                observed_version,
                ..
            } => {
                assert_eq!(observed_effort.as_deref(), Some("high"));
                assert_eq!(capabilities, &["streaming-tools".to_owned()]);
                assert_eq!(observed_version, &None, "never claim version unobserved");
            }
            other => panic!("expected Init, got {other:?}"),
        }
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn interleaved_deltas_and_complete_message_produce_one_final_text() {
        let adapter = successful_adapter();
        assert_eq!(adapter.partial_text().as_deref(), Some("Hello"));
        let exit = exit_with(Some(0), None, false);
        let outcome = adapter.terminal_outcome(&exit);
        match outcome {
            NativeTerminalOutcome::Completed {
                text,
                session_id,
                observed_model,
                observed_effort,
                ..
            } => {
                assert_eq!(text, "Hello", "complete message must not duplicate deltas");
                assert_eq!(session_id, "sess-1");
                assert_eq!(observed_model.as_deref(), Some("claude-sonnet-4-5"));
                assert_eq!(observed_effort, None);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        // result:null falls back to accumulated visible text.
        let mut adapter = ClaudeStreamAdapter::new();
        for line in [INIT, DELTA_HEL, DELTA_LO, MSG_HELLO, RESULT_OK_NULL] {
            push(&mut adapter, line);
        }
        match adapter.terminal_outcome(&exit) {
            NativeTerminalOutcome::Completed { text, .. } => assert_eq!(text, "Hello"),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_identical_frame_is_idempotent_but_repeated_deltas_concat() {
        let mut adapter = ClaudeStreamAdapter::new();
        assert!(!push(&mut adapter, INIT).is_empty());
        assert!(
            push(&mut adapter, INIT).is_empty(),
            "byte-identical repeat is a no-op"
        );
        // Identical consecutive deltas are legitimate and must concatenate.
        let delta: &str = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ab"}}}"#;
        assert!(matches!(
            push(&mut adapter, delta)[0],
            NativeEvent::TextDelta { .. }
        ));
        assert!(matches!(
            push(&mut adapter, delta)[0],
            NativeEvent::TextDelta { .. }
        ));
        assert_eq!(adapter.partial_text().as_deref(), Some("abab"));
    }

    #[test]
    fn correctness_unknowns_are_protocol_violations() {
        let mut adapter = ClaudeStreamAdapter::new();
        let cases: &[(&str, &str)] = &[
            ("{not json", "malformed_json"),
            (r#"{"no_type":1}"#, "missing_frame_type"),
            (r#"{"type":"system"}"#, "system_missing_subtype"),
            (
                r#"{"type":"system","subtype":"init","model":"m"}"#,
                "init_missing_session_id",
            ),
            (
                r#"{"type":"result","subtype":"success","is_error":false,"result":null,"usage":null}"#,
                "result_missing_session_id",
            ),
            (
                r#"{"type":"result","is_error":false,"result":null,"session_id":"s"}"#,
                "result_missing_subtype",
            ),
            (
                r#"{"type":"result","subtype":"success","result":null,"session_id":"s"}"#,
                "result_missing_is_error",
            ),
            (
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read"}]},"session_id":"s"}"#,
                "tool_use_missing_id",
            ),
            (
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1"}]},"session_id":"s"}"#,
                "tool_use_missing_name",
            ),
            (
                r#"{"type":"assistant","message":{"content":[{"type":"text"}]},"session_id":"s"}"#,
                "text_block_missing_text",
            ),
            (
                r#"{"type":"assistant","message":{"content":[{"no_type":1}]},"session_id":"s"}"#,
                "block_missing_type",
            ),
            (
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]},"session_id":"s"}"#,
                "tool_result_missing_id",
            ),
            (r#"{"type":"stream_event"}"#, "stream_event_missing_event"),
            (
                r#"{"type":"stream_event","event":{"type":"content_block_delta"}}"#,
                "delta_missing_object",
            ),
            (
                r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta"}}}"#,
                "text_delta_missing_text",
            ),
            (
                r#"{"type":"system","subtype":"api_retry","max_retries":3}"#,
                "api_retry_missing_attempt",
            ),
            (
                r#"{"type":"assistant","message":{"content":[]},"session_id":"s","parent_tool_use_id":7}"#,
                "parent_tool_use_id_type",
            ),
            (
                r#"{"type":"system","subtype":"status","status":7}"#,
                "system_status_type",
            ),
            (
                r#"{"type":"system","subtype":"compact_boundary","trigger":7}"#,
                "compact_boundary_trigger_type",
            ),
            (r#"{"type":"rate_limit_event"}"#, "rate_limit_shape"),
            (
                r#"{"type":"rate_limit_event","rate_limit":"allowed"}"#,
                "rate_limit_shape",
            ),
            (
                r#"{"type":"rate_limit_event","rate_limit":{"resetsAt":"soon"}}"#,
                "rate_limit_shape",
            ),
        ];
        for (line, code) in cases {
            let err = adapter
                .push_line(line.as_bytes())
                .expect_err("shape must fail explicitly");
            assert_eq!(err.code, *code, "unexpected code for {line}");
        }
        // A frame that is not valid UTF-8 fails as invalid_utf8.
        let err = adapter
            .push_line(&[0xff, 0xfe, 0x00])
            .expect_err("invalid UTF-8 must fail");
        assert_eq!(err.code, "invalid_utf8");
        // Conflicting second root result contradicts the first.
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        push(&mut adapter, RESULT_OK);
        let err = adapter
            .push_line(result_line("error_max_turns", true, None).as_bytes())
            .expect_err("conflicting root result must fail");
        assert_eq!(err.code, "conflicting_root_result");
        // A blank keep-alive line is tolerated.
        assert!(push(&mut ClaudeStreamAdapter::new(), "   ").is_empty());
    }

    /// Forward compatibility: unknown system subtypes, top-level types,
    /// assistant block types and result subtypes are tolerated silently and
    /// only counted in the bounded diagnostic record.
    #[test]
    fn unknown_vendor_shapes_are_tolerated_and_recorded() {
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        for line in [
            r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":50,"estimated_tokens_delta":50}"#,
            r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":90,"estimated_tokens_delta":40}"#,
            r#"{"type":"system","subtype":"telepathy"}"#,
            r#"{"type":"watermelon"}"#,
            r#"{"type":"system","subtype":"model_refusal_fallback"}"#,
        ] {
            assert!(push(&mut adapter, line).is_empty(), "{line}");
        }
        // Unknown block skipped, siblings still processed.
        let events = push(
            &mut adapter,
            r#"{"type":"assistant","message":{"content":[{"type":"hologram"},{"type":"text","text":"Hello"}]},"session_id":"sess-1","parent_tool_use_id":null}"#,
        );
        assert_eq!(
            events,
            vec![NativeEvent::MessageCompleted {
                text: "Hello".into()
            }]
        );
        let ignored = adapter.ignored_frames();
        assert_eq!(ignored.get("system:thinking_tokens"), Some(&2));
        assert_eq!(ignored.get("system:telepathy"), Some(&1));
        assert_eq!(ignored.get("frame:watermelon"), Some(&1));
        assert_eq!(ignored.get("system:model_refusal_fallback"), Some(&1));
        assert_eq!(ignored.get("block:hologram"), Some(&1));
        // Unknown result subtype: a root result mapped to Failed, and the
        // root result stays the only terminal authority.
        let events = push(
            &mut adapter,
            &result_line("error_something_new", true, Some("boom")),
        );
        assert!(!events.is_empty());
        assert!(adapter.saw_root_result());
        match adapter.terminal_outcome(&exit_with(Some(0), None, false)) {
            NativeTerminalOutcome::Failed { message, .. } => assert!(message.contains("boom")),
            other => panic!("unknown result subtype must fail, got {other:?}"),
        }
    }

    #[test]
    fn ignored_frame_record_is_bounded() {
        let mut adapter = ClaudeStreamAdapter::new();
        for i in 0..(MAX_IGNORED_KEYS + 10) {
            push(
                &mut adapter,
                &format!(r#"{{"type":"system","subtype":"novel_{i}"}}"#),
            );
        }
        assert_eq!(adapter.ignored_frames().len(), MAX_IGNORED_KEYS);
        assert_eq!(adapter.ignored_overflow(), 10);
    }

    #[test]
    fn additive_metadata_is_tolerated_and_ignored() {
        let mut adapter = ClaudeStreamAdapter::new();
        let lines = vec![
            r#"{"type":"prompt_suggestion","suggestion":"ask more"}"#,
            r#"{"type":"plugin_install","plugin":"x","status":"installed"}"#,
            r#"{"type":"system","subtype":"plugin_install","status":"done"}"#,
            r#"{"type":"system","subtype":"hook_started","hook":"pre"}"#,
            r#"{"type":"system","subtype":"hook_progress"}"#,
            r#"{"type":"system","subtype":"hook_response"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"brace"}}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"echo of input"}]},"session_id":"sess-1"}"#,
            r#"{"type":"user","message":{"content":"plain echo"},"session_id":"sess-1"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"known field on known type","vendor_private":{"x":1}}]},"session_id":"sess-1","parent_tool_use_id":null,"uuid":"abc","request_id":"req"}"#,
        ];
        for line in lines {
            push(&mut adapter, line);
        }
        // The only visible-text effect is the complete message with extra
        // unknown FIELDS, which are ignored per field not per frame.
        assert_eq!(
            adapter.partial_text().as_deref(),
            Some("known field on known type")
        );
    }

    #[test]
    fn missing_result_finishes_as_no_result_with_retained_partial_text() {
        let mut adapter = ClaudeStreamAdapter::new();
        for line in [INIT, DELTA_HEL, DELTA_LO] {
            push(&mut adapter, line);
        }
        match adapter.finish() {
            FinishDisposition::NoResult { partial_text } => {
                assert_eq!(partial_text.as_deref(), Some("Hello"))
            }
        }
        assert!(!adapter.saw_root_result());
        assert!(adapter.result_evidence().is_none());
        // An adapter that saw no text at all reports None, not "".
        assert_eq!(ClaudeStreamAdapter::new().partial_text(), None);
    }

    #[test]
    fn auth_quota_and_other_error_results_classify() {
        let cases: &[(&str, FailureClass)] = &[
            ("Please log in again to continue", FailureClass::NotSignedIn),
            ("oauth token exchange failed", FailureClass::NotSignedIn),
            ("Your token has expired", FailureClass::AuthExpired),
            ("insufficient credit balance", FailureClass::QuotaExhausted),
            (
                "quota exceeded for organization",
                FailureClass::QuotaExhausted,
            ),
            (
                "rate limit reached, retry later",
                FailureClass::QuotaExhausted,
            ),
            (
                "model claude-x is not allowed for this account",
                FailureClass::ModelNotEntitled,
            ),
            (
                "you do not have access to model claude-x",
                FailureClass::ModelNotEntitled,
            ),
            ("vendor exploded internally", FailureClass::Internal),
            (
                "Not logged in · Please run /login",
                FailureClass::NotSignedIn,
            ),
            (
                "Login expired · Please run /login",
                FailureClass::AuthExpired,
            ),
            (
                "OAuth token revoked · Please run /login",
                FailureClass::NotSignedIn,
            ),
            ("Usage limit reached", FailureClass::QuotaExhausted),
            ("You're out of extra usage", FailureClass::QuotaExhausted),
            ("spend limit reached", FailureClass::QuotaExhausted),
            ("You've hit your limit", FailureClass::QuotaExhausted),
            ("Credit balance is too low", FailureClass::QuotaExhausted),
            // Auth wording that merely mentions a limit is NOT quota.
            (
                "Login reached its time limit · Please run /login",
                FailureClass::NotSignedIn,
            ),
        ];
        for (text, expected) in cases {
            let mut adapter = ClaudeStreamAdapter::new();
            push(&mut adapter, INIT);
            push(
                &mut adapter,
                &result_line("error_during_execution", true, Some(text)),
            );
            match adapter.terminal_outcome(&exit_with(Some(1), None, false)) {
                NativeTerminalOutcome::Failed {
                    class,
                    partial_text,
                    ..
                } => {
                    assert_eq!(class, *expected, "text: {text}");
                    assert_eq!(partial_text, None);
                }
                other => panic!("expected Failed for {text}, got {other:?}"),
            }
        }
        // error_max_turns is the vendor turn-deadline family.
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        push(&mut adapter, &result_line("error_max_turns", true, None));
        match adapter.terminal_outcome(&exit_with(Some(1), None, false)) {
            NativeTerminalOutcome::Failed { class, message, .. } => {
                assert_eq!(class, FailureClass::TurnDeadline);
                assert!(message.contains("error_max_turns"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // success with is_error:true is still a failure.
        let mut adapter = ClaudeStreamAdapter::new();
        push(
            &mut adapter,
            &result_line("success", true, Some("quota exceeded")),
        );
        match adapter.terminal_outcome(&exit_with(Some(0), None, false)) {
            NativeTerminalOutcome::Failed { class, .. } => {
                assert_eq!(class, FailureClass::QuotaExhausted)
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn partial_success_then_failure_keeps_partial_text() {
        let mut adapter = ClaudeStreamAdapter::new();
        for line in [INIT, DELTA_HEL, DELTA_LO] {
            push(&mut adapter, line);
        }
        push(
            &mut adapter,
            &result_line(
                "error_during_execution",
                true,
                Some("provider quota exceeded"),
            ),
        );
        match adapter.terminal_outcome(&exit_with(Some(1), None, false)) {
            NativeTerminalOutcome::Failed {
                class,
                partial_text,
                ..
            } => {
                assert_eq!(class, FailureClass::QuotaExhausted);
                assert_eq!(partial_text.as_deref(), Some("Hello"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn child_events_keep_identity_and_never_overwrite_root_text() {
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        push(&mut adapter, MSG_HELLO);
        let child_text = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"child scratchpad output"},{"type":"thinking","thinking":"child hidden"}]},"session_id":"sess-1","parent_tool_use_id":"toolu_parent"}"#;
        assert!(
            push(&mut adapter, child_text).is_empty(),
            "child text must not surface as root text"
        );
        assert_eq!(adapter.partial_text().as_deref(), Some("Hello"));
        let events = push(&mut adapter, CHILD_TOOL_RESULT);
        match &events[0] {
            NativeEvent::ToolObservation {
                vendor_tool_use_id,
                phase,
                input,
                parent_tool_use_id,
                ..
            } => {
                assert_eq!(vendor_tool_use_id, "toolu_9");
                assert_eq!(*phase, VendorToolPhase::Result);
                assert!(input.is_none());
                assert_eq!(parent_tool_use_id.as_deref(), Some("toolu_parent"));
            }
            other => panic!("expected ToolObservation, got {other:?}"),
        }
        // Root observations carry None parent.
        let events = push(&mut adapter, TOOL_RESULT);
        match &events[0] {
            NativeEvent::ToolObservation {
                parent_tool_use_id, ..
            } => {
                assert_eq!(parent_tool_use_id, &None)
            }
            other => panic!("expected ToolObservation, got {other:?}"),
        }
        push(&mut adapter, RESULT_OK_NULL);
        match adapter.terminal_outcome(&exit_with(Some(0), None, false)) {
            NativeTerminalOutcome::Completed { text, .. } => assert_eq!(text, "Hello"),
            other => panic!("expected Completed with root text, got {other:?}"),
        }
    }

    #[test]
    fn tool_observations_are_data_only_never_dispatchable() {
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        let events = push(&mut adapter, TOOL_USE);
        assert_eq!(events.len(), 1);
        match &events[0] {
            NativeEvent::ToolObservation {
                vendor_tool_use_id,
                name,
                phase,
                input,
                parent_tool_use_id,
                is_error,
                result,
            } => {
                assert_eq!((is_error, result), (&None, &None));
                assert_eq!(vendor_tool_use_id, "toolu_1");
                assert_eq!(name, "Read");
                assert_eq!(*phase, VendorToolPhase::Requested);
                assert_eq!(input.as_ref().unwrap()["path"], "x");
                assert_eq!(parent_tool_use_id, &None);
            }
            other => panic!("ToolObservation expected, never a dispatchable request: {other:?}"),
        }
        let events = push(&mut adapter, TOOL_RESULT);
        // The result resolves the tool name from the earlier tool_use and
        // carries the (absent => false) error flag and its text.
        assert!(matches!(
            &events[0],
            NativeEvent::ToolObservation {
                phase: VendorToolPhase::Result,
                input: None,
                name,
                is_error: Some(false),
                result: Some(text),
                ..
            } if name == "Read" && text == "data"
        ));
    }

    #[test]
    fn tool_result_carries_error_flag_array_text_and_bounded_content() {
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        // Unknown id keeps the empty name; is_error true; string content.
        let denied = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"{\"denied\":true}","is_error":true,"tool_use_id":"toolu_x"}]},"parent_tool_use_id":null,"session_id":"sess-1"}"#;
        match &push(&mut adapter, denied)[0] {
            NativeEvent::ToolObservation {
                name,
                is_error,
                result,
                ..
            } => {
                assert_eq!(name, "");
                assert_eq!(*is_error, Some(true));
                assert_eq!(result.as_deref(), Some("{\"denied\":true}"));
            }
            other => panic!("{other:?}"),
        }
        // Array content: text parts joined with \n, non-text skipped.
        let arr = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_y","content":[{"type":"text","text":"a"},{"type":"image","source":{}},{"type":"text","text":"b"}]}]},"session_id":"sess-1"}"#;
        match &push(&mut adapter, arr)[0] {
            NativeEvent::ToolObservation { result, .. } => {
                assert_eq!(result.as_deref(), Some("a\nb"))
            }
            other => panic!("{other:?}"),
        }
        // No textual content => None.
        let none = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_z","content":[{"type":"image"}]}]},"session_id":"sess-1"}"#;
        match &push(&mut adapter, none)[0] {
            NativeEvent::ToolObservation { result, .. } => assert_eq!(*result, None),
            other => panic!("{other:?}"),
        }
        // Oversized content is clipped on a char boundary with a marker.
        let big = "é".repeat(MAX_TOOL_RESULT_BYTES);
        let line = serde_json::json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"toolu_big","content":big}]}})
        .to_string();
        match &push(&mut adapter, &line)[0] {
            NativeEvent::ToolObservation { result, .. } => {
                let text = result.as_deref().unwrap();
                assert!(text.ends_with(TOOL_RESULT_TRUNCATION_MARKER));
                assert!(text.len() <= MAX_TOOL_RESULT_BYTES + TOOL_RESULT_TRUNCATION_MARKER.len());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn init_mcp_servers_parse_tolerantly() {
        let mut adapter = ClaudeStreamAdapter::new();
        let line = r#"{"type":"system","subtype":"init","session_id":"s","mcp_servers":[{"name":"agentkit","status":"failed","source":"dynamic"},{"name":"nostatus"},7,{"status":"x"}]}"#;
        match &push(&mut adapter, line)[0] {
            NativeEvent::Init { mcp_servers, .. } => assert_eq!(
                mcp_servers,
                &vec![McpServerStatus {
                    name: "agentkit".into(),
                    status: "failed".into()
                }]
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn thinking_maps_to_reasoning_delta() {
        let mut adapter = ClaudeStreamAdapter::new();
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"considering","signature":"sig"}]},"session_id":"sess-1","parent_tool_use_id":null}"#;
        let events = push(&mut adapter, line);
        assert_eq!(
            events,
            vec![NativeEvent::ReasoningDelta {
                text: "considering".into()
            }]
        );
    }

    /// Review round-4 F9 (public SDK contract): a withheld redacted-thinking
    /// block inside an assistant message must never reject the message —
    /// the visible sibling text still surfaces, and the withheld block is
    /// carried only as an opaque marker.
    #[test]
    fn redacted_thinking_block_is_opaque_and_never_blocks_visible_text() {
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        let line = r#"{"type":"assistant","message":{"id":"msg_r","content":[{"type":"redacted_thinking","data":"AQoeb3BhcXVlLXBheWxvYWQ=","signature":null},{"type":"text","text":"visible answer"}]},"session_id":"sess-1","parent_tool_use_id":null}"#;
        let events = push(&mut adapter, line);
        assert_eq!(
            events,
            vec![NativeEvent::MessageCompleted {
                text: "visible answer".into()
            }],
            "the withheld block contributes no event of its own"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, NativeEvent::ReasoningDelta { .. })),
            "withheld reasoning must never surface as reasoning text"
        );
        assert_eq!(
            adapter.partial_text().as_deref(),
            Some("visible answer"),
            "the visible answer after the withheld block survives"
        );
        // Carried verbatim as an opaque marker: payload preserved untouched,
        // nothing displayed.
        assert_eq!(
            adapter.redacted_thinking_markers(),
            &[serde_json::json!({
                "type": "redacted_thinking",
                "data": "AQoeb3BhcXVlLXBheWxvYWQ=",
                "signature": null
            })]
        );
        assert_eq!(adapter.redacted_thinking_dropped(), 0);
        // The withheld block never becomes a terminal fact.
        assert!(!adapter.saw_root_result());

        // Opaque means opaque: a block with no payload at all, and one with
        // a payload of an arbitrary shape, are both accepted.
        let mut adapter = ClaudeStreamAdapter::new();
        let bare = r#"{"type":"assistant","message":{"content":[{"type":"redacted_thinking"},{"type":"text","text":"still answered"}]},"session_id":"sess-1","parent_tool_use_id":null}"#;
        let events = push(&mut adapter, bare);
        assert_eq!(
            events,
            vec![NativeEvent::MessageCompleted {
                text: "still answered".into()
            }]
        );
        assert_eq!(
            adapter.redacted_thinking_markers(),
            &[serde_json::json!({"type": "redacted_thinking"})]
        );
        let weird = r#"{"type":"assistant","message":{"content":[{"type":"redacted_thinking","data":[1,"two",null]}]},"session_id":"sess-1","parent_tool_use_id":null}"#;
        assert!(
            push(&mut adapter, weird).is_empty(),
            "a withheld block is not content"
        );
        assert_eq!(
            adapter.redacted_thinking_markers()[1],
            serde_json::json!({"type": "redacted_thinking", "data": [1, "two", null]})
        );
        assert_eq!(
            adapter.partial_text().as_deref(),
            Some("still answered"),
            "the opaque marker never enters the text accumulator"
        );

        // Child-scoped withheld blocks never enter the root marker list.
        let child = r#"{"type":"assistant","message":{"content":[{"type":"redacted_thinking","data":"x"}]},"session_id":"sess-1","parent_tool_use_id":"toolu_parent"}"#;
        assert!(push(&mut adapter, child).is_empty());
        assert_eq!(adapter.redacted_thinking_markers().len(), 2);
    }

    /// Review round-4 F9: the documented `model_refusal_no_fallback` system
    /// notification is informational — it never errors the stream and never
    /// becomes a terminal outcome; the root `result` frame stays the
    /// authority.
    #[test]
    fn model_refusal_no_fallback_is_informational_until_the_result() {
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        let refusal = r#"{"type":"system","subtype":"model_refusal_no_fallback","model":"claude-sonnet-4-5","session_id":"sess-1"}"#;
        let events = adapter
            .push_line(refusal.as_bytes())
            .expect("documented refusal notification is never a parse error");
        assert_eq!(
            events,
            vec![NativeEvent::Warning {
                code: "model_refusal_no_fallback".to_owned(),
                message: "vendor reported a model refusal with no fallback model; \
                          the root result frame remains the terminal authority"
                    .to_owned(),
            }],
            "mirrors the non-fatal vendor-notice shape"
        );
        // No terminal authority: a clean exit with only progress frames
        // stays Uncertain, and no result was observed.
        assert!(!adapter.saw_root_result());
        match adapter.terminal_outcome(&exit_with(Some(0), None, false)) {
            NativeTerminalOutcome::Uncertain { .. } => {}
            other => panic!("a refusal notice must not complete a turn: {other:?}"),
        }
        // The authoritative result frame still decides: refusal notice
        // followed by a valid success completes normally.
        for line in [DELTA_HEL, DELTA_LO, MSG_HELLO, RESULT_OK] {
            push(&mut adapter, line);
        }
        match adapter.terminal_outcome(&exit_with(Some(0), None, false)) {
            NativeTerminalOutcome::Completed { text, .. } => assert_eq!(text, "Hello"),
            other => panic!("expected Completed after the refusal notice, got {other:?}"),
        }
        // A refusal followed by an error result fails on the result's facts
        // alone.
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        push(&mut adapter, refusal);
        push(&mut adapter, DELTA_HEL);
        push(
            &mut adapter,
            &result_line("error_during_execution", true, Some("model refused")),
        );
        match adapter.terminal_outcome(&exit_with(Some(1), None, false)) {
            NativeTerminalOutcome::Failed { class, .. } => {
                assert_eq!(class, FailureClass::Internal)
            }
            other => panic!("the result frame stays authoritative, got {other:?}"),
        }
        // `model_refusal_fallback` is undocumented: it now falls under the
        // generic unknown-subtype tolerance (no event, no violation).
        assert!(
            push(
                &mut adapter,
                r#"{"type":"system","subtype":"model_refusal_fallback"}"#
            )
            .is_empty()
        );
    }

    #[test]
    fn usage_is_per_turn_estimated_and_billing_unknown() {
        let adapter = successful_adapter();
        let evidence = adapter.result_evidence().expect("root result observed");
        assert!(adapter.saw_root_result());
        assert_eq!(evidence.subtype, "success");
        assert!(!evidence.is_error);
        assert_eq!(evidence.session_id, "sess-1");
        assert_eq!(
            evidence.observed_model.as_deref(),
            Some("claude-sonnet-4-5")
        );
        let usage = evidence.usage.expect("usage present");
        let tokens = usage.per_turn.expect("per-turn tokens");
        assert_eq!(
            tokens,
            TokenSet {
                input_tokens: Some(11),
                output_tokens: Some(5),
                cache_read_tokens: Some(3),
                cache_write_tokens: Some(2),
            }
        );
        assert_eq!(usage.cumulative, None, "cumulative unknown, never guessed");
        assert_eq!(usage.api_equivalent_estimate_usd, Some(0.004));
        assert_eq!(usage.estimate_provenance, UsageProvenance::Estimated);
        assert_eq!(usage.billing_inclusion, BillingInclusion::Unknown);
        // No cost figure => provenance Unknown (unknown is not zero).
        let mut adapter = ClaudeStreamAdapter::new();
        push(
            &mut adapter,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"s","usage":{"input_tokens":1},"total_cost_usd":null}"#,
        );
        let usage = adapter.result_evidence().unwrap().usage.unwrap();
        assert_eq!(usage.estimate_provenance, UsageProvenance::Unknown);
        assert_eq!(usage.api_equivalent_estimate_usd, None);
        assert_eq!(usage.per_turn.unwrap().output_tokens, None);
        // usage:null => evidence.usage None and Completed.usage None.
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, RESULT_OK_NULL);
        assert!(adapter.result_evidence().unwrap().usage.is_none());
        match adapter.terminal_outcome(&exit_with(Some(0), None, false)) {
            NativeTerminalOutcome::Completed { usage, .. } => assert!(usage.is_none()),
            other => panic!("expected Completed, got {other:?}"),
        }
        // The Usage event fires exactly once, from the result frame.
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        let events = push(&mut adapter, RESULT_OK);
        let usage_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, NativeEvent::Usage(_)))
            .collect();
        assert_eq!(usage_events.len(), 1);
    }

    #[test]
    fn permission_denials_collected_from_all_shapes() {
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"partial","session_id":"sess-1","usage":null,"permission_denials":["Bash",{"tool_name":"Write"},{"toolName":"Edit"},{"note":"nameless"},42,null]}"#;
        let events = push(&mut adapter, line);
        let evidence = adapter.result_evidence().unwrap();
        assert_eq!(
            evidence.permission_denials,
            vec!["Bash".to_owned(), "Write".to_owned(), "Edit".to_owned()]
        );
        let denied: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                NativeEvent::PermissionDenied { tool_name } => Some(tool_name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(denied, vec!["Bash", "Write", "Edit"]);
        match adapter.terminal_outcome(&exit_with(Some(0), None, false)) {
            NativeTerminalOutcome::Completed {
                permission_denials, ..
            } => {
                assert_eq!(permission_denials, evidence.permission_denials);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        // The dedicated permission_denied system frame also maps, with the
        // "unknown" fallback when the vendor omits tool_name.
        let mut adapter = ClaudeStreamAdapter::new();
        let events = push(
            &mut adapter,
            r#"{"type":"system","subtype":"permission_denied","tool_name":"WebFetch"}"#,
        );
        assert_eq!(
            events,
            vec![NativeEvent::PermissionDenied {
                tool_name: "WebFetch".into()
            }]
        );
        let events = push(
            &mut adapter,
            r#"{"type":"system","subtype":"permission_denied"}"#,
        );
        assert_eq!(
            events,
            vec![NativeEvent::PermissionDenied {
                tool_name: "unknown".into()
            }]
        );
    }

    #[test]
    fn completed_requires_root_result_and_exit0_never_completes() {
        let exit = exit_with(Some(0), None, false);
        // Full-looking successful stream WITHOUT the root result frame.
        let mut adapter = ClaudeStreamAdapter::new();
        for line in [INIT, DELTA_HEL, DELTA_LO, MSG_HELLO] {
            push(&mut adapter, line);
        }
        match adapter.terminal_outcome(&exit) {
            NativeTerminalOutcome::Uncertain {
                class,
                detail,
                partial_text,
            } => {
                assert_eq!(class, FailureClass::ExitWithoutResult);
                assert_eq!(detail, "clean exit without root result");
                assert_eq!(partial_text.as_deref(), Some("Hello"));
            }
            other => panic!("exit-0 without result must be Uncertain, got {other:?}"),
        }
        // The stateless helper agrees (never produces Completed).
        let observed = [NativeEvent::TextDelta {
            text: "Hello".into(),
        }];
        match claude_terminal_outcome(&exit, &observed, Some("Hello")) {
            NativeTerminalOutcome::Uncertain { .. } => {}
            other => panic!("free helper must never complete without result, got {other:?}"),
        }
        // With the result, both entry points complete identically.
        let adapter = successful_adapter();
        assert!(matches!(
            adapter.terminal_outcome(&exit),
            NativeTerminalOutcome::Completed { .. }
        ));
        let evidence = adapter.result_evidence().unwrap();
        let via_harness =
            claude_terminal_outcome_with_result(&exit, Some(&evidence), &observed, Some("Hello"));
        assert_eq!(via_harness, adapter.terminal_outcome(&exit));
        // No result at all: the free helper keeps partial and reports exit facts.
        match claude_terminal_outcome(&exit, &[], None) {
            NativeTerminalOutcome::Uncertain { partial_text, .. } => {
                assert_eq!(partial_text, None)
            }
            other => panic!("expected Uncertain, got {other:?}"),
        }
    }

    #[test]
    fn deferred_paths_never_complete() {
        // Deferred root subtype.
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        push(&mut adapter, MSG_HELLO);
        push(
            &mut adapter,
            &result_line("deferred", false, Some("delegated")),
        );
        match adapter.terminal_outcome(&exit_with(Some(0), None, false)) {
            NativeTerminalOutcome::DeferredBlocked { partial_text, .. } => {
                assert_eq!(partial_text.as_deref(), Some("Hello"));
            }
            other => panic!("deferred must block, got {other:?}"),
        }
        // Child-scoped result marker with no root result at all.
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        push(&mut adapter, DELTA_HEL);
        let child_result = r#"{"type":"result","subtype":"success","is_error":false,"result":"child work","session_id":"sess-1","parent_tool_use_id":"toolu_parent"}"#;
        push(&mut adapter, child_result);
        assert!(
            !adapter.saw_root_result(),
            "child marker is not a root result"
        );
        match adapter.terminal_outcome(&exit_with(Some(0), None, false)) {
            NativeTerminalOutcome::DeferredBlocked {
                reason,
                partial_text,
            } => {
                assert!(reason.contains("toolu_parent"));
                assert_eq!(partial_text.as_deref(), Some("Hel"));
            }
            other => panic!("child result marker must block, got {other:?}"),
        }
    }

    #[test]
    fn exit_rows_follow_the_decision_table() {
        let observed: [NativeEvent; 0] = [];
        // Graceful interrupt acknowledged (SIGINT path).
        match claude_terminal_outcome(&exit_with(None, Some(2), true), &observed, Some("mid")) {
            NativeTerminalOutcome::Interrupted { partial_text } => {
                assert_eq!(partial_text.as_deref(), Some("mid"))
            }
            other => panic!("expected Interrupted, got {other:?}"),
        }
        // SIGTERM after an acknowledged interrupt (exit convention 143).
        assert!(matches!(
            claude_terminal_outcome(&exit_with(Some(143), None, true), &observed, None),
            NativeTerminalOutcome::Interrupted { .. }
        ));
        // Live: `claude -p` answers a user Stop (SIGINT) with a clean
        // exit 0 and no result frame; with a requested interrupt that is
        // Interrupted, also for 130 and signal 9 / 15.
        for exit in [
            exit_with(Some(0), None, true),
            exit_with(Some(130), None, true),
            exit_with(None, Some(15), true),
            exit_with(None, Some(9), true),
        ] {
            assert!(
                matches!(
                    claude_terminal_outcome(&exit, &observed, None),
                    NativeTerminalOutcome::Interrupted { .. }
                ),
                "{exit:?}"
            );
        }
        // Exit 0 WITHOUT an interrupt request stays Uncertain; an unreaped
        // child (neither code nor signal) is not Interrupted either.
        assert!(matches!(
            claude_terminal_outcome(&exit_with(Some(0), None, false), &observed, None),
            NativeTerminalOutcome::Uncertain { .. }
        ));
        assert!(!matches!(
            claude_terminal_outcome(&exit_with(None, None, true), &observed, None),
            NativeTerminalOutcome::Interrupted { .. }
        ));
        // A root result always wins over the exit rows.
        let adapter = successful_adapter();
        assert!(matches!(
            adapter.terminal_outcome(&exit_with(Some(0), None, true)),
            NativeTerminalOutcome::Completed { .. }
        ));
        // SIGTERM from outside, no interrupt request: uncertain, retryable family.
        match claude_terminal_outcome(&exit_with(Some(143), None, false), &observed, None) {
            NativeTerminalOutcome::Uncertain { class, detail, .. } => {
                assert_eq!(class, FailureClass::ProcessLost);
                assert_eq!(detail, "SIGTERM from outside without result");
                assert!(FailureClass::ProcessLost.retryable());
            }
            other => panic!("expected Uncertain, got {other:?}"),
        }
        // Crash signal without result: definitively lost process.
        match claude_terminal_outcome(&exit_with(None, Some(9), false), &observed, None) {
            NativeTerminalOutcome::Failed { class, .. } => {
                assert_eq!(class, FailureClass::ProcessLost)
            }
            other => panic!("expected Failed/ProcessLost, got {other:?}"),
        }
        // Nonzero exit without result: exit-without-result family.
        match claude_terminal_outcome(&exit_with(Some(1), None, false), &observed, None) {
            NativeTerminalOutcome::Failed { class, message, .. } => {
                assert_eq!(class, FailureClass::ExitWithoutResult);
                assert!(message.contains("exit_code=Some(1)"));
            }
            other => panic!("expected Failed/ExitWithoutResult, got {other:?}"),
        }
        // Denials observed without a result are reported in the failure
        // message so the uncertainty is visible, never silent.
        let with_denial = [NativeEvent::PermissionDenied {
            tool_name: "Bash".into(),
        }];
        match claude_terminal_outcome(&exit_with(Some(1), None, false), &with_denial, None) {
            NativeTerminalOutcome::Failed { message, .. } => {
                assert!(message.contains("1 permission denial(s) observed"))
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn api_retry_maps_error_categories_without_claiming_failure() {
        let mut adapter = ClaudeStreamAdapter::new();
        let cases: &[(&str, Option<FailureClass>)] = &[
            (
                r#"{"type":"system","subtype":"api_retry","attempt":2,"max_retries":5,"error":"rate_limit"}"#,
                None,
            ),
            (
                r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":5,"error":"overloaded_server"}"#,
                None,
            ),
            (
                r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":3,"error":"authentication_failed"}"#,
                Some(FailureClass::AuthExpired),
            ),
            (
                r#"{"type":"system","subtype":"api_retry","attempt":3,"max_retries":3,"error":"billing_error"}"#,
                Some(FailureClass::BillingBlocked),
            ),
            (
                r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":3}"#,
                None,
            ),
        ];
        for (line, expected) in cases {
            let events = push(&mut adapter, line);
            match &events[0] {
                NativeEvent::Retry {
                    attempt,
                    max_retries,
                    error_class,
                } => {
                    assert!(*attempt >= 1 && *max_retries >= 3);
                    assert_eq!(error_class, expected, "line: {line}");
                }
                other => panic!("expected Retry, got {other:?}"),
            }
        }
    }

    #[test]
    fn input_frame_is_one_ndjson_line_or_refused() {
        let bytes = claude_input_frame("hello world").expect("frame");
        assert_eq!(
            bytes,
            b"{\"type\":\"user\",\"client_composed\":true,\"message\":{\"role\":\"user\",\"content\":\"hello world\"}}\n"
        );
        assert!(
            bytes
                .windows(b"\"client_composed\":true".len())
                .any(|w| w == b"\"client_composed\":true"),
            "literal-input marker present verbatim (snake_case wire field)"
        );
        // Newlines inside content are JSON-escaped, never injected raw.
        let bytes = claude_input_frame("line one\nline two\r\n\ntab\there").expect("frame");
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert!(!bytes[..bytes.len() - 1].contains(&b'\n'));
        assert!(!bytes[..bytes.len() - 1].contains(&b'\r'));
        // Valid UTF-8 round-trips through JSON unchanged.
        let parsed: serde_json::Value =
            serde_json::from_slice(&bytes[..bytes.len() - 1]).expect("valid JSON");
        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["message"]["role"], "user");
        assert_eq!(
            parsed["message"]["content"],
            "line one\nline two\r\n\ntab\there"
        );
        // Empty / whitespace-only input is refused, never spawned.
        for empty in ["", "   ", "\n\t "] {
            let refusal = claude_input_frame(empty).expect_err("must refuse");
            assert_eq!(refusal.code, "empty_input");
            assert!(!refusal.retryable);
        }
    }

    #[test]
    fn rate_limit_and_status_and_compact_boundary_are_tolerated_progress() {
        let mut adapter = ClaudeStreamAdapter::new();
        // Public Agent SDK shape (`SDKRateLimitEvent`): `rate_limit_info`
        // with numeric `resetsAt` Unix seconds, utilization and limit type.
        let events = push(
            &mut adapter,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1762592400,"utilization":0.42,"rateLimitType":"weekly"},"uuid":"u-1","session_id":"sess-1"}"#,
        );
        assert_eq!(
            events,
            vec![NativeEvent::RateLimit {
                status: "allowed_warning".into(),
                resets_at: Some(1762592400.0),
                utilization: Some(0.42),
                rate_limit_type: Some("weekly".into()),
            }]
        );
        // Legacy `rate_limit` envelope with a string-encoded numeric
        // reset: the same timestamp survives, never dropped.
        let events = push(
            &mut adapter,
            r#"{"type":"rate_limit_event","rate_limit":{"status":"allowed","resetsAt":"1762592400","utilization":0.42},"session_id":"sess-1"}"#,
        );
        assert_eq!(
            events,
            vec![NativeEvent::RateLimit {
                status: "allowed".into(),
                resets_at: Some(1762592400.0),
                utilization: Some(0.42),
                rate_limit_type: None,
            }]
        );
        // Minimal shape: status only.
        let events = push(
            &mut adapter,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}}"#,
        );
        assert_eq!(
            events,
            vec![NativeEvent::RateLimit {
                status: "rejected".into(),
                resets_at: None,
                utilization: None,
                rate_limit_type: None,
            }]
        );
        // system/status: documented "compacting" and the null clear.
        let events = push(
            &mut adapter,
            r#"{"type":"system","subtype":"status","status":"compacting"}"#,
        );
        assert_eq!(
            events,
            vec![NativeEvent::Status {
                status: Some("compacting".into())
            }]
        );
        let events = push(
            &mut adapter,
            r#"{"type":"system","subtype":"status","status":null}"#,
        );
        assert_eq!(events, vec![NativeEvent::Status { status: None }]);
        let events = push(&mut adapter, r#"{"type":"system","subtype":"status"}"#);
        assert_eq!(events, vec![NativeEvent::Status { status: None }]);
        // system/compact_boundary, with and without trigger.
        let events = push(
            &mut adapter,
            r#"{"type":"system","subtype":"compact_boundary","trigger":"auto"}"#,
        );
        assert_eq!(
            events,
            vec![NativeEvent::CompactBoundary {
                trigger: Some("auto".into())
            }]
        );
        let events = push(
            &mut adapter,
            r#"{"type":"system","subtype":"compact_boundary"}"#,
        );
        assert_eq!(events, vec![NativeEvent::CompactBoundary { trigger: None }]);
        // Never terminal-affecting: with no root result, the progress
        // frames alone still yield Uncertain on a clean exit…
        match adapter.terminal_outcome(&exit_with(Some(0), None, false)) {
            NativeTerminalOutcome::Uncertain { .. } => {}
            other => panic!("progress frames must not complete a turn: {other:?}"),
        }
        // …and a full successful stream keeps completing identically.
        let mut adapter = ClaudeStreamAdapter::new();
        for line in [INIT, DELTA_HEL, DELTA_LO, MSG_HELLO] {
            push(&mut adapter, line);
        }
        for line in [
            r#"{"type":"rate_limit_event","rate_limit":{"status":"allowed"}}"#,
            r#"{"type":"system","subtype":"status","status":"compacting"}"#,
            r#"{"type":"system","subtype":"compact_boundary","trigger":"auto"}"#,
            RESULT_OK,
        ] {
            push(&mut adapter, line);
        }
        match adapter.terminal_outcome(&exit_with(Some(0), None, false)) {
            NativeTerminalOutcome::Completed { text, .. } => assert_eq!(text, "Hello"),
            other => panic!("expected Completed, got {other:?}"),
        }
        // Unknown subtypes are tolerated (typo included), but never
        // produce events.
        assert!(
            push(
                &mut adapter,
                r#"{"type":"system","subtype":"compact_boundry"}"#
            )
            .is_empty()
        );
    }

    #[test]
    fn complete_root_blocks_and_messages_accumulate() {
        // Two complete text blocks of ONE message: both appear, joined.
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        let two_blocks = r#"{"type":"assistant","message":{"id":"msg_1","content":[{"type":"text","text":"first block"},{"type":"tool_use","id":"t1","name":"Read","input":{}},{"type":"text","text":"second block"}]},"session_id":"sess-1","parent_tool_use_id":null}"#;
        let events = push(&mut adapter, two_blocks);
        let completed: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, NativeEvent::MessageCompleted { .. }))
            .collect();
        assert_eq!(completed.len(), 2, "both text blocks surface");
        assert_eq!(
            adapter.partial_text().as_deref(),
            Some("first block\n\nsecond block")
        );
        // A SECOND root message: appended, never overwriting the first.
        let second = r#"{"type":"assistant","message":{"id":"msg_2","content":[{"type":"text","text":"follow-up"}]},"session_id":"sess-1","parent_tool_use_id":null}"#;
        push(&mut adapter, second);
        assert_eq!(
            adapter.partial_text().as_deref(),
            Some("first block\n\nsecond block\n\nfollow-up")
        );
        // A complete block supersedes the incomplete deltas of its own
        // message (same id) without duplicating them.
        let mut adapter = ClaudeStreamAdapter::new();
        let d1 = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"},"message":{"id":"msg_9"}},"session_id":"sess-1"}"#;
        let d2 = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"},"message":{"id":"msg_9"}},"session_id":"sess-1"}"#;
        push(&mut adapter, d1);
        push(&mut adapter, d2);
        assert_eq!(adapter.partial_text().as_deref(), Some("Hello"));
        let complete = r#"{"type":"assistant","message":{"id":"msg_9","content":[{"type":"text","text":"Hello there"}]},"session_id":"sess-1","parent_tool_use_id":null}"#;
        push(&mut adapter, complete);
        assert_eq!(adapter.partial_text().as_deref(), Some("Hello there"));
    }

    #[test]
    fn child_stream_deltas_never_touch_root_accumulators() {
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        push(&mut adapter, DELTA_HEL);
        let child_delta = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"SECRET-CHILD-OUTPUT"}},"parent_tool_use_id":"toolu_parent","session_id":"sess-1"}"#;
        assert!(
            push(&mut adapter, child_delta).is_empty(),
            "child-scoped delta emits no root event"
        );
        assert_eq!(
            adapter.partial_text().as_deref(),
            Some("Hel"),
            "child delta must not pollute root deltas"
        );
        // Child complete text likewise never overwrites the root text.
        let child_msg = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"CHILD"}]},"session_id":"sess-1","parent_tool_use_id":"toolu_parent"}"#;
        assert!(push(&mut adapter, child_msg).is_empty());
        assert_eq!(adapter.partial_text().as_deref(), Some("Hel"));
        // A child delta alone produces no root text at all.
        let mut fresh = ClaudeStreamAdapter::new();
        push(&mut fresh, child_delta);
        assert_eq!(fresh.partial_text(), None);
    }

    #[test]
    fn tool_progress_heartbeats_are_nonterminal_metadata() {
        // Documented top-level tool-progress frame (including the ~30 s
        // foreground heartbeat): accepted, never terminal, no root events.
        let mut adapter = ClaudeStreamAdapter::new();
        for line in [INIT, DELTA_HEL, DELTA_LO, MSG_HELLO] {
            push(&mut adapter, line);
        }
        for _ in 0..6 {
            let events = push(
                &mut adapter,
                r#"{"type":"tool_progress","tool_use_id":"toolu_bash_1","tool_name":"Bash","parent_tool_use_id":null,"elapsed_time_seconds":30.5,"uuid":"hb","session_id":"sess-1"}"#,
            );
            assert!(events.is_empty(), "heartbeat emits no root event");
        }
        // Child-scoped heartbeat: tolerated too (parent identity checked).
        let events = push(
            &mut adapter,
            r#"{"type":"tool_progress","tool_use_id":"toolu_bash_2","tool_name":"Bash","parent_tool_use_id":"toolu_parent","elapsed_time_seconds":30.0,"session_id":"sess-1"}"#,
        );
        assert!(events.is_empty());
        // System-subtype form as well.
        let events = push(
            &mut adapter,
            r#"{"type":"system","subtype":"tool_progress","tool_use_id":"toolu_bash_1","tool_name":"Bash","elapsed_time_seconds":31.0,"session_id":"sess-1"}"#,
        );
        assert!(events.is_empty());
        push(&mut adapter, RESULT_OK);
        match adapter.terminal_outcome(&exit_with(Some(0), None, false)) {
            NativeTerminalOutcome::Completed { text, .. } => assert_eq!(text, "Hello"),
            other => panic!("heartbeats must not disturb completion: {other:?}"),
        }
        // A malformed parent identity is still a violation.
        let mut adapter = ClaudeStreamAdapter::new();
        let err = adapter
            .push_line(
                r#"{"type":"tool_progress","tool_use_id":"t","tool_name":"Bash","parent_tool_use_id":7}"#
                    .as_bytes(),
            )
            .expect_err("typed parent still enforced");
        assert_eq!(err.code, "parent_tool_use_id_type");
    }

    #[test]
    fn progress_values_malformed_warn_but_never_terminate() {
        let mut adapter = ClaudeStreamAdapter::new();
        let events = push(
            &mut adapter,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":"soon","utilization":"full","rateLimitType":7}}"#,
        );
        assert!(matches!(
            &events[0],
            NativeEvent::RateLimit { status, resets_at: None, utilization: None, rate_limit_type: None }
                if status == "allowed"
        ));
        assert!(events[1..].iter().all(|e| matches!(
            e,
            NativeEvent::Warning { code, .. } if code == "rate_limit_shape"
        )));
        assert!(
            events.len() >= 2,
            "malformed values are visible as warnings"
        );
    }

    #[test]
    fn partial_deltas_after_a_completed_block_are_retained() {
        // Completed message, then a second message still streaming when
        // the turn is cut short: the completed text AND the in-flight
        // partial survive, in order, without duplication.
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        push(&mut adapter, MSG_HELLO);
        let d1 = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Sec"},"message":{"id":"msg_2"}},"session_id":"sess-1"}"#;
        let d2 = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ond part"},"message":{"id":"msg_2"}},"session_id":"sess-1"}"#;
        push(&mut adapter, d1);
        push(&mut adapter, d2);
        assert_eq!(
            adapter.partial_text().as_deref(),
            Some("Hello\n\nSecond part")
        );
        // Interrupted without the second message's complete block.
        match adapter.terminal_outcome(&exit_with(None, Some(2), true)) {
            NativeTerminalOutcome::Interrupted { partial_text } => {
                assert_eq!(partial_text.as_deref(), Some("Hello\n\nSecond part"))
            }
            other => panic!("expected Interrupted with retained partial, got {other:?}"),
        }
        // When the complete block does arrive, no duplication occurs.
        let second = r#"{"type":"assistant","message":{"id":"msg_2","content":[{"type":"text","text":"Second part"}]},"session_id":"sess-1","parent_tool_use_id":null}"#;
        push(&mut adapter, second);
        assert_eq!(
            adapter.partial_text().as_deref(),
            Some("Hello\n\nSecond part")
        );
    }

    #[test]
    fn error_result_details_are_preserved_and_classified() {
        // An error result with errors[] details but no result text: the
        // details reach the user-visible failure, not just the subtype.
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        push(
            &mut adapter,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":null,"session_id":"sess-1","errors":["upstream model overloaded (529)","retry budget exhausted"]}"#,
        );
        match adapter.terminal_outcome(&exit_with(Some(1), None, false)) {
            NativeTerminalOutcome::Failed { class, message, .. } => {
                assert_eq!(class, FailureClass::Internal);
                assert!(
                    message.contains("upstream model overloaded (529)")
                        && message.contains("retry budget exhausted"),
                    "vendor details preserved: {message}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // errors[] objects contribute their message-ish field, and their
        // text participates in classification (529-style overload is not
        // magic — but a quota detail maps to the quota family).
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        push(
            &mut adapter,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":null,"session_id":"sess-1","errors":[{"type":"quota","message":"credit balance depleted"}]}"#,
        );
        match adapter.terminal_outcome(&exit_with(Some(1), None, false)) {
            NativeTerminalOutcome::Failed { class, message, .. } => {
                assert_eq!(class, FailureClass::QuotaExhausted);
                assert!(message.contains("credit balance depleted"), "{message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // Evidence keeps the details verbatim for downstream diagnostics.
        let mut adapter = ClaudeStreamAdapter::new();
        push(
            &mut adapter,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"top","session_id":"s","errors":["a","b"]}"#,
        );
        assert_eq!(
            adapter.result_evidence().unwrap().errors,
            vec!["a".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    fn error_result_details_are_bounded_with_a_visible_omission_note() {
        let errors: Vec<String> = (1..=12).map(|n| format!("detail-{n:02}")).collect();
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        push(
            &mut adapter,
            &serde_json::json!({
                "type": "result",
                "subtype": "error_during_execution",
                "is_error": true,
                "result": null,
                "session_id": "sess-1",
                "errors": errors,
            })
            .to_string(),
        );
        let evidence = adapter.result_evidence().expect("root result");
        assert_eq!(evidence.errors.len(), MAX_ERROR_DETAILS + 1);
        assert_eq!(evidence.errors[0], "detail-01");
        assert_eq!(evidence.errors[MAX_ERROR_DETAILS - 1], "detail-08");
        assert_eq!(
            evidence.errors[MAX_ERROR_DETAILS], "… 4 further vendor error detail(s) omitted",
            "the cap is visible, never silent"
        );
        match adapter.terminal_outcome(&exit_with(Some(1), None, false)) {
            NativeTerminalOutcome::Failed { message, .. } => {
                assert!(
                    message.contains("detail-01") && message.contains("detail-08"),
                    "{message}"
                );
                assert!(
                    message.contains("4 further vendor error detail(s) omitted"),
                    "{message}"
                );
                assert!(
                    !message.contains("detail-09"),
                    "omitted rows stay omitted: {message}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn error_result_details_are_clipped_and_secret_redacted() {
        let long = "x".repeat(5_000);
        let mut adapter = ClaudeStreamAdapter::new();
        push(&mut adapter, INIT);
        push(
            &mut adapter,
            &serde_json::json!({
                "type": "result",
                "subtype": "error_during_execution",
                "is_error": true,
                "result": null,
                "session_id": "sess-1",
                "errors": [
                    long,
                    "invalid credential sk-ant-api03-SECRETVALUE1234567890 rejected",
                    "Authorization: Bearer abcdefghIJKLMNop was refused",
                    "the task-force bearer of token said nothing secret",
                ],
            })
            .to_string(),
        );
        let evidence = adapter.result_evidence().expect("root result");
        // Clip: capped length, explicit ellipsis, never a split character.
        assert!(evidence.errors[0].chars().count() <= MAX_ERROR_DETAIL_CHARS + 1);
        assert!(evidence.errors[0].ends_with('…'));
        // Redact: the credential-shaped runs are gone, context survives.
        assert!(
            evidence.errors[1].contains("[redacted]"),
            "{:?}",
            evidence.errors[1]
        );
        assert!(
            !evidence.errors[1].contains("SECRETVALUE"),
            "credential leaked"
        );
        assert!(evidence.errors[1].contains("invalid credential"));
        assert!(evidence.errors[1].contains("rejected"));
        assert!(
            evidence.errors[2].contains("[redacted]"),
            "{:?}",
            evidence.errors[2]
        );
        assert!(
            !evidence.errors[2].contains("abcdefghIJKLMNop"),
            "bearer token leaked"
        );
        // Prose survives: "task-force" is not a credential, the short
        // "bearer of" run is below the credible-length threshold.
        assert!(
            evidence.errors[3].contains("task-force"),
            "{:?}",
            evidence.errors[3]
        );
        assert!(
            !evidence.errors[3].contains("[redacted]"),
            "{:?}",
            evidence.errors[3]
        );
    }

    #[test]
    fn qualified_min_version_is_the_permission_prompts_floor() {
        assert_eq!(QUALIFIED_MIN_VERSION, (2, 1, 259));
    }
}
