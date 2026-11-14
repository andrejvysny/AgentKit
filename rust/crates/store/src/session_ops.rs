//! Durable session mechanics (ADR 0016 §2/§5, ADR 0019).
//!
//! The atomic units the session service runs as single store transactions:
//! parking a turn on a human request, answering one (with the exact-call
//! approval, plan-grant, and standing-approval bookkeeping of
//! `human-answer.ts`), settling with queued-turn conversion, and the
//! interrupt/park pair. Chat metadata is the durable home of the session
//! descriptor, the execution grant, and the standing approvals, so every
//! mutation of those keys is a read-modify-write inside one transaction.

use crate::ConversationTimestamp;
use crate::agent_inputs::{
    AgentInputPatch, AgentInputQuery, AgentInputRecord, AgentInputStatus, CreateAgentInput,
    get_input, insert_input, list_inputs, parse_iso_millis, transition_input,
};
use crate::conversations::{MessageRecord, save_message};
use crate::tasks::{
    append as append_task_events, end_attempt, lifecycle_event, save, validate_projection,
};
use crate::util::{json, required};
use crate::{
    ChatRecord, Lease, LeaseFence, MessageInput, Result, SqliteStore, StoreError, SubmitTask,
    TaskRecord, TaskStatus,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

/// The chat-metadata vocabulary and its read/write rules are the sessions
/// context's (ADR 0024 §2); this adapter applies them inside one transaction.
pub use agentkit_sessions::{
    APPROVE_MARKER, COMPACT_REQUESTED_METADATA_KEY, Decision, EXECUTION_METADATA_KEY,
    ExecutionStateRead, SESSION_METADATA_KEY, STANDING_APPROVALS_METADATA_KEY,
    find_active_standing_grant, inspect_execution_state, is_approval_request, is_hex64,
    parse_answer_expiry, plan_request_of, read_execution_state, read_standing_key, standing_grants,
    with_execution, with_standing,
};
/// The settlement, admission and answer payloads are the sessions context's
/// (ADR 0024 §2): the port speaks them, this adapter reads and writes them.
pub use agentkit_sessions::{
    AnswerHumanRequest, AnswerOutcome, AnswerReceipt, QueueAdmission, QueueOrStage, SessionSettle,
    StageQueuedTurn, StagedCandidate, TurnStaging,
};
use agentkit_sessions::{execution_state_for_grant, read_decision as shared_read_decision};

// ---------------------------------------------------------------------------
// Chat metadata helpers (shared by the transactional writers below and by
// the host's session service reads).
// ---------------------------------------------------------------------------

fn set_metadata(db: &Connection, chat: &mut ChatRecord, metadata: Value, now: i64) -> Result<()> {
    chat.metadata = metadata;
    chat.updated_at = ConversationTimestamp::from_epoch_millis(now)?;
    db.execute(
        "UPDATE chats SET record=? WHERE id=?",
        params![json(db, chat)?, chat.id],
    )?;
    Ok(())
}

fn writable_standing(metadata: &Value, session_id: &str) -> Result<Vec<Value>> {
    agentkit_sessions::writable_standing_grants(metadata).ok_or_else(|| {
        StoreError::InvalidInput(format!(
            "The session's standing approvals are unreadable and were not overwritten. ({session_id})"
        ))
    })
}

/// The grants after making room for one more; revoked grants drop oldest
/// first, a full list of active ones refuses.
fn make_room(grants: Vec<Value>, session_id: &str) -> Result<Vec<Value>> {
    agentkit_sessions::make_room(grants).ok_or_else(|| {
        StoreError::InvalidInput(format!(
            "A session keeps at most {} standing approvals; revoke one first. ({session_id})",
            agentkit_sessions::MAX_STANDING_APPROVALS
        ))
    })
}

fn write_standing_grant(db: &Connection, session_id: &str, grant: &Value, now: i64) -> Result<()> {
    let mut chat: ChatRecord = required(db, "chats", "id", session_id)?;
    let grants = writable_standing(&chat.metadata, session_id)?;
    let grant_id = grant.get("grantId").and_then(Value::as_str);
    if grants
        .iter()
        .any(|existing| existing.get("grantId").and_then(Value::as_str) == grant_id)
    {
        // Idempotent on `grantId`: an existing grant (even a revoked one) is
        // left alone, so replaying an answer can never resurrect a revocation.
        return Ok(());
    }
    let mut next = make_room(grants, session_id)?;
    next.push(grant.clone());
    let metadata = with_standing(&chat.metadata, next);
    set_metadata(db, &mut chat, metadata, now)
}

// ---------------------------------------------------------------------------
// Approval request content helpers (shared with the host).
// ---------------------------------------------------------------------------

/// The answer-shape rule is the sessions context's; this adapter renders its
/// refusal as its own invalid-input fault.
fn read_decision(response: &Value) -> Result<Decision> {
    shared_read_decision(response).map_err(StoreError::InvalidInput)
}

fn answer_envelope(response: &Value) -> String {
    json!({
        "ok": true, "status": "ok", "summary": "The user answered.",
        "warnings": [], "truncated": false,
        "data": {"answer": response},
    })
    .to_string()
}

fn deny_envelope(note: &Option<String>) -> String {
    let message = note
        .clone()
        .unwrap_or_else(|| "The user denied this tool call.".into());
    json!({
        "ok": false, "status": "error", "summary": "The user denied this tool call.",
        "warnings": [], "truncated": false,
        "data": {
            "errorCode": "approval_denied",
            "errorMessage": message,
            "phase": "guard",
            "retryable": false,
        },
    })
    .to_string()
}

fn request_tool_name(content: &Value) -> String {
    content
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or("request_user_input")
        .to_owned()
}

fn stale(request_id: &str, reason: &str) -> StoreError {
    StoreError::AnswerStale(format!(
        "Request {request_id} can no longer be answered: {reason}."
    ))
}

/// The message of a typed stale-answer refusal. The session layer surfaces
/// it verbatim under `answer_stale`; unrelated transitions remain invalid state.
pub fn answer_stale_reason(error: &StoreError) -> Option<&str> {
    match error {
        StoreError::AnswerStale(message) => Some(message),
        _ => None,
    }
}

/// The session layer's pre-transaction mirror of the settlement's first
/// write-free act: an approval answer must read `{ decision, note?,
/// remember? }` before any stale gate speaks for the request, exactly as
/// TypeScript's `readDecision` precedes them in the answer transaction.
pub fn check_approval_answer_shape(content: &Value, response: &Value) -> Result<()> {
    if is_approval_request(content) {
        read_decision(response).map(|_| ())
    } else {
        Ok(())
    }
}

fn append_answer(db: &Connection, input: &AppendAnswer, now: i64) -> Result<()> {
    let envelope = &input.envelope_json;
    let message = MessageInput {
        id: Some(input.id.clone()),
        chat_id: input.request.session_id.clone(),
        run_id: Some(input.task_id.clone()),
        role: "tool".into(),
        content: Value::String(envelope.clone()),
        parent_message_id: Some(input.parent_message_id.clone()),
        tool_call_id: input.request.tool_call_id.clone(),
        model_result_json: Some(envelope.clone()),
        metadata: json!({
            "internal": true,
            "toolName": request_tool_name(&input.request.content),
            "humanAnswer": true,
            "answerId": input.answer_id,
            "requestId": input.request.input_id,
            "responseHash": input.response_hash,
        }),
        activate: false,
        ..MessageInput::default()
    };
    crate::conversations::append(db, message, now)?;
    Ok(())
}

struct AppendAnswer {
    id: String,
    request: AgentInputRecord,
    task_id: String,
    parent_message_id: String,
    envelope_json: String,
    answer_id: String,
    response_hash: String,
}

pub(crate) fn last_message_of_run(
    db: &Connection,
    chat_id: &str,
    run_id: &str,
) -> Result<Option<MessageRecord>> {
    db.query_row(
        "SELECT record FROM messages WHERE chat_id=?1 
         AND json_extract(record,'$.runId')=?2 ORDER BY order_key DESC LIMIT 1",
        params![chat_id, run_id],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|text| serde_json::from_str(&text).map_err(Into::into))
    .transpose()
}

// ---------------------------------------------------------------------------
// Answer
// ---------------------------------------------------------------------------

impl SqliteStore {
    /// Record a human's answer and re-admit the parked task, all in ONE
    /// transaction. The task transition goes first: it is the write that
    /// rejects when the turn was cancelled or already resumed.
    pub async fn answer_human_request(&self, input: AnswerHumanRequest) -> Result<AnswerOutcome> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let now_text = ConversationTimestamp::from_epoch_millis(input.now)?
                .wire_text()?
                .into_owned();
            let request = get_input(&transaction, &input.request_id)?.ok_or_else(|| {
                StoreError::NotFound(format!("Human request not found: {}", input.request_id))
            })?;
            if request.kind != crate::agent_inputs::AgentInputKind::HumanRequest {
                return Err(StoreError::NotFound(format!(
                    "Human request not found: {}",
                    input.request_id
                )));
            }
            let approval0 = if is_approval_request(&request.content) {
                Some(read_decision(&input.response)?)
            } else {
                None
            };
            let approval = match (&approval0, &input.policy_denial) {
                (Some(decision), None) => Some(decision.clone()),
                (Some(_), Some(reason)) => Some(Decision {
                    approve: false,
                    note: Some(format!("plan_not_authorized: {reason}")),
                    remember: None,
                }),
                (None, _) => None,
            };
            let envelope_json = match &approval {
                Some(decision) if !decision.approve => deny_envelope(&decision.note),
                _ => answer_envelope(&input.response),
            };
            let task_id = request
                .target_task_id
                .clone()
                .ok_or_else(|| stale(&input.request_id, "it names no turn"))?;
            let task: Option<TaskRecord> = get_task_row(&transaction, &task_id)?;
            let enqueue_if_queued = |task: &Option<TaskRecord>| {
                task.as_ref()
                    .filter(|task| task.status == TaskStatus::Queued)
                    .map(|task| (task.task_id.clone(), task.scope_id.clone()))
            };
            let marker = format!("{APPROVE_MARKER}{}", input.response_hash);

            if request.status == AgentInputStatus::Answered
                && request
                    .applied_message_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with(APPROVE_MARKER))
            {
                // An approve has no tool record: the marker carries the
                // response hash.
                if request.answer_id.as_deref() != Some(input.answer_id.as_str())
                    || request.applied_message_id.as_deref() != Some(marker.as_str())
                {
                    return Err(StoreError::AnswerConflict(format!(
                        "answer_conflict: Request {} was already answered differently.",
                        input.request_id
                    )));
                }
                // Observation only. The marker and every grant it authorizes
                // commit in one SQLite transaction, so a replayed answer can
                // never be owed a grant; re-applying one here would undo a
                // later revocation, mode restriction or newer grant.
                transaction.commit()?;
                return Ok(AnswerOutcome {
                    receipt: AnswerReceipt {
                        request_id: input.request_id,
                        answer_id: input.answer_id,
                        turn_id: task_id,
                        message_id: String::new(),
                        duplicate: true,
                    },
                    enqueue: enqueue_if_queued(&task),
                });
            }

            if request.status == AgentInputStatus::Answered {
                let recorded = request
                    .applied_message_id
                    .as_deref()
                    .map(|id| crate::util::get::<MessageRecord>(&transaction, "messages", "id", id))
                    .transpose()?
                    .flatten();
                // Observation only. The answer record and its receipt commit
                // in one SQLite transaction; a receipt naming a record that
                // is not there is missing evidence, not a retryable half
                // commit, and must never be rewritten as a fresh answer.
                if request.answer_id.as_deref() != Some(input.answer_id.as_str())
                    || recorded
                        .as_ref()
                        .and_then(|record| record.metadata.get("responseHash"))
                        .and_then(Value::as_str)
                        != Some(input.response_hash.as_str())
                {
                    return Err(StoreError::AnswerConflict(format!(
                        "answer_conflict: Request {} was already answered differently.",
                        input.request_id
                    )));
                }
                transaction.commit()?;
                return Ok(AnswerOutcome {
                    receipt: AnswerReceipt {
                        request_id: input.request_id,
                        answer_id: input.answer_id,
                        turn_id: task_id,
                        message_id: request.applied_message_id.unwrap_or_default(),
                        duplicate: true,
                    },
                    // The first answer may have committed and died before it
                    // could poke.
                    enqueue: enqueue_if_queued(&task),
                });
            }
            if request.status == AgentInputStatus::Cancelled {
                return Err(stale(&input.request_id, "it was cancelled"));
            }
            let task = task
                .filter(|task| task.status == TaskStatus::WaitingApproval)
                .ok_or_else(|| stale(&input.request_id, "its turn is no longer waiting"))?;
            if request
                .expires_at
                .as_deref()
                .and_then(parse_iso_millis)
                .is_some_and(|expires| expires <= input.now)
            {
                return Err(stale(&input.request_id, "it expired"));
            }

            if approval.as_ref().is_some_and(|decision| decision.approve) {
                // Refuse before committing anything the grant could not
                // follow through on.
                if approval.as_ref().and_then(|decision| decision.remember) == Some(true) {
                    if read_standing_key(&request.content).is_none() {
                        return Err(StoreError::InvalidInput(format!(
                            "This call is not eligible for a standing approval. ({})",
                            input.request_id
                        )));
                    }
                    let grant_id = format!("grant:{}", input.request_id);
                    let chat: ChatRecord =
                        required(&transaction, "chats", "id", &request.session_id)?;
                    let grants = writable_standing(&chat.metadata, &request.session_id)?;
                    if !grants.iter().any(|grant| {
                        grant.get("grantId").and_then(Value::as_str) == Some(grant_id.as_str())
                    }) {
                        make_room(grants, &request.session_id)?;
                    }
                }
                let transition = (|| -> Result<()> {
                    requeue_task(&transaction, &task, input.now)?;
                    transition_input(
                        &transaction,
                        &input.request_id,
                        &[AgentInputStatus::Pending],
                        AgentInputStatus::Answered,
                        &AgentInputPatch {
                            answer_id: Some(input.answer_id.clone()),
                            applied_message_id: Some(marker.clone()),
                            ..Default::default()
                        },
                        &now_text,
                    )?;
                    if let (Some(plan), Some(granted_mode)) = (
                        approval0
                            .as_ref()
                            .filter(|decision| decision.approve)
                            .and_then(|_| {
                                plan_request_of(&request.content, &input.plan_tool_names)
                            }),
                        input.granted_mode.as_deref(),
                    ) {
                        apply_plan_grant(
                            &transaction,
                            &request.session_id,
                            &PlanGrant {
                                request_id: &input.request_id,
                                turn_id: &task_id,
                                plan: &plan,
                                granted_mode,
                            },
                            &now_text,
                            input.now,
                        )?;
                    }
                    // AFTER both transitions: a stale, cancelled or
                    // lost-race answer threw above and never reaches this
                    // line, so it can never leave a grant.
                    if approval.as_ref().and_then(|decision| decision.remember) == Some(true) {
                        write_request_grant(
                            &transaction,
                            &request,
                            &input.answer_id,
                            &now_text,
                            input.now,
                        )?;
                    }
                    Ok(())
                })();
                if let Err(error) = transition {
                    if matches!(error, StoreError::AgentInputTransition(_)) {
                        return Err(stale(
                            &input.request_id,
                            "a concurrent answer or cancel won",
                        ));
                    }
                    return Err(error);
                }
                transaction.commit()?;
                return Ok(AnswerOutcome {
                    receipt: AnswerReceipt {
                        request_id: input.request_id,
                        answer_id: input.answer_id,
                        turn_id: task_id.clone(),
                        message_id: String::new(),
                        duplicate: false,
                    },
                    enqueue: Some((task_id, task.scope_id)),
                });
            }

            let last = last_message_of_run(&transaction, &request.session_id, &task_id)?
                .ok_or_else(|| {
                    StoreError::NotFound(format!("Turn {task_id} has no run records."))
                })?;
            let transition = (|| -> Result<()> {
                requeue_task(&transaction, &task, input.now)?;
                transition_input(
                    &transaction,
                    &input.request_id,
                    &[AgentInputStatus::Pending],
                    AgentInputStatus::Answered,
                    &AgentInputPatch {
                        answer_id: Some(input.answer_id.clone()),
                        applied_message_id: Some(input.message_id.clone()),
                        ..Default::default()
                    },
                    &now_text,
                )?;
                Ok(())
            })();
            if let Err(error) = transition {
                if matches!(error, StoreError::AgentInputTransition(_)) {
                    return Err(stale(
                        &input.request_id,
                        "a concurrent answer or cancel won",
                    ));
                }
                return Err(error);
            }
            append_answer(
                &transaction,
                &AppendAnswer {
                    id: input.message_id.clone(),
                    request: request.clone(),
                    task_id: task_id.clone(),
                    parent_message_id: last.id,
                    envelope_json: envelope_json.clone(),
                    answer_id: input.answer_id.clone(),
                    response_hash: input.response_hash.clone(),
                },
                input.now,
            )?;
            transaction.commit()?;
            Ok(AnswerOutcome {
                receipt: AnswerReceipt {
                    request_id: input.request_id,
                    answer_id: input.answer_id,
                    turn_id: task_id.clone(),
                    message_id: input.message_id,
                    duplicate: false,
                },
                enqueue: Some((task_id, task.scope_id)),
            })
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Task row helpers shared by the compound operations.
// ---------------------------------------------------------------------------

fn get_task_row(db: &Connection, task_id: &str) -> Result<Option<TaskRecord>> {
    crate::util::get(db, "tasks", "id", task_id)
}

fn requeue_task(db: &Connection, task: &TaskRecord, now: i64) -> Result<()> {
    let mut task = task.clone();
    task.status = TaskStatus::Queued;
    task.available_at = now;
    task.finished_at = None;
    save(db, &task)
}

/// Write the plan grant into the session's execution state (the one
/// authoritative write; the host resolves the policy beforehand).
/// The host-authorized plan grant the store writes into the session's
/// execution state — the one authoritative write of that state.
struct PlanGrant<'a> {
    request_id: &'a str,
    turn_id: &'a str,
    plan: &'a Value,
    granted_mode: &'a str,
}

fn apply_plan_grant(
    db: &Connection,
    session_id: &str,
    grant: &PlanGrant<'_>,
    now_text: &str,
    now: i64,
) -> Result<()> {
    let PlanGrant {
        request_id,
        turn_id,
        plan,
        granted_mode,
    } = *grant;
    if granted_mode == "plan" || granted_mode.is_empty() {
        return Err(StoreError::InvalidInput(format!(
            "Plan approval cannot grant plan mode. ({request_id})"
        )));
    }
    let mut chat: ChatRecord = required(db, "chats", "id", session_id)
        .map_err(|_| StoreError::NotFound(format!("Session {session_id} has no chat to grant.")))?;
    let prior = read_execution_state(&chat.metadata);
    // Absent optional keys are OMITTED, never written as null: a legal
    // submit_plan without a mode must not persist `"requestedMode": null`,
    // which the strict reader below (correctly) calls malformed and which
    // would then erase the whole execution state from `get_execution`.
    let mut grant_record = serde_json::Map::new();
    grant_record.insert("grantId".into(), format!("grant:{request_id}").into());
    grant_record.insert(
        "planDigest".into(),
        plan.get("planDigest")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
    );
    if let Some(text) = plan.get("plan").and_then(Value::as_str) {
        grant_record.insert("plan".into(), text.into());
    }
    grant_record.insert("mode".into(), granted_mode.into());
    if let Some(mode) = plan.get("requestedMode").and_then(Value::as_str) {
        grant_record.insert("requestedMode".into(), mode.into());
    }
    grant_record.insert("requestId".into(), request_id.into());
    grant_record.insert("turnId".into(), turn_id.into());
    grant_record.insert("approvedAt".into(), now_text.into());
    let grant = Value::Object(grant_record);
    let state = execution_state_for_grant(&grant, now_text, prior).ok_or_else(|| {
        StoreError::InvalidInput(format!(
            "Plan approval grant is invalid: mode \"plan\" is never grantable and planDigest is required. ({request_id})"
        ))
    })?;
    let metadata = with_execution(&chat.metadata, Some(state));
    set_metadata(db, &mut chat, metadata, now)
}

fn write_request_grant(
    db: &Connection,
    request: &AgentInputRecord,
    answer_id: &str,
    now_text: &str,
    now: i64,
) -> Result<()> {
    let Some((digest, label)) = read_standing_key(&request.content) else {
        return Err(StoreError::InvalidInput(format!(
            "This call is not eligible for a standing approval. ({})",
            request.input_id
        )));
    };
    let grant = json!({
        "grantId": format!("grant:{}", request.input_id),
        "digest": digest,
        "label": label,
        "toolName": request_tool_name(&request.content),
        "sourceRequestId": request.input_id,
        "answerId": answer_id,
        "grantedAt": now_text,
    });
    write_standing_grant(db, &request.session_id, &grant, now)
}

impl SqliteStore {
    /// Revoke a standing approval. Unknown id is not found; a second revoke
    /// is the idempotent same answer.
    pub async fn revoke_standing_approval(
        &self,
        session_id: String,
        grant_id: String,
        now: i64,
    ) -> Result<Value> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let mut chat: ChatRecord = required(&transaction, "chats", "id", &session_id)?;
            let mut grants = writable_standing(&chat.metadata, &session_id)?;
            let Some(index) = grants.iter().position(|grant| {
                grant.get("grantId").and_then(Value::as_str) == Some(grant_id.as_str())
            }) else {
                return Err(StoreError::NotFound(format!(
                    "Standing approval not found: {grant_id} ({session_id})"
                )));
            };
            if grants[index]
                .get("revokedAt")
                .is_some_and(|value| !value.is_null())
            {
                return Ok(grants[index].clone());
            }
            let now_text = ConversationTimestamp::from_epoch_millis(now)?
                .wire_text()?
                .into_owned();
            grants[index]["revokedAt"] = now_text.clone().into();
            let metadata = with_standing(&chat.metadata, grants.clone());
            set_metadata(&transaction, &mut chat, metadata, now)?;
            transaction.commit()?;
            Ok(grants[index].clone())
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Parking a turn on a human request (ADR 0016 §2).
// ---------------------------------------------------------------------------

/// An in-flight assistant record the host wants finalized BEFORE the park
/// commits: `(message id, content)`.
pub struct ParkTurn {
    pub fence: LeaseFence,
    pub assistant_message: Option<(String, Value)>,
    pub events: Vec<Value>,
    pub request: Option<CreateAgentInput>,
    pub now: i64,
}

impl SqliteStore {
    /// Commit a parked turn: RUNNING -> WAITING_APPROVAL, the attempt ends
    /// normally, the human request exists — in ONE transaction, so history
    /// never shows a running task without its request, and a parked turn
    /// never holds the scope.
    pub async fn park_turn(&self, input: ParkTurn) -> Result<Option<String>> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            crate::tasks::fence(&transaction, &input.fence, input.now)?;
            let mut task: TaskRecord = required(&transaction, "tasks", "id", &input.fence.task_id)?;
            if task.status != TaskStatus::Running || task.cancel_requested {
                return Err(StoreError::LeaseLost(input.fence.task_id.clone()));
            }
            let now = input.now;
            task.status = TaskStatus::WaitingApproval;
            save(&transaction, &task)?;
            lifecycle_event(&transaction, &task, "task.waiting_approval", now)?;
            let lease: Lease = required(&transaction, "leases", "task_id", &task.task_id)?;
            end_attempt(&transaction, &lease, "completed", now, None)?;
            append_task_events(&transaction, &task.task_id, input.events, now)?;
            let now_text = ConversationTimestamp::from_epoch_millis(now)?
                .wire_text()?
                .into_owned();
            let mut request_id = None;
            if let Some(request) = input.request {
                let (record, _) = insert_input(&transaction, request, &now_text)?;
                request_id = Some(record.input_id);
            }
            if let Some((message_id, content)) = &input.assistant_message {
                let mut message: MessageRecord =
                    required(&transaction, "messages", "id", message_id)?;
                message.content = content.clone();
                save_message(&transaction, &message)?;
            }
            transaction.execute("DELETE FROM leases WHERE task_id=?", params![task.task_id])?;
            transaction.commit()?;
            Ok(request_id)
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Mid-run projection of events, records, and inputs (ADR 0016 §2).
// ---------------------------------------------------------------------------

pub struct AppendProjectedInputs {
    pub fence: LeaseFence,
    pub events: Vec<Value>,
    /// Tool/assistant records to append (validated by the projection rules).
    pub messages: Vec<MessageInput>,
    pub creates: Vec<CreateAgentInput>,
    /// Inputs receipted in this transaction: PENDING -> APPLIED. Each entry
    /// OWNS the single user record its placement writes — the transition,
    /// the record and the receipt commit together or not at all.
    pub applies: Vec<AppliedSteer>,
    /// Fills `appliedTaskId` of the created inputs.
    pub task_id: String,
    pub now: i64,
}

/// A steer placement: the durable input and the one message it projects.
pub struct AppliedSteer {
    pub input_id: String,
    pub message: MessageInput,
}

/// What a projection actually committed. A cancelled or differently-placed
/// input appears in NEITHER list: its message and receipt were skipped.
#[derive(Debug, Default)]
pub struct ProjectionCommit {
    pub created: Vec<String>,
    pub applied: Vec<String>,
}

impl SqliteStore {
    /// Fenced projection: events, messages, and input creation/application
    /// in ONE transaction. Creation is idempotent by input id (a replay of
    /// the same record commits nothing new), and a steer apply is a real
    /// CAS: only PENDING inputs gain their message here, and a replay of
    /// the IDENTICAL placement (same message id) is an idempotent no-op
    /// that still reports the input as applied. A cancelled steer or one
    /// placed by another owner writes nothing.
    pub async fn append_projected_inputs(
        &self,
        input: AppendProjectedInputs,
    ) -> Result<ProjectionCommit> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            crate::tasks::fence(&transaction, &input.fence, input.now)?;
            let task: TaskRecord = required(&transaction, "tasks", "id", &input.fence.task_id)?;
            let now = input.now;
            append_task_events(&transaction, &task.task_id, input.events, now)?;
            let now_text = ConversationTimestamp::from_epoch_millis(now)?
                .wire_text()?
                .into_owned();
            for message in &input.messages {
                validate_projection(&task, message)?;
                crate::conversations::append(&transaction, message.clone(), now)?;
            }
            for apply in &input.applies {
                if apply.message.id.is_none() {
                    return Err(StoreError::InvalidInput(
                        "a steer placement must carry its message id".into(),
                    ));
                }
                validate_projection(&task, &apply.message)?;
            }
            let mut created = vec![];
            for create in input.creates {
                let (record, was_created) = insert_input(&transaction, create, &now_text)?;
                if was_created {
                    created.push(record.input_id);
                }
            }
            for id in &created {
                // The row was inserted Pending in THIS transaction; the only
                // way it is not pending now is a racing apply, which already
                // set the same fields.
                ignore_lost_cas(transition_input(
                    &transaction,
                    id,
                    &[AgentInputStatus::Pending],
                    AgentInputStatus::Applied,
                    &AgentInputPatch {
                        applied_task_id: Some(input.task_id.clone()),
                        ..Default::default()
                    },
                    &now_text,
                ))?;
            }
            let mut applied = vec![];
            for apply in &input.applies {
                let patch = AgentInputPatch {
                    applied_task_id: Some(input.task_id.clone()),
                    applied_message_id: apply.message.id.clone(),
                    ..Default::default()
                };
                match transition_input(
                    &transaction,
                    &apply.input_id,
                    &[AgentInputStatus::Pending],
                    AgentInputStatus::Applied,
                    &patch,
                    &now_text,
                ) {
                    Ok(_) => {
                        crate::conversations::append(&transaction, apply.message.clone(), now)?;
                        applied.push(apply.input_id.clone());
                    }
                    Err(StoreError::AgentInputTransition(_)) => {
                        // Meaningful CAS: only the identical earlier
                        // placement counts as an idempotent replay. A won
                        // cancellation (or another owner's placement) leaves
                        // no message and no receipt from this attempt.
                        let replay =
                            get_input(&transaction, &apply.input_id)?.is_some_and(|record| {
                                record.status == AgentInputStatus::Applied
                                    && record.applied_message_id == apply.message.id
                            });
                        if replay {
                            applied.push(apply.input_id.clone());
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
            transaction.commit()?;
            Ok(ProjectionCommit { created, applied })
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Explicit interruption (ADR 0016 §2).
// ---------------------------------------------------------------------------

impl SqliteStore {
    /// Record the request to interrupt a turn: a queued task parks directly
    /// (nothing is running to notice), a running task gets the durable
    /// request the execution loop observes.
    pub async fn request_interrupt(&self, task_id: String, now: i64) -> Result<()> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let mut task: TaskRecord = match get_task_row(&transaction, &task_id)? {
                Some(task) => task,
                None => {
                    return Err(StoreError::NotFound(format!(
                        "Task {task_id} does not exist."
                    )));
                }
            };
            if task.status.is_terminal() {
                return Ok(());
            }
            match task.status {
                TaskStatus::Queued => {
                    task.status = TaskStatus::Interrupted;
                    save(&transaction, &task)?;
                }
                TaskStatus::Running if !task.cancel_requested => {
                    task.interrupt_requested = true;
                    save(&transaction, &task)?;
                    lifecycle_event(&transaction, &task, "task.interrupt_requested", now)?;
                }
                _ => {}
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    /// The execution loop observed the interrupt request at a safe boundary:
    /// park the RUNNING turn with its attempt and lease ended.
    pub async fn park_interrupted(&self, fence: LeaseFence, now: i64) -> Result<()> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            crate::tasks::fence(&transaction, &fence, now)?;
            let mut task: TaskRecord = required(&transaction, "tasks", "id", &fence.task_id)?;
            if task.status != TaskStatus::Running {
                return Err(StoreError::InvalidTransition(format!(
                    "Task {} cannot move running -> interrupted: its status is {:?}.",
                    task.task_id, task.status
                )));
            }
            task.status = TaskStatus::Interrupted;
            task.interrupt_requested = false;
            save(&transaction, &task)?;
            let lease: Lease = required(&transaction, "leases", "task_id", &task.task_id)?;
            end_attempt(&transaction, &lease, "interrupted", now, None)?;
            transaction.execute("DELETE FROM leases WHERE task_id=?", params![task.task_id])?;
            lifecycle_event(&transaction, &task, "task.interrupted", now)?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Session-aware settle (ADR 0016 §5): the store-side half of settling.
// ---------------------------------------------------------------------------

pub(crate) fn settle_session_inputs(
    db: &Connection,
    task: &TaskRecord,
    completed: bool,
    now: i64,
    extra: &SessionSettle,
) -> Result<()> {
    let now_text = ConversationTimestamp::from_epoch_millis(now)?
        .wire_text()?
        .into_owned();
    let steers = list_inputs(
        db,
        &AgentInputQuery {
            session_id: task.scope_id.clone(),
            kinds: Some(vec![crate::agent_inputs::AgentInputKind::Steer]),
            statuses: Some(vec![AgentInputStatus::Pending]),
            target_task_id: None,
        },
    )?;
    for steer in steers.into_iter().filter(|record| {
        record
            .target_task_id
            .as_deref()
            .is_none_or(|id| id == task.task_id)
    }) {
        // The AUTHORITATIVE policy is the top-level record field the
        // service writes; `content.onSettled` is not a second source.
        let cancel = steer.on_settled == Some(crate::agent_inputs::AgentInputSettlePolicy::Cancel);
        if cancel {
            ignore_lost_cas(transition_input(
                db,
                &steer.input_id,
                &[AgentInputStatus::Pending],
                AgentInputStatus::Cancelled,
                &AgentInputPatch::default(),
                &now_text,
            ))?;
        }
    }
    if !completed {
        // Failed and cancelled turns promote nothing (ADR 0016 §4): queued
        // follow-ups and steers stay PENDING and visible, and an explicit
        // flushQueue releases the oldest. A user who cancelled a turn did
        // not ask for the queued one to start on its own — and a failed
        // turn must not swallow the queue with it. Only the cancel-policy
        // steers above were removed; everything else is untouched.
        return Ok(());
    }
    // Staging a second turn into a scope that already runs one is exactly
    // what the claim guard exists to prevent; the conversion must respect
    // the same invariant.
    let busy: Option<String> = db
        .query_row(
            "SELECT id FROM tasks WHERE scope=?1 AND json_extract(record,'$.kind')=?2 \
             AND id<>?3 AND (status IN ('running','waiting_approval','interrupted') \
             OR (status='queued' AND json_extract(record,'$.retryPending')=1)) LIMIT 1",
            params![task.scope_id, task.kind, task.task_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if busy.is_some() || unrelated_live_task(db, &task.scope_id, &task.kind, Some(&task.task_id))? {
        return Ok(());
    }
    // Eligibility selection belongs INSIDE this transaction: the pending
    // set is re-read here, against the state this commit is about to write,
    // so a settlement never commits a terminal turn with promotable work
    // still queued and a silent worker. The host's staged list keeps the
    // queue order and the ids it minted; whatever it could not have known
    // about is staged from the settling turn and converted here instead —
    // an input that arrived after the snapshot was taken, and the candidate
    // behind one that was cancelled in the same window (a supplied
    // candidate that is gone, cancelled or no longer pending simply loses
    // its CAS here, which is exactly what lets the scan move on).
    let mut promoted = false;
    for candidate in &extra.convert {
        if convert_candidate(db, &task.scope_id, candidate, now)? {
            promoted = true;
            break;
        }
    }
    if promoted {
        return Ok(());
    }
    // Without the session's CURRENT descriptor snapshot there is no honest
    // config to stamp a new turn with: the settling task's own payload is
    // the config some EARLIER submission was made with, and reusing it is
    // exactly how a mid-flight model change gets stamped onto a turn that
    // was created after the change. Promote nothing missed then — the input
    // stays Pending and visible, and an explicit flush stages it properly.
    let Some(staging) = &extra.staging else {
        return Ok(());
    };
    for record in promotable_pending(db, task, &extra.convert)? {
        let Promotion::Stage(candidate) = stage_missed(staging, &record, now)? else {
            continue;
        };
        if convert_candidate(db, &task.scope_id, &candidate, now)? {
            break;
        }
    }
    Ok(())
}

/// One queued input the settlement must still try, and the staged turn it
/// becomes. `Promotion::Skip` is the staging filter: unusable content, or a
/// steer this settlement may not claim, stays pending and visible — it must
/// never abort a healthy settlement with an error.
enum Promotion {
    Stage(Box<StagedCandidate>),
    Skip,
}

/// The queued inputs this settlement may promote that the host's snapshot
/// did not stage: pending follow-ups, plus steers with no target or aimed
/// at the settling turn and not marked `cancel` — the same population the
/// top of [`settle_session_inputs`] arbitrates, in arrival order.
fn promotable_pending(
    db: &Connection,
    task: &TaskRecord,
    supplied: &[StagedCandidate],
) -> Result<Vec<AgentInputRecord>> {
    let pending = list_inputs(
        db,
        &AgentInputQuery {
            session_id: task.scope_id.clone(),
            kinds: Some(vec![
                crate::agent_inputs::AgentInputKind::FollowUp,
                crate::agent_inputs::AgentInputKind::Steer,
            ]),
            statuses: Some(vec![AgentInputStatus::Pending]),
            target_task_id: None,
        },
    )?;
    Ok(pending
        .into_iter()
        .filter(|record| {
            !supplied
                .iter()
                .any(|candidate| candidate.input_id == record.input_id)
                && promotable(record, task)
        })
        .collect())
}

/// The settlement's own eligibility rules for one queued input (mirrors the
/// service's staging filter; see [`stage_missed`]).
fn promotable(record: &AgentInputRecord, task: &TaskRecord) -> bool {
    if record.kind == crate::agent_inputs::AgentInputKind::Steer {
        // A steer aimed at some OTHER turn is that turn's business: the
        // settle path only arbitrates steers with no target or aimed at the
        // settling turn, whose own row is already terminal in this
        // transaction.
        if record
            .target_task_id
            .as_deref()
            .is_some_and(|target| target != task.task_id)
        {
            return false;
        }
        // `cancel` steers were cancelled above; converting one here would
        // contradict the policy the human set.
        if record.on_settled == Some(crate::agent_inputs::AgentInputSettlePolicy::Cancel) {
            return false;
        }
    }
    queue_content(&record.content).is_some()
}

/// Stage the turn a snapshot missed, under the session's [`TurnStaging`]
/// contract: the SAME payload a submission made right now would carry (the
/// current descriptor's config, the session's turn kind), with ids derived
/// from the input itself, which keeps the promotion idempotent
/// (`turn:<inputId>`, and for a steer the same `steer:<inputId>` receipt the
/// in-run projection uses). The settling turn is NOT the template: its
/// payload is another turn's creation-time snapshot.
fn stage_missed(staging: &TurnStaging, record: &AgentInputRecord, now: i64) -> Result<Promotion> {
    let Some(content) = queue_content(&record.content) else {
        return Ok(Promotion::Skip);
    };
    let config = staging
        .config
        .as_object()
        .ok_or_else(|| StoreError::InvalidInput("turn staging config must be an object".into()))?;
    let steer = record.kind == crate::agent_inputs::AgentInputKind::Steer;
    let user_message_id = if steer {
        format!("steer:{}", record.input_id)
    } else {
        format!("msg_q:{}", record.input_id)
    };
    let assistant_message_id = format!("msg_a:{}", record.input_id);
    let task_id = format!("turn:{}", record.input_id);
    // The contract's config keys, then the two ids this promotion mints. A
    // promoted queued turn is a root turn, exactly like the service's
    // staging: no parent, whoever promoted it last is nobody's parent.
    let mut payload = config.clone();
    payload.remove("parentMessageId");
    payload.insert(
        "assistantMessageId".into(),
        Value::from(assistant_message_id.clone()),
    );
    payload.insert("userMessageId".into(), Value::from(user_message_id.clone()));
    Ok(Promotion::Stage(Box::new(StagedCandidate {
        input_id: record.input_id.clone(),
        task: SubmitTask {
            task_id: task_id.clone(),
            kind: staging.kind.clone(),
            scope_id: record.session_id.clone(),
            payload: Value::Object(payload),
            priority: 0,
            now,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: true,
        },
        messages: vec![
            MessageInput {
                id: Some(user_message_id.clone()),
                chat_id: record.session_id.clone(),
                role: "user".into(),
                content,
                parent_message_id: None,
                ..Default::default()
            },
            MessageInput {
                id: Some(assistant_message_id),
                chat_id: record.session_id.clone(),
                run_id: Some(task_id),
                role: "assistant".into(),
                content: Value::String(String::new()),
                parent_message_id: Some(user_message_id),
                metadata: json!({"placeholder": true}),
                ..Default::default()
            },
        ],
    })))
}

/// What [`convert_candidate`] can write: a non-empty string or a non-empty
/// content-block array under `{"content": ...}`. Anything else stays
/// pending and visible; a poison record must never abort a healthy
/// settlement.
fn queue_content(content: &Value) -> Option<Value> {
    let value = content.get("content")?;
    let usable = match value {
        Value::String(text) => !text.is_empty(),
        Value::Array(blocks) => !blocks.is_empty(),
        _ => false,
    };
    usable.then(|| value.clone())
}

/// Convert one queued input into a turn: insert the task, append its
/// messages, mark the input converted. Returns false (without touching
/// anything) when the input is gone or no longer pending in this session.
pub(crate) fn convert_candidate(
    db: &Connection,
    scope_id: &str,
    candidate: &StagedCandidate,
    now: i64,
) -> Result<bool> {
    let now_text = ConversationTimestamp::from_epoch_millis(now)?
        .wire_text()?
        .into_owned();
    let Some(record) = get_input(db, &candidate.input_id)? else {
        return Ok(false);
    };
    if record.status != AgentInputStatus::Pending || record.session_id != scope_id {
        return Ok(false);
    }
    // A scope another task still holds makes `tasks::insert` raise
    // `ScopeBusy`. That must only forfeit THIS candidate (the input stays
    // Pending), never abort the caller's transaction, so the write runs
    // under its own savepoint.
    db.execute_batch("SAVEPOINT convert_candidate")?;
    match convert_claimed(db, &record, candidate, &now_text, now) {
        Ok(()) => {
            db.execute_batch("RELEASE convert_candidate")?;
            Ok(true)
        }
        Err(error) => {
            let rolled_back =
                db.execute_batch("ROLLBACK TO convert_candidate; RELEASE convert_candidate");
            match (error, rolled_back) {
                // Swallowing ScopeBusy is only sound if the partial writes are gone.
                (StoreError::ScopeBusy(_), Ok(())) => Ok(false),
                (StoreError::ScopeBusy(_), Err(rollback)) => Err(rollback.into()),
                // Any other error aborts the caller's transaction anyway;
                // never mask it with a secondary rollback failure.
                (error, _) => Err(error),
            }
        }
    }
}

fn convert_claimed(
    db: &Connection,
    record: &AgentInputRecord,
    candidate: &StagedCandidate,
    now_text: &str,
    now: i64,
) -> Result<()> {
    // Claim the input BEFORE staging anything: the Pending -> Converted
    // transition is the CAS. A lost race returns false having written
    // nothing; the task and its messages land only behind the receipt.
    let user_message_id = (record.kind == crate::agent_inputs::AgentInputKind::Steer)
        .then(|| {
            candidate
                .messages
                .iter()
                .find(|message| message.role == "user")
                .and_then(|message| message.id.clone())
        })
        .flatten();
    let applied_task_id = candidate.task.task_id.clone();
    transition_input(
        db,
        &candidate.input_id,
        &[AgentInputStatus::Pending],
        AgentInputStatus::Converted,
        &AgentInputPatch {
            applied_task_id: Some(applied_task_id),
            applied_message_id: user_message_id,
            ..Default::default()
        },
        now_text,
    )?;
    let mut submit = candidate.task.clone();
    submit.now = now;
    let task = crate::tasks::insert(db, submit)?;
    for message in &candidate.messages {
        if message.role == "user" {
            if message.chat_id != candidate.task.scope_id {
                return Err(StoreError::InvalidInput(
                    "A turn may only append messages to its own chat.".into(),
                ));
            }
            // User turn messages carry the raw payload (a string or content
            // blocks), the same shape every other write path uses.
            let usable = match &message.content {
                Value::String(text) => !text.is_empty(),
                Value::Array(blocks) => !blocks.is_empty(),
                _ => false,
            };
            if !usable {
                return Err(StoreError::InvalidInput(
                    "Projected user messages must carry user content.".into(),
                ));
            }
        } else {
            validate_projection(&task, message)?;
        }
        crate::conversations::append(db, message.clone(), now)?;
    }
    Ok(())
}

/// Cancel every pending human request that names this turn (a cancelled
/// parked turn must not leave answerable requests behind).
pub(crate) fn cancel_turn_requests(
    db: &Connection,
    session_id: &str,
    turn_id: &str,
    now: i64,
) -> Result<()> {
    let now_text = ConversationTimestamp::from_epoch_millis(now)?
        .wire_text()?
        .into_owned();
    for record in list_inputs(
        db,
        &AgentInputQuery {
            session_id: session_id.to_owned(),
            kinds: Some(vec![crate::agent_inputs::AgentInputKind::HumanRequest]),
            statuses: Some(vec![AgentInputStatus::Pending]),
            target_task_id: Some(turn_id.to_owned()),
        },
    )? {
        ignore_lost_cas(transition_input(
            db,
            &record.input_id,
            &[AgentInputStatus::Pending],
            AgentInputStatus::Cancelled,
            &AgentInputPatch::default(),
            &now_text,
        ))?;
    }
    Ok(())
}

/// A lost agent-input compare-and-set is an expected race outcome;
/// every other failure is real and must not be swallowed.
fn ignore_lost_cas(result: Result<AgentInputRecord>) -> Result<()> {
    match result {
        Ok(_) | Err(StoreError::AgentInputTransition(_)) => Ok(()),
        Err(error) => Err(error),
    }
}

/// A running, parked or interrupted task of ANOTHER kind in the scope: it
/// cannot promise to promote a queued input when it ends. A merely queued
/// task is not counted here; the exclusive-scope refusal from the insert is
/// caught by the savepoint in [`convert_candidate`].
fn unrelated_live_task(
    db: &Connection,
    session_id: &str,
    task_kind: &str,
    exclude_task_id: Option<&str>,
) -> Result<bool> {
    Ok(db
        .query_row(
            "SELECT id FROM tasks WHERE scope=?1 AND json_extract(record,'$.kind')<>?2 \
             AND (?3 IS NULL OR id<>?3) \
             AND status IN ('running','interrupted','waiting_approval') LIMIT 1",
            params![session_id, task_kind, exclude_task_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some())
}

// ---------------------------------------------------------------------------
// Queue flushing (the service's `flushQueue`)
// ---------------------------------------------------------------------------

impl SqliteStore {
    /// Convert the given queued input into a turn, or return `None` when
    /// the scope is busy or the input is no longer pending in this session.
    /// The busy check, the CAS, the task insert and the message append are
    /// ONE transaction: a refusal mid-way (invalid content, a lost race)
    /// writes NOTHING — never a queued task without its messages or receipt.
    pub async fn stage_queued_turn(&self, input: StageQueuedTurn) -> Result<Option<String>> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            let busy: Option<String> = transaction
                .query_row(
                    "SELECT id FROM tasks WHERE scope=?1 AND json_extract(record,'$.kind')=?2 \
                     AND (?3 IS NULL OR id<>?3) AND (status IN ('running','waiting_approval','interrupted') \
                     OR (status='queued' AND json_extract(record,'$.retryPending')=1)) LIMIT 1",
                    params![input.session_id, input.task_kind, input.exclude_task_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if busy.is_some()
                || unrelated_live_task(
                    &transaction,
                    &input.session_id,
                    &input.task_kind,
                    input.exclude_task_id.as_deref(),
                )?
            {
                return Ok(None);
            }
            let Some(record) = get_input(&transaction, &input.input_id)? else {
                return Ok(None);
            };
            if record.status != AgentInputStatus::Pending
                || record.session_id != input.session_id
                || record.kind != input.expected_kind
            {
                return Ok(None);
            }
            let task_id = input.task.task_id.clone();
            let candidate = StagedCandidate {
                input_id: input.input_id,
                task: input.task,
                messages: input.messages,
            };
            if convert_candidate(&transaction, &input.session_id, &candidate, input.now)? {
                transaction.commit()?;
                Ok(Some(task_id))
            } else {
                Ok(None)
            }
        })
        .await
    }
}

impl SqliteStore {
    /// The ONE write that admits a queued input: the active-turn check and
    /// the insertion commit together, so there is no window for a settlement
    /// to slip between them.
    ///
    /// Two outcomes, both durable:
    /// * [`QueueAdmission::Queued`] — the scope holds a live turn, so the
    ///   input is stored Pending and THAT turn's settlement transaction (which
    ///   re-reads the pending set against its own commit) owns the promotion.
    /// * [`QueueAdmission::Staged`] — no active turn can claim the input, so
    ///   this same write converts it into the next turn through
    ///   [`convert_candidate`], the same CAS every promotion path shares.
    ///
    /// A check-then-write across two calls is exactly what stranded a
    /// follow-up: the host read "busy", the turn settled (re-reading an empty
    /// queue), and the insert then landed behind a terminal turn with no
    /// settlement left to run.
    pub async fn queue_or_stage_turn(&self, input: QueueOrStage) -> Result<QueueAdmission> {
        self.call(move |db| {
            let transaction = crate::transaction::atomic(db)?;
            // Only this session's turn kind drives its settlement queue.
            // An unrelated task sharing the scope cannot promise to promote
            // an accepted input when it finishes; reject before inserting it.
            let unrelated: Option<String> = transaction
                .query_row(
                    "SELECT id FROM tasks WHERE scope=?1 \
                     AND json_extract(record,'$.kind')<>?2 \
                     AND status IN ('queued','running','interrupted','waiting_approval') \
                     LIMIT 1",
                    params![input.session_id, input.task_kind],
                    |row| row.get(0),
                )
                .optional()?;
            if unrelated.is_some() {
                return Err(StoreError::ScopeBusy(input.session_id));
            }
            let active: Option<String> = transaction
                .query_row(
                    "SELECT id FROM tasks WHERE scope=?1 \
                     AND status IN ('queued','running','interrupted','waiting_approval') \
                     LIMIT 1",
                    params![input.session_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let mut create = input.input;
            if let Some(active) = &active
                && input.target_the_live_turn
            {
                // A steer with no explicit target belongs to the turn that
                // actually holds the scope: that run reads its steers by
                // target, and its settlement arbitrates them.
                create.target_task_id = Some(active.clone());
            }
            let now_text = ConversationTimestamp::from_epoch_millis(input.now)?
                .wire_text()?
                .into_owned();
            let (record, _) = insert_input(&transaction, create, &now_text)?;
            if active.is_some()
                || record.status != AgentInputStatus::Pending
                || record.on_settled == Some(crate::agent_inputs::AgentInputSettlePolicy::Cancel)
            {
                // Busy (or already settled/cancelled): queue only. A
                // `cancel`-policy steer is never started as a turn — the
                // human asked for it to be dropped, not run.
                transaction.commit()?;
                return Ok(QueueAdmission::Queued(record));
            }
            let Some(candidate) = input.candidate else {
                // Nothing to stage this into (unusable content): the input
                // stays Pending and visible, never a turn, never an error.
                transaction.commit()?;
                return Ok(QueueAdmission::Queued(record));
            };
            if convert_candidate(&transaction, &input.session_id, &candidate, input.now)? {
                let task: TaskRecord =
                    required(&transaction, "tasks", "id", &candidate.task.task_id)?;
                transaction.commit()?;
                return Ok(QueueAdmission::Staged(task));
            }
            // The CAS lost (the input stopped being Pending mid-write): leave
            // it exactly as the insert made it and report the queue.
            let record = get_input(&transaction, &record.input_id)?.unwrap_or(record);
            transaction.commit()?;
            Ok(QueueAdmission::Queued(record))
        })
        .await
    }
}

impl SqliteStore {
    /// Record a deferred compaction request (ADR 0017 part 2). Refused while
    /// the scope holds a live turn — that turn is already assembling its
    /// history — and the check shares one actor job with the write.
    pub async fn request_compact(
        &self,
        session_id: String,
        task_kind: String,
        expected_descriptor: Value,
        now: i64,
    ) -> Result<()> {
        self.call(move |db| {
            let busy: Option<String> = db
                .query_row(
                    "SELECT id FROM tasks WHERE scope=?1 AND json_extract(record,'$.kind')=?2 \
                     AND status IN ('queued','running','waiting_approval','interrupted') LIMIT 1",
                    params![session_id, task_kind],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if busy.is_some() {
                return Err(StoreError::ScopeBusy(session_id));
            }
            let mut chat: ChatRecord = required(db, "chats", "id", &session_id)?;
            if chat.metadata.get(SESSION_METADATA_KEY) != Some(&expected_descriptor) {
                return Err(StoreError::SessionDescriptorChanged(session_id));
            }
            let mut metadata = chat.metadata.as_object().cloned().unwrap_or_default();
            metadata.insert(
                COMPACT_REQUESTED_METADATA_KEY.into(),
                Value::from(
                    ConversationTimestamp::from_epoch_millis(now)?
                        .wire_text()?
                        .into_owned(),
                ),
            );
            set_metadata(db, &mut chat, Value::Object(metadata), now)?;
            Ok(())
        })
        .await
    }
}
