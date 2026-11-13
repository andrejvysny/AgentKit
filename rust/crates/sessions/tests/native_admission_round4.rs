//! Review round-4 F8: public admission must be consistent across the
//! immediate and queued paths (one digest, one conflict answer) and
//! semantically validated BEFORE anything persists (no successful
//! admission whose rows the executor is guaranteed to refuse).

use agentkit_engine::{SharedRuntime, SystemRuntime};
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

fn continuation_scope(
    conversation: &str,
    parent: &str,
    position: i64,
    config_identity: &str,
) -> Value {
    json!({
        "conversation": conversation,
        "parent": parent,
        "historyPosition": position,
        "commitParent": format!("{parent}:answer"),
        "commitHistoryPosition": position + 1,
        "backend": "claude-code",
        "executable": "/usr/bin/claude",
        "configIdentity": config_identity
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

fn fixture() -> (SqliteStore, AgentSessionService) {
    let store = SqliteStore::memory(32).unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: Arc::new(SystemRuntime) as SharedRuntime,
        plan_approval: None,
        wake: None,
    });
    (store, service)
}

async fn create(service: &AgentSessionService) {
    service
        .create_session(&CreateSessionInput {
            session_id: Some("native-chat".into()),
            profile_id: "coding".into(),
            turn_kind: Some("onemind.chat".into()),
            metadata: Some(json!({
                "agentkit.nativeExecution": { "executionTarget": { "native": native_target() } }
            })),
            ..Default::default()
        })
        .await
        .unwrap();
}

fn frozen_with_input(id: &str, text: &str) -> Map<String, Value> {
    let mut frozen = Map::new();
    frozen.insert("turnInput".into(), turn_input(id, text));
    frozen
}

fn turn_id_of(result: &AgentSendResult) -> &str {
    match result {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        other => panic!("expected a turn, got {other:?}"),
    }
}

/// Review round-4 F8-a reproduced: an IMMEDIATE admission (idle session,
/// the turn starts straight away and no input row is written) replayed
/// under the SAME `inputId` with a DIFFERENT frozen target silently
/// returned the old turn. The immediate path now digests the same
/// admission envelope the queued path compares — the replay conflicts
/// exactly like a queued restatement would, and the faithful restatement
/// still replays.
#[tokio::test]
async fn immediate_replay_with_swapped_frozen_target_conflicts() {
    let (store, service) = fixture();
    create(&service).await;
    let first = service
        .send(
            "native-chat",
            &send_input("one", "i1", frozen_with_input("i1", "one")),
        )
        .await
        .unwrap();
    let turn_id = turn_id_of(&first).to_owned();
    assert_eq!(turn_id, "turn:i1");
    assert!(
        store.get_input("i1".into()).await.unwrap().is_none(),
        "the immediate path writes no input row — the turn itself is the record"
    );

    // Faithful restatement: the same turn, again, no second task.
    let again = service
        .send(
            "native-chat",
            &send_input("one", "i1", frozen_with_input("i1", "one")),
        )
        .await
        .expect("identical restatement replays the turn");
    assert_eq!(turn_id_of(&again), turn_id);
    assert_eq!(store.list_tasks(None).await.unwrap().len(), 1);

    // Same id, swapped frozen identity: the same conflict the queued path
    // produces — never a silent replay of the original bytes.
    let mut swapped = frozen_with_input("i1", "one");
    swapped.insert("turnInput".into(), turn_input("i1", "TOTALLY DIFFERENT"));
    let error = service
        .send("native-chat", &send_input("one", "i1", swapped))
        .await
        .expect_err("a swapped frozen target must conflict");
    assert!(
        error.failure_event_code() == "input_conflict",
        "expected an input_conflict, got {}: {}",
        error.failure_event_code(),
        error
    );

    // Swapped CONTENT under the same id conflicts too.
    let error = service
        .send(
            "native-chat",
            &send_input("TWO", "i1", frozen_with_input("i1", "one")),
        )
        .await
        .expect_err("swapped content must conflict");
    assert!(
        error.failure_event_code() == "input_conflict",
        "expected an input_conflict, got {}: {}",
        error.failure_event_code(),
        error
    );
}

/// Review round-4 F8-b reproduced: a typed continuation scope whose
/// configuration identity contradicts the execution target was ACCEPTED
/// and PERSISTED at admission, dying only at claim. Admission now runs
/// the executor's own cross-field validator: the send is refused and
/// nothing durable is left behind.
#[tokio::test]
async fn contradictory_continuation_identity_is_refused_at_admission() {
    let (store, service) = fixture();
    create(&service).await;
    let mut frozen = frozen_with_input("bad-1", "drift");
    frozen.insert(
        "continuationScope".into(),
        // Same shape the executor accepts at parse time — but the config
        // identity contradicts the session-frozen target ("cfg-test").
        continuation_scope("conv-1", "msg-1", 2, "cfg-OTHER"),
    );
    let error = service
        .send("native-chat", &send_input("drift", "bad-1", frozen))
        .await
        .expect_err("a contradicting frozen scope is refused at send");
    assert_eq!(error.failure_event_code(), "invalid_input");
    assert!(
        error.to_string().contains("config identity"),
        "the refusal names the drifted field: {}",
        error
    );
    assert!(
        store.get_task("turn:bad-1").await.unwrap().is_none(),
        "a refused admission leaves no task behind"
    );
    assert!(
        store.get_input("bad-1".into()).await.unwrap().is_none(),
        "a refused admission leaves no queued input behind"
    );
    assert!(
        store.list_tasks(None).await.unwrap().is_empty(),
        "nothing persisted at all"
    );
}

/// The agreeing shape still admits cleanly: the scope matching the
/// frozen target is accepted and durably queued.
#[tokio::test]
async fn agreeing_continuation_identity_still_admits() {
    let (store, service) = fixture();
    create(&service).await;
    // A live turn first, so the second send takes the QUEUED shape —
    // the digest the input row stores is the immediate path's digest too.
    let _first = service
        .send(
            "native-chat",
            &send_input("one", "i1", frozen_with_input("i1", "one")),
        )
        .await
        .unwrap();
    let mut frozen = frozen_with_input("i2", "two");
    frozen.insert(
        "continuationScope".into(),
        continuation_scope("conv-1", "msg-1", 2, "cfg-test"),
    );
    let queued = service
        .send("native-chat", &send_input("two", "i2", frozen.clone()))
        .await
        .expect("an agreeing scope admits");
    assert!(matches!(queued, AgentSendResult::Queued { .. }));
    assert!(store.get_input("i2".into()).await.unwrap().is_some());
}
