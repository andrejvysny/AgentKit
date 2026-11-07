//! AGENTKIT-90 acceptance cells for the bounded task projection read
//! model and the consumer redactor, at the store boundary where the
//! durable rows live: the projection is DERIVED (recomputed from the
//! task row and a bounded page of the event log on every read), so it
//! must rebuild identically from reloaded rows — after restart and
//! mid-cancellation — and the redactor must narrow only the wire, never
//! the log.

mod support;

use agentkit_store::*;
use serde_json::{Value, json};
use support::task;

fn envelope(seq: u64, kind: &str, extra: Value) -> Value {
    let mut event = json!({
        "type": kind,
        "seq": seq,
        "eventId": format!("evt-{seq}"),
        "timestamp": "2026-11-07T12:00:00.000Z",
        "contractVersion": "0.7.0",
    });
    for (key, value) in extra.as_object().expect("extra object") {
        event[key] = value.clone();
    }
    event
}

/// Submit, claim, and append durable events; returns the task id plus
/// the attempt identity the lease stamped into every event.
async fn seeded(
    store: &SqliteStore,
    id: &str,
    events: Vec<Value>,
) -> (String, Option<String>, LeaseFence) {
    store.submit(task(id, "scope")).await.unwrap();
    let claimed = store
        .claim(ClaimInput {
            owner_id: "worker".into(),
            now: 2_000,
            ttl_ms: 100_000,
            kinds: vec![],
            busy_scopes: vec![],
            max_attempts: 3,
        })
        .await
        .unwrap()
        .expect("queued task claims");
    let attempt = claimed.lease.attempt_id.clone();
    let fence = claimed.lease.fence();
    store
        .append_events(claimed.lease.fence(), events, 2_100)
        .await
        .unwrap();
    (claimed.task.task_id, Some(attempt), fence)
}

// ---------------------------------------------------------------------------
// Derivation is canonical: same rows, same projection — across restart.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn projection_rebuilds_identically_from_reloaded_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("projection.db");
    let events = vec![
        envelope(
            1,
            "run.usage",
            json!({"usage": {"promptTokens": 100, "completionTokens": 40, "totalTokens": 140}}),
        ),
        envelope(
            2,
            "run.usage",
            json!({"usage": {"promptTokens": 5, "cachedPromptTokens": 2}}),
        ),
        envelope(
            3,
            "run.approval",
            json!({"approval": {"state": "granted", "approvalId": "a1", "toolName": "edit"}}),
        ),
        envelope(
            4,
            "run.context",
            json!({"sourceRefs": [{"id": "skill:review", "kind": "skill", "label": "Review"}]}),
        ),
    ];
    let first = SqliteStore::open(&path, 4).unwrap();
    let (task_id, attempt, _fence) = seeded(&first, "keep", events).await;
    let before = first.task_projection(&task_id, 0).await.unwrap().unwrap();
    assert_eq!(before.attempt_id.as_deref(), attempt.as_deref());
    assert_eq!(before.usage.prompt_tokens, Some(105.0), "usage totals sum");
    assert_eq!(before.usage.completion_tokens, Some(40.0));
    assert_eq!(before.usage.cached_prompt_tokens, Some(2.0));
    assert_eq!(before.usage.usage_events, 2);
    assert_eq!(before.approval.as_ref().unwrap().state, "granted");
    assert_eq!(before.source_refs.len(), 1);

    // RESTART: a fresh process reads the same durable rows and derives
    // the same view. No truth column can drift because there is none.
    drop(first);
    let reopened = SqliteStore::open(&path, 4).unwrap();
    let after = reopened
        .task_projection(&task_id, 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after, before, "projection drifted across restart");

    // PURE derivation: same inputs, byte-identical output.
    let task_row = reopened.get_task(&task_id).await.unwrap().unwrap();
    let records = reopened
        .events_after_number(&task_id, None, 1_000)
        .await
        .unwrap();
    let once = derive_task_projection(&task_row, attempt.as_deref(), &records);
    let twice = derive_task_projection(&task_row, attempt.as_deref(), &records);
    assert_eq!(once, twice);
    assert_eq!(once, before);
}

#[tokio::test]
async fn projection_agrees_with_canonical_state_mid_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cancel.db");
    let store = SqliteStore::open(&path, 4).unwrap();
    let (task_id, _attempt, fence) = seeded(
        &store,
        "cancel-me",
        vec![envelope(1, "run.note", json!({}))],
    )
    .await;
    assert_eq!(
        store
            .task_projection(&task_id, 0)
            .await
            .unwrap()
            .unwrap()
            .task
            .status,
        TaskStatus::Running
    );

    // A LEASED task: cancel requests interruption; the row records the
    // request immediately and the runner settles the terminal at the
    // safe boundary. The projection mirrors the row at every step.
    store.cancel_task(&task_id, 3_000).await.unwrap();
    let mid = store.task_projection(&task_id, 0).await.unwrap().unwrap();
    assert_eq!(mid.task.status, TaskStatus::Running);
    assert!(mid.task.cancel_requested, "the request is canonical now");

    store
        .settle(SettleTask {
            fence,
            now: 3_500,
            status: TaskStatus::Cancelled,
            error: None,
            events: vec![],
            messages: vec![],
        })
        .await
        .unwrap();
    let cancelled = store.task_projection(&task_id, 0).await.unwrap().unwrap();
    assert_eq!(cancelled.task.status, TaskStatus::Cancelled);
    assert!(cancelled.task.cancel_requested);
    assert_eq!(cancelled.task.finished_at, Some(3_500));

    // Reopening mid-cancelled state rebuilds the same canonical view.
    drop(store);
    let reopened = SqliteStore::open(&path, 4).unwrap();
    assert_eq!(
        reopened
            .task_projection(&task_id, 0)
            .await
            .unwrap()
            .unwrap(),
        cancelled,
        "mid-cancellation state survived restart identically"
    );
}

#[tokio::test]
async fn unknown_usage_is_unknown_and_never_a_zero() {
    let store = SqliteStore::memory(4).unwrap();
    let (task_id, attempt, _fence) =
        seeded(&store, "silent", vec![envelope(1, "run.note", json!({}))]).await;
    let projection = store.task_projection(&task_id, 0).await.unwrap().unwrap();
    // No event ever reported a number: every counter is None, and the
    // audit field proves WHY (zero usage events, not zeros).
    assert_eq!(projection.usage.prompt_tokens, None);
    assert_eq!(projection.usage.completion_tokens, None);
    assert_eq!(projection.usage.total_tokens, None);
    assert_eq!(projection.usage.cached_prompt_tokens, None);
    assert_eq!(projection.usage.reasoning_tokens, None);
    assert_eq!(projection.usage.usage_events, 0);
    assert_eq!(projection.approval, None);
    assert!(projection.source_refs.is_empty());
    assert_eq!(projection.attempt_id, attempt);

    // An explicit JSON null is "asked and not told" — still not a zero,
    // and it does not MAKE the counter known.
    let store = SqliteStore::memory(4).unwrap();
    let (task_id, _, _fence) = seeded(
        &store,
        "nulls",
        vec![envelope(
            1,
            "run.usage",
            json!({"usage": {"promptTokens": null, "totalTokens": null}}),
        )],
    )
    .await;
    let projection = store.task_projection(&task_id, 0).await.unwrap().unwrap();
    assert_eq!(projection.usage.prompt_tokens, None);
    assert_eq!(projection.usage.total_tokens, None);
    assert_eq!(
        projection.usage.usage_events, 1,
        "the event did report usage"
    );

    // Serialize: unknown counters leave as null, never 0.
    let wire = serde_json::to_value(&projection.usage).unwrap();
    assert_eq!(wire["promptTokens"], Value::Null);
    assert_ne!(wire["promptTokens"], json!(0));
}

#[tokio::test]
async fn the_window_is_bounded_and_head_first() {
    let store = SqliteStore::memory(4).unwrap();
    let (task_id, _, _fence) = seeded(
        &store,
        "many",
        (1..=6)
            .map(|seq| envelope(seq, "run.note", json!({})))
            .collect(),
    )
    .await;
    let tight = store.task_projection(&task_id, 2).await.unwrap().unwrap();
    assert_eq!(tight.event_count, 2, "the projection costs O(window)");
    assert_eq!(tight.last_seq, Some(2.0), "the window is the log head");
    let wide = store.task_projection(&task_id, 0).await.unwrap().unwrap();
    assert_eq!(wide.event_count, 6, "0 means the host default window");
    assert_eq!(wide.last_seq, Some(6.0));
}

// ---------------------------------------------------------------------------
// The consumer redactor.
// ---------------------------------------------------------------------------

#[test]
fn redaction_replaces_credentials_bags_and_oversized_tool_text() {
    let huge = "y".repeat(MAX_TOOL_TEXT_BYTES + 1);
    let event = json!({
        "type": "run.tool.result",
        "seq": 4.0,
        "eventId": "evt-4",
        "agentId": "agent-9",
        "attemptId": "attempt-9",
        "status": "running",
        "toolName": "http.fetch",
        "arguments": {"api-key": "S1", "API_KEY": "S2", "apiKey": "S3", "password": "S4"},
        "headers": {"cookie": "session=x", "x-trace": "t"},
        "env": {"TOKEN": "abc"},
        "result": huge,
        "secretRefs": ["vault://ref/1"]
    });
    let redacted = redact_event(&event);
    let wire = serde_json::to_string(&redacted).unwrap();
    for sentinel in ["S1", "S2", "S3", "S4", "session=x", "abc"] {
        assert!(!wire.contains(sentinel), "{sentinel} crossed the wire");
    }
    assert_eq!(redacted["arguments"]["apiKey"], json!("***"));
    assert_eq!(
        redacted["headers"]["x-trace"],
        json!("***"),
        "bags go whole"
    );
    assert_eq!(redacted["env"]["TOKEN"], json!("***"));
    // Oversized tool text collapses to a bounded summary, not a cut.
    assert_eq!(redacted["result"]["truncated"], json!(true));
    assert_eq!(redacted["result"]["bytes"], json!(huge.len()));
    assert_eq!(redacted["result"]["sha256"].as_str().unwrap().len(), 64);
    // REFERENCES are not values: they survive so history stays auditable.
    assert_eq!(redacted["secretRefs"][0], json!("vault://ref/1"));
    // Identity and ordering survive: redaction narrows payloads only.
    assert_eq!(redacted["agentId"], json!("agent-9"));
    assert_eq!(redacted["attemptId"], json!("attempt-9"));
    assert_eq!(redacted["seq"], json!(4.0));
    assert_eq!(redacted["status"], json!("running"));
    assert_eq!(redacted["toolName"], json!("http.fetch"));
}

#[test]
fn redaction_leaves_non_tool_text_and_nulls_alone() {
    let long = "z".repeat(MAX_TOOL_TEXT_BYTES + 1);
    // A non-tool event's long string is not a tool payload.
    let event = json!({"type": "run.note", "seq": 1.0, "data": long});
    assert_eq!(redact_event(&event)["data"], json!(long));
    // A credential key with a null value stays null (absence is honest).
    let event = json!({"type": "run.tool.request", "seq": 1.0, "arguments": {"token": null}});
    assert_eq!(redact_event(&event)["arguments"]["token"], Value::Null);
    // Nested tool payloads are redacted where the payload keys appear.
    let event = json!({
        "type": "run.tool.request", "seq": 1.0,
        "data": {"arguments": {"authorization": "S", "result": long}}
    });
    let redacted = redact_event(&event);
    assert_eq!(redacted["data"]["arguments"]["authorization"], json!("***"));
    assert_eq!(
        redacted["data"]["arguments"]["result"]["truncated"],
        json!(true)
    );
}
