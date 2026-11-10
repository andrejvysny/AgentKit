//! Session configuration validation, immutable staging, reasoning patches and admission gates.

use agentkit_engine::{SharedRuntime, SystemRuntime, TaskExecutor};
use agentkit_sessions::{
    AgentHost, AgentSendResult, AgentSessionService, AgentSessionServiceDeps, CreateSessionInput,
    ExecutionContext, HostOptions, HostResult, SendInput, WhenBusy,
};
use agentkit_store::SqliteStore;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

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
        profile_id: "coding".into(),
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
async fn submit_refuses_missing_selected_skill_revision_before_admission() {
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
    assert_eq!(error.code, "invalid_input");
    assert!(
        error
            .message
            .contains("Selected skill revision changed or disappeared")
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
    assert_eq!(queued.code, "invalid_input");
    assert!(
        queued
            .message
            .contains("Selected skill revision changed or disappeared")
    );
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
async fn submit_stages_context_assembly_overrides() {
    // `policy` and `toolResultElision` are no longer refused at staging:
    // the executor honors them (`chat_context.rs::resolve_context_assembly`,
    // `chat_history.rs::HistoryPolicy`, `tool_result_elision.rs`). What the
    // descriptor grammar admits, staging must carry verbatim.
    let fixture = fixture();
    let context = json!({
        "policy": "final_answers",
        "windowTokens": 8192,
        "toolResultElision": {"keepRecentTurns": 3, "maxChars": 500}
    });
    let input = CreateSessionInput {
        context: Some(context.clone()),
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
    assert_eq!(task.payload["sessionContext"], context);
    // Values OUTSIDE the contract's enumeration stay refused — at the
    // descriptor door, with the stable validation error (`session_descriptors.rs`
    // pins the grammar; the executor re-validates the snapshot with
    // `session_context_invalid`).
    let error = fixture
        .service
        .configure("s1", None, Some(json!({"policy": "everything"})), None)
        .await
        .expect_err("invented policy accepted");
    assert_eq!(error.code, "invalid_input");
}

#[tokio::test]
async fn submit_snapshots_provider_reasoning_effort() {
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
    assert_eq!(task.payload["reasoningEffort"], "high");
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
    let result = fixture
        .service
        .send("s1", &send_input(json!("three"), "in-3", WhenBusy::Reject))
        .await
        .unwrap();
    let task = fixture
        .store
        .get_task(turn_id(&result))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.payload["reasoningEffort"], "high");
}

// ---------------------------------------------------------------------------
// Admission — session turns are created as tasks of the descriptor's turn
// kind. With the host's admission gate bound (`with_gate`), every admission
// that would create a task — turn submit, queue-or-stage, flush promotion —
// refuses a kind with no registered `TaskExecutor` with the stable
// `no_executor_for_kind` code BEFORE the store transaction begins, so a
// refused admission lands zero rows. Whole admissions are rejected, never
// admitted-except-the-missing-one: the store admits an input with its task
// and messages atomically, so there is no partial batch to keep or drop.
// ---------------------------------------------------------------------------

struct CountingTurn(Arc<AtomicUsize>);

#[async_trait]
impl TaskExecutor<agentkit_store::SqliteStore> for CountingTurn {
    async fn execute(&self, _context: ExecutionContext) -> HostResult<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn host_for(store: &SqliteStore, runtime: &SharedRuntime, kinds: &[&str]) -> AgentHost {
    let executors: HashMap<String, Arc<dyn TaskExecutor<agentkit_store::SqliteStore>>> = kinds
        .iter()
        .map(|kind| {
            (
                (*kind).to_owned(),
                Arc::new(CountingTurn(Arc::new(AtomicUsize::new(0))))
                    as Arc<dyn TaskExecutor<agentkit_store::SqliteStore>>,
            )
        })
        .collect();
    AgentHost::new(
        store.clone(),
        executors,
        runtime.clone(),
        HostOptions::default(),
    )
    .unwrap()
}

#[tokio::test]
async fn admission_refuses_a_turn_kind_with_no_executor_before_landing_rows() {
    let store = SqliteStore::memory(64).unwrap();
    let runtime = Arc::new(SystemRuntime) as SharedRuntime;
    let host = host_for(&store, &runtime, &["other.kind"]);
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: runtime.clone(),
        plan_approval: None,
        wake: None,
    })
    .with_gate(host.admission_gate());
    service.create_session(&create_input()).await.unwrap();

    let error = service
        .send("s1", &send_input(json!("hello"), "in-0", WhenBusy::Reject))
        .await
        .expect_err("chat.turn has no executor here");
    assert_eq!(error.code, "no_executor_for_kind");
    assert!(error.message.contains("chat.turn"));
    assert!(
        store
            .list_tasks(Some("s1".into()))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(store.messages("s1", true).await.unwrap().is_empty());

    // Busy session (an admitted turn the gated host will never claim): the
    // batch-shaped queue admission refuses input+task+messages together.
    let ungated = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: runtime.clone(),
        plan_approval: None,
        wake: None,
    });
    ungated
        .send("s1", &send_input(json!("first"), "in-0", WhenBusy::Reject))
        .await
        .unwrap();
    let queued = service
        .send("s1", &send_input(json!("later"), "in-1", WhenBusy::Queue))
        .await
        .expect_err("the queue path is gated too");
    assert_eq!(queued.code, "no_executor_for_kind");
    assert_eq!(
        store.list_tasks(Some("s1".into())).await.unwrap().len(),
        1,
        "only the admitted turn exists; the refused batch left no task"
    );
    assert!(
        service.get_state("s1").await.unwrap().pending.is_empty(),
        "the refused batch left no queued input either"
    );
    assert_eq!(
        service
            .flush_queue("s1")
            .await
            .expect_err("promotion is an admission too")
            .code,
        "no_executor_for_kind"
    );

    // The refusal is per-registry, not a verdict on the kind: a host that
    // registers chat.turn claims and runs exactly what the gated host
    // refused to admit.
    let turns = Arc::new(AtomicUsize::new(0));
    let runner = AgentHost::new(
        store.clone(),
        HashMap::from([(
            "chat.turn".to_owned(),
            Arc::new(CountingTurn(turns.clone()))
                as Arc<dyn TaskExecutor<agentkit_store::SqliteStore>>,
        )]),
        runtime,
        HostOptions::default(),
    )
    .unwrap();
    assert!(runner.run_once().await.unwrap());
    assert_eq!(turns.load(Ordering::SeqCst), 1);
}
