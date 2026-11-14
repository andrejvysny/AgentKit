//! The records the sessions context's repository port speaks in (ADR 0024 §2).
//!
//! These are the session aggregate's durable shapes: the queued steering
//! input, the chat and its messages, the pinned agent definition, the
//! execution receipt, and the settlement payload a settling turn stages.
//! They live here, not in an adapter: the context owns the vocabulary its
//! port uses, and `agentkit-store` re-exports them so the names stay one
//! authority across the workspace.

use agentkit_engine::{MessageInput, SubmitTask, TaskRecord};
use agentkit_kernel::ConversationTimestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The provider-model catalog namespace a session's pinned model is read from.
pub const PROVIDER_MODELS_NAMESPACE: &str = "agentkit.compat.provider-models";

agentkit_contracts::impl_json_via_serde!(AgentDefinition);
agentkit_contracts::impl_json_via_serde!(AgentInputRecord);
agentkit_contracts::impl_json_via_serde!(AgentRecord);
agentkit_contracts::impl_json_via_serde!(ChatRecord);
agentkit_contracts::impl_json_via_serde!(ExecutionReceiptRecord);
agentkit_contracts::impl_json_via_serde!(MessageRecord);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentInputKind {
    Steer,
    FollowUp,
    HumanRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentInputStatus {
    Pending,
    Applied,
    Converted,
    Answered,
    Cancelled,
}

/// What happens to a steer whose target turn ends before it is placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentInputSettlePolicy {
    FollowUp,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInputRecord {
    /// Primary key and the caller's idempotency key.
    pub input_id: String,
    /// The chat id.
    pub session_id: String,
    /// AGENTKIT-88: the persistent agent this input is addressed to, set
    /// only for teammate-mailbox rows. Absence is the honest projection
    /// for a session input — there is no default that could misattribute
    /// an input to some agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub kind: AgentInputKind,
    pub status: AgentInputStatus,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub content: Value,
    /// Caller-supplied digest of `content`; how a replay is told from a conflict.
    pub content_hash: String,
    /// Store-assigned, strictly increasing insertion order (the FIFO key).
    pub order: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_task_id: Option<String>,
    /// Steer only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_settled: Option<AgentInputSettlePolicy>,
    /// Human request: the generation and call an answer must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Receipt: where the input landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug)]
pub struct CreateAgentInput {
    pub input_id: String,
    pub session_id: String,
    pub kind: AgentInputKind,
    pub content: Value,
    pub content_hash: String,
    pub target_task_id: Option<String>,
    pub on_settled: Option<AgentInputSettlePolicy>,
    pub attempt_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct AgentInputQuery {
    pub session_id: String,
    pub kinds: Option<Vec<AgentInputKind>>,
    pub statuses: Option<Vec<AgentInputStatus>>,
    pub target_task_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct AgentInputPatch {
    pub applied_task_id: Option<String>,
    pub applied_message_id: Option<String>,
    pub answer_id: Option<String>,
}

/// Parse the ISO-8601 subset JavaScript `Date.parse` accepts for the
/// timestamps this stack writes (`YYYY-MM-DDTHH:MM:SS.mmmZ`, optional
/// fraction, extended `+YYYYYY` years, optional `Z`/offset). `None` mirrors
/// `Date.parse` yielding `NaN`: an unreadable expiry never expires.
/// A time without an offset is read as UTC.
pub fn parse_iso_millis(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    let digits = |pos: usize, len: usize| -> Option<(i64, usize)> {
        let slice = bytes.get(pos..pos + len)?;
        if slice.is_empty() || !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        Some((std::str::from_utf8(slice).ok()?.parse().ok()?, pos + len))
    };
    let take = |pos: usize, ch: u8| -> Option<usize> {
        bytes.get(pos).filter(|byte| **byte == ch).map(|_| pos + 1)
    };
    let (year, p) = if bytes.first().is_some_and(|byte| *byte == b'+') {
        digits(1, 6)?
    } else {
        digits(0, 4)?
    };
    let p = take(p, b'-')?;
    let (month, p) = digits(p, 2)?;
    let p = take(p, b'-')?;
    let (day, p) = digits(p, 2)?;
    let (mut hour, mut minute, mut second, mut fraction) = (0, 0, 0, 0);
    if let Some(p2) = take(p, b'T') {
        let (h, p2) = digits(p2, 2)?;
        let p2 = take(p2, b':')?;
        let (m, p2) = digits(p2, 2)?;
        let p2 = take(p2, b':')?;
        let (s, mut p2) = digits(p2, 2)?;
        hour = h;
        minute = m;
        second = s;
        if let Some(p3) = take(p2, b'.') {
            let start = p3;
            let mut end = p3;
            while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            if end == start {
                return None;
            }
            let (value, _) = digits(start, (end - start).min(3))?;
            fraction = match (end - start).min(3) {
                1 => value * 100,
                2 => value * 10,
                _ => value,
            };
            p2 = end;
        }
        // Range checks precede both return paths, so an offset suffix cannot
        // smuggle an impossible calendar or clock value past them.
        if !valid_ranges(month, day, (hour, minute, second, fraction)) {
            return None;
        }
        match bytes.get(p2) {
            None => {}
            Some(b'Z') | Some(b'z') if p2 + 1 == bytes.len() => {}
            Some(sign @ (b'+' | b'-')) => {
                let offset_sign = if *sign == b'+' { 1 } else { -1 };
                let (oh, q) = digits(p2 + 1, 2)?;
                let q = take(q, b':')?;
                let (om, q) = digits(q, 2)?;
                if q != bytes.len() || oh > 23 || om > 59 {
                    return None;
                }
                return Some(
                    days_from_civil(year, month, day) * 86_400_000
                        + hour * 3_600_000
                        + minute * 60_000
                        + second * 1_000
                        + fraction
                        - offset_sign * (oh * 3_600_000 + om * 60_000),
                );
            }
            _ => return None,
        }
    } else if p != bytes.len() {
        return None;
    }
    if !valid_ranges(month, day, (hour, minute, second, fraction)) {
        return None;
    }
    Some(
        days_from_civil(year, month, day) * 86_400_000
            + hour * 3_600_000
            + minute * 60_000
            + second * 1_000
            + fraction,
    )
}

/// `Date.parse` parity: hour 24 is accepted only as exactly `24:00:00.000`.
fn valid_ranges(month: i64, day: i64, time: (i64, i64, i64, i64)) -> bool {
    let (hour, minute, second, fraction) = time;
    (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && minute <= 59
        && second <= 59
        && (hour <= 23 || hour == 24 && minute == 0 && second == 0 && fraction == 0)
}

/// Days since the Unix epoch (Howard Hinnant's civil algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The complete lifecycle vocabulary. `Deleted` is a tombstone: identity,
/// configuration, and revision ledger stay durable while every read path
/// treats the row as absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentLifecycle {
    Active,
    Archived,
    Deleted,
}

/// What produced one configuration revision. The ledger vocabulary is
/// closed: `create` is written only by identity creation (including a
/// duplicate's fresh first revision), `model_change` only by a model
/// preference change, `restore` only by an archive→active restore, `edit`
/// by every other accepted definition update.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRevisionSource {
    Create,
    Edit,
    ModelChange,
    Restore,
}

/// One selected skill under an immutable revision, the same
/// `{id, revision}` pairing the managed-content store keys published
/// skill documents by.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSkillSelection {
    pub id: String,
    pub revision: String,
}

/// Budget ceilings of a definition. Every present limit is a positive
/// safe integer; absence means "this ceiling is unset, the host default
/// governs".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_iterations: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls_per_iteration: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_budget_ms: Option<i64>,
}

/// The effective configuration of one agent — the snapshot unit the
/// revision ledger appends. `deny_unknown_fields` is the smuggling gate:
/// there is no field that could carry a secret value or a foreign scope,
/// and an envelope or payload that tries answers with a validation error.
/// Credential material enters only through `secret_refs`: opaque,
/// scheme-qualified references resolved by the secret store, never values.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub instructions: String,
    /// Scoped resource references this agent may use. These are GRANTS
    /// (run-profile resolution treats membership as authority), so
    /// duplicate and import clear them rather than copy them.
    #[serde(default)]
    pub resources: Vec<String>,
    /// Skills selected under pinned revisions.
    #[serde(default)]
    pub skills: Vec<AgentSkillSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// The permission CEILING (`read_only|ask|auto_edit|full_auto|plan`,
    /// the core permission vocabulary the host service validates); a
    /// ceiling narrows what a run under this agent may ever attempt, it
    /// grants nothing by itself.
    #[serde(default)]
    pub permission_mode: String,
    /// Tool allow-list ceiling; empty means the host default set.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub budgets: AgentBudgets,
    /// Secret REFERENCES only (e.g. `vault://...`); a value here is refused
    /// at write time. Duplicate strips these by default.
    #[serde(default)]
    pub secret_refs: Vec<String>,
}

/// One durable agent row: identity, lifecycle, the current effective
/// definition, and the timestamps. Serialized whole into `agents.record`;
/// the indexed columns mirror exactly these fields at write time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRecord {
    pub agent_id: String,
    pub scope: String,
    pub owner_id: String,
    pub lifecycle: AgentLifecycle,
    pub current_revision: i64,
    pub definition: AgentDefinition,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRecord {
    pub id: String,
    pub title: Option<String>,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub metadata: Value,
    pub archived: bool,
    pub created_at: ConversationTimestamp,
    pub updated_at: ConversationTimestamp,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRecord {
    pub id: String,
    pub chat_id: String,
    pub run_id: Option<String>,
    pub role: String,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub content: Value,
    pub parent_message_id: Option<String>,
    pub tool_call_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "agentkit_contracts::deserialize_preserving"
    )]
    pub tool_calls: Option<Value>,
    pub model_result_json: Option<String>,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub metadata: Value,
    pub order_key: u64,
    pub depth: u32,
    pub branch_index: u32,
    pub active: bool,
    /// Archived messages remain readable by id for audit but never replay:
    /// `archive_branch` marks a whole branch and the listing ports hide them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    pub created_at: ConversationTimestamp,
}

/// One durable execution receipt. Field names mirror the TypeScript
/// reference implementation (`ToolExecutionRecord`) so cross-language
/// fixtures compare one-to-one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionReceiptRecord {
    /// Deterministic id: sha-256 hex over the canonical
    /// `{runId, toolCallId, approvalRequestId ?? "initial"}` binding.
    pub execution_id: String,
    pub run_id: String,
    /// The chat whose runs own this effect. Persisted in the record so
    /// chat-scoped queries (uncertainty across runs, recovery) need no
    /// join onto the continuation binding table — session turns have
    /// receipts but no provider continuation.
    pub chat_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    /// sha-256 hex of the canonical-JSON arguments.
    pub args_sha256: String,
    /// Which canonicalizer produced `args_sha256`. Native admissions write
    /// the native hash (the field is then omitted); imported rows carry
    /// `ts-canonical` so the guard consults both hashers.
    #[serde(default, skip_serializing_if = "ArgsHashProvenance::is_canonical")]
    pub args_hash_provenance: ArgsHashProvenance,
    /// sha-256 hex of the host execution-context binding.
    pub context_sha256: String,
    pub attempt_id: String,
    /// The answered approval input consumed by this execution (single-use
    /// via the `approval_request_id` UNIQUE index); absent until the
    /// approval flow spends one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_request_id: Option<String>,
    /// Authorization generation being spent; absent until approvals exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_generation: Option<String>,
    pub status: ExecutionReceiptStatus,
    /// Serialized model-facing `ToolResult`, present iff `completed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub result: Option<serde_json::Value>,
    /// Audit of an explicit operator resolution of an uncertain receipt;
    /// absent for receipts the normal execution path completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<ExecutionResolution>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionReceiptStatus {
    Admitted,
    Completed,
    /// Closed by an operator as `not_started` / `acknowledged`: no result
    /// exists, and the receipt no longer blocks an identical reissue.
    Resolved,
}

/// How an operator closed an uncertain (admitted, no result) receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionOutcome {
    /// The effect happened; the operator supplied its result.
    Completed,
    /// The effect never started; reissuing is safe.
    NotStarted,
    /// The operator accepts the uncertainty and lets a reissue proceed.
    Acknowledged,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionResolution {
    pub outcome: ResolutionOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub resolved_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
}

/// Which canonicalizer produced a receipt's `argsSha256`. The two ports do
/// NOT agree on one canonical JSON: the TypeScript
/// `packages/host/src/sessions/hash.ts` runs `Object.entries(...).sort()`
/// through `Object.fromEntries` + `JSON.stringify`, and JavaScript reorders
/// array-index keys (canonical decimal integers < 2^53 are object-property
/// ordering law) numerically FIRST, then strings in the sorted insertion
/// order — so `{ "10": "a", "2": "b" }` stringifies as `{"2":"b","10":"a"}`,
/// while the Rust map (`BTreeMap`) serializes `{ "10": "a", "2": "b" }`
/// lexically. An imported receipt therefore carries a hash the native
/// canonicalizer can never reproduce from the same arguments; without this
/// provenance dimension the cross-run uncertainty guard (which matches on
/// tool name + `args_sha256`) would read an imported admitted effect as
/// absence and let the identical mutation run a second time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArgsHashProvenance {
    /// The native Rust canonicalizer (`durable_execution::args_sha256`).
    /// Absent from the record JSON means exactly this — existing native
    /// receipts keep their bytes.
    #[default]
    Canonical,
    /// The TypeScript pipeline (`canonicalJson` → `sha256Hex`, verified
    /// against the real `hash.ts` under bun). Only the importer stamps it.
    TsCanonical,
}

impl ArgsHashProvenance {
    /// The serde default is `canonical`, so the field is omitted from the
    /// record JSON for native rows: the wire identity of a native receipt
    /// never changes.
    fn is_canonical(&self) -> bool {
        *self == Self::Canonical
    }
}

/// Stable message prefix for "this receipt is not (or no longer) uncertain".
pub const RECEIPT_NOT_UNCERTAIN: &str = "receipt_not_uncertain";
/// Stable message prefix: the receipt's run may still execute its tool body.
pub const RECEIPT_RUN_ACTIVE: &str = "receipt_run_active";

#[derive(Clone, Debug)]
pub struct ResolveExecution {
    pub outcome: ResolutionOutcome,
    /// Required (a tool-result object) for `Completed`; forbidden otherwise.
    pub result: Option<serde_json::Value>,
    pub note: Option<String>,
    pub resolved_by: Option<String>,
    pub now: i64,
}

/// it authorizes (ADR 0016 §5). The host resolves the plan policy BEFORE the
/// transaction (`policy_denial`, `granted_mode`); the authoritative grant
/// write still happens here, against the record re-read inside.
#[derive(Clone)]
pub struct AnswerHumanRequest {
    pub request_id: String,
    pub answer_id: String,
    /// The caller's JSON response (raw, before envelope wrapping).
    pub response: Value,
    /// sha-256 hex of the serialized response; duplicate detection and the
    /// approve marker compare against it.
    pub response_hash: String,
    pub now: i64,
    /// Id for the tool record a non-approval answer appends.
    pub message_id: String,
    /// Tool names whose approvable refusal is a PLAN approval.
    pub plan_tool_names: Vec<String>,
    /// Host plan-veto resolved before the transaction: the answer is forced
    /// to deny with `plan_not_authorized: {reason}`.
    pub policy_denial: Option<String>,
    /// The mode already resolved from the plan policy; required when this
    /// answer approves a plan.
    pub granted_mode: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnswerReceipt {
    pub request_id: String,
    pub answer_id: String,
    pub turn_id: String,
    pub message_id: String,
    /// True when this exact answer had already been recorded.
    pub duplicate: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnswerOutcome {
    pub receipt: AnswerReceipt,
    /// Task to hand to the queue after the transaction commits.
    pub enqueue: Option<(String, String)>,
}

/// A follow-up the settling turn converts into a task on its way out.
#[derive(Clone)]
pub struct StagedCandidate {
    pub input_id: String,
    pub task: SubmitTask,
    pub messages: Vec<MessageInput>,
}

/// The config every NEWLY STAGED turn of a session carries, captured from
/// the session descriptor at the moment the task is created.
///
/// This is THE staging contract: normal submission, an explicit queue flush,
/// settlement promotion, and the recovery of a half-admitted input all stamp
/// a turn from this snapshot, so a promoted turn is what a submission made
/// NOW would have staged. It exists because a settlement used to template
/// its missed staging off the SETTLING task's payload — a turn created
/// before a mid-flight `set_model` — and so handed the next turn a stale
/// model. Nothing may clone a live task's payload to stage a new turn.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnStaging {
    /// The task kind this session's turns run under.
    pub kind: String,
    /// The payload keys the descriptor carries at task creation (`chatId`,
    /// `model`, `providerId`, `reasoningEffort`). Message ids are added per
    /// promotion by [`stage_missed`].
    pub config: Value,
}

/// The host-supplied conversion candidates, in queue order (follow-ups
/// first, then steers not marked `cancel`). They are the settlement's
/// preferred staging, not its whole decision: the settle transaction re-reads
/// the queue itself and stages anything the snapshot could not have named
/// (see `settle_session_inputs`), so no interleaving can leave a promotable
/// input pending behind a terminal turn.
#[derive(Clone)]
pub struct SessionSettle {
    pub convert: Vec<StagedCandidate>,
    /// The exact persisted descriptor used by the session planner. The
    /// transaction validates both promotion and refusal decisions against
    /// this snapshot; even an unsupported config may become supported before
    /// commit. `None` is for callers without a session descriptor.
    pub expected_descriptor: Option<Value>,
    /// The authoritative config for inputs the snapshot missed. `None`
    /// promotes nothing missed — fail-closed, the inputs stay Pending and
    /// visible for an explicit flush — because promoting without a current
    /// descriptor is how a stale model gets stamped onto the next turn.
    pub staging: Option<TurnStaging>,
}

/// Stage one queued input into a turn when the session's scope is free.
/// The busy check and the conversion share one actor job, so a flush can
/// never stage a second turn behind a running one.
pub struct StageQueuedTurn {
    pub session_id: String,
    /// Task kind the busy check matches against (the session's turn kind).
    pub task_kind: String,
    /// The turn whose settlement triggered this flush (excluded from busy).
    pub exclude_task_id: Option<String>,
    pub input_id: String,
    pub expected_kind: AgentInputKind,
    pub task: SubmitTask,
    pub messages: Vec<MessageInput>,
    pub now: i64,
}

/// The input the unified admission writes, and the turn it becomes when the
/// scope turns out to be free. See the port's `queue_or_stage_turn`.
pub struct QueueOrStage {
    pub session_id: String,
    /// The session's turn kind, whose settlement owns this input's promotion.
    pub task_kind: String,
    pub input: CreateAgentInput,
    /// Fill a queued steer's target with the live turn the check finds.
    pub target_the_live_turn: bool,
    /// The fully staged turn, built by the host under [`TurnStaging`];
    /// `None` when the content can never become a turn (the input then only
    /// ever queues, and stays visible).
    pub candidate: Option<StagedCandidate>,
    pub now: i64,
}

/// What the port's `queue_or_stage_turn` committed.
pub enum QueueAdmission {
    /// Stored Pending behind a live turn (or already settled).
    Queued(AgentInputRecord),
    /// No active turn could claim it, so this write staged it.
    Staged(TaskRecord),
}

impl agentkit_engine::SessionSettlement for SessionSettle {
    fn staged_kinds(&self) -> Vec<String> {
        self.convert
            .iter()
            .map(|candidate| candidate.task.kind.clone())
            .chain(self.staging.iter().map(|staging| staging.kind.clone()))
            .collect()
    }

    fn without_promotion(self) -> Self {
        Self {
            convert: vec![],
            staging: None,
            expected_descriptor: self.expected_descriptor,
        }
    }
}

/// Canonical (recursively key-sorted) serialization digest of a profile
/// document. The single digest authority: the resolver seals with it, the
/// adapter recomputes it on every read, and a seal that does not match the
/// bytes fails closed.
pub fn run_profile_digest(profile: &Value) -> String {
    let canonical = canonicalize(profile);
    // Value serialization cannot fail; sorting keys makes it order-stable.
    let text = serde_json::to_string(&canonical).expect("canonical profile serialization");
    agentkit_kernel::sha256_hex(text)
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                out.insert(key.clone(), canonicalize(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}
