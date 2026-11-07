//! Public structured-stream adapter and event normalization (AGENTKIT-111).
//!
//! Decodes official Claude Code `--output-format stream-json` NDJSON frames
//! into [`NativeEvent`]s and owns the single authoritative terminal-outcome
//! decision table (AGENTKIT-110/111/116). Correctness-affecting unknown
//! shapes fail explicitly with [`ProtocolViolation`]; documented additive
//! metadata is tolerated and ignored.
//!
//! Vendor frame shapes accepted here (official docs, qualified versions):
//! * `{"type":"system","subtype":"init","session_id":S,"model":S,"tools":[..],
//!    "mcp_servers":[..],"capabilities":[S]?,"claude_code_version":S?}`
//! * `{"type":"assistant","message":{"content":[{"type":"text","text":S} |
//!    {"type":"tool_use","id":S,"name":S,"input":V} |
//!    {"type":"thinking","thinking":S}]},"session_id":S,
//!    "parent_tool_use_id":S|null}`
//! * `{"type":"user","message":{"content":[{"type":"tool_result",
//!    "tool_use_id":S,..}]},"session_id":S,"parent_tool_use_id":S?}`
//! * `{"type":"stream_event","event":{"type":"content_block_delta",
//!    "delta":{"type":"text_delta","text":S}},...}` with partial messages
//! * `{"type":"system","subtype":"api_retry","attempt":N,"max_retries":N,
//!    "error":S|null,...}`
//! * `{"type":"system","subtype":"permission_denied","tool_name":S?}`
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
    BillingInclusion, FailureClass, FinishDisposition, NativeEvent, NativeRefusal,
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

/// Incremental decoder from framed stdout lines to normalized events.
///
/// Text accumulation rule (AGENTKIT-111): one user-visible text sequence
/// per turn equals the **latest complete assistant message text** if any
/// complete root message arrived, else the **concatenated `text_delta`s**.
/// A complete message that equals the accumulated deltas therefore
/// supersedes them without duplication; child-scoped blocks
/// (`parent_tool_use_id` non-null) never touch the root accumulator.
#[derive(Clone, Debug, Default)]
pub struct ClaudeStreamAdapter {
    /// Concatenated visible `text_delta`s seen so far (root stream only).
    delta_acc: String,
    /// Latest complete root assistant message text; supersedes deltas.
    last_complete: Option<String>,
    /// Model identity from the most recent `init` frame.
    init_model: Option<String>,
    /// Parsed root result facts, set once by the first root `result` frame.
    root_result: Option<ResultEvidence>,
    /// Reason captured when a child-scoped `result` marker was seen.
    deferred: Option<String>,
    /// Last non-`stream_event` raw line, for idempotent duplicate-frame
    /// tolerance (a byte-identical repeat is a no-op).
    last_frame: Option<String>,
}

impl ClaudeStreamAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Concatenated visible root text: latest complete message text if any
    /// complete root message arrived, else accumulated deltas; `None` when
    /// no root text was seen at all.
    pub fn partial_text(&self) -> Option<String> {
        if let Some(complete) = &self.last_complete {
            return Some(complete.clone());
        }
        if self.delta_acc.is_empty() {
            None
        } else {
            Some(self.delta_acc.clone())
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
            // Qualified additive metadata (ignored): plugin-install and
            // prompt-suggestion frames.
            Some("plugin_install") | Some("prompt_suggestion") => Ok(Vec::new()),
            Some(other) => Err(violation(
                "unknown_frame_type",
                format!("unknown top-level frame type '{other}'"),
            )),
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
            "permission_denied" => {
                let tool_name = frame
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                Ok(vec![NativeEvent::PermissionDenied { tool_name }])
            }
            // Hook lifecycle and plugin-install system frames are additive
            // progress metadata; correctness of the turn does not depend on
            // them (AGENTKIT-111 tolerance list).
            "hook_started" | "hook_progress" | "hook_response" | "plugin_install" => Ok(Vec::new()),
            other => Err(violation(
                "unknown_subtype",
                format!("unknown system subtype '{other}'"),
            )),
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
        self.init_model = observed_model.clone();
        Ok(vec![NativeEvent::Init {
            session_id,
            observed_model,
            observed_effort,
            observed_version,
            capabilities,
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
                    // never overwrite root text and never surface as root
                    // user-visible text.
                    if parent.is_none() {
                        // Complete message supersedes overlapping deltas.
                        self.last_complete = Some(text.clone());
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
                    events.push(NativeEvent::ToolObservation {
                        vendor_tool_use_id: id,
                        name,
                        phase: VendorToolPhase::Requested,
                        input,
                        parent_tool_use_id: parent.clone(),
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
                other => {
                    return Err(violation(
                        "unknown_content_block",
                        format!("unknown assistant content block type '{other}'"),
                    ));
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
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                events.push(NativeEvent::ToolObservation {
                    vendor_tool_use_id: tool_use_id.to_owned(),
                    name,
                    phase: VendorToolPhase::Result,
                    input: None,
                    parent_tool_use_id: parent.clone(),
                });
            }
        }
        Ok(events)
    }

    fn on_stream_event(&mut self, frame: &Value) -> Result<Vec<NativeEvent>, ProtocolViolation> {
        let event = frame
            .get("event")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                violation(
                    "stream_event_missing_event",
                    "stream_event without event object",
                )
            })?;
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
        self.delta_acc.push_str(text);
        Ok(vec![NativeEvent::TextDelta {
            text: text.to_owned(),
        }])
    }

    fn on_result(&mut self, frame: &Value) -> Result<Vec<NativeEvent>, ProtocolViolation> {
        let subtype = required_str(frame, "subtype")
            .ok_or_else(|| violation("result_missing_subtype", "result frame without subtype"))?
            .to_owned();
        if !matches!(
            subtype.as_str(),
            "success" | "error_max_turns" | "error_during_execution" | "deferred"
        ) {
            return Err(violation(
                "result_unknown_subtype",
                format!("unknown result subtype '{subtype}'"),
            ));
        }
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
/// `{"type":"user","message":{"role":"user","content":text}}` + `\n`.
/// Content newlines are JSON-escaped by the serializer, so the frame is
/// always exactly one NDJSON line (AGENTKIT-110/113).
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
            NativeTerminalOutcome::Failed {
                class: classify_result(ev),
                message: ev
                    .result_text
                    .clone()
                    .unwrap_or_else(|| format!("vendor result subtype '{}'", ev.subtype)),
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
    // Graceful interrupt acknowledged by the vendor (SIGINT path or its
    // 128+15 exit convention) leaves the turn unfinished, distinct from an
    // outside SIGTERM without an interrupt request (AGENTKIT-116).
    if exit.after_interrupt
        && (exit.signal == Some(2) || exit.signal == Some(15) || exit.exit_code == Some(143))
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
        "{} {}",
        ev.subtype,
        ev.result_text.clone().unwrap_or_default()
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
            (r#"{"type":"watermelon"}"#, "unknown_frame_type"),
            (r#"{"no_type":1}"#, "missing_frame_type"),
            (
                r#"{"type":"system","subtype":"telepathy"}"#,
                "unknown_subtype",
            ),
            (
                r#"{"type":"system","subtype":"init","model":"m"}"#,
                "init_missing_session_id",
            ),
            (
                r#"{"type":"result","subtype":"success","is_error":false,"result":null,"usage":null}"#,
                "result_missing_session_id",
            ),
            (
                r#"{"type":"result","subtype":"nope","is_error":false,"result":null,"session_id":"s"}"#,
                "result_unknown_subtype",
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
                r#"{"type":"assistant","message":{"content":[{"type":"hologram"}]},"session_id":"s"}"#,
                "unknown_content_block",
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
            } => {
                assert_eq!(vendor_tool_use_id, "toolu_1");
                assert_eq!(name, "Read");
                assert_eq!(*phase, VendorToolPhase::Requested);
                assert_eq!(input.as_ref().unwrap()["path"], "x");
                assert_eq!(parent_tool_use_id, &None);
            }
            other => panic!("ToolObservation expected, never a dispatchable request: {other:?}"),
        }
        let events = push(&mut adapter, TOOL_RESULT);
        assert!(matches!(
            &events[0],
            NativeEvent::ToolObservation {
                phase: VendorToolPhase::Result,
                input: None,
                ..
            }
        ));
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
            b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello world\"}}\n"
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
    fn qualified_min_version_is_the_permission_prompts_floor() {
        assert_eq!(QUALIFIED_MIN_VERSION, (2, 1, 259));
    }
}
