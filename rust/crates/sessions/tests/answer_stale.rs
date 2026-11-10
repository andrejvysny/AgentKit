//! Answer-stale parity with the TypeScript authority
//! (`packages/host/src/sessions/human-answer.ts`): every stale reason
//! (`it names no turn`, `it was cancelled`, `its turn is no longer waiting`,
//! `it expired`) crosses as `answer_stale` with the TS message text; a same
//! replay stays a duplicate success; an answer that differs stays
//! `answer_conflict`.

use agentkit_engine::{SharedRuntime, SystemRuntime};
use agentkit_sessions::*;
use agentkit_store::*;
use serde_json::{Value, json};
use std::sync::Arc;

const SESSION: &str = "s1";
const DIGEST: &str = "a5ffc907b0a2d9c0f8b9d1647a3e5cb13d24f7bd0f9a41e2c3d5e7f8091a2b3c";

async fn fixture() -> (SqliteStore, AgentSessionService) {
    let store = SqliteStore::memory(8).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime,
        plan_approval: None,
        wake: None,
    });
    service
        .create_session(&CreateSessionInput {
            session_id: Some(SESSION.into()),
            profile_id: "coding".into(),
            model: Some("model".into()),
            ..CreateSessionInput::default()
        })
        .await
        .unwrap();
    (store, service)
}

fn approval_content(standing: bool) -> Value {
    let mut content = json!({
        "kind": "approval",
        "toolName": "write_file",
        "argsSha256": DIGEST,
        "prompt": "write x",
    });
    if standing {
        content["standing"] = json!({"digest": DIGEST, "label": "write a.txt"});
    }
    content
}

fn question_content() -> Value {
    json!({"kind": "question", "prompt": "which file?", "choices": ["a.txt"]})
}

fn request(
    input_id: &str,
    target_task_id: Option<&str>,
    content: Value,
    expires_at: Option<&str>,
) -> CreateAgentInput {
    CreateAgentInput {
        input_id: input_id.into(),
        session_id: SESSION.into(),
        kind: AgentInputKind::HumanRequest,
        content_hash: format!("hash-{input_id}"),
        content,
        target_task_id: target_task_id.map(str::to_owned),
        on_settled: None,
        attempt_id: Some("attempt-1".into()),
        tool_call_id: Some("call-1".into()),
        expires_at: expires_at.map(str::to_owned),
    }
}

fn answer(request_id: &str, answer_id: &str, response: Value) -> AnswerInput {
    AnswerInput {
        request_id: request_id.into(),
        answer_id: answer_id.into(),
        response,
    }
}

/// A submitted turn, still `queued` — the fixture for "its turn is no longer
/// waiting".
async fn queued_turn(store: &SqliteStore, turn: &str) {
    store
        .submit(SubmitTask {
            task_id: turn.into(),
            kind: CHAT_TURN_TASK_KIND.to_owned(),
            scope_id: SESSION.into(),
            payload: json!({"chatId": SESSION}),
            priority: 0,
            now: 1000,
            available_at: Some(1000),
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .unwrap();
}

/// A turn parked on the request, with its run chain present so the
/// question-answer path has a tool record to append under.
async fn parked_turn(store: &SqliteStore, turn: &str, req: CreateAgentInput) {
    let user = MessageInput {
        id: Some(format!("{turn}-user")),
        chat_id: SESSION.into(),
        run_id: None,
        role: "user".into(),
        content: json!({"role": "user", "content": "do it"}),
        ..MessageInput::default()
    };
    let assistant = MessageInput {
        id: Some(format!("{turn}-assistant")),
        chat_id: SESSION.into(),
        run_id: Some(turn.into()),
        role: "assistant".into(),
        content: json!({"role": "assistant", "content": "working"}),
        ..MessageInput::default()
    };
    store
        .submit_turn(
            SubmitTask {
                task_id: turn.into(),
                kind: CHAT_TURN_TASK_KIND.to_owned(),
                scope_id: SESSION.into(),
                payload: json!({"chatId": SESSION}),
                priority: 0,
                now: 1000,
                available_at: Some(1000),
                depends_on: vec![],
                parent_task_id: None,
                exclusive_scope: false,
            },
            vec![user, assistant],
        )
        .await
        .unwrap();
    let claimed = store
        .claim_with_concurrency(
            ClaimInput {
                owner_id: "worker".into(),
                now: 1500,
                ttl_ms: 1_000_000,
                kinds: vec![],
                busy_scopes: vec![],
                max_attempts: 3,
            },
            1,
        )
        .await
        .unwrap()
        .expect("the seeded turn is claimable");
    assert_eq!(claimed.task.task_id, turn);
    store
        .park_turn(ParkTurn {
            fence: claimed.lease.fence(),
            assistant_message: None,
            events: vec![],
            request: Some(req),
            now: 1600,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn a_request_that_names_no_turn_is_stale() {
    let (store, service) = fixture().await;
    store
        .create_input(request("req-no-turn", None, question_content(), None), 1000)
        .await
        .unwrap();
    let error = service
        .answer(&answer("req-no-turn", "ans-1", json!({"answer": "a.txt"})))
        .await
        .unwrap_err();
    assert_eq!(error.failure_event_code(), "answer_stale");
    assert_eq!(
        session_message(&error),
        "Request req-no-turn can no longer be answered: it names no turn."
    );
}

#[tokio::test]
async fn a_cancelled_request_is_stale() {
    let (store, service) = fixture().await;
    store
        .create_input(
            request("req-cancelled", Some("turn-gone"), question_content(), None),
            1000,
        )
        .await
        .unwrap();
    store
        .transition_input(
            "req-cancelled".into(),
            vec![AgentInputStatus::Pending],
            AgentInputStatus::Cancelled,
            AgentInputPatch::default(),
            1200,
        )
        .await
        .unwrap();
    let error = service
        .answer(&answer(
            "req-cancelled",
            "ans-1",
            json!({"answer": "a.txt"}),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.failure_event_code(), "answer_stale");
    assert_eq!(
        session_message(&error),
        "Request req-cancelled can no longer be answered: it was cancelled."
    );
}

#[tokio::test]
async fn a_turn_that_is_no_longer_waiting_is_stale() {
    let (store, service) = fixture().await;
    queued_turn(&store, "turn-queued").await;
    store
        .create_input(
            request(
                "req-not-waiting",
                Some("turn-queued"),
                question_content(),
                None,
            ),
            1000,
        )
        .await
        .unwrap();
    let error = service
        .answer(&answer(
            "req-not-waiting",
            "ans-1",
            json!({"answer": "a.txt"}),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.failure_event_code(), "answer_stale");
    assert_eq!(
        session_message(&error),
        "Request req-not-waiting can no longer be answered: its turn is no longer waiting."
    );
}

#[tokio::test]
async fn a_turn_whose_record_is_gone_is_stale() {
    let (store, service) = fixture().await;
    store
        .create_input(
            request("req-gone", Some("turn-missing"), question_content(), None),
            1000,
        )
        .await
        .unwrap();
    let error = service
        .answer(&answer("req-gone", "ans-1", json!({"answer": "a.txt"})))
        .await
        .unwrap_err();
    assert_eq!(error.failure_event_code(), "answer_stale");
    assert_eq!(
        session_message(&error),
        "Request req-gone can no longer be answered: its turn is no longer waiting."
    );
}

#[tokio::test]
async fn an_expired_request_is_stale() {
    let (store, service) = fixture().await;
    parked_turn(
        &store,
        "turn-expired",
        request(
            "req-expired",
            Some("turn-expired"),
            question_content(),
            Some("2020-01-01T00:00:00.000Z"),
        ),
    )
    .await;
    let error = service
        .answer(&answer("req-expired", "ans-1", json!({"answer": "a.txt"})))
        .await
        .unwrap_err();
    assert_eq!(error.failure_event_code(), "answer_stale");
    assert_eq!(
        session_message(&error),
        "Request req-expired can no longer be answered: it expired."
    );
}

#[tokio::test]
async fn a_malformed_approval_answer_to_a_cancelled_request_is_invalid_input() {
    // The TypeScript transaction reads the answer's SHAPE before any stale
    // gate: a cancelled approval with a malformed response is
    // `invalid_input`, never `answer_stale`.
    let (store, service) = fixture().await;
    store
        .create_input(
            request(
                "req-malformed",
                Some("turn-gone"),
                approval_content(false),
                None,
            ),
            1000,
        )
        .await
        .unwrap();
    store
        .transition_input(
            "req-malformed".into(),
            vec![AgentInputStatus::Pending],
            AgentInputStatus::Cancelled,
            AgentInputPatch::default(),
            1200,
        )
        .await
        .unwrap();
    let error = service
        .answer(&answer(
            "req-malformed",
            "ans-1",
            json!({"decision": "maybe"}),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.failure_event_code(), "invalid_input");
}

#[tokio::test]
async fn a_remembered_answer_without_a_standing_key_is_invalid_input_before_stale() {
    // `assertRememberEligible` is the other pre-transaction refusal: it
    // fires before `it names no turn` would classify the same record.
    let (store, service) = fixture().await;
    store
        .create_input(
            request("req-remember", None, approval_content(false), None),
            1000,
        )
        .await
        .unwrap();
    let error = service
        .answer(&answer(
            "req-remember",
            "ans-1",
            json!({"decision": "approve", "remember": true}),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.failure_event_code(), "invalid_input");
    assert_eq!(
        session_message(&error),
        "This call is not eligible for a standing approval."
    );
}

#[tokio::test]
async fn a_parked_turn_still_answers_and_replays_as_a_duplicate() {
    let (store, service) = fixture().await;
    parked_turn(
        &store,
        "turn-ok",
        request("req-ok", Some("turn-ok"), approval_content(false), None),
    )
    .await;
    let response = json!({"decision": "approve"});
    let receipt = service
        .answer(&answer("req-ok", "ans-1", response.clone()))
        .await
        .unwrap();
    assert!(!receipt.duplicate);
    assert_eq!(receipt.turn_id, "turn-ok");
    assert_eq!(receipt.message_id, "");
    assert_eq!(
        store.get_task("turn-ok").await.unwrap().unwrap().status,
        TaskStatus::Queued,
        "the answer re-admitted the parked turn"
    );
    // The same answer replayed is the caller retrying: a duplicate receipt,
    // never an error.
    let replay = service
        .answer(&answer("req-ok", "ans-1", response))
        .await
        .unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.turn_id, "turn-ok");
    assert_eq!(replay.message_id, "");
}

#[tokio::test]
async fn a_replayed_question_answer_still_succeeds_as_a_duplicate() {
    let (store, service) = fixture().await;
    parked_turn(
        &store,
        "turn-question",
        request(
            "req-question",
            Some("turn-question"),
            question_content(),
            None,
        ),
    )
    .await;
    let response = json!({"answer": "a.txt"});
    let receipt = service
        .answer(&answer("req-question", "ans-1", response.clone()))
        .await
        .unwrap();
    assert!(!receipt.duplicate);
    assert!(!receipt.message_id.is_empty());
    let replay = service
        .answer(&answer("req-question", "ans-1", response))
        .await
        .unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.message_id, receipt.message_id);
}

#[tokio::test]
async fn an_answered_request_answered_differently_still_conflicts() {
    let (store, service) = fixture().await;
    parked_turn(
        &store,
        "turn-conflict",
        request(
            "req-conflict",
            Some("turn-conflict"),
            approval_content(false),
            None,
        ),
    )
    .await;
    service
        .answer(&answer(
            "req-conflict",
            "ans-1",
            json!({"decision": "approve"}),
        ))
        .await
        .unwrap();
    // A different RESPONSE under the same answer id conflicts.
    let error = service
        .answer(&answer(
            "req-conflict",
            "ans-1",
            json!({"decision": "deny"}),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.failure_event_code(), "answer_conflict");
    assert!(
        session_message(&error).contains("Request req-conflict was already answered differently."),
        "unexpected message: {}",
        session_message(&error)
    );
    // A different ANSWER ID with the same response conflicts too.
    let error = service
        .answer(&answer(
            "req-conflict",
            "ans-2",
            json!({"decision": "approve"}),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.failure_event_code(), "answer_conflict");
}

fn session_message(error: &agentkit_sessions::SessionsError) -> String {
    let display = error.to_string();
    let prefix = format!("{}: ", error.failure_event_code());
    display.strip_prefix(&prefix).unwrap_or(&display).to_owned()
}
