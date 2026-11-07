use super::{Problem, store_error};
use agentkit_store::{
    ChatRecord, ConversationTimestamp, MessageRecord, ProposalRecord, TaskRecord,
};
use serde_json::{Value, json};

pub(super) fn instant(ms: i64) -> String {
    ConversationTimestamp::from_epoch_millis(ms)
        .and_then(|value| value.wire_text().map(|text| text.into_owned()))
        .expect("persisted timestamp must fit JavaScript Date range")
}
pub(super) fn chat(record: ChatRecord) -> Result<Value, Problem> {
    let mut value = json!({"id":record.id,"createdAt":conversation_time(&record.created_at)?,"updatedAt":conversation_time(&record.updated_at)?,
        "archived":record.archived,"metadata":record.metadata});
    if let Some(title) = record.title {
        value["title"] = json!(title);
    }
    Ok(value)
}
pub(super) fn message(record: MessageRecord) -> Result<Value, Problem> {
    let mut value = json!({"id":record.id,"chatId":record.chat_id,"role":record.role,"content":record.content,
        "branchIndex":record.branch_index,"active":record.active,"metadata":record.metadata,"createdAt":conversation_time(&record.created_at)?});
    for (key, entry) in [
        ("runId", record.run_id.map(Value::String)),
        (
            "parentMessageId",
            record.parent_message_id.map(Value::String),
        ),
        ("toolCallId", record.tool_call_id.map(Value::String)),
        ("toolCalls", record.tool_calls),
    ] {
        if let Some(entry) = entry {
            value[key] = entry;
        }
    }
    Ok(value)
}

pub(super) fn conversation_time(timestamp: &ConversationTimestamp) -> Result<String, Problem> {
    timestamp
        .wire_text()
        .map(|text| text.into_owned())
        .map_err(|error| store_error(error.into()))
}
pub(super) fn run(task: TaskRecord) -> Value {
    let mut value = json!({"runId":task.task_id,"chatId":task.payload["chatId"],"scopeId":task.scope_id,
        "status":task.status,"createdAt":instant(task.enqueued_at)});
    if let Some(ms) = task.started_at {
        value["startedAt"] = json!(instant(ms));
    }
    if let Some(ms) = task.finished_at {
        value["finishedAt"] = json!(instant(ms));
    }
    if let Some(error) = task.error {
        value["error"] = json!(error);
    }
    value
}
pub(super) fn proposal(record: ProposalRecord) -> Value {
    let summary = record
        .reason
        .clone()
        .or_else(|| record.envelope["summary"].as_str().map(str::to_owned));
    let decision=record.decision.as_ref().map(|decision| {
        let mut value=json!({"actor":decision["actor"],"decidedAt":instant(record.decided_at.unwrap_or(record.created_at))});
        if let Some(id)=decision.get("decidedBy").or_else(||decision.get("subject").and_then(|s|s.get("id"))) {value["decidedBy"]=id.clone();}
        if let Some(id)=decision.get("policyId"){value["policyId"]=id.clone();}
        if let Some(reason)=&record.reason{value["reason"]=json!(reason);}value
    });
    let mut value = json!({"id":record.id,"chatId":record.chat_id,"scopeKey":record.scope_key,
        "toolName":record.tool_name,"kind":record.kind,"risk":record.risk,"status":record.status,
        "warnings":record.warnings,"truncated":record.truncated,"createdAt":instant(record.created_at)});
    for (key, entry) in [
        ("runId", record.run_id.map(Value::String)),
        ("actionId", record.action_id.map(Value::String)),
        ("summary", summary.map(Value::String)),
        ("decision", decision),
        ("decidedAt", record.decided_at.map(|ms| json!(instant(ms)))),
        ("claimedAt", record.claimed_at.map(|ms| json!(instant(ms)))),
        ("appliedAt", record.applied_at.map(|ms| json!(instant(ms)))),
    ] {
        if let Some(entry) = entry {
            value[key] = entry;
        }
    }
    value
}

/// AGENTKIT-90: the bounded wire view of an admitted agent task. Canonical
/// status and identity come from the task ROW (the one truth); nothing here
/// is recomputed. Secret material is structurally absent: the row carries
/// `run_profile` as an opaque digest-and-references pin, and only the
/// digest crosses this boundary.
pub(super) fn agent_task(task: &TaskRecord) -> Value {
    let mut value = json!({
        "taskId": task.task_id,
        "agentId": task.agent_id,
        "agentRevision": task.agent_revision,
        "admissionKey": task.admission_key,
        "kind": task.kind,
        "status": task.status,
        "priority": task.priority,
        "enqueuedAt": task.enqueued_at,
        "availableAt": task.available_at,
        "attemptCount": task.attempt_count,
        "cancelRequested": task.cancel_requested,
        "dependsOn": task.depends_on,
    });
    for (key, entry) in [
        ("startedAt", task.started_at),
        ("finishedAt", task.finished_at),
    ] {
        if let Some(ms) = entry {
            value[key] = json!(ms);
        }
    }
    if let Some(error) = &task.error {
        value["error"] = json!(error);
    }
    if let Some(parent) = &task.parent_task_id {
        value["parentTaskId"] = json!(parent);
    }
    if let Some(pin) = &task.run_profile {
        // The digest only — the pin's reference lists are inspected
        // through the dedicated run-profile explanation route.
        value["runProfileDigest"] = json!(pin.digest);
    }
    value
}

/// The bounded projection view: canonical task plus the derived attempt /
/// approval / usage / source windows. `usage` counters serialize as `null`
/// while unknown — a consumer must never read absence as zero.
pub(super) fn agent_task_projection(projection: &agentkit_store::TaskProjection) -> Value {
    let mut value = json!({
        "task": agent_task(&projection.task),
        "eventCount": projection.event_count,
        "usage": serde_json::to_value(&projection.usage)
            .expect("usage projection is plain numbers"),
        "sourceRefs": projection.source_refs,
    });
    if let Some(attempt_id) = &projection.attempt_id {
        value["attemptId"] = json!(attempt_id);
    }
    if let Some(seq) = projection.last_seq {
        value["lastSeq"] = json!(seq);
    }
    if let Some(approval) = &projection.approval {
        value["approval"] =
            serde_json::to_value(approval).expect("approval projection is plain strings");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_projection_preserves_imported_timestamp_text() {
        for text in [
            "2026-10-22T12:00:00.123456789Z",
            "2026-10-22T14:00:00+02:00",
            "invalid timestamp",
            "9223372036854776000",
        ] {
            let chat_record = ChatRecord {
                id: "chat".into(),
                title: None,
                metadata: json!({}),
                archived: false,
                created_at: text.into(),
                updated_at: text.into(),
            };
            let projected = chat(chat_record).unwrap();
            assert_eq!(projected["createdAt"], text);
            assert_eq!(projected["updatedAt"], text);
            let record: MessageRecord = serde_json::from_value(json!({"id":"m","chatId":"chat",
                "runId":null,"role":"user","content":"x","parentMessageId":null,
                "toolCallId":null,"toolCalls":null,"modelResultJson":null,"metadata":{},
                "orderKey":1,"depth":0,"branchIndex":0,"active":true,"createdAt":text}))
            .unwrap();
            assert_eq!(message(record).unwrap()["createdAt"], text);
        }
    }

    #[test]
    fn invalid_native_epoch_returns_problem_instead_of_panicking() {
        let record = ChatRecord {
            id: "chat".into(),
            title: None,
            metadata: json!({}),
            archived: false,
            created_at: i64::MAX.into(),
            updated_at: 0.into(),
        };
        assert!(chat(record).is_err());
    }
}
