//! D2 + D4: session configuration boundaries.
//!
//! - Content selection (the descriptor's Skills), context assembly overrides
//!   and provider reasoning effort are REFUSED at session submit before any
//!   turn effect, with the same `not_implemented` code and message shape the
//!   HTTP boundary uses for `contentSelection` (`http/turns.rs` 501:
//!   `{"code":"not_implemented","detail":"Native prompt templates and skills
//!   are not configured."}`) — native execution must never dispatch a
//!   prompt-only request that silently omits what the session selected.
//! - `compact` is refused for the same reason (no consumer of
//!   `compactRequestedAt`).
//! - `set_model` patches `reasoningEffort` three-state: omitted preserves,
//!   explicit null clears, a value replaces; an in-flight turn keeps the
//!   payload snapshot it was submitted with.

use agentkit_host::{
    AgentSendResult, AgentSessionService, AgentSessionServiceDeps, CONTENT_UNSUPPORTED_MESSAGE,
    CreateSessionInput, SendInput, SharedRuntime, SystemRuntime, WhenBusy,
};
use agentkit_store::SqliteStore;
use serde_json::{Value, json};
use std::sync::Arc;

struct Fixture {
    store: SqliteStore,
    service: AgentSessionService,
}

fn fixture() -> Fixture {
    let store = SqliteStore::memory(64).unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: Arc::new(SystemRuntime) as SharedRuntime,
        plan_approval: None,
        wake: None,
    });
    Fixture { store, service }
}

fn create_input() -> CreateSessionInput {
    CreateSessionInput {
        session_id: Some("s1".into()),
        profile_id: "p1".into(),
        model: Some("model".into()),
        ..CreateSessionInput::default()
    }
}

fn send_input(content: Value, input_id: &str, when_busy: WhenBusy) -> SendInput {
    SendInput {
        content,
        input_id: Some(input_id.into()),
        when_busy,
        turn_id: None,
        on_settled: None,
    }
}

fn turn_id(result: &AgentSendResult) -> &str {
    match result {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        AgentSendResult::Queued { input } => panic!("expected a turn, got queued {:?}", input),
    }
}

// ---------------------------------------------------------------------------
// D2 — descriptor validation at creation surfaces malformed-present values
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_rejects_malformed_present_descriptor_values() {
    for (what, input) in [
        (
            "scalar workspace",
            CreateSessionInput {
                workspace: Some(json!("/project")),
                ..create_input()
            },
        ),
        (
            "explicit null workspace",
            CreateSessionInput {
                workspace: Some(Value::Null),
                ..create_input()
            },
        ),
        (
            "string windowTokens",
            CreateSessionInput {
                context: Some(json!({"windowTokens": "4096"})),
                ..create_input()
            },
        ),
        (
            "zero windowTokens",
            CreateSessionInput {
                context: Some(json!({"windowTokens": 0})),
                ..create_input()
            },
        ),
        (
            "scalar context",
            CreateSessionInput {
                context: Some(json!("transcript")),
                ..create_input()
            },
        ),
        (
            "skills entry without revision",
            CreateSessionInput {
                skills: Some(vec![json!({"id": "a"})]),
                ..create_input()
            },
        ),
        (
            "non-object skills entry",
            CreateSessionInput {
                skills: Some(vec![json!("a")]),
                ..create_input()
            },
        ),
        (
            "reserved turnKind",
            CreateSessionInput {
                turn_kind: Some("chat.secret".into()),
                ..create_input()
            },
        ),
    ] {
        let error = fixture()
            .service
            .create_session(&input)
            .await
            .expect_err(&format!("{what} accepted"));
        assert_eq!(error.code, "invalid_input", "{what}");
    }
}

#[tokio::test]
async fn create_accepts_legal_descriptors_including_host_kinds() {
    for kind in ["project.1", "project._worker", "project.worker"] {
        let input = CreateSessionInput {
            turn_kind: Some(kind.into()),
            ..create_input()
        };
        let state = fixture()
            .service
            .create_session(&input)
            .await
            .unwrap_or_else(|error| panic!("{kind} rejected: {error}"));
        assert_eq!(state.descriptor["turnKind"], json!(kind));
    }
    let input = CreateSessionInput {
        workspace: Some(json!({"root": "/project", "trusted": true})),
        ..create_input()
    };
    let state = fixture().service.create_session(&input).await.unwrap();
    assert_eq!(
        state.descriptor["workspace"],
        json!({"root": "/project", "trusted": true})
    );
}

// ---------------------------------------------------------------------------
// D2 — submission refuses what the native executor cannot honor
// ---------------------------------------------------------------------------

#[tokio::test]
async fn submit_refuses_selected_skills_with_http_boundary_parity() {
    let fixture = fixture();
    let input = CreateSessionInput {
        skills: Some(vec![json!({"id": "skill", "revision": "1"})]),
        ..create_input()
    };
    fixture.service.create_session(&input).await.unwrap();

    let error = fixture
        .service
        .send("s1", &send_input(json!("hello"), "in-1", WhenBusy::Reject))
        .await
        .expect_err("selected Skills dispatched");
    // Same code and message as the HTTP boundary's 501 problem
    // (`http/turns.rs`): code `not_implemented`, detail below.
    assert_eq!(error.code, "not_implemented");
    assert_eq!(error.message, CONTENT_UNSUPPORTED_MESSAGE);
    assert_eq!(
        error.message,
        "Native prompt templates and skills are not configured."
    );
    // Refused before any turn effect, like the HTTP assertion that a
    // rejected submit leaves no tasks and no messages behind.
    assert!(
        fixture
            .store
            .list_tasks(Some("s1".into()))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(fixture.store.messages("s1", true).await.unwrap().is_empty());

    // Queueing is refused too — no half-accepted selection waiting to
    // convert later.
    let queued = fixture
        .service
        .send("s1", &send_input(json!("hello"), "in-1", WhenBusy::Queue))
        .await
        .expect_err("selected Skills queued");
    assert_eq!(queued.code, "not_implemented");
    assert_eq!(queued.message, CONTENT_UNSUPPORTED_MESSAGE);
    assert!(
        fixture
            .store
            .list_tasks(Some("s1".into()))
            .await
            .unwrap()
            .is_empty(),
        "the queue path staged a turn despite the refusal"
    );
}

#[tokio::test]
async fn empty_skills_keep_the_payload_unchanged() {
    // Absent selection dispatches exactly like a session without Skills.
    let fixture = fixture();
    let input = CreateSessionInput {
        skills: Some(vec![]),
        ..create_input()
    };
    fixture.service.create_session(&input).await.unwrap();
    let result = fixture
        .service
        .send("s1", &send_input(json!("hello"), "in-1", WhenBusy::Reject))
        .await
        .unwrap();
    let task = fixture
        .store
        .get_task(turn_id(&result))
        .await
        .unwrap()
        .unwrap();
    assert!(task.payload.get("contentSelection").is_none());
}

#[tokio::test]
async fn submit_refuses_unsupported_context_policy() {
    let fixture = fixture();
    let input = CreateSessionInput {
        context: Some(json!({"policy": "final_answers", "windowTokens": 8192})),
        ..create_input()
    };
    fixture.service.create_session(&input).await.unwrap();
    let error = fixture
        .service
        .send("s1", &send_input(json!("hello"), "in-1", WhenBusy::Reject))
        .await
        .expect_err("context overrides dispatched");
    assert_eq!(error.code, "not_implemented");
    assert!(
        error.message.contains("context assembly"),
        "unclear refusal: {}",
        error.message
    );
    assert!(
        fixture
            .store
            .list_tasks(Some("s1".into()))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn submit_refuses_provider_reasoning_effort() {
    let fixture = fixture();
    fixture
        .service
        .create_session(&create_input())
        .await
        .unwrap();
    fixture
        .service
        .set_model("s1", None, "model", Some(Some("high")))
        .await
        .unwrap();
    let error = fixture
        .service
        .send("s1", &send_input(json!("hello"), "in-1", WhenBusy::Reject))
        .await
        .expect_err("reasoning effort dispatched");
    assert_eq!(error.code, "not_implemented");
    assert!(
        error.message.contains("reasoning effort"),
        "unclear refusal: {}",
        error.message
    );
    assert!(
        fixture
            .store
            .list_tasks(Some("s1".into()))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn compact_records_deferred_request() {
    let fixture = fixture();
    fixture
        .service
        .create_session(&create_input())
        .await
        .unwrap();
    fixture.service.compact("s1").await.unwrap();
    let chat = fixture.store.get_chat("s1").await.unwrap().unwrap();
    assert!(
        chat.metadata
            .get("agentkit.session.compactRequestedAt")
            .is_some(),
        "compact did not record the deferred request"
    );
    // Unknown sessions keep the not-found answer.
    assert_eq!(
        fixture.service.compact("ghost").await.unwrap_err().code,
        "session_not_found"
    );
}

// ---------------------------------------------------------------------------
// D4 — three-state reasoning-effort patch semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reasoning_effort_patch_preserves_clears_and_replaces() {
    let fixture = fixture();
    fixture
        .service
        .create_session(&create_input())
        .await
        .unwrap();

    // Some(Some(value)) replaces.
    let state = fixture
        .service
        .set_model("s1", Some("p1"), "model-b", Some(Some("high")))
        .await
        .unwrap();
    assert_eq!(state.descriptor["model"], json!("model-b"));
    assert_eq!(state.descriptor["providerId"], json!("p1"));
    assert_eq!(state.descriptor["reasoningEffort"], json!("high"));

    // None (omitted) PRESERVES — the regression: the old code removed the
    // setting whenever the effort was omitted.
    let state = fixture
        .service
        .set_model("s1", None, "model-c", None)
        .await
        .unwrap();
    assert_eq!(state.descriptor["model"], json!("model-c"));
    assert_eq!(
        state.descriptor["providerId"],
        json!("p1"),
        "omitted providerId must be preserved"
    );
    assert_eq!(
        state.descriptor["reasoningEffort"],
        json!("high"),
        "omitted reasoningEffort must be preserved"
    );

    // Some(Some(value)) replaces again.
    let state = fixture
        .service
        .set_model("s1", None, "model-c", Some(Some("low")))
        .await
        .unwrap();
    assert_eq!(state.descriptor["reasoningEffort"], json!("low"));

    // Some(None) (explicit null) clears — and clearing is idempotent.
    for _ in 0..2 {
        let state = fixture
            .service
            .set_model("s1", None, "model-d", Some(None))
            .await
            .unwrap();
        assert_eq!(state.descriptor["model"], json!("model-d"));
        assert!(
            state.descriptor.get("reasoningEffort").is_none(),
            "explicit null must clear: {}",
            state.descriptor
        );
    }
}

#[tokio::test]
async fn reasoning_effort_patch_validates_and_keeps_state_on_rejection() {
    let fixture = fixture();
    fixture
        .service
        .create_session(&create_input())
        .await
        .unwrap();
    fixture
        .service
        .set_model("s1", None, "model-a", Some(Some("medium")))
        .await
        .unwrap();
    let error = fixture
        .service
        .set_model("s1", None, "model-b", Some(Some("ultra")))
        .await
        .expect_err("unknown effort accepted");
    assert_eq!(error.code, "invalid_input");
    let state = fixture.service.get_state("s1").await.unwrap();
    assert_eq!(state.descriptor["model"], json!("model-a"));
    assert_eq!(state.descriptor["reasoningEffort"], json!("medium"));
    assert_eq!(
        fixture
            .service
            .set_model("s1", None, "", Some(Some("low")))
            .await
            .unwrap_err()
            .code,
        "invalid_input"
    );
}

#[tokio::test]
async fn running_turn_keeps_its_snapshot_later_turns_see_the_patch() {
    let fixture = fixture();
    let input = CreateSessionInput {
        model: Some("alpha".into()),
        ..create_input()
    };
    fixture.service.create_session(&input).await.unwrap();

    // Turn 1 runs with its submitted snapshot.
    let first = fixture
        .service
        .send("s1", &send_input(json!("one"), "in-1", WhenBusy::Reject))
        .await
        .unwrap();
    let first_id = turn_id(&first).to_owned();
    let snapshot = fixture.store.get_task(&first_id).await.unwrap().unwrap();
    assert_eq!(snapshot.payload["model"], json!("alpha"));

    // Patch while the turn is in flight (model only, so the patch itself is
    // dispatchable): the running task keeps its payload, the session does not.
    fixture
        .service
        .set_model("s1", None, "beta", None)
        .await
        .unwrap();
    let running = fixture.store.get_task(&first_id).await.unwrap().unwrap();
    assert_eq!(
        running.payload["model"],
        json!("alpha"),
        "the in-flight turn lost its submitted snapshot"
    );
    assert_eq!(
        fixture.service.get_state("s1").await.unwrap().descriptor["model"],
        json!("beta")
    );

    // The next turn carries the patch.
    fixture.service.cancel("s1", &first_id).await.unwrap();
    let second = fixture
        .service
        .send("s1", &send_input(json!("two"), "in-2", WhenBusy::Reject))
        .await
        .unwrap();
    let second_id = turn_id(&second).to_owned();
    let patched = fixture.store.get_task(&second_id).await.unwrap().unwrap();
    assert_eq!(patched.payload["model"], json!("beta"));

    // An effort patch reaches the LATER dispatch path too: the in-flight
    // turn keeps its effort-less snapshot while a refused next submit proves
    // the patched effort is what the executor would now have to carry.
    fixture
        .service
        .set_model("s1", None, "gamma", Some(Some("high")))
        .await
        .unwrap();
    let running = fixture.store.get_task(&second_id).await.unwrap().unwrap();
    assert_eq!(running.payload["model"], json!("beta"));
    assert!(running.payload.get("reasoningEffort").is_none());
    fixture.service.cancel("s1", &second_id).await.unwrap();
    let error = fixture
        .service
        .send("s1", &send_input(json!("three"), "in-3", WhenBusy::Reject))
        .await
        .expect_err("later turn ignored the patched effort");
    assert_eq!(error.code, "not_implemented");
    assert!(error.message.contains("reasoning effort"));
}
