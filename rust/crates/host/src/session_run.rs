//! The session seam of one chat turn (ADR 0016 §3, ADR 0019 §2): what the
//! executor needs from the durable session while a turn runs — the steering
//! source at tool boundaries, and the answered approvals whose exact calls
//! this attempt resumes.
//!
//! This is deliberately thin: state lives in the store, the service owns the
//! lifecycle, and the executor only asks these two questions per attempt.

use crate::session::{read_session_descriptor, session_turn_kind};
use crate::*;
use agentkit_core::{ChatMessage, SteeringItem, SteeringSource, ToolCall};
use agentkit_store::{
    AgentInputKind, AgentInputQuery, AgentInputRecord, AgentInputStatus, AppendProjectedInputs,
    MessageInput, MessageRecord, is_approval_request,
};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};

/// Is this task the session turn of its chat (the chat carries a session
/// descriptor and the task kind is the session's turn kind)?
pub(crate) async fn is_session_turn(context: &ExecutionContext, chat_id: &str) -> HostResult<bool> {
    let Some(chat) = context.store.get_chat(chat_id).await? else {
        return Ok(false);
    };
    let Some(descriptor) = read_session_descriptor(&chat.metadata) else {
        return Ok(false);
    };
    Ok(session_turn_kind(&descriptor) == context.task.kind)
}

/// Steers pending for this turn, offered to the loop at tool boundaries.
/// A placement persists the user record and receipts the input as applied
/// in ONE fenced transaction, so a crash cannot show the model a steer the
/// history does not contain — nor lose one it did see. DELIVERY is the
/// durable `run.input.applied` announcement, not the placement: until the
/// log carries an input's announcement, this source re-offers placements
/// applied to the run, so a poll future dropped between commit and return
/// (the timeout race) or an attempt that died before the event sink ran
/// cannot silently swallow a steering. The loop's `applied_steers` set
/// absorbs the repetition within an attempt; [`backfill_applied_steers`]
/// reconciles the log at attempt start after a crash. Announced inputs are
/// never offered again — the durable log, not in-memory state, is the
/// dedupe.
pub(crate) struct SessionSteering {
    context: ExecutionContext,
    chat_id: String,
    /// Incremental view of this task's announcement log: a poll must not
    /// rescan a log that one run can grow to thousands of events, so the
    /// cursor and the seen-announcement ids live across polls.
    ledger: tokio::sync::Mutex<AnnouncementLedger>,
}

#[derive(Default)]
struct AnnouncementLedger {
    cursor: Option<f64>,
    announced: HashSet<String>,
}

impl SessionSteering {
    pub(crate) fn new(context: ExecutionContext, chat_id: &str) -> Self {
        Self {
            context,
            chat_id: chat_id.to_owned(),
            ledger: tokio::sync::Mutex::new(AnnouncementLedger::default()),
        }
    }

    /// Steers durably applied to this run whose announcement the log does
    /// not carry yet — the exact predicate `backfill_applied_steers`
    /// reconciles against, minus the append. Never throws: a failed read
    /// simply offers nothing extra this poll (the next poll retries).
    async fn applied_unannounced(&self) -> Vec<AgentInputRecord> {
        let candidates = match self
            .context
            .store
            .list_inputs(AgentInputQuery {
                session_id: self.chat_id.clone(),
                kinds: Some(vec![AgentInputKind::Steer]),
                statuses: Some(vec![AgentInputStatus::Applied]),
                target_task_id: Some(self.context.task.task_id.clone()),
            })
            .await
        {
            Ok(records) if !records.is_empty() => records,
            _ => return vec![],
        };
        let mut ledger = self.ledger.lock().await;
        loop {
            let batch = match self
                .context
                .store
                .events_after_number(&self.context.task.task_id, ledger.cursor, 10_000)
                .await
            {
                Ok(batch) => batch,
                Err(_) => return vec![],
            };
            let Some(seq) = batch.last().map(|record| record.seq) else {
                break;
            };
            for record in batch {
                let event = &record.event;
                if event["type"] == "run.input.applied"
                    && let Some(input_id) = event["data"]["inputId"].as_str()
                {
                    ledger.announced.insert(input_id.to_owned());
                }
            }
            if ledger.cursor.is_none_or(|seen| seq > seen) {
                ledger.cursor = Some(seq);
            } else {
                break;
            }
        }
        let mut reoffers = vec![];
        for record in candidates {
            if ledger.announced.contains(&record.input_id) {
                continue;
            }
            let Some(message_id) = record.applied_message_id.as_deref() else {
                continue;
            };
            // Re-offer only what the placement actually committed: Applied
            // WITH its projected record on this run.
            let placed = match self.context.store.get_message(message_id).await {
                Ok(Some(message)) => {
                    message.run_id.as_deref() == Some(self.context.task.task_id.as_str())
                }
                _ => false,
            };
            if placed {
                reoffers.push(record);
            }
        }
        reoffers
    }
}

#[async_trait]
impl SteeringSource for SessionSteering {
    async fn pending(&self) -> Vec<SteeringItem> {
        // Placements from earlier polls whose announcement never reached
        // the log come first: they arrived before anything this poll will
        // place, and the loop's `applied_steers` set makes the repetition
        // invisible to the model.
        let mut items: Vec<SteeringItem> = self
            .applied_unannounced()
            .await
            .into_iter()
            .filter_map(|record| {
                let content = record
                    .content
                    .get("content")
                    .filter(|content| content.is_string() || content.is_array())?
                    .clone();
                Some(SteeringItem {
                    input_id: record.input_id,
                    message: ChatMessage {
                        role: "user".into(),
                        content,
                        name: None,
                        tool_call_id: None,
                        tool_calls: vec![],
                        metadata: None,
                    },
                })
            })
            .collect();
        let records = match self
            .context
            .store
            .list_inputs(AgentInputQuery {
                session_id: self.chat_id.clone(),
                kinds: Some(vec![AgentInputKind::Steer]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: Some(self.context.task.task_id.clone()),
            })
            .await
        {
            Ok(records) => records,
            Err(_) => return items,
        };
        for record in records {
            let Some(content) = record
                .content
                .get("content")
                .filter(|content| content.is_string() || content.is_array())
                .cloned()
            else {
                continue;
            };
            let message_id = format!("steer:{}", record.input_id);
            let placed = self
                .context
                .store
                .append_projected_inputs(AppendProjectedInputs {
                    fence: self.context.fence.clone(),
                    events: vec![],
                    messages: vec![],
                    creates: vec![],
                    applies: vec![agentkit_store::AppliedSteer {
                        input_id: record.input_id.clone(),
                        message: MessageInput {
                            id: Some(message_id),
                            chat_id: self.chat_id.clone(),
                            run_id: Some(self.context.task.task_id.clone()),
                            role: "user".into(),
                            content: content.clone(),
                            metadata: serde_json::json!({
                                "steering": true,
                                "inputId": record.input_id,
                            }),
                            ..MessageInput::default()
                        },
                    }],
                    task_id: self.context.task.task_id.clone(),
                    now: self.context.runtime.now_ms(),
                })
                .await;
            // Deliver only what THIS transaction's CAS actually placed: a
            // cancellation that won, or another owner's placement, leaves no
            // message and no delivered item. An identical replay of the same
            // receipt counts as placed.
            if !placed.is_ok_and(|commit| commit.applied.contains(&record.input_id)) {
                continue;
            }
            items.push(SteeringItem {
                input_id: record.input_id,
                message: ChatMessage {
                    role: "user".into(),
                    content,
                    name: None,
                    tool_call_id: None,
                    tool_calls: vec![],
                    metadata: None,
                },
            });
        }
        items
    }
}

/// Reconciliation of the `run.input.applied` announcement (ADR 0016 §3).
///
/// The recovery contract for the steer pair is: THE PLACEMENT TRANSACTION
/// IS THE UNIT OF RECORD. [`SessionSteering::pending`] commits the Applied
/// CAS, the single projected user record, and the applied receipt in ONE
/// fenced store transaction; that triple — the input's Applied status plus
/// `appliedMessageId` plus the durable record — is authoritative for
/// "this input was applied to this run". Placement is the point of no
/// return: once committed, the steer is in the durable history and the
/// model sees it (immediately, or on any later attempt when history is
/// rebuilt from records). The `run.input.applied` run event is a
/// projection of that triple — an announcement for external observers of
/// the event log — written later by the event sink in its own transaction.
/// Nothing in the host derives behavior from the event; it is consumed
/// only through the task-event cursor.
///
/// A crash or a failed event write between the two transactions therefore
/// leaves durable truth ahead of the log. Recovery reconciles the log to
/// the truth: every attempt of the run (this runs at attempt setup, before
/// the loop derives its own event sequence) gives each input durably
/// applied against this run — whose projected record exists — exactly one
/// `run.input.applied` event. The operation is idempotent: already
/// announced inputs are skipped, so it is safe on every attempt, including
/// the first (which has no applied inputs yet). The backfilled event
/// carries `step: 0` with `reconciled: true`: the provider step the dead
/// attempt polled at is loop-local and unrecoverable, so 0 is the honest
/// sentinel ("announced by reconciliation"), never a 1-based step lie.
pub(crate) async fn backfill_applied_steers(
    context: &ExecutionContext,
    chat_id: &str,
) -> HostResult<()> {
    let applied = context
        .store
        .list_inputs(AgentInputQuery {
            session_id: chat_id.to_owned(),
            kinds: Some(vec![AgentInputKind::Steer]),
            statuses: Some(vec![AgentInputStatus::Applied]),
            target_task_id: Some(context.task.task_id.clone()),
        })
        .await?;
    if applied.is_empty() {
        return Ok(());
    }
    // The run's own announcement set, cursor-paged because one task's log
    // can outgrow the store's per-call event page. Dedupe is by data.inputId.
    let mut announced: HashSet<String> = HashSet::new();
    let mut cursor: Option<f64> = None;
    loop {
        let batch = context
            .store
            .events_after_number(&context.task.task_id, cursor, 10_000)
            .await?;
        let Some(seq) = batch.last().map(|record| record.seq) else {
            break;
        };
        for record in batch {
            let event = &record.event;
            if event["type"] == "run.input.applied"
                && let Some(input_id) = event["data"]["inputId"].as_str()
            {
                announced.insert(input_id.to_owned());
            }
        }
        if cursor.is_none_or(|seen| seq > seen) {
            cursor = Some(seq);
        } else {
            // A log whose page does not advance is broken; stop rather
            // than spin. Missing announcements then stay visible as the
            // gap they are.
            break;
        }
    }
    for record in applied {
        // The query above already restricts to STEER + Applied: exactly the
        // live emitter's population. Answered approvals carry an `applied`
        // stamp too, but they resume through the approval path, which emits
        // no `run.input.applied`; promoted queued turns enter as their own
        // turn. Neither may be announced here.
        if announced.contains(&record.input_id) {
            continue;
        }
        let Some(message_id) = record.applied_message_id.clone() else {
            continue;
        };
        // Only announce what the placement actually committed: Applied
        // WITH its projected record on this run. An Applied input without
        // the record is not a placed steer and stays unannounced.
        let placed = context
            .store
            .get_message(&message_id)
            .await?
            .is_some_and(|message| {
                message.run_id.as_deref() == Some(context.task.task_id.as_str())
            });
        if !placed {
            continue;
        }
        // The canonical event envelope (seq, timestamp, contractVersion,
        // attemptId, fence, event gate) belongs to `ExecutionContext::emit`
        // — a reconciled announcement must be indistinguishable from a
        // live one apart from its honest step sentinel.
        context
            .emit(
                "run.input.applied",
                serde_json::json!({
                    "inputId": record.input_id,
                    "kind": "steer",
                    "step": 0,
                    "reconciled": true,
                }),
            )
            .await?;
    }
    Ok(())
}

/// Resolve the answered approvals that target this turn (port of
/// `turn/approval-resume.ts`). Only calls that the run actually declared,
/// were never answered, and carry the EXACT arguments the human approved
/// become resume calls; anything else gets an honest tool record explaining
/// why it was not run. Returns the approvals map (tool call id -> request
/// id, consumed at admission) and the calls in declared order.
///
/// Resolution is per DECLARATION, not per answered request: one tool call
/// identity may hold several answered asks (an ordinary approval plus a later
/// uncertain reissue, or generations N then N+1), and exactly ONE of them —
/// the newest whose arguments still match the declared call — resumes it. The
/// older answers are durable history and are never re-spent, and nothing is
/// killed or replayed here: a resumed call runs once because the durable
/// execution receipt admits one execution per identity, not because a second
/// dispatch is suppressed downstream.
pub(crate) async fn resolve_approved_calls(
    context: &ExecutionContext,
    chat_id: &str,
    anchor: &str,
) -> HostResult<(
    HashMap<String, crate::governance::ApprovalGrant>,
    Vec<ToolCall>,
)> {
    let requests = context
        .store
        .list_inputs(AgentInputQuery {
            session_id: chat_id.to_owned(),
            kinds: Some(vec![AgentInputKind::HumanRequest]),
            statuses: Some(vec![AgentInputStatus::Answered]),
            target_task_id: Some(context.task.task_id.clone()),
        })
        .await?;
    let requests: Vec<_> = requests
        .into_iter()
        .filter(|request| {
            request.tool_call_id.is_some()
                && is_approval_request(&request.content)
                // Only APPROVE answers resume: the store leaves the
                // `approval:` marker in `appliedMessageId` exactly when it
                // took the no-record approve branch. Denials (which wrote a
                // refusal record instead) never resume anything.
                && request
                    .applied_message_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with(crate::session::APPROVAL_MARKER))
        })
        .collect();
    if requests.is_empty() {
        return Ok((HashMap::new(), vec![]));
    }
    // The walk must see the run's own INACTIVE records too: the denial an
    // answer wrote is precisely what tells this walk the call is answered.
    let records = context.store.messages(chat_id, true).await?;
    let by_id: HashMap<&str, &MessageRecord> = records
        .iter()
        .map(|record| (record.id.as_str(), record))
        .collect();
    // Walk this run's own record chain back to the anchor: what the run
    // declared, and what the history already answers.
    let mut answered: HashSet<String> = HashSet::new();
    let mut declared: Vec<ToolCall> = vec![];
    let mut current: Option<String> = records
        .iter()
        .rev()
        .find(|record| {
            record.run_id.as_deref() == Some(context.task.task_id.as_str()) && record.id != anchor
        })
        .map(|record| record.id.clone());
    while let Some(id) = current {
        if id == anchor {
            break;
        }
        let Some(record) = by_id.get(id.as_str()) else {
            break;
        };
        if record.role == "tool" {
            if let Some(call_id) = &record.tool_call_id {
                answered.insert(call_id.clone());
            }
        } else if record.role == "assistant"
            && record.run_id.as_deref() == Some(context.task.task_id.as_str())
        {
            let calls: Vec<ToolCall> = record
                .tool_calls
                .clone()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default();
            // Walking backwards: a later visit (earlier declaration) of a
            // repeated call id overwrites, matching the TS map rebuild.
            for call in calls {
                if let Some(existing) = declared.iter_mut().find(|existing| existing.id == call.id)
                {
                    *existing = call;
                } else {
                    declared.push(call);
                }
            }
        }
        current = record.parent_message_id.clone();
    }
    // Declared order (assistant records were walked newest-first).
    declared.reverse();

    // One resumed call per DECLARATION, in declaration order. `list_inputs`
    // returns requests by ascending insertion `order`, so a single call can
    // carry several answered asks — an ordinary approval and a later
    // uncertain reissue, or generations N then N+1 of the same ask. The live
    // grant is the NEWEST answered request whose arguments still match the
    // call the run declared; the older answers stay as durable history and
    // stay unspendable: admission compares the grant's
    // `uncertainty_generation` against the unresolved receipt's execution id
    // (host `covers` + the store's pending-generation gate), and the
    // receipt's UNIQUE approval index spends each answer exactly once.
    //
    // Resolution is keyed by declaration, NOT by answered request: one push
    // per request re-dispatched the same call, so a tool whose durable
    // receipt replayed a single effect still produced one tool record and one
    // event per answer. A refusal record is written only when NOTHING
    // resolves the declaration — an answered ask whose arguments differ from
    // the declared call is not evidence the call went unrun once a newer
    // (or older) answer authorized exactly it.
    let mut approvals = HashMap::new();
    let mut resume = vec![];
    for call in &declared {
        let call_id = call.id.as_str();
        if answered.contains(call_id) {
            continue; // already executed and recorded: nothing to resume
        }
        let mut grant = None;
        for request in requests.iter().rev() {
            if request.tool_call_id.as_deref() != Some(call_id) {
                continue;
            }
            let approved_hash = request.content["argsSha256"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            // One JavaScript-semantic parser at every digest site: the initial
            // dispatch hashed these arguments through `parse_javascript_json`,
            // so the resume comparison must use it too. Plain Serde would
            // re-spell `1.0` differently from the canonical `1` and refuse a
            // call the human actually approved.
            let hash = agentkit_contracts::parse_javascript_json(&call.arguments_json)
                .ok()
                .map(|arguments| crate::governance::args_sha256(&arguments));
            if hash.as_deref() != Some(approved_hash.as_str()) {
                continue; // this ask was about different arguments
            }
            let generation = request.content.get("approvalGeneration");
            let legacy_generation = request.content.get("uncertaintyGeneration");
            let context_hash = request.content.get("contextSha256");
            if generation.is_some_and(|value| !value.is_string())
                || legacy_generation.is_some_and(|value| !value.is_string())
                || context_hash.is_some_and(|value| !value.is_string())
                || generation.is_some()
                    && legacy_generation.is_some()
                    && generation != legacy_generation
            {
                continue;
            }
            grant = Some(crate::governance::ApprovalGrant {
                request_id: request.input_id.clone(),
                args_sha256: approved_hash,
                // An answer to an uncertain re-issue names the exact
                // unresolved execution it authorizes; the durable gate
                // admits only that generation. An ordinary approval
                // carries none.
                uncertainty_generation: generation
                    .or(legacy_generation)
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                context_sha256: context_hash.and_then(Value::as_str).map(str::to_owned),
            });
            break;
        }
        let Some(grant) = grant else {
            // A call the human never (or no longer) authorized is a refusal,
            // recorded — never a silent run of something they did not approve.
            if requests
                .iter()
                .any(|request| request.tool_call_id.as_deref() == Some(call_id))
            {
                append_approval_mismatch(context, chat_id, anchor, call).await?;
            }
            continue;
        };
        approvals.insert(call_id.to_owned(), grant);
        resume.push(call.clone());
    }
    Ok((approvals, resume))
}

async fn append_approval_mismatch(
    context: &ExecutionContext,
    chat_id: &str,
    anchor: &str,
    call: &ToolCall,
) -> HostResult<()> {
    let id = format!("approval-mismatch:{}:{}", context.task.task_id, call.id);
    if context.store.get_message(&id).await?.is_some() {
        return Ok(());
    }
    let message = "The arguments of this call differ from what the user approved; it was not run.";
    let envelope = serde_json::json!({
        "ok": false,
        "status": "error",
        "summary": message,
        "warnings": [],
        "truncated": false,
        "data": {
            "errorCode": "approval_mismatch",
            "errorMessage": message,
            "phase": "guard",
            "retryable": false,
        },
    });
    context
        .store
        .append_projected_inputs(AppendProjectedInputs {
            fence: context.fence.clone(),
            events: vec![],
            messages: vec![MessageInput {
                id: Some(id),
                chat_id: chat_id.to_owned(),
                run_id: Some(context.task.task_id.clone()),
                role: "tool".into(),
                content: envelope.clone(),
                model_result_json: Some(envelope.to_string()),
                tool_call_id: Some(call.id.clone()),
                parent_message_id: Some(anchor.to_owned()),
                metadata: serde_json::json!({"internal": true, "toolName": call.name}),
                activate: false,
                ..MessageInput::default()
            }],
            creates: vec![],
            applies: vec![],
            task_id: context.task.task_id.clone(),
            now: context.runtime.now_ms(),
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_store::{
        AgentInputPatch, ClaimInput, CreateAgentInput, MessageInput, SqliteStore, SubmitTask,
    };
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    /// A claimed task plus the execution context the runner would build,
    /// on real wall-clock time so `SystemRuntime` never outlives the lease.
    async fn harness() -> (SqliteStore, ExecutionContext) {
        let store = SqliteStore::memory(8).unwrap();
        let now = SystemRuntime.now_ms();
        store
            .create_chat("chat", None, json!({}), now)
            .await
            .unwrap();
        let user = MessageInput {
            id: Some("run-user".into()),
            chat_id: "chat".into(),
            run_id: None,
            role: "user".into(),
            content: json!("go"),
            ..MessageInput::default()
        };
        store
            .submit_turn(
                SubmitTask {
                    task_id: "run".into(),
                    kind: "session_turn".into(),
                    scope_id: "chat".into(),
                    payload: json!({"chatId": "chat"}),
                    priority: 0,
                    now,
                    available_at: None,
                    depends_on: vec![],
                    parent_task_id: None,
                    exclusive_scope: false,
                },
                vec![user],
            )
            .await
            .unwrap();
        let claimed = store
            .claim_with_concurrency(
                ClaimInput {
                    owner_id: "worker".into(),
                    now,
                    ttl_ms: 1_000_000_000,
                    kinds: vec![],
                    busy_scopes: vec![],
                    max_attempts: 3,
                },
                1,
            )
            .await
            .unwrap()
            .expect("the queued turn claims");
        let context = ExecutionContext {
            task: claimed.task,
            attempt_id: claimed.lease.attempt_id.clone(),
            cancellation: CancellationToken::new(),
            interruption: CancellationToken::new(),
            suspension: Arc::new(Mutex::new(None::<agentkit_core::HumanRequestSpec>)),
            store: store.clone(),
            fence: claimed.lease.fence(),
            runtime: Arc::new(SystemRuntime),
            event_gate: Arc::new(Mutex::new(())),
        };
        (store, context)
    }

    fn steer(id: &str) -> CreateAgentInput {
        CreateAgentInput {
            input_id: id.into(),
            session_id: "chat".into(),
            kind: AgentInputKind::Steer,
            content: json!({"content": "watch out"}),
            content_hash: format!("hash-{id}"),
            target_task_id: Some("run".into()),
            on_settled: None,
            attempt_id: None,
            tool_call_id: None,
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn an_unannounced_placement_is_reoffered_until_the_log_carries_it() {
        let (store, context) = harness().await;
        store.create_input(steer("st-1"), 2000).await.unwrap();
        let steering = SessionSteering::new(context.clone(), "chat");

        // First poll: the fenced CAS placement delivers.
        let items = steering.pending().await;
        assert_eq!(items.len(), 1, "the pending steer is placed and offered");
        assert_eq!(items[0].input_id, "st-1");
        assert_eq!(items[0].message.content, json!("watch out"));
        let record = store.get_input("st-1".into()).await.unwrap().unwrap();
        assert_eq!(record.status, AgentInputStatus::Applied);
        assert_eq!(record.applied_message_id.as_deref(), Some("steer:st-1"));

        // The poll future died AFTER the placement committed (the timeout
        // race) — the durable side says applied, the log never announced,
        // and the Pending list can no longer recover the steering. The next
        // poll must re-offer it; only the announcement closes the window.
        let items = steering.pending().await;
        assert_eq!(items.len(), 1, "an unannounced placement re-offers");
        assert_eq!(items[0].input_id, "st-1");

        context
            .emit(
                "run.input.applied",
                json!({"inputId": "st-1", "kind": "steer", "step": 1}),
            )
            .await
            .unwrap();
        assert!(
            steering.pending().await.is_empty(),
            "the announcement is the delivery ledger"
        );

        // A fresh source (the next attempt) agrees: the log, not memory,
        // is the dedupe.
        let fresh = SessionSteering::new(context.clone(), "chat");
        assert!(fresh.pending().await.is_empty());

        // However many polls re-offered, exactly one durable record exists.
        let messages = store.messages("chat", false).await.unwrap();
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.id == "steer:st-1")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn backfill_announces_what_a_crash_left_between_placement_and_log() {
        let (store, context) = harness().await;
        store.create_input(steer("st-2"), 2000).await.unwrap();
        let steering = SessionSteering::new(context.clone(), "chat");
        // Placed, never announced: the attempt "crashed" after commit.
        assert_eq!(steering.pending().await.len(), 1);

        backfill_applied_steers(&context, "chat").await.unwrap();
        let announced: Vec<_> = store
            .events("run", None, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|record| record.event["type"] == "run.input.applied")
            .collect();
        assert_eq!(announced.len(), 1, "exactly one reconciled event");
        assert_eq!(announced[0].event["data"]["inputId"], "st-2");
        assert_eq!(announced[0].event["data"]["reconciled"], true);
        assert_eq!(announced[0].event["data"]["step"], 0, "honest sentinel");

        // Idempotent: a second reconciliation adds nothing.
        backfill_applied_steers(&context, "chat").await.unwrap();
        assert_eq!(
            store
                .events("run", None, 100)
                .await
                .unwrap()
                .into_iter()
                .filter(|record| record.event["type"] == "run.input.applied")
                .count(),
            1
        );

        // And the source now agrees with the reconciled log.
        assert!(steering.pending().await.is_empty());
    }

    #[tokio::test]
    async fn unplaced_pending_steers_are_offered_once_and_cancelled_never() {
        let (store, context) = harness().await;
        store.create_input(steer("st-3"), 2000).await.unwrap();
        store.create_input(steer("st-4"), 2001).await.unwrap();
        // st-4 is cancelled before any poll sees it.
        store
            .transition_input(
                "st-4".into(),
                vec![AgentInputStatus::Pending],
                AgentInputStatus::Cancelled,
                AgentInputPatch::default(),
                2050,
            )
            .await
            .unwrap();
        let steering = SessionSteering::new(context.clone(), "chat");
        let items = steering.pending().await;
        assert_eq!(items.len(), 1, "only the live steer is offered");
        assert_eq!(items[0].input_id, "st-3");

        // st-3's placement went unannounced (same source, still open) —
        // it re-offers; st-4's cancellation is terminal and never offers.
        let items = steering.pending().await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].input_id, "st-3");
        assert!(
            !store
                .messages("chat", false)
                .await
                .unwrap()
                .iter()
                .any(|message| message.id == "steer:st-4"),
            "a cancelled steer has no projected record"
        );
    }
}
