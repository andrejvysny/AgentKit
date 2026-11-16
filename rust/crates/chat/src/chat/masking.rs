//! Batched masking (ADR 0034, layer 4): the replacement for sliding elision
//! and the per-turn trim.
//!
//! The provider-facing history stays byte-stable for many turns. Only when the
//! estimate E passes `trigger_ratio` of the message budget B (window minus
//! reserve minus tool definitions) does one batch run: tool results outside
//! the last turns, oldest first, until E is down to `target_ratio` of B. A
//! masked result is replaced by `[result masked to save context · N bytes ·
//! handle out_…]`; its full text sits in the output store (written at mask
//! time when it was still inline), so the model can page it back. The batch is
//! saved as a derived `context_masks` record and applied on every assembly, so
//! the canonical history is untouched and a masked prefix never changes again.
//! Images in user messages older than `image_max_age_turns` join the same
//! batch, as placeholders that name the attachment (and carry its id when it
//! has one). Images are NOT copied to the output store: the attachment stays
//! in the stored history, so the placeholder is the only thing that changes.
//!
//! A batch that would free less than `min_gain_ratio` of B is not worth the
//! cache break: nothing is masked and compaction is signalled instead. So is a
//! batch that leaves E above the trigger, or one whose every write the store
//! refused.
use super::{
    compaction::{self, Entry},
    context_budget::{self, ContextBudget},
};
use crate::{ChatError, ChatResult, ExecutionContext};
use agentkit_core::{ChatMessage, OutputHandle, OutputMeta, OutputStore, ToolDefinition};
use agentkit_kernel::sha256_hex;
use agentkit_store::MessageRecord;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

/// Hash of one message's bytes by message id, filled once per assembly so a
/// record never rehashes a message another record already covered.
type Hashes = HashMap<String, String>;
/// Position of each message id; stable while contents are replaced in place.
type Positions = HashMap<String, usize>;
/// Output already written in this batch: content sha256 to (handle, bytes).
type Stored = HashMap<String, (String, u64)>;

/// Tools whose results are never masked: edits and plans carry decisions the
/// model must keep verbatim; discovery results define what it may call; memory
/// results are already curated. The paging tools would only mask a page, and
/// a loaded toolset guide is what lets the model call its tools at all.
const EXCLUDED_TOOLS: [&str; 9] = [
    "edit_file",
    "write_file",
    "apply_patch",
    "submit_plan",
    "tool_search",
    "tool_describe",
    "toolset_load",
    "read_output",
    "search_output",
];
const EXCLUDED_PREFIX: &str = "memory_";
const MASKED_PREFIX: &str = "[result masked to save context";
/// A handle's length, for sizing a body before the handle exists.
const PLACEHOLDER_HANDLE: &str = "out_00000000000000000000000000000000";
const HASH_TAG: &str = "agentkit-mask-source-v2";
/// The meta tool that runs a deferred tool by name.
const TOOL_CALL: &str = "tool_call";

/// Thresholds as profile settings (`context.masking` in the session
/// descriptor); the defaults are the spec's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MaskSettings {
    /// `false` turns masking off (the legacy `toolResultElision: false` opt-out).
    pub(crate) enabled: bool,
    pub(crate) trigger_ratio: f64,
    pub(crate) target_ratio: f64,
    pub(crate) min_gain_ratio: f64,
    pub(crate) keep_recent_turns: u64,
    pub(crate) image_max_age_turns: u64,
}

impl Default for MaskSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger_ratio: 0.80,
            target_ratio: 0.50,
            min_gain_ratio: 0.20,
            keep_recent_turns: 3,
            image_max_age_turns: 6,
        }
    }
}

impl MaskSettings {
    /// The session's `context.masking` record over the defaults. The
    /// descriptor grammar (`agentkit_sessions`) is the one place thresholds
    /// are validated; every descriptor reaching here has passed it.
    pub(crate) fn resolve(options: Option<&Value>) -> Self {
        let mut settings = Self::default();
        let Some(values) = options.and_then(Value::as_object) else {
            return settings;
        };
        if let Some(enabled) = values.get("enabled").and_then(Value::as_bool) {
            settings.enabled = enabled;
        }
        if let Some(ratio) = values.get("triggerRatio").and_then(Value::as_f64) {
            settings.trigger_ratio = ratio;
        }
        if let Some(ratio) = values.get("targetRatio").and_then(Value::as_f64) {
            settings.target_ratio = ratio;
        }
        if let Some(ratio) = values.get("minGainRatio").and_then(Value::as_f64) {
            settings.min_gain_ratio = ratio;
        }
        if let Some(turns) = values.get("keepRecentTurns").and_then(Value::as_u64) {
            settings.keep_recent_turns = turns;
        }
        if let Some(turns) = values.get("imageMaxAgeTurns").and_then(Value::as_u64) {
            settings.image_max_age_turns = turns;
        }
        settings
    }
}

pub(super) struct Input<'a> {
    pub context: &'a ExecutionContext,
    pub chat_id: &'a str,
    pub settings: MaskSettings,
    pub budget: Option<&'a ContextBudget>,
    pub tools: &'a [ToolDefinition],
    /// Without an output store a result cannot be masked (INV-6: it must stay
    /// readable); images still can.
    pub store: Option<&'a Arc<dyn OutputStore>>,
}

#[derive(Debug, Default, PartialEq)]
pub(super) struct Outcome {
    /// Masking would not pay for its cache break: compact instead.
    pub compact: bool,
    pub results_masked: usize,
    pub images_masked: usize,
    /// Estimated tokens around the new batch.
    pub tokens: (u64, u64),
    /// The ids of the masks the provider view carries: stored ones that still
    /// match the history, and the batch written now.
    pub applied: Vec<String>,
    /// A cache break (INV-3) was already reported while applying: masking was
    /// switched off, or stored masks no longer matched.
    pub reported_break: bool,
}

impl Outcome {
    /// A new batch changed the provider-facing history this turn.
    pub(super) fn rewrote(&self) -> bool {
        self.results_masked + self.images_masked > 0
    }
}

#[derive(Debug, PartialEq)]
enum Kind {
    Tool { call_id: String, name: String },
    Images { names: Vec<String> },
}

#[derive(Debug, PartialEq)]
struct Candidate {
    index: usize,
    saved: u64,
    kind: Kind,
}

#[derive(Debug, PartialEq)]
struct Plan {
    picks: Vec<Candidate>,
    compact: bool,
}

fn utf16(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}

fn scaled(budget: u64, ratio: f64) -> u64 {
    (budget as f64 * ratio).floor() as u64
}

fn entries_tokens(entries: &[Entry]) -> u64 {
    entries
        .iter()
        .map(|entry| context_budget::message_tokens(&entry.0))
        .sum()
}

fn masked_body(bytes: u64, handle: &str) -> String {
    format!("{MASKED_PREFIX} · {bytes} bytes · handle {handle}]")
}

fn is_masked(content: &Value) -> bool {
    content
        .as_str()
        .is_some_and(|text| text.starts_with(MASKED_PREFIX))
}

/// The `read_file` details persisted with each tool result, by message id,
/// with the turn each result belongs to: the number of user messages before it
/// in the stored order, so a turn number never depends on what the view kept.
pub(super) type ReadIndex = HashMap<String, (Value, u64)>;

pub(super) fn read_index(records: &[MessageRecord]) -> ReadIndex {
    let mut users: Vec<u64> = records
        .iter()
        .filter(|record| {
            record.role == "user"
                && record.active
                && record.metadata["internal"] != true
                && record.metadata["steering"] != true
        })
        .map(|record| record.order_key)
        .collect();
    users.sort_unstable();
    records
        .iter()
        .filter(|record| record.role == "tool")
        .filter_map(|record| {
            let details = record
                .metadata
                .get(agentkit_core::READ_DETAILS_METADATA_KEY)?;
            let turn = users.partition_point(|key| *key < record.order_key) as u64;
            Some((record.id.clone(), (details.clone(), turn)))
        })
        .collect()
}

/// Most live reads handed to the tools: the newest ones, since a repeated read
/// is nearly always of something recent and the list rides in every call's
/// metadata.
const MAX_LIVE_READS: usize = 64;

/// The `read_file` results the FINAL provider view (after masking and
/// compaction) still shows whole: persisted, not masked, an intact envelope
/// and details that say the model saw the range in full. A tool may answer a
/// repeat of one with a stub; anything else is answered in full.
pub(super) fn live_reads(entries: &[Entry], index: &ReadIndex) -> Vec<Value> {
    let mut live = Vec::new();
    for (message, id) in entries {
        let Some(id) = id else {
            continue;
        };
        let Some((details, turn)) = index.get(id) else {
            continue;
        };
        if message.role != "tool"
            || is_masked(&message.content)
            || !envelope_intact(&message.content)
        {
            continue;
        }
        let files = details.get("files").and_then(Value::as_array);
        let reads = match files {
            Some(files) => files.iter().collect::<Vec<_>>(),
            None => vec![details],
        };
        for read in reads {
            if read["complete"] != true {
                continue;
            }
            let (Some(path), Some(sha256)) =
                (read["canonicalPath"].as_str(), read["sha256"].as_str())
            else {
                continue;
            };
            let (Some(offset), Some(lines)) = (read["offset"].as_i64(), read["lines"].as_u64())
            else {
                continue;
            };
            live.push(json!({
                "path": path,
                "offset": offset,
                "limit": read["limit"].as_i64(),
                "sha256": sha256,
                "lines": lines,
                "messageId": id,
                "turn": turn,
            }));
        }
    }
    let excess = live.len().saturating_sub(MAX_LIVE_READS);
    live.drain(..excess);
    live
}

/// The stored result reached the model as the tool built it: a successful
/// envelope the byte budget did not cut.
fn envelope_intact(content: &Value) -> bool {
    content
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .is_some_and(|envelope| envelope["ok"] == true && envelope["truncated"] == false)
}

fn is_excluded(tool: &str) -> bool {
    EXCLUDED_TOOLS.contains(&tool) || tool.starts_with(EXCLUDED_PREFIX)
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Index of the `turns`-th newest user message: every `user` message opens a
/// turn and the current one counts. 0 when the history has fewer turns, which
/// leaves nothing older to mask.
fn turn_boundary(entries: &[Entry], turns: u64) -> usize {
    let mut seen = 0u64;
    for index in (0..entries.len()).rev() {
        if entries[index].0.role == "user" {
            seen += 1;
            if seen >= turns.max(1) {
                return index;
            }
        }
    }
    0
}

/// First entry a new mask may touch. A summary's source hash covers every
/// entry it replaced, so those bytes must never change after it was written.
fn summary_floor(entries: &[Entry], summaries: &[Value]) -> usize {
    for record in summaries {
        if compaction::apply_summary(entries, record).is_some()
            && let Some(index) = entries
                .iter()
                .position(|entry| entry.1.as_deref() == record["firstKeptMessageId"].as_str())
        {
            return index;
        }
    }
    0
}

/// Call id to tool name, from the assistant messages that made the calls. A
/// deferred `tool_call` is named by the inner tool it runs (its `name`
/// argument), so exclusions apply to what actually produced the result.
fn call_names(entries: &[Entry]) -> HashMap<String, String> {
    entries
        .iter()
        .flat_map(|(message, _)| message.tool_calls.iter())
        .map(|call| {
            (
                call.id.clone(),
                effective_tool(&call.name, &call.arguments_json),
            )
        })
        .collect()
}

fn effective_tool(name: &str, arguments_json: &str) -> String {
    if name != TOOL_CALL {
        return name.to_owned();
    }
    serde_json::from_str::<Value>(arguments_json)
        .ok()
        .and_then(|arguments| arguments["name"].as_str().map(str::to_owned))
        .filter(|inner| !inner.is_empty())
        .unwrap_or_else(|| name.to_owned())
}

fn image_name(part: &Value) -> String {
    let named = ["name", "filename"]
        .iter()
        .find_map(|key| part[*key].as_str().filter(|name| !name.is_empty()));
    let name = match (named, part["source"]["kind"].as_str()) {
        (Some(name), _) => name.to_owned(),
        (None, Some("url")) => {
            let url = part["source"]["url"].as_str().unwrap_or_default();
            let path = url.split(['?', '#']).next().unwrap_or(url);
            path.rsplit('/').next().unwrap_or(path).to_owned()
        }
        (None, Some("ref")) => part["source"]["ref"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        (None, Some("data")) => format!(
            "inline {}",
            part["source"]["mediaType"].as_str().unwrap_or("image")
        ),
        _ => String::new(),
    };
    let name: String = name.chars().take(80).collect();
    if name.is_empty() {
        "image".into()
    } else {
        name
    }
}

fn image_names(content: &Value) -> Vec<String> {
    content.as_array().map_or_else(Vec::new, |parts| {
        parts
            .iter()
            .filter(|part| part["type"].as_str() == Some("image"))
            .map(image_name)
            .collect()
    })
}

/// The content with each image part replaced by a text placeholder naming the
/// attachment and, for a referenced one, its id. `None` when the number of
/// images no longer matches `names`.
fn mask_images(content: &Value, names: &[String]) -> Option<Value> {
    let parts = content.as_array()?;
    let mut next = names.iter();
    let masked: Vec<Value> = parts
        .iter()
        .map(|part| {
            if part["type"].as_str() != Some("image") {
                return Some(part.clone());
            }
            let name = next.next()?;
            // The ref is the attachment id the model can name again.
            let id = part["source"]["ref"].as_str().filter(|id| {
                part["source"]["kind"].as_str() == Some("ref") && *id != name.as_str()
            });
            let text = match id {
                Some(id) => {
                    format!("[image omitted to save context · attachment {name} · id {id}]")
                }
                None => format!("[image omitted to save context · attachment {name}]"),
            };
            Some(json!({"type":"text","text":text}))
        })
        .collect::<Option<_>>()?;
    next.next().is_none().then_some(Value::Array(masked))
}

fn candidates(
    entries: &[Entry],
    floor: usize,
    settings: &MaskSettings,
    with_store: bool,
) -> Vec<Candidate> {
    let tool_boundary = turn_boundary(entries, settings.keep_recent_turns);
    let image_boundary = turn_boundary(entries, settings.image_max_age_turns);
    let names = call_names(entries);
    let mut found = vec![];
    for (index, (message, id)) in entries.iter().enumerate().skip(floor) {
        let Some(id) = id.as_deref() else {
            continue;
        };
        if message.role == "tool" && with_store && index < tool_boundary {
            // A synthetic result states an unresolved effect: it stays.
            if id.contains("#missing:") || is_masked(&message.content) {
                continue;
            }
            let Some(call_id) = message.tool_call_id.clone() else {
                continue;
            };
            let Some(name) = names
                .get(&call_id)
                .cloned()
                .or_else(|| message.name.clone())
            else {
                continue;
            };
            if is_excluded(&name) {
                continue;
            }
            let bytes = content_text(&message.content).len() as u64;
            let body = utf16(&masked_body(bytes, PLACEHOLDER_HANDLE)).div_ceil(4);
            let saved = context_budget::message_tokens(message).saturating_sub(body);
            if saved > 0 {
                found.push(Candidate {
                    index,
                    saved,
                    kind: Kind::Tool { call_id, name },
                });
            }
        } else if message.role == "user" && index < image_boundary {
            let names = image_names(&message.content);
            let Some(content) = mask_images(&message.content, &names).filter(|_| !names.is_empty())
            else {
                continue;
            };
            let mut masked = message.clone();
            masked.content = content;
            let saved = context_budget::message_tokens(message)
                .saturating_sub(context_budget::message_tokens(&masked));
            if saved > 0 {
                found.push(Candidate {
                    index,
                    saved,
                    kind: Kind::Images { names },
                });
            }
        }
    }
    found
}

/// Pure planning: which candidates to mask, oldest first, to bring `before`
/// down to the target, and whether the whole batch is worth running.
fn plan(
    entries: &[Entry],
    before: u64,
    floor: usize,
    settings: &MaskSettings,
    budget: u64,
    with_store: bool,
) -> Plan {
    let mut found = candidates(entries, floor, settings, with_store);
    let potential: u64 = found.iter().map(|candidate| candidate.saved).sum();
    if potential < scaled(budget, settings.min_gain_ratio) {
        return Plan {
            picks: vec![],
            compact: true,
        };
    }
    let target = scaled(budget, settings.target_ratio);
    let mut freed = 0u64;
    let mut take = 0;
    for candidate in &found {
        if before.saturating_sub(freed) <= target {
            break;
        }
        freed += candidate.saved;
        take += 1;
    }
    found.truncate(take);
    Plan {
        picks: found,
        compact: false,
    }
}

/// Tokens of the provider-facing history: `entries` with the newest
/// applicable summary standing in for what it replaced.
fn view_tokens(entries: &[Entry], summaries: &[Value]) -> u64 {
    summaries
        .iter()
        .find_map(|record| compaction::apply_summary(entries, record))
        .map_or_else(|| entries_tokens(entries), |view| entries_tokens(&view))
}

fn index_of(entries: &[Entry]) -> Positions {
    let mut positions = Positions::new();
    for (at, (_, id)) in entries.iter().enumerate() {
        if let Some(id) = id {
            positions.entry(id.clone()).or_insert(at);
        }
    }
    positions
}

fn message_hash(message: &ChatMessage, id: &str) -> Option<String> {
    let row = json!([id, message.role, message.content, message.tool_call_id]);
    agentkit_contracts::stringify_javascript_value(&row)
        .ok()
        .map(sha256_hex)
}

/// One hash over the covered messages, built from per-message hashes so each
/// message is hashed once per assembly however many records mention it.
fn source_hash(entries: &[Entry], indexes: &[usize], hashes: &mut Hashes) -> Option<String> {
    let mut parts = Vec::with_capacity(indexes.len());
    for &index in indexes {
        let (message, id) = entries.get(index)?;
        let id = id.as_ref()?;
        if !hashes.contains_key(id) {
            hashes.insert(id.clone(), message_hash(message, id)?);
        }
        parts.push(hashes[id].clone());
    }
    let encoded = agentkit_contracts::stringify_javascript_value(&json!([HASH_TAG, parts])).ok()?;
    Some(sha256_hex(encoded))
}

/// Applies one record in place. Nothing changes unless every covered message
/// is present and its bytes still hash to the recorded source.
fn apply_record(
    entries: &mut [Entry],
    record: &Value,
    positions: &Positions,
    hashes: &mut Hashes,
) -> bool {
    let Some(replacements) = replacements(entries, record, positions, hashes) else {
        return false;
    };
    for (index, content) in replacements {
        if let Some(id) = &entries[index].1 {
            hashes.remove(id);
        }
        entries[index].0.content = content;
    }
    true
}

fn replacements(
    entries: &[Entry],
    record: &Value,
    positions: &Positions,
    hashes: &mut Hashes,
) -> Option<Vec<(usize, Value)>> {
    let mut out: Vec<(usize, Value)> = vec![];
    for mask in record["masks"].as_array()? {
        let (id, call) = (mask["messageId"].as_str()?, mask["callId"].as_str()?);
        let index = *positions.get(id)?;
        let message = &entries[index].0;
        if message.role != "tool" || message.tool_call_id.as_deref() != Some(call) {
            return None;
        }
        let body = masked_body(mask["bytes"].as_u64()?, mask["handle"].as_str()?);
        out.push((index, Value::String(body)));
    }
    for image in record["images"].as_array()? {
        let index = *positions.get(image["messageId"].as_str()?)?;
        if entries[index].0.role != "user" {
            return None;
        }
        let names = image["names"]
            .as_array()?
            .iter()
            .map(|name| name.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?;
        out.push((index, mask_images(&entries[index].0.content, &names)?));
    }
    out.sort_by_key(|(index, _)| *index);
    let indexes: Vec<usize> = out.iter().map(|(index, _)| *index).collect();
    (source_hash(entries, &indexes, hashes)? == record["sourceHash"].as_str()?).then_some(out)
}

/// The stored masks in creation order. A record whose source no longer
/// matches (edited or re-branched history) is not applied, never half-applied;
/// its mask ids come back so the caller can report the break.
fn apply_records(
    entries: &mut [Entry],
    records: &[Value],
    positions: &Positions,
    hashes: &mut Hashes,
) -> Vec<String> {
    let mut skipped = vec![];
    for record in records.iter().rev() {
        if !apply_record(entries, record, positions, hashes) {
            skipped.push(record["maskId"].as_str().unwrap_or_default().to_owned());
        }
    }
    skipped
}

/// `out_` plus 32 hex digits after the last `handle ` in a stored preview.
fn find_handle(text: &str) -> Option<OutputHandle> {
    let at = text.rfind("handle out_")? + "handle ".len();
    text.get(at..at + 36)
        .and_then(|candidate| OutputHandle::parse(candidate).ok())
}

/// The handle holding the full result. A handle quoted in the text is trusted
/// only when the stored output IS this result: same bytes (sha256 and size),
/// or the output the run stored for this very call (a preview's own handle).
/// Any other result is written now, once per content within the batch. `None`
/// leaves the result unmasked.
async fn handle_for(
    input: &Input<'_>,
    store: &Arc<dyn OutputStore>,
    message: &ChatMessage,
    call_id: &str,
    tool: &str,
    stored: &mut Stored,
) -> Option<(String, u64)> {
    let text = content_text(&message.content);
    let sha = sha256_hex(&text);
    let len = text.len() as u64;
    if let Some(handle) = find_handle(&text)
        && let Ok(info) = store.info(&handle).await
        && info.meta.session == input.chat_id
        && ((info.sha256 == sha && info.byte_len == len) || info.meta.call_id == call_id)
    {
        return Some((handle.to_string(), info.byte_len));
    }
    if let Some(known) = stored.get(&sha) {
        return Some(known.clone());
    }
    let meta = OutputMeta {
        session: input.chat_id.to_owned(),
        task: input.context.task.task_id.clone(),
        call_id: call_id.to_owned(),
        tool: tool.to_owned(),
        mime: "text/plain".into(),
        exit_code: None,
    };
    let handle = store.put(meta, text.into_bytes()).await.ok()?;
    stored.insert(sha, (handle.to_string(), len));
    Some((handle.to_string(), len))
}

pub(super) async fn apply(input: Input<'_>, entries: &mut [Entry]) -> ChatResult<Outcome> {
    if !input.settings.enabled {
        return Ok(Outcome {
            reported_break: report_disabled(input.context, input.chat_id).await?,
            ..Outcome::default()
        });
    }
    let context = input.context;
    let records = context.store.list_context_masks(input.chat_id).await?;
    let (positions, mut hashes) = (index_of(entries), Hashes::new());
    let skipped = apply_records(entries, &records, &positions, &mut hashes);
    if !skipped.is_empty() {
        report_invalidated(context, &skipped).await?;
    }
    let applied: Vec<String> = records
        .iter()
        .filter_map(|record| record["maskId"].as_str())
        .filter(|id| !skipped.iter().any(|skipped| skipped == id))
        .map(str::to_owned)
        .collect();
    let mut outcome = batch(&input, entries, &positions, &mut hashes).await?;
    outcome.applied.extend(applied);
    outcome.reported_break = !skipped.is_empty();
    Ok(outcome)
}

/// Masks a new batch when the stored ones leave the history over the trigger.
async fn batch(
    input: &Input<'_>,
    entries: &mut [Entry],
    positions: &Positions,
    hashes: &mut Hashes,
) -> ChatResult<Outcome> {
    let context = input.context;
    let Some(budget) = input
        .budget
        .map(|budget| budget.message_budget(input.tools))
    else {
        return Ok(Outcome::default());
    };
    let summaries = context.store.list_context_summaries(input.chat_id).await?;
    // What the provider gets is the view with the stored summary applied:
    // the summarized bytes no longer count towards the trigger.
    let before = view_tokens(entries, &summaries);
    if before <= scaled(budget, input.settings.trigger_ratio) {
        return Ok(Outcome::default());
    }
    let floor = summary_floor(entries, &summaries);
    let plan = plan(
        entries,
        before,
        floor,
        &input.settings,
        budget,
        input.store.is_some(),
    );
    if plan.compact {
        return Ok(Outcome {
            compact: true,
            ..Outcome::default()
        });
    }
    run_batch(
        input,
        entries,
        &plan.picks,
        (budget, before),
        positions,
        hashes,
    )
    .await
}

/// Chat metadata key: the mask ids whose removal was already reported.
const DISABLED_REPORTED_KEY: &str = "agentkit.session.maskDisabledReported";

/// Masking was switched off on a session that holds masks: the provider view
/// is now the original bytes, which changes the prefix (INV-3). That is
/// reported once for the masks it concerns; the metadata keeps the state so a
/// later turn, or another worker, does not report it again.
async fn report_disabled(context: &ExecutionContext, chat_id: &str) -> ChatResult<bool> {
    let records = context.store.list_context_masks(chat_id).await?;
    if records.is_empty() {
        return Ok(false);
    }
    let ids: Vec<Value> = records
        .iter()
        .map(|record| record["maskId"].clone())
        .collect();
    let reported = context
        .store
        .get_chat(chat_id)
        .await?
        .map(|chat| chat.metadata[DISABLED_REPORTED_KEY].clone());
    if reported == Some(Value::Array(ids.clone())) {
        return Ok(false);
    }
    let count = ids.len();
    super::session_meta::update(context, chat_id, move |metadata| {
        metadata[DISABLED_REPORTED_KEY] = Value::Array(ids);
    })
    .await?;
    context
        .emit(
            "context.cache_break",
            json!({"reason":"mask_invalidated","cause":"masking_disabled","records_skipped":count}),
        )
        .await?;
    Ok(true)
}

/// A stored record that could not apply: the provider view fell back to the
/// original bytes, which is a cache break (INV-3) and worth a warning.
async fn report_invalidated(context: &ExecutionContext, skipped: &[String]) -> ChatResult<()> {
    context
        .emit(
            "context.cache_break",
            json!({"reason":"mask_invalidated","records_skipped":skipped.len()}),
        )
        .await?;
    context
        .emit(
            "run.warning",
            json!({"code":"mask_invalidated","message":format!(
                "{} stored context mask record(s) no longer match the history and were not applied: {}.",
                skipped.len(),
                skipped.join(", ")
            )}),
        )
        .await?;
    Ok(())
}

/// Writes the picked results' outputs (the plan is final by now), saves one
/// record and masks `entries`. A batch that masked nothing, or left E above
/// the trigger, asks for compaction in the same turn.
async fn run_batch(
    input: &Input<'_>,
    entries: &mut [Entry],
    picks: &[Candidate],
    (budget, before): (u64, u64),
    positions: &Positions,
    hashes: &mut Hashes,
) -> ChatResult<Outcome> {
    let context = input.context;
    let full_before = entries_tokens(entries);
    let timestamp =
        agentkit_store::ConversationTimestamp::from_epoch_millis(context.runtime.now_ms())
            .and_then(|value| value.wire_text().map(|text| text.into_owned()))
            .map_err(|_| ChatError::InvalidRuntimeTimestamp)?;
    // Fail before any write when the picks cannot be hashed.
    let all: Vec<usize> = picks.iter().map(|candidate| candidate.index).collect();
    source_hash(entries, &all, hashes).ok_or(ChatError::SummarySourceHashFailed)?;
    let (mut masks, mut images, mut covered) = (vec![], vec![], vec![]);
    let (mut stored, mut refused) = (Stored::new(), 0usize);
    for candidate in picks {
        let (message, id) = &entries[candidate.index];
        let Some(id) = id.clone() else {
            continue;
        };
        match &candidate.kind {
            Kind::Tool { call_id, name } => {
                let Some(store) = input.store else {
                    continue;
                };
                let Some((handle, bytes)) =
                    handle_for(input, store, message, call_id, name, &mut stored).await
                else {
                    refused += 1;
                    continue;
                };
                masks.push(json!({"messageId":id,"callId":call_id,"handle":handle,"bytes":bytes}));
            }
            Kind::Images { names } => images.push(json!({"messageId":id,"names":names})),
        }
        covered.push(candidate.index);
    }
    if refused > 0 {
        context
            .emit(
                "run.warning",
                json!({"code":"mask_store_refused","message":format!("The output store refused {refused} result(s); they stay unmasked.")}),
            )
            .await?;
    }
    let source =
        source_hash(entries, &covered, hashes).ok_or(ChatError::SummarySourceHashFailed)?;
    let mut record = json!({
        "maskId": uuid::Uuid::now_v7().to_string(),
        "sessionId": input.chat_id,
        "coveredCallIds": masks.iter().map(|mask| mask["callId"].clone()).collect::<Vec<_>>(),
        "masks": masks,
        "images": images,
        "sourceHash": source,
        "tokensBefore": before,
        "tokensAfter": before,
        "createdAt": timestamp,
    });
    let mut masked = entries.to_vec();
    if covered.is_empty() || !apply_record(&mut masked, &record, positions, hashes) {
        return Ok(Outcome {
            compact: true,
            ..Outcome::default()
        });
    }
    // Masks only touch entries after the summary, so the whole saving comes
    // off the summary-applied view.
    let after = before.saturating_sub(full_before.saturating_sub(entries_tokens(&masked)));
    record["tokensAfter"] = after.into();
    commit(input, record.clone()).await?;
    entries.clone_from_slice(&masked);
    // INV-3: the cached prefix changed at the first masked message. The turn
    // reports that as one `context.cache_break` once it knows whether a
    // compaction follows (see `rewrite::emit_break`).
    Ok(Outcome {
        compact: after > scaled(budget, input.settings.trigger_ratio),
        results_masked: masks_len(&record, "masks"),
        images_masked: masks_len(&record, "images"),
        tokens: (before, after),
        applied: record["maskId"]
            .as_str()
            .map(str::to_owned)
            .into_iter()
            .collect(),
        reported_break: false,
    })
}

fn masks_len(record: &Value, key: &str) -> usize {
    record[key].as_array().map_or(0, Vec::len)
}

async fn commit(input: &Input<'_>, record: Value) -> ChatResult<()> {
    let context = input.context;
    let tx = context.store.begin_transaction().await?;
    let view = tx.view();
    view.save_context_mask(record).await?;
    view.verify_fence(&context.fence, context.runtime.now_ms())
        .await?;
    context.check_cancelled()?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
