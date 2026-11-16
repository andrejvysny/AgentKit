//! Output store port (roadmap 2.1, ADR 0034): a tool result larger than the
//! inline limit is written to the store before the model first sees it, and
//! the model gets a line-aligned preview plus a handle it can page with
//! `read_output` and `search_output`.
//!
//! This module holds the port, its value types, the pure text helpers every
//! implementation shares (line slicing, search, preview), and the
//! [`OutputGate`] the run loop calls on every tool result. Storage itself is
//! an adapter: `agentkit-store` keeps bytes in a private file and an index
//! row; `agentkit-testkit` keeps both in memory; one conformance suite runs
//! against both (ADR 0024).
use crate::{ToolResult, redact_value};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

mod shaped;
use shaped::{SHAPED_FOOTER_ROOM, SHAPED_MAX_SHARE_PERCENT, ShapedText, strip_marker, take_shaped};
pub use shaped::{SHAPED_HIDDEN_KEY, SHAPED_TEXT_KEY, shaped_value};

/// Default inline limit: a result at or below this many bytes is shown whole.
pub const DEFAULT_INLINE_LIMIT: usize = 8 * 1024;
/// Preview shape for a successful result: head and tail byte budgets.
pub const HEAD_BYTES: usize = 3 * 1024;
pub const TAIL_BYTES: usize = 2 * 1024;
/// Preview shape when the exit code is non-zero: errors sit at the end.
pub const FAILURE_HEAD_BYTES: usize = 1024;
pub const FAILURE_TAIL_BYTES: usize = 4 * 1024;
/// Room kept for the hidden-bytes marker and the footer inside the limit.
const PREVIEW_OVERHEAD: usize = 512;
/// Most bytes one stored output holds; a larger result is stored cut at this
/// size and its footer says so.
pub const MAX_STORED_BYTES: usize = 64 * 1024 * 1024;
/// Smallest inline limit a host may configure.
pub const MIN_INLINE_LIMIT: usize = 1024;
/// Retention after the last read or write of an output.
pub const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// The framework tools that page the store; their own results are never
/// stored again.
pub const READ_OUTPUT: &str = "read_output";
pub const SEARCH_OUTPUT: &str = "search_output";
/// Metadata key the run loop sets on a tool's context when an output store
/// is in play: the tool may then return its full text, because the store
/// holds it and the model sees a preview.
pub const OUTPUT_STORE_METADATA_KEY: &str = "outputStore";
/// Metadata key carrying the inline limit (bytes) that applies to the tool
/// being called, so a tool that cuts its own text can leave it whole when
/// the gate will store it.
pub const OUTPUT_INLINE_LIMIT_METADATA_KEY: &str = "outputInlineLimit";
/// Message-metadata key under which the host keeps the details of a persisted
/// `read_file` result (path, range, hash, whether it showed the range whole).
pub const READ_DETAILS_METADATA_KEY: &str = "readDetails";
/// Tool-metadata key carrying the reads the provider view of this run still
/// shows whole: a JSON array of `{path, offset, limit, sha256, lines,
/// messageId, turn}`, derived by the host from persisted details. A tool may
/// point at such a read; with the key absent every read is answered in full.
pub const LIVE_READS_METADATA_KEY: &str = "liveReads";

/// Chat-metadata object a subagent child carries (written once at spawn).
pub const SUBAGENT_METADATA_KEY: &str = "agentkit.subagent";
/// Field of [`SUBAGENT_METADATA_KEY`] listing the output handles the child's
/// brief granted it read access to (roadmap 4.1, DEL-02). The grant record is
/// keyed by the child session (its chat) and holds exactly the handles the
/// spawning parent could itself read.
pub const OUTPUT_GRANTS_METADATA_KEY: &str = "outputGrants";

const HANDLE_PREFIX: &str = "out_";

#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("no stored output {0}")]
    NotFound(String),
    #[error("not an output handle: {0}")]
    InvalidHandle(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("output storage failed: {0}")]
    Storage(String),
    #[error("stored output {0} is damaged")]
    Integrity(String),
}

/// `out_` plus a UUID v7 as 32 lowercase hex digits. The shape is checked on
/// every parse because a handle names a file.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct OutputHandle(String);

/// A handle read from the wire goes through the same strict `parse`.
impl<'de> Deserialize<'de> for OutputHandle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl OutputHandle {
    pub fn generate() -> Self {
        Self(format!("{HANDLE_PREFIX}{}", uuid::Uuid::now_v7().simple()))
    }

    pub fn parse(text: &str) -> Result<Self, OutputError> {
        let valid = text.strip_prefix(HANDLE_PREFIX).is_some_and(|rest| {
            rest.len() == 32
                && rest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(OutputError::InvalidHandle(text.chars().take(80).collect()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OutputHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Who produced an output. `session` is the chat scope the run belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputMeta {
    pub session: String,
    pub task: String,
    pub call_id: String,
    pub tool: String,
    pub mime: String,
    pub exit_code: Option<i32>,
}

/// One stored output as the index knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputInfo {
    pub handle: OutputHandle,
    pub meta: OutputMeta,
    pub byte_len: u64,
    pub line_count: u64,
    pub sha256: String,
    pub created_ms: i64,
    pub expires_ms: i64,
}

/// A slice request. Lines are 1-based; a zero start line reads from line 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteOrLineRange {
    Bytes { start: u64, len: u64 },
    Lines { start_line: u64, max_lines: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSlice {
    /// The requested lines with their own separators (no trailing one), or
    /// the requested bytes cut back to character boundaries.
    pub text: String,
    /// First and last line the slice touches (1-based, inclusive). An empty
    /// slice past the end has `end_line == start_line - 1`.
    pub start_line: u64,
    pub end_line: u64,
    pub total_lines: u64,
    pub total_bytes: u64,
    /// True when the output continues after this slice.
    pub has_more: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    /// 1-based line number.
    pub line: u64,
    /// The line, cut to a bounded length.
    pub text: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PurgeReport {
    pub outputs: u64,
    pub bytes: u64,
}

#[async_trait]
pub trait OutputStore: Send + Sync {
    /// Store `bytes` and return the handle. The caller has already redacted.
    async fn put(&self, meta: OutputMeta, bytes: Vec<u8>) -> Result<OutputHandle, OutputError>;
    /// The index row of a handle.
    async fn info(&self, handle: &OutputHandle) -> Result<OutputInfo, OutputError>;
    async fn read(
        &self,
        handle: &OutputHandle,
        range: ByteOrLineRange,
    ) -> Result<OutputSlice, OutputError>;
    async fn search(
        &self,
        handle: &OutputHandle,
        pattern: &str,
        max_matches: u32,
    ) -> Result<Vec<Match>, OutputError>;
    /// Remove every output of `session`: files and rows.
    async fn purge_session(&self, session: &str) -> Result<PurgeReport, OutputError>;
}

/// Decides whose outputs a session may page. `read_output` and
/// `search_output` allow the caller's own session, its descendant subagent
/// sessions and the handles an explicit grant names: a child never reads its
/// parent's or a sibling's outputs, except the exact handles its brief
/// listed as `output` context refs (roadmap 4.1, DEL-02).
#[async_trait]
pub trait SessionTree: Send + Sync {
    /// True when `owner` is `caller` itself or a descendant of it.
    async fn may_read(&self, caller: &str, owner: &str) -> bool;

    /// True when `caller` may read `handle`, which `owner`'s session holds:
    /// the tree rule of [`Self::may_read`], or a per-handle grant recorded
    /// for `caller`. A tree without grants answers by the tree rule alone.
    async fn may_read_handle(&self, caller: &str, owner: &str, _handle: &OutputHandle) -> bool {
        self.may_read(caller, owner).await
    }
}

/// The tree of a host that knows no delegation: a session reads only itself.
pub struct ExactSession;

#[async_trait]
impl SessionTree for ExactSession {
    async fn may_read(&self, caller: &str, owner: &str) -> bool {
        caller == owner
    }
}

/// Lines in `data`. A line ends at `\n`, `\r\n` or a lone `\r`; a final
/// unterminated line counts.
pub fn count_lines(data: &[u8]) -> u64 {
    LineSpans { data, at: 0 }.count() as u64
}

/// `(start, end)` of every line, terminator excluded.
fn line_spans(data: &[u8]) -> Vec<(usize, usize)> {
    LineSpans { data, at: 0 }.collect()
}

struct LineSpans<'a> {
    data: &'a [u8],
    at: usize,
}

impl Iterator for LineSpans<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.at;
        if start >= self.data.len() {
            return None;
        }
        for (offset, byte) in self.data[start..].iter().enumerate() {
            let end = start + offset;
            match byte {
                b'\n' => {
                    self.at = end + 1;
                    return Some((start, end));
                }
                b'\r' => {
                    let crlf = self.data.get(end + 1) == Some(&b'\n');
                    self.at = end + 1 + usize::from(crlf);
                    return Some((start, end));
                }
                _ => {}
            }
        }
        self.at = self.data.len();
        Some((start, self.data.len()))
    }
}

/// The slice both adapters return, so they cannot disagree on line
/// numbering or on where a byte range is cut.
pub fn slice_bytes(data: &[u8], range: ByteOrLineRange) -> OutputSlice {
    let total_bytes = data.len() as u64;
    let spans = line_spans(data);
    let total_lines = spans.len() as u64;
    match range {
        ByteOrLineRange::Lines {
            start_line,
            max_lines,
        } => {
            let first = start_line.max(1);
            let first_index = usize::try_from(first - 1).unwrap_or(usize::MAX);
            let wanted = usize::try_from(max_lines.max(1)).unwrap_or(usize::MAX);
            if first_index >= spans.len() {
                return OutputSlice {
                    text: String::new(),
                    start_line: first,
                    end_line: first - 1,
                    total_lines,
                    total_bytes,
                    has_more: false,
                };
            }
            let last_index = first_index.saturating_add(wanted).min(spans.len()) - 1;
            let (from, _) = spans[first_index];
            let (_, to) = spans[last_index];
            OutputSlice {
                text: String::from_utf8_lossy(&data[from..to]).into_owned(),
                start_line: first,
                end_line: last_index as u64 + 1,
                total_lines,
                total_bytes,
                has_more: last_index + 1 < spans.len(),
            }
        }
        ByteOrLineRange::Bytes { start, len } => {
            let total = data.len();
            let mut from = usize::try_from(start).unwrap_or(usize::MAX).min(total);
            let mut to = from
                .saturating_add(usize::try_from(len).unwrap_or(usize::MAX))
                .min(total);
            // Never cut inside a UTF-8 sequence.
            while from < to && (data[from] & 0xC0) == 0x80 {
                from += 1;
            }
            while to > from && to < total && (data[to] & 0xC0) == 0x80 {
                to -= 1;
            }
            let line_of = |offset: usize| -> u64 {
                spans
                    .iter()
                    .position(|(_, end)| offset <= *end)
                    .map_or(total_lines, |index| index as u64 + 1)
            };
            OutputSlice {
                text: String::from_utf8_lossy(&data[from..to]).into_owned(),
                start_line: line_of(from).max(1),
                end_line: line_of(to.saturating_sub(1).max(from)).max(1),
                total_lines,
                total_bytes,
                has_more: to < total,
            }
        }
    }
}

const MATCH_TEXT_BYTES: usize = 300;

/// Lines of `data` matching `pattern`: a regular expression, or the literal
/// text when it does not parse as one.
pub fn search_bytes(
    data: &[u8],
    pattern: &str,
    max_matches: u32,
) -> Result<Vec<Match>, OutputError> {
    if pattern.is_empty() {
        return Err(OutputError::Invalid("the pattern is empty".into()));
    }
    let build = |text: &str| regex::RegexBuilder::new(text).size_limit(1 << 20).build();
    let expression = build(pattern)
        .or_else(|_| build(&regex::escape(pattern)))
        .map_err(|error| OutputError::Invalid(format!("the pattern is not usable: {error}")))?;
    let mut matches = Vec::new();
    for (index, (from, to)) in line_spans(data).into_iter().enumerate() {
        if matches.len() >= max_matches as usize {
            break;
        }
        let line = String::from_utf8_lossy(&data[from..to]);
        if expression.is_match(&line) {
            matches.push(Match {
                line: index as u64 + 1,
                text: cut(&line, MATCH_TEXT_BYTES),
            });
        }
    }
    Ok(matches)
}

fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// 48213 -> "48,213".
fn group(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn is_break(byte: u8) -> bool {
    byte == b'\n' || byte == b'\r'
}

/// End of the line-aligned head, and whether a single line longer than the
/// budget had to be cut inside.
fn head_boundary(raw: &str, budget: usize) -> (usize, bool) {
    if budget == 0 {
        return (0, false);
    }
    if raw.len() <= budget {
        return (raw.len(), false);
    }
    match raw.as_bytes()[..budget]
        .iter()
        .rposition(|byte| is_break(*byte))
    {
        Some(at) => (at + 1, false),
        None => {
            let mut end = budget;
            while !raw.is_char_boundary(end) {
                end -= 1;
            }
            (end, true)
        }
    }
}

/// Start of the line-aligned tail, and whether a single line longer than the
/// budget had to be cut inside.
fn tail_boundary(raw: &str, budget: usize) -> (usize, bool) {
    let len = raw.len();
    let start = len.saturating_sub(budget);
    if budget == 0 {
        return (len, false);
    }
    if start == 0 {
        return (0, false);
    }
    let bytes = raw.as_bytes();
    let before = bytes[start - 1];
    // A start between the `\r` and `\n` of one terminator is not a line start.
    if before == b'\n' || (before == b'\r' && bytes[start] != b'\n') {
        return (start, false);
    }
    if let Some(at) = bytes[start..].iter().position(|byte| is_break(*byte)) {
        let crlf = bytes[start + at] == b'\r' && bytes.get(start + at + 1) == Some(&b'\n');
        let next = start + at + 1 + usize::from(crlf);
        if next < len {
            return (next, false);
        }
    }
    let mut from = start;
    while !raw.is_char_boundary(from) {
        from += 1;
    }
    (from, true)
}

/// Length of `text` as the content of a JSON string.
fn escaped_len(text: &str) -> usize {
    serde_json::to_string(text).map_or(text.len(), |json| json.len().saturating_sub(2))
}

fn compose_preview(
    raw: &str,
    handle: &OutputHandle,
    head: usize,
    tail: usize,
    cut_from: Option<usize>,
) -> String {
    let (head_end, head_cut) = head_boundary(raw, head);
    let (tail_start, tail_cut) = tail_boundary(raw, tail);
    let tail_start = tail_start.max(head_end);
    let hidden = &raw[head_end..tail_start];
    let mut out = String::with_capacity(head + tail + PREVIEW_OVERHEAD);
    out.push_str(&raw[..head_end]);
    if head_cut {
        out.push_str(" [line cut]");
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "[… {} bytes and {} lines hidden …]\n",
        group(hidden.len() as u64),
        group(count_lines(hidden.as_bytes()))
    ));
    let tail_text = &raw[tail_start..];
    if tail_cut && !tail_text.is_empty() {
        out.push_str("[line cut] ");
    }
    out.push_str(tail_text);
    if !tail_text.is_empty() && !tail_text.ends_with('\n') {
        out.push('\n');
    }
    let capped = cut_from.map_or(String::new(), |whole| {
        format!(
            " · truncated at the {} MiB storage cap, the result was {} bytes",
            MAX_STORED_BYTES / (1024 * 1024),
            group(whole as u64)
        )
    });
    out.push_str(&format!(
        "[output stored: {} bytes, {} lines{capped} · handle {handle}\n read_output(handle, start_line, max_lines) · search_output(handle, pattern)]",
        group(raw.len() as u64),
        group(count_lines(raw.as_bytes())),
    ));
    out
}

/// The text the model reads for a stored output: line-aligned head, a marker
/// naming what is hidden, line-aligned tail, and the footer with the handle.
/// A failure moves the budget to the tail, where errors are. `limit` bounds
/// the preview as the content of a JSON string (the envelope escapes it), so
/// the footer and handle are never cut by the envelope's byte budget.
/// `cut_from` is the original length when storage cut the text.
pub fn render_preview(
    raw: &str,
    handle: &OutputHandle,
    failure: bool,
    limit: usize,
    cut_from: Option<usize>,
) -> String {
    let (mut head, mut tail) = if failure {
        (FAILURE_HEAD_BYTES, FAILURE_TAIL_BYTES)
    } else {
        (HEAD_BYTES, TAIL_BYTES)
    };
    let available = limit.saturating_sub(PREVIEW_OVERHEAD);
    if head + tail > available {
        head = available * head / (head + tail);
        tail = available - head;
    }
    for _ in 0..16 {
        let out = compose_preview(raw, handle, head, tail, cut_from);
        let escaped = escaped_len(&out);
        if escaped <= limit || head + tail == 0 {
            return out;
        }
        // Each dropped raw byte drops at least one escaped byte.
        let over = escaped - limit + 32;
        let total = head + tail;
        let drop_head = (over * head / total).min(head);
        head -= drop_head;
        tail = tail.saturating_sub(over - drop_head);
    }
    compose_preview(raw, handle, 0, 0, cut_from)
}

/// One object rendered as sections, so a field holding multi-line text
/// (stdout, a file body) keeps its real lines instead of one escaped line.
fn render_fields<'a>(fields: impl Iterator<Item = (&'a String, &'a Value)>) -> String {
    let mut out = String::new();
    for (key, value) in fields {
        match value {
            Value::String(text) if text.contains('\n') || text.len() > 200 => {
                out.push_str(&format!("=== {key} ===\n{text}\n"));
            }
            Value::String(text) => out.push_str(&format!("{key}: {text}\n")),
            Value::Object(_) | Value::Array(_) => out.push_str(&format!(
                "{key}: {}\n",
                serde_json::to_string_pretty(value).unwrap_or_default()
            )),
            other => out.push_str(&format!("{key}: {other}\n")),
        }
    }
    out
}

/// The text of a result that gets stored, and its media type. Text results
/// stay text, so lines mean something; structured results are laid out as
/// sections for the same reason.
fn raw_text(value: &Value) -> (String, &'static str) {
    match value {
        Value::String(text) => (text.clone(), "text/plain"),
        Value::Object(map) if map.get("text").is_some_and(Value::is_string) => {
            let text = map["text"].as_str().unwrap_or_default().to_owned();
            let rest = render_fields(map.iter().filter(|(key, _)| key.as_str() != "text"));
            if rest.is_empty() {
                (text, "text/plain")
            } else {
                (
                    format!("{text}\n\n--- structured ---\n{rest}"),
                    "text/plain",
                )
            }
        }
        Value::Object(map) => (render_fields(map.iter()), "text/plain"),
        other => (
            serde_json::to_string_pretty(other).unwrap_or_default(),
            "application/json",
        ),
    }
}

fn exit_code_of(result: &ToolResult) -> Option<i32> {
    [result.data.as_ref(), result.model_data.as_ref()]
        .into_iter()
        .flatten()
        .find_map(|value| {
            ["exitCode", "exit_code"]
                .iter()
                .find_map(|key| value.get(*key).and_then(Value::as_i64))
        })
        .and_then(|code| i32::try_from(code).ok())
}

/// Identity of the call whose result is shaped. `max_bytes` is the byte
/// budget of the tool's context; the inline limit never exceeds it.
pub struct OutputCall<'a> {
    pub session: &'a str,
    pub task: &'a str,
    pub call_id: &'a str,
    pub tool: &'a str,
    pub max_bytes: usize,
}

type Redactor = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Room the envelope's own fields (status, warnings, summary) take out of
/// the result byte budget; the preview must fit in what is left.
fn envelope_reserve(max_bytes: usize) -> usize {
    crate::envelope::text_budget(max_bytes) * 2 + 256
}

fn exempt(tool: &str) -> bool {
    tool == READ_OUTPUT || tool == SEARCH_OUTPUT
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// A result's model-visible value after redaction, and its text layout.
struct Prepared {
    in_model_data: bool,
    value: Value,
    changed: bool,
    raw: String,
    mime: &'static str,
    shaped: Option<ShapedText>,
}

/// The gate's own check of a shaped text: it hides something, fits the inline
/// limit with its footer, and with that footer is at most 80% of the raw text.
fn usable_shape(prepared: &Prepared, limit: usize) -> Option<&ShapedText> {
    let shaped = prepared.shaped.as_ref()?;
    let extra =
        SHAPED_FOOTER_ROOM + shaped.what.len() + shaped.note.as_ref().map_or(0, String::len);
    let fits = escaped_len(&shaped.text) + extra <= limit;
    let cost = shaped.text.len() + extra;
    (shaped.hidden > 0 && fits && cost * 100 <= prepared.raw.len() * SHAPED_MAX_SHARE_PERCENT)
        .then_some(shaped)
}

/// The run loop's seam: stores a large result and swaps its model-visible
/// data for the preview. Fails open: when the store cannot take the bytes the
/// (redacted) result is returned whole with a warning, and the envelope's
/// byte budget bounds it as before.
#[derive(Clone)]
pub struct OutputGate {
    store: Arc<dyn OutputStore>,
    inline_limit: usize,
    tool_limits: HashMap<String, usize>,
    redactor: Option<Redactor>,
}

impl OutputGate {
    pub fn new(store: Arc<dyn OutputStore>) -> Self {
        Self {
            store,
            inline_limit: DEFAULT_INLINE_LIMIT,
            tool_limits: HashMap::new(),
            redactor: None,
        }
    }

    pub fn with_inline_limit(mut self, bytes: usize) -> Self {
        self.inline_limit = bytes;
        self
    }

    /// Per-tool override of the inline limit.
    pub fn with_tool_limit(mut self, tool: impl Into<String>, bytes: usize) -> Self {
        self.tool_limits.insert(tool.into(), bytes);
        self
    }

    /// Extra text redaction applied after the built-in credential-key pass.
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = Some(redactor);
        self
    }

    pub fn store(&self) -> &Arc<dyn OutputStore> {
        &self.store
    }

    /// The configured inline limit of `tool`.
    pub fn inline_limit(&self, tool: &str) -> usize {
        self.tool_limits
            .get(tool)
            .copied()
            .unwrap_or(self.inline_limit)
    }

    /// The limit that applies: the configured one, held inside the byte
    /// budget of the context so the envelope never cuts a preview.
    pub fn effective_limit(&self, tool: &str, max_bytes: usize) -> usize {
        self.inline_limit(tool)
            .min(max_bytes.saturating_sub(envelope_reserve(max_bytes)))
    }

    /// Whether [`Self::shape`] would store `result` of `tool`: it is over the
    /// limit once redacted. The MCP bridge keeps its own cap unless this
    /// holds.
    pub fn will_store(&self, tool: &str, max_bytes: usize, result: &ToolResult) -> bool {
        !exempt(tool)
            && self.prepare(result).is_some_and(|prepared| {
                let limit = self.effective_limit(tool, max_bytes);
                usable_shape(&prepared, limit).is_some() || prepared.raw.len() > limit
            })
    }

    fn redact_text(&self, text: &str) -> String {
        self.redactor
            .as_ref()
            .map_or_else(|| text.to_owned(), |redactor| redactor(text))
    }

    fn redact_strings(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.redact_text(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.redact_strings(item)),
            Value::Object(map) => map.values_mut().for_each(|item| self.redact_strings(item)),
            _ => {}
        }
    }

    fn prepare(&self, result: &ToolResult) -> Option<Prepared> {
        let (source, in_model_data) = match result.model_data.as_ref().filter(|v| !v.is_null()) {
            Some(value) => (value, true),
            None => (result.data.as_ref()?, false),
        };
        let mut value = source.clone();
        redact_value(&mut value);
        if self.redactor.is_some() {
            self.redact_strings(&mut value);
        }
        let shaped = take_shaped(&mut value);
        let changed = value != *source;
        let (raw, mime) = raw_text(&value);
        Some(Prepared {
            in_model_data,
            value,
            changed,
            raw,
            mime,
            shaped,
        })
    }

    pub async fn shape(&self, call: OutputCall<'_>, mut result: ToolResult) -> ToolResult {
        if call.session.is_empty() || exempt(call.tool) {
            for value in [result.model_data.as_mut(), result.data.as_mut()]
                .into_iter()
                .flatten()
            {
                strip_marker(value);
            }
            return result;
        }
        let exit_code = exit_code_of(&result);
        let failure = exit_code.map_or(!result.ok, |code| code != 0);
        let limit = self.effective_limit(call.tool, call.max_bytes);
        self.shape_data(&call, &mut result, exit_code, failure, limit)
            .await;
        self.shape_attachments(&call, &mut result, limit).await;
        result
    }

    fn meta(&self, call: &OutputCall<'_>, mime: &str, exit_code: Option<i32>) -> OutputMeta {
        OutputMeta {
            session: call.session.to_owned(),
            task: call.task.to_owned(),
            call_id: call.call_id.to_owned(),
            tool: call.tool.to_owned(),
            mime: mime.to_owned(),
            exit_code,
        }
    }

    /// Store `raw` (cut at the storage cap) and render its preview.
    async fn store_preview(
        &self,
        meta: OutputMeta,
        mut raw: String,
        failure: bool,
        limit: usize,
    ) -> Result<String, OutputError> {
        let whole = raw.len();
        let cut_from = (whole > MAX_STORED_BYTES).then_some(whole);
        raw.truncate(floor_boundary(&raw, MAX_STORED_BYTES));
        let handle = self.store.put(meta, raw.clone().into_bytes()).await?;
        Ok(render_preview(&raw, &handle, failure, limit, cut_from))
    }

    /// Store `raw` and render the shaped text with its hidden-count footer.
    async fn store_shaped(
        &self,
        meta: OutputMeta,
        raw: &str,
        shaped: &ShapedText,
    ) -> Result<String, OutputError> {
        let whole = raw.len();
        let mut stored = raw.to_owned();
        stored.truncate(floor_boundary(&stored, MAX_STORED_BYTES));
        let cut_from = (whole > MAX_STORED_BYTES).then_some(whole);
        let handle = self.store.put(meta, stored.into_bytes()).await?;
        let mut text = shaped.text.clone();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let note = shaped
            .note
            .as_ref()
            .map_or(String::new(), |note| format!(" · {note}"));
        let capped = cut_from.map_or(String::new(), |whole| {
            format!(
                " · the stored text is cut at the {} MiB storage cap, the result was {} bytes",
                MAX_STORED_BYTES / (1024 * 1024),
                group(whole as u64)
            )
        });
        text.push_str(&format!(
            "[{} {} hidden{note}{capped} · handle {handle}]",
            group(shaped.hidden),
            shaped.what
        ));
        Ok(text)
    }

    async fn shape_data(
        &self,
        call: &OutputCall<'_>,
        result: &mut ToolResult,
        exit_code: Option<i32>,
        failure: bool,
        limit: usize,
    ) {
        let Some(prepared) = self.prepare(result) else {
            return;
        };
        let usable = usable_shape(&prepared, limit).cloned();
        let Prepared {
            in_model_data,
            value,
            changed,
            raw,
            mime,
            ..
        } = prepared;
        let put_back = |result: &mut ToolResult, value: Value| {
            if in_model_data {
                result.model_data = Some(value);
            } else {
                result.data = Some(value);
            }
        };
        let raw = if self.redactor.is_some() {
            self.redact_text(&raw)
        } else {
            raw
        };
        let meta = self.meta(call, mime, exit_code);
        if let Some(shaped) = usable.as_ref() {
            // The raw text always goes to the store; the model reads the
            // shaped text and the tool's structured data stays as it was.
            match self.store_shaped(meta.clone(), &raw, shaped).await {
                Ok(text) => result.model_data = Some(Value::String(text)),
                Err(error) => {
                    put_back(result, value);
                    result.warnings.push(format!(
                        "The full result could not be stored ({error}); the shaped text was dropped and the raw result kept whole, bounded by envelope."
                    ));
                }
            }
            return;
        }
        if raw.len() <= limit {
            if changed {
                put_back(result, value);
            }
            return;
        }
        match self.store_preview(meta, raw, failure, limit).await {
            Ok(preview) => {
                result.data = Some(Value::String(preview));
                result.model_data = None;
                result.truncated = false;
            }
            Err(error) => {
                put_back(result, value);
                result.warnings.push(format!(
                    "The full result could not be stored ({error}); it was kept whole, bounded by envelope."
                ));
            }
        }
    }

    /// Text attachments take the same path as the data: redacted, and stored
    /// with a preview when over the limit.
    async fn shape_attachments(
        &self,
        call: &OutputCall<'_>,
        result: &mut ToolResult,
        limit: usize,
    ) {
        let Some(parts) = result.model_content.as_mut() else {
            return;
        };
        for part in parts.iter_mut() {
            if part["type"].as_str() != Some("text") {
                continue;
            }
            let Some(text) = part["text"].as_str() else {
                continue;
            };
            let text = self.redact_text(text);
            if text.len() <= limit {
                part["text"] = Value::String(text);
                continue;
            }
            let meta = self.meta(call, "text/plain", None);
            match self.store_preview(meta, text.clone(), false, limit).await {
                Ok(preview) => part["text"] = Value::String(preview),
                Err(error) => {
                    part["text"] = Value::String(text);
                    result.warnings.push(format!(
                        "An attachment could not be stored ({error}); it was kept whole, bounded by envelope."
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
