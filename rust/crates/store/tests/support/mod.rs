#![allow(dead_code)]
use agentkit_store::*;
use serde_json::{Value, json};

pub fn task(id: &str, scope: &str) -> SubmitTask {
    SubmitTask {
        task_id: id.into(),
        kind: "test".into(),
        scope_id: scope.into(),
        payload: json!({"chatId":scope}),
        priority: 0,
        now: 1000,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: false,
    }
}

pub fn claim(now: i64) -> ClaimInput {
    ClaimInput {
        owner_id: "worker".into(),
        now,
        ttl_ms: 100,
        kinds: vec![],
        busy_scopes: vec![],
        max_attempts: 3,
    }
}

pub fn event(seq: u64, id: &str) -> Value {
    json!({"type":"run.test","seq":seq,"eventId":id,"timestamp":"2026-10-22T12:00:00.000Z","contractVersion":"0.7.0","data":{"nested":true}})
}

pub fn message(id: &str, chat: &str, run: Option<&str>) -> MessageInput {
    MessageInput {
        id: Some(id.into()),
        chat_id: chat.into(),
        run_id: run.map(str::to_owned),
        content: json!(id),
        ..Default::default()
    }
}

pub async fn finish(store: &SqliteStore, lease: Lease, status: TaskStatus, now: i64) {
    store
        .settle(SettleTask {
            fence: lease.fence(),
            now,
            status,
            error: None,
            events: vec![],
            messages: vec![],
        })
        .await
        .unwrap();
}
