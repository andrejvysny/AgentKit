//! The same seeded operation schedule runs against TypeScript's two adapters.
use agentkit_store::*;
use serde_json::{Value, json};
use std::collections::HashMap;

const VECTORS: &str = include_str!("../../../fixtures/conformance/seeded-v1.json");

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap()
}

fn strings(value: &Value, key: &str) -> Vec<String> {
    value[key].as_array().map_or_else(Vec::new, |items| {
        items
            .iter()
            .map(|item| item.as_str().unwrap().into())
            .collect()
    })
}

fn event(value: &Value) -> Value {
    json!({"seq": value["seq"], "eventId": value["id"], "type": value["type"].as_str().unwrap_or("task.conformance"),
        "timestamp": value["timestamp"].as_str().unwrap_or("2026-10-22T12:00:00.000Z"), "contractVersion": value["contractVersion"].as_str().unwrap_or("0.7.0")})
}

fn normalize_error(error: StoreError) -> Value {
    let code = match &error {
        StoreError::Duplicate(_) => "duplicate_task",
        StoreError::ScopeBusy(_) => "chat_busy",
        StoreError::LeaseLost(_) => "lease_lost",
        StoreError::InvalidInput(message) if message.starts_with("unknown dependency:") => {
            "unknown_dependency"
        }
        StoreError::InvalidInput(message) if message.starts_with("event seq must be ") => {
            "seq_conflict"
        }
        StoreError::InvalidImport { reason }
            if reason == "forward_parent" || reason == "unknown_parent" =>
        {
            "invalid_import"
        }
        StoreError::Database(rusqlite::Error::SqliteFailure(_, Some(message)))
            if message == "UNIQUE constraint failed: events.task_id, events.event_id" =>
        {
            "seq_conflict"
        }
        _ => panic!("unexpected conformance error: {error:?}"),
    };
    json!({"error": code})
}

async fn checkpoint(store: &SqliteStore, ids: &[String]) -> Result<Value> {
    let mut values = Vec::new();
    for id in ids {
        let task = store.get_task(id).await?.unwrap();
        let events: Vec<_> = store
            .events(id, None, 10000)
            .await?
            .into_iter()
            .map(|entry| json!({"seq": entry.event["seq"], "id": entry.event["eventId"]}))
            .collect();
        values.push(json!({"taskId": task.task_id, "scopeId": task.scope_id,
            "status": task.status, "priority": task.priority, "dependsOn": task.depends_on,
            "parentTaskId": task.parent_task_id, "attemptCount": task.attempt_count,
            "poisonCount": task.poison_count, "events": events, "nextSeq": store.next_sequence(id).await?}));
    }
    Ok(agentkit_contracts::javascript_json_value(&json!(values)))
}

async fn import(store: &SqliteStore, action: &Value, now: i64) -> Result<()> {
    let chat_id = text(action, "chatId");
    let messages = action["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| MessageRecord {
            id: text(message, "id").into(),
            chat_id: chat_id.into(),
            run_id: None,
            role: text(message, "role").into(),
            content: message["content"].clone(),
            parent_message_id: message["parentId"].as_str().map(str::to_owned),
            tool_call_id: None,
            tool_calls: None,
            model_result_json: None,
            metadata: if message["internal"] == true {
                json!({"internal": true})
            } else {
                json!({})
            },
            order_key: 0,
            depth: 0,
            branch_index: 0,
            active: message["active"].as_bool().unwrap(),
            created_at: now.into(),
        })
        .collect();
    store
        .import_conversation(ImportConversation {
            chat: ChatRecord {
                id: chat_id.into(),
                title: Some("Migrated".into()),
                metadata: json!({"source": "conformance"}),
                archived: false,
                created_at: now.into(),
                updated_at: now.into(),
            },
            messages,
        })
        .await?;
    Ok(())
}

async fn submit_task(store: &SqliteStore, action: &Value, now: i64) -> Result<Value> {
    store
        .submit(SubmitTask {
            task_id: text(action, "id").into(),
            kind: action["kind"].as_str().unwrap_or("test").into(),
            scope_id: text(action, "scope").into(),
            payload: json!({"value": action["id"]}),
            priority: action["priority"].as_i64().unwrap().try_into().unwrap(),
            now,
            available_at: action["available"].as_i64(),
            depends_on: strings(action, "depends"),
            parent_task_id: action["parent"].as_str().map(str::to_owned),
            exclusive_scope: action["exclusive"] == true,
        })
        .await?;
    Ok(json!({"ok":true}))
}
async fn claim_task(
    store: &SqliteStore,
    leases: &mut HashMap<String, Lease>,
    action: &Value,
    now: i64,
) -> Result<Value> {
    let result = store
        .claim(ClaimInput {
            owner_id: "conformance".into(),
            now,
            ttl_ms: 100,
            kinds: strings(action, "kinds"),
            busy_scopes: strings(action, "busy"),
            max_attempts: 3,
        })
        .await?;
    Ok(match result {
        Some(claimed) => {
            leases.insert(text(action, "slot").into(), claimed.lease);
            json!(claimed.task.task_id)
        }
        None => Value::Null,
    })
}
async fn write_events(
    store: &SqliteStore,
    leases: &HashMap<String, Lease>,
    action: &Value,
    now: i64,
) -> Result<Value> {
    let lease = &leases[text(action, "slot")];
    store
        .append_events(
            lease.fence(),
            action["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(event)
                .collect(),
            now,
        )
        .await?;
    Ok(json!({"ok":true}))
}
async fn settle_task(
    store: &SqliteStore,
    leases: &HashMap<String, Lease>,
    action: &Value,
    now: i64,
) -> Result<Value> {
    let lease = &leases[text(action, "slot")];
    store
        .settle(SettleTask {
            fence: lease.fence(),
            now,
            status: serde_json::from_value(action["status"].clone())?,
            error: None,
            events: vec![],
            messages: vec![],
        })
        .await?;
    Ok(json!({"ok":true}))
}
async fn read_import(store: &SqliteStore, action: &Value) -> Result<Value> {
    let chat_id = text(action, "chatId");
    if store.get_chat(chat_id).await?.is_none() {
        return Ok(json!({"exists": false, "messages": []}));
    }
    let messages: Vec<_> = store.messages(chat_id, true).await?.into_iter().map(|message|
                json!({"id": message.id, "role": message.role, "content": message.content,
                    "parentId": message.parent_message_id, "active": message.active, "metadata": message.metadata,
                    "orderKey": message.order_key, "depth": message.depth, "branchIndex": message.branch_index})
            ).collect();
    Ok(json!({"exists": store.get_chat(chat_id).await?.is_some(), "messages": messages}))
}
async fn operate(
    store: &SqliteStore,
    leases: &mut HashMap<String, Lease>,
    action: &Value,
    now: i64,
) -> Result<Value> {
    match text(action, "op") {
        "submit" => return submit_task(store, action, now).await,
        "claim" => return claim_task(store, leases, action, now).await,
        "append" => return write_events(store, leases, action, now).await,
        "settle" => return settle_task(store, leases, action, now).await,
        "cancel" => {
            store.cancel_task(text(action, "id"), now).await?;
        }
        "resume" => {
            store.resume_task(text(action, "id"), now).await?;
        }
        "recover" => {
            return Ok(json!(
                store.recover(now, 3, 0, RecoveryMode::Manual).await?.len()
            ));
        }
        "checkpoint" => return checkpoint(store, &strings(action, "ids")).await,
        "import" => import(store, action, now).await?,
        "readImport" => return read_import(store, action).await,
        other => panic!("unknown conformance operation {other}"),
    }
    Ok(json!({"ok":true}))
}

#[tokio::test]
async fn seeded_store_recovery_and_import_vectors() {
    let vectors: Value = serde_json::from_str(VECTORS).unwrap();
    assert_eq!(vectors["formatVersion"], 1);
    let directory = tempfile::tempdir().unwrap();
    let mut report = json!({"memory": {}, "file": {}});
    for mode in ["memory", "file"] {
        for scenario in vectors["scenarios"].as_array().unwrap() {
            let name = text(scenario, "name");
            let store = if mode == "memory" {
                SqliteStore::memory(32).unwrap()
            } else {
                SqliteStore::open(directory.path().join(format!("{name}.db")), 32).unwrap()
            };
            let mut leases = HashMap::new();
            let mut transcript = Vec::new();
            for (index, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
                let action = &step["action"];
                let actual = operate(&store, &mut leases, action, step["at"].as_i64().unwrap())
                    .await
                    .unwrap_or_else(normalize_error);
                assert_eq!(
                    actual,
                    step["expected"],
                    "{mode}/{name}/{index}/{}",
                    text(action, "op")
                );
                transcript.push(actual);
            }
            report[mode][name] = json!(transcript);
        }
    }
    if let Ok(path) = std::env::var("AGENTKIT_CONFORMANCE_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}
