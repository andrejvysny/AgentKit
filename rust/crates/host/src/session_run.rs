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
    AgentInputKind, AgentInputQuery, AgentInputStatus, AppendProjectedInputs, MessageInput,
    MessageRecord, is_approval_request,
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
/// history does not contain — nor lose one it did see. Applied steers are
/// never offered again, including after a resume: the durable status, not
/// in-memory state, is the dedupe.
pub(crate) struct SessionSteering {
    context: ExecutionContext,
    chat_id: String,
}

impl SessionSteering {
    pub(crate) fn new(context: ExecutionContext, chat_id: &str) -> Self {
        Self {
            context,
            chat_id: chat_id.to_owned(),
        }
    }
}

#[async_trait]
impl SteeringSource for SessionSteering {
    async fn pending(&self) -> Vec<SteeringItem> {
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
            Err(_) => return vec![],
        };
        let mut items = vec![];
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
                    messages: vec![MessageInput {
                        id: Some(message_id.clone()),
                        chat_id: self.chat_id.clone(),
                        run_id: Some(self.context.task.task_id.clone()),
                        role: "user".into(),
                        content: content.clone(),
                        metadata: serde_json::json!({
                            "steering": true,
                            "inputId": record.input_id,
                        }),
                        ..MessageInput::default()
                    }],
                    creates: vec![],
                    applies: vec![agentkit_store::AppliedSteer {
                        input_id: record.input_id.clone(),
                        message_id,
                    }],
                    task_id: self.context.task.task_id.clone(),
                    now: self.context.runtime.now_ms(),
                })
                .await;
            if placed.is_err() {
                // The fence is gone or a racing projection won; the input
                // stays pending and the queue flush owns it from here.
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

/// Resolve the answered approvals that target this turn (port of
/// `turn/approval-resume.ts`). Only calls that the run actually declared,
/// were never answered, and carry the EXACT arguments the human approved
/// become resume calls; anything else gets an honest tool record explaining
/// why it was not run. Returns the approvals map (tool call id -> request
/// id, consumed at admission) and the calls in declared order.
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

    let mut approvals = HashMap::new();
    let mut resume = vec![];
    for request in requests {
        let Some(call_id) = request.tool_call_id.clone() else {
            continue;
        };
        if answered.contains(&call_id) {
            continue; // already executed and recorded: nothing to resume
        }
        let Some(position) = declared.iter().position(|call| call.id == call_id) else {
            continue; // the run never declared this call
        };
        let call = &declared[position];
        let approved_hash = request.content["argsSha256"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let hash = serde_json::from_str::<serde_json::Value>(&call.arguments_json)
            .ok()
            .map(|arguments| crate::governance::args_sha256(&arguments));
        if hash.as_deref() != Some(approved_hash.as_str()) {
            // Changed (or unparseable) arguments are a refusal, recorded —
            // never a silent run of something the human did not approve.
            append_approval_mismatch(context, chat_id, anchor, call).await?;
            continue;
        }
        approvals.insert(
            call_id.clone(),
            crate::governance::ApprovalGrant {
                request_id: request.input_id.clone(),
                args_sha256: approved_hash,
            },
        );
        resume.push(call.clone());
    }
    resume.sort_by_key(|call| declared.iter().position(|c| c.id == call.id).unwrap_or(0));
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
