//! AGENTKIT-112: public session normalization must carry native fields,
//! and native admission must be validated at SEND time — never discovered
//! only after the task was queued and claimed.

use agentkit_engine::{Runtime, SharedRuntime, SystemRuntime};
use agentkit_sessions::{
    AgentSendResult, AgentSessionService, AgentSessionServiceDeps, CreateSessionInput, SendInput,
    WhenBusy,
};
use agentkit_store::SqliteStore;
use serde_json::{Map, Value, json};
use std::sync::Arc;

fn native_target() -> Value {
    json!({
        "schemaVersion": 1,
        "backend": "claude-code",
        "backendVersion": "2.1.310",
        "profile": "plainChat",
        "executable": "/usr/bin/claude",
        "requestedModel": "sonnet",
        "authExpectation": "subscriptionLogin",
        "capabilities": {
            "backend": "claude-code",
            "matrixVersion": 1,
            "qualifiedVersion": "2.1.310",
            "flags": {}
        },
        "configIdentity": "cfg-test",
        "content": {
            "policyVersion": 1,
            "schemaVersion": "agentkit.chat.content.v1",
            "text": true,
            "extractedText": true,
            "images": false,
            "files": false
        }
    })
}

fn turn_input(id: &str, text: &str) -> Value {
    json!({
        "inputId": id,
        "text": text,
        "contextBlocks": [],
        "resume": {"mode": "fresh", "seed_history": []},
        "turnDeadlineMs": 5000
    })
}

fn continuation_scope(conversation: &str, parent: &str, position: i64) -> Value {
    json!({
        "conversation": conversation,
        "parent": parent,
        "historyPosition": position,
        "commitParent": format!("{parent}:answer"),
        "commitHistoryPosition": position + 1,
        "backend": "claude-code",
        "executable": "/usr/bin/claude",
        "configIdentity": "cfg-test"
    })
}

fn send_input(content: &str, input_id: &str, frozen: Map<String, Value>) -> SendInput {
    SendInput {
        content: json!(content),
        input_id: Some(input_id.into()),
        when_busy: WhenBusy::Queue,
        turn_id: None,
        on_settled: None,
        frozen_payload: Some(frozen),
    }
}

fn fixture() -> (SqliteStore, AgentSessionService<SqliteStore>) {
    let store = SqliteStore::memory(32).unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: Arc::new(SystemRuntime) as SharedRuntime,
        plan_approval: None,
        wake: None,
    });
    (store, service)
}

async fn create(service: &AgentSessionService<SqliteStore>, native_execution: Option<Value>) {
    let metadata = native_execution
        .map(|native| json!({"agentkit.nativeExecution": native}))
        .unwrap_or_else(|| json!({}));
    service
        .create_session(&CreateSessionInput {
            session_id: Some("native-chat".into()),
            profile_id: "coding".into(),
            turn_kind: Some("onemind.chat".into()),
            metadata: Some(metadata),
            ..Default::default()
        })
        .await
        .unwrap();
}

fn turn_id_of(result: &AgentSendResult) -> &str {
    match result {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        AgentSendResult::Queued { input } => {
            panic!("expected a turn, queued {:?}", input.input_id)
        }
    }
}

fn frozen_with_input(id: &str, text: &str) -> Map<String, Value> {
    let mut frozen = Map::new();
    frozen.insert("turnInput".into(), turn_input(id, text));
    frozen
}

#[tokio::test]
async fn session_frozen_native_stanza_reaches_every_turn() {
    let (store, service) = fixture();
    create(
        &service,
        Some(json!({ "executionTarget": { "native": native_target() } })),
    )
    .await;

    // Normalization KEEPS the stanza: the descriptor is its durable home.
    let state = service.get_state("native-chat").await.unwrap();
    assert_eq!(
        state.descriptor["nativeExecution"]["executionTarget"]["native"]["backend"],
        Value::from("claude-code")
    );

    // Every submitted turn inherits the session-frozen execution target,
    // plus the per-turn frozen input and continuation scope.
    let mut frozen = frozen_with_input("i1", "hello");
    frozen.insert(
        "continuationScope".into(),
        continuation_scope("conv-1", "root", 1),
    );
    let result = service
        .send("native-chat", &send_input("question", "i1", frozen))
        .await
        .unwrap();
    let task = store.get_task(turn_id_of(&result)).await.unwrap().unwrap();
    assert_eq!(task.kind, "onemind.chat");
    assert_eq!(
        task.payload["executionTarget"]["native"]["executable"],
        Value::from("/usr/bin/claude")
    );
    assert_eq!(task.payload["turnInput"]["text"], Value::from("hello"));
    assert_eq!(
        task.payload["continuationScope"]["historyPosition"],
        Value::from(1)
    );
}

#[tokio::test]
async fn queued_frozen_stanza_survives_the_flush() {
    let (store, service) = fixture();
    create(
        &service,
        Some(json!({ "executionTarget": { "native": native_target() } })),
    )
    .await;

    let first = service
        .send(
            "native-chat",
            &send_input("one", "i1", frozen_with_input("i1", "one")),
        )
        .await
        .unwrap();

    // Busy with the first turn: the second input queues, frozen bytes ride
    // the durable envelope.
    let mut second = frozen_with_input("i2", "two");
    second.insert(
        "continuationScope".into(),
        continuation_scope("conv-1", "msg-1", 2),
    );
    let queued = service
        .send("native-chat", &send_input("two", "i2", second))
        .await
        .unwrap();
    assert!(matches!(queued, AgentSendResult::Queued { .. }));

    // The live turn goes away; the flush promotes the queued input and the
    // staged task carries the IDENTICAL frozen bytes plus the session
    // stanza.
    store
        .cancel_task(turn_id_of(&first), SystemRuntime.now_ms())
        .await
        .unwrap();
    let flushed = service.flush_queue("native-chat").await.unwrap();
    let turn_id = match flushed {
        Some(AgentSendResult::Turn { turn_id, .. }) => turn_id,
        other => panic!("expected the flush to promote the input, got {other:?}"),
    };
    let task = store.get_task(&turn_id).await.unwrap().unwrap();
    assert_eq!(
        task.payload["turnInput"]["text"],
        Value::from("two"),
        "flushed payload: {}",
        task.payload
    );
    assert_eq!(
        task.payload["continuationScope"]["historyPosition"],
        Value::from(2)
    );
    assert_eq!(
        task.payload["executionTarget"]["native"]["backend"],
        Value::from("claude-code")
    );
}

/// Review R9: a native session whose turn carries NO `turnInput` anywhere
/// in the merged view is refused at SEND time — the executor could only
/// fail it at dispatch.
#[tokio::test]
async fn native_admission_without_turn_input_is_refused() {
    let (_store, service) = fixture();
    create(
        &service,
        Some(json!({ "executionTarget": { "native": native_target() } })),
    )
    .await;
    let error = service
        .send(
            "native-chat",
            &SendInput {
                content: json!("hello"),
                input_id: Some("i1".into()),
                when_busy: WhenBusy::Queue,
                turn_id: None,
                on_settled: None,
                frozen_payload: None,
            },
        )
        .await
        .expect_err("a native turn must carry its frozen input");
    assert_eq!(error.failure_event_code(), "invalid_input");
    assert!(
        error.to_string().contains("turnInput"),
        "the refusal names the missing key: {}",
        error
    );
}

/// Review R9 mirror: a per-turn input cannot hide a missing execution
/// target — the MERGED view must be complete either way.
#[tokio::test]
async fn native_admission_without_execution_target_is_refused() {
    let (_store, service) = fixture();
    create(&service, None).await;
    let error = service
        .send(
            "native-chat",
            &send_input("hello", "i1", frozen_with_input("i1", "hello")),
        )
        .await
        .expect_err("an inherited target is part of the contract");
    assert_eq!(error.failure_event_code(), "invalid_input");
    assert!(
        error.to_string().contains("executionTarget"),
        "the refusal names the missing key: {}",
        error
    );
}

/// Review R9 (R3's admission half): exact resume requires the complete
/// scoped binding at the door, mirroring the executor's
/// `continuation_scope_missing` dispatch refusal.
#[tokio::test]
async fn exact_resume_without_continuation_scope_is_refused() {
    let (_store, service) = fixture();
    create(
        &service,
        Some(json!({ "executionTarget": { "native": native_target() } })),
    )
    .await;
    let mut frozen = Map::new();
    frozen.insert(
        "turnInput".into(),
        json!({
            "inputId": "i1",
            "text": "resume me",
            "contextBlocks": [],
            "resume": {"mode": "exact", "vendorSessionId": "vs-1"},
            "turnDeadlineMs": 5000
        }),
    );
    let error = service
        .send("native-chat", &send_input("resume me", "i1", frozen))
        .await
        .expect_err("unscoped exact resume is refused, not queued to fail");
    assert_eq!(error.failure_event_code(), "invalid_input");
    assert!(
        error.to_string().contains("continuationScope"),
        "the refusal names the missing binding: {}",
        error
    );
}

/// Review R9: a queued input restated under the SAME `inputId` with
/// DIFFERENT frozen bytes is an `input_conflict` — never a silent replay
/// of the original target. The identical restatement still replays.
#[tokio::test]
async fn queued_replay_with_swapped_frozen_bytes_conflicts() {
    let (_store, service) = fixture();
    create(
        &service,
        Some(json!({ "executionTarget": { "native": native_target() } })),
    )
    .await;
    let _first = service
        .send(
            "native-chat",
            &send_input("one", "i1", frozen_with_input("i1", "one")),
        )
        .await
        .unwrap();
    let mut second = frozen_with_input("i2", "two");
    second.insert(
        "continuationScope".into(),
        continuation_scope("conv-1", "msg-1", 2),
    );
    let queued = service
        .send("native-chat", &send_input("two", "i2", second.clone()))
        .await
        .unwrap();
    assert!(matches!(queued, AgentSendResult::Queued { .. }));
    // The faithful restatement replays the queued input.
    let again = service
        .send("native-chat", &send_input("two", "i2", second.clone()))
        .await
        .expect("identical restatement replays");
    assert!(matches!(again, AgentSendResult::Queued { .. }));
    // Same id, changed frozen identity: conflict, not a silent replay.
    let mut swapped = second.clone();
    swapped.insert("turnInput".into(), turn_input("i2", "TWO DIFFERENT"));
    let error = service
        .send("native-chat", &send_input("two", "i2", swapped))
        .await
        .expect_err("a swapped frozen target must conflict");
    assert!(
        error.to_string().contains("input_conflict"),
        "expected an input_conflict, got {}: {}",
        error.failure_event_code(),
        error
    );
}

#[tokio::test]
async fn malformed_native_admission_is_refused_at_send() {
    let (store, service) = fixture();
    create(
        &service,
        Some(json!({ "executionTarget": { "native": native_target() } })),
    )
    .await;

    // A frozen executionTarget that no longer parses is a SEND-time
    // refusal: nothing is queued, nothing is claimable.
    let mut frozen = Map::new();
    frozen.insert(
        "executionTarget".into(),
        json!({ "native": { "schemaVersion": 1 } }),
    );
    let error = service
        .send("native-chat", &send_input("bad", "bad-1", frozen))
        .await
        .unwrap_err();
    assert_eq!(error.failure_event_code(), "invalid_input");
    assert!(
        error.to_string().contains("executionTarget"),
        "the refusal must name the bad key: {}",
        error
    );
    assert!(store.get_task("turn:bad-1").await.unwrap().is_none());
    assert!(
        store.get_input("bad-1".into()).await.unwrap().is_none(),
        "a refused send leaves no queued input behind"
    );
}

#[tokio::test]
async fn unknown_frozen_key_is_refused() {
    let (_store, service) = fixture();
    create(
        &service,
        Some(json!({ "executionTarget": { "native": native_target() } })),
    )
    .await;
    let mut frozen = Map::new();
    frozen.insert("smuggled".into(), json!({"argv": ["evil"]}));
    let error = service
        .send("native-chat", &send_input("smuggle", "smug-1", frozen))
        .await
        .unwrap_err();
    assert_eq!(error.failure_event_code(), "invalid_input");
    assert!(error.to_string().contains("smuggled"));
}

#[tokio::test]
async fn malformed_session_native_stanza_is_refused_at_creation() {
    let (_store, service) = fixture();
    // turnInput is per-turn admission — freezing it at session scope is
    // always a refusal, never a silent drop.
    let error = service
        .create_session(&CreateSessionInput {
            session_id: Some("native-chat".into()),
            profile_id: "coding".into(),
            turn_kind: Some("onemind.chat".into()),
            metadata: Some(json!({
                "agentkit.nativeExecution": { "turnInput": turn_input("x", "frozen forever") }
            })),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.failure_event_code(), "invalid_input");

    // Non-record values are malformed-present, never absent.
    let error = service
        .create_session(&CreateSessionInput {
            session_id: Some("native-chat".into()),
            profile_id: "coding".into(),
            turn_kind: Some("onemind.chat".into()),
            metadata: Some(json!({ "agentkit.nativeExecution": "claude" })),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.failure_event_code(), "invalid_input");

    // Review round-3 F5: an INHERITED stanza is typed at creation, not
    // just present — a garbage execution target can never become a
    // session whose turns only die after queueing and claim.
    let error = service
        .create_session(&CreateSessionInput {
            session_id: Some("native-chat".into()),
            profile_id: "coding".into(),
            turn_kind: Some("onemind.chat".into()),
            metadata: Some(json!({
                "agentkit.nativeExecution": { "executionTarget": "garbage" }
            })),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.failure_event_code(), "invalid_input");
}
