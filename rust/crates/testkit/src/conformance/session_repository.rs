//! Conformance suite for `agentkit_sessions::SessionRepository`: the
//! behaviour every implementation must share. Each case runs on a fresh
//! repository; every instant is passed in, so no case reads a clock, and
//! every refusal is asserted through a [`RepositoryFault`] predicate, never
//! against a concrete error enum.
//!
//! The port persists only what the session service drives. The facts it only
//! reads, and the states it cannot create (a turn PARKED on a human request,
//! an admitted execution receipt, a task whose run profile is pinned), come
//! from [`SessionFixture`]: each backend seeds them with its OWN inherent
//! writes. That is the split [`super::task_repository`] makes for
//! `settle_session`, for the same reason — a backend cannot be held to a rule
//! through a writer the port does not have.

use agentkit_engine::{MessageInput, RepositoryFault, SubmitTask, TaskInputReason, TaskStatus};
use agentkit_sessions::{
    AgentInputKind, AgentInputPatch, AgentInputQuery, AgentInputStatus, AnswerHumanRequest,
    ChatRecord, CreateAgentInput, ExecutionReceiptRecord, ExecutionReceiptStatus, QueueAdmission,
    QueueOrStage, RECEIPT_NOT_UNCERTAIN, RECEIPT_RUN_ACTIVE, ResolutionOutcome, ResolveExecution,
    SESSION_METADATA_KEY, SessionMutation, SessionRepository, StageQueuedTurn, StagedCandidate,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{any::Any, future::Future, pin::Pin};

/// The facts the port only READS, plus the durable states it cannot create,
/// seeded from the backend's own store API. The handle wraps the repository
/// under test, so one fixture per case is built alongside it.
#[async_trait]
pub trait SessionFixture: Clone + Send + Sync + 'static {
    /// The repository the case drives.
    type Repo: SessionRepository;

    /// The fault type of the fixture's own seeded writes.
    type Error: std::error::Error + Send + Sync + 'static;

    /// The repository under test.
    fn repo(&self) -> &Self::Repo;

    /// Register a provider and its capability evidence: the catalog rows
    /// `get_provider` and `get_provider_capabilities` read.
    async fn put_provider(&self, provider: Value, capabilities: Value) -> Result<(), Self::Error>;
    /// Remove a provider and everything catalogued under its id.
    async fn delete_provider(&self, id: &str) -> Result<(), Self::Error>;
    /// Replace a provider's model catalog (`list_provider_models`).
    async fn put_provider_models(&self, id: &str, models: Vec<Value>) -> Result<(), Self::Error>;
    /// Publish one skill document, returning its catalog entry (whose
    /// `revision` the caller then selects).
    async fn put_skill(
        &self,
        source: &str,
        path: &str,
        content: &str,
    ) -> Result<Value, Self::Error>;
    /// Publish one settings row (`get_setting`, and the context summaries).
    async fn put_setting(
        &self,
        namespace: &str,
        key: &str,
        value: Value,
    ) -> Result<(), Self::Error>;
    /// Create one agent identity at definition revision 1.
    async fn put_agent(
        &self,
        agent_id: &str,
        definition: agentkit_sessions::AgentDefinition,
        now: i64,
    ) -> Result<(), Self::Error>;

    /// Park a RUNNING turn on a human request: the turn waits for an answer
    /// and the request row exists (the store's `park_turn`).
    async fn park_turn(
        &self,
        task_id: &str,
        request: CreateAgentInput,
        now: i64,
    ) -> Result<String, Self::Error>;

    /// Append events to `task_id` (the store's fenced `append_events`).
    async fn put_events(
        &self,
        task_id: &str,
        events: Vec<Value>,
        now: i64,
    ) -> Result<(), Self::Error>;

    /// Record an admitted execution receipt against `receipt.run_id`, and
    /// end that run when `settle_run` says it is already gone (the store's
    /// `admit_execution` plus a crash-like settle).
    async fn admit_receipt(
        &self,
        receipt: ExecutionReceiptRecord,
        settle_run: bool,
        now: i64,
    ) -> Result<(), Self::Error>;

    /// Admit a turn whose binding pins `profile` as its effective run
    /// profile, the subject of `run_profile_dispatch_refusal`.
    async fn put_pinned_turn(
        &self,
        task_id: &str,
        agent_id: &str,
        profile: Value,
        now: i64,
    ) -> Result<(), Self::Error>;
}

/// `context_summaries::CONTEXT_SUMMARIES_NAMESPACE`: the settings namespace
/// the derived context summaries live in. The suite names it because it
/// seeds those rows; both twins must read the same key.
pub const CONTEXT_SUMMARIES_NAMESPACE: &str = "agentkit.compat.context-summaries";

/// The repository a case drives.
fn repo<T: SessionFixture>(fixture: &T) -> &T::Repo {
    fixture.repo()
}

/// Every seeded write is a success; a backend that cannot seed the fact has
/// no business running the suite.
macro_rules! seed {
    ($call:expr) => {
        match $call.await {
            Ok(value) => value,
            Err(error) => panic!("fixture write failed: {error}"),
        }
    };
}

/// Every port call the same way: unwrap the value, or name the fault. The
/// committed value needs no `Debug`; only the fault is formatted.
fn port<T, E: std::fmt::Debug>(outcome: Result<T, E>) -> T {
    match outcome {
        Ok(value) => value,
        Err(error) => panic!("port call failed: {error:?}"),
    }
}

type CaseFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

macro_rules! suite {
    ($($name:ident),+ $(,)?) => {
        /// Names of every case, in run order.
        pub const CASES: &[&str] = &[$(stringify!($name)),+];

        fn cases<T: SessionFixture>() -> Vec<(&'static str, fn(T) -> CaseFuture)> {
            vec![$((stringify!($name), (|fixture| Box::pin($name(fixture))) as fn(T) -> CaseFuture)),+]
        }
    };
}

suite!(
    create_chat_get_chat_roundtrip,
    create_chat_duplicate_is_duplicate,
    get_chat_missing_is_none,
    patch_session_patches_descriptor,
    patch_session_refuses_non_agent_session,
    patch_session_fork_descriptor,
    patch_session_execution_mode_rejects_empty,
    patch_session_execution_mode_roundtrip,
    fork_copies_history_up_to_the_message,
    fork_refuses_a_foreign_or_inactive_fork_point,
    messages_hide_inactive_without_the_flag,
    list_context_summaries_newest_first,
    get_setting_absent_and_present,
    events_page_after_a_cursor,
    list_tasks_by_scope_and_all,
    create_input_replays_identical_bytes,
    create_input_conflicts_on_different_bytes,
    get_input_missing_is_none,
    list_inputs_filters_by_kind_status_and_target,
    transition_input_refuses_a_wrong_from,
    transition_input_applies_the_patch,
    queue_or_stage_queues_behind_a_live_turn,
    queue_or_stage_stages_when_the_scope_is_free,
    stage_queued_turn_busy_then_free,
    answer_human_request_records_the_answer,
    answer_human_request_refuses_a_second_answer,
    answer_human_request_refuses_a_stale_answer,
    request_interrupt_marks_a_running_turn,
    request_compact_refuses_a_stale_descriptor,
    request_compact_stamps_a_current_descriptor,
    revoke_standing_approval_is_idempotent,
    submit_turn_writes_the_task_and_its_messages,
    catalog_reads_and_absences,
    resolve_run_content_reports_unavailable_skills,
    run_profile_dispatch_refusal_reads_the_pinned_facts,
    list_executions_by_chat_and_resolve,
    resolve_execution_refuses_a_finished_receipt,
    resolve_execution_refuses_an_active_run,
);

/// Runs every case against a repository built fresh by `make`. Panics,
/// naming the case, on the first failure.
pub async fn run<T, F, Fut>(make: F)
where
    T: SessionFixture,
    F: Fn() -> Fut,
    Fut: Future<Output = T>,
{
    for (name, case) in cases::<T>() {
        let fixture = make().await;
        if let Err(error) = tokio::spawn(case(fixture)).await {
            panic!(
                "session repository conformance case `{name}` failed: {}",
                describe(error.into_panic_or_cancel())
            );
        }
    }
}

trait PanicText {
    fn into_panic_or_cancel(self) -> Box<dyn Any + Send>;
}

impl PanicText for tokio::task::JoinError {
    fn into_panic_or_cancel(self) -> Box<dyn Any + Send> {
        if self.is_panic() {
            self.into_panic()
        } else {
            Box::new(format!("case task cancelled: {self}"))
        }
    }
}

fn describe(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => match payload.downcast::<&str>() {
            Ok(text) => (*text).to_owned(),
            Err(_) => "non-text panic".to_owned(),
        },
    }
}

// ---- fixtures --------------------------------------------------------------

/// Every case's `now`: a plain instant well inside the Date range.
const NOW: i64 = 1_700_000_000_000;

/// A session descriptor that `read_session_descriptor` accepts.
fn descriptor() -> Value {
    json!({
        "version": 1,
        "profile": {"id": "profile-1"},
        "model": "m1",
        "createdAt": wire(NOW),
    })
}

fn metadata_with_descriptor() -> Value {
    json!({SESSION_METADATA_KEY: descriptor()})
}

fn steer(input_id: &str, session: &str, text: &str) -> CreateAgentInput {
    CreateAgentInput {
        input_id: input_id.into(),
        session_id: session.into(),
        kind: AgentInputKind::Steer,
        content: json!({"content": text}),
        content_hash: format!("hash-{input_id}"),
        target_task_id: None,
        on_settled: None,
        attempt_id: None,
        tool_call_id: None,
        expires_at: None,
    }
}

fn follow_up(input_id: &str, session: &str, text: &str) -> CreateAgentInput {
    CreateAgentInput {
        kind: AgentInputKind::FollowUp,
        ..steer(input_id, session, text)
    }
}

fn human_request(input_id: &str, session: &str, turn: &str, content: Value) -> CreateAgentInput {
    CreateAgentInput {
        input_id: input_id.into(),
        session_id: session.into(),
        kind: AgentInputKind::HumanRequest,
        content,
        content_hash: format!("hash-{input_id}"),
        target_task_id: Some(turn.into()),
        on_settled: None,
        attempt_id: Some(format!("attempt-{input_id}")),
        tool_call_id: Some(format!("call-{input_id}")),
        expires_at: None,
    }
}

fn approval_content(standing: bool) -> Value {
    let mut content = json!({
        "kind": "approval", "toolName": "write_file", "argsSha256": "a".repeat(64),
    });
    if standing {
        content["standing"] = json!({"digest": "b".repeat(64), "label": "write files"});
    }
    content
}

fn answer(request_id: &str, answer_id: &str, response: Value) -> AnswerHumanRequest {
    AnswerHumanRequest {
        request_id: request_id.into(),
        answer_id: answer_id.into(),
        response,
        response_hash: format!("sha-{answer_id}"),
        now: NOW + 1,
        message_id: format!("{answer_id}-msg"),
        plan_tool_names: vec![],
        policy_denial: None,
        granted_mode: None,
    }
}

fn turn(task_id: &str, scope: &str) -> SubmitTask {
    SubmitTask {
        task_id: task_id.into(),
        kind: "session.turn".into(),
        scope_id: scope.into(),
        payload: json!({"chatId": scope, "model": "m1"}),
        priority: 0,
        now: NOW,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: false,
    }
}

/// A run's assistant record: what a non-approval answer's tool row is
/// appended after (`session_ops::last_message_of_run` must find it).
fn assistant_message(id: &str, chat: &str, run: &str) -> MessageInput {
    MessageInput {
        id: Some(id.into()),
        chat_id: chat.into(),
        run_id: Some(run.into()),
        role: "assistant".into(),
        content: json!({"text": "thinking"}),
        ..Default::default()
    }
}

/// A turn's user record: the store's `submit_turn` accepts a user message
/// only with no run id, projected against the turn's own chat.
fn user_message(id: &str, chat: &str, text: &str) -> MessageInput {
    MessageInput {
        id: Some(id.into()),
        chat_id: chat.into(),
        run_id: None,
        content: json!(text),
        ..Default::default()
    }
}

fn staged(input_id: &str, session: &str, text: &str) -> StagedCandidate {
    StagedCandidate {
        input_id: input_id.into(),
        task: turn(&format!("task-{input_id}"), session),
        messages: vec![user_message(&format!("msg-{input_id}"), session, text)],
    }
}

fn admission(input_id: &str, session: &str, text: &str) -> QueueOrStage {
    QueueOrStage {
        session_id: session.into(),
        task_kind: "session.turn".into(),
        input: follow_up(input_id, session, text),
        target_the_live_turn: true,
        candidate: Some(staged(input_id, session, text)),
        now: NOW,
    }
}

fn flush(input_id: &str, session: &str, text: &str) -> StageQueuedTurn {
    StageQueuedTurn {
        session_id: session.into(),
        task_kind: "session.turn".into(),
        exclude_task_id: None,
        input_id: input_id.into(),
        expected_kind: AgentInputKind::FollowUp,
        task: turn(&format!("task-{input_id}"), session),
        messages: vec![user_message(&format!("msg-{input_id}"), session, text)],
        now: NOW,
    }
}

/// A chat carrying a readable session descriptor — the precondition of
/// every session-metadata write.
async fn session_chat<T: SessionFixture>(fixture: &T, id: &str) -> ChatRecord {
    port(
        repo(fixture)
            .create_chat(
                id,
                Some(format!("chat {id}")),
                metadata_with_descriptor(),
                NOW,
            )
            .await,
    )
}

/// Submit a turn in `session`'s scope and leave it QUEUED: the live turn the
/// queue's busy checks and `patch_session`'s atomicity rest on.
async fn queued_turn<T: SessionFixture>(fixture: &T, task_id: &str, session: &str) {
    port(
        repo(fixture)
            .submit_turn(
                turn(task_id, session),
                vec![user_message(&format!("{task_id}-user"), session, "go")],
            )
            .await,
    );
}

fn provider(id: &str) -> Value {
    json!({
        "id": id, "label": "Seed", "kind": "local",
        "baseUrl": "http://127.0.0.1:9", "defaultModel": "m1", "enabled": true,
    })
}

fn capabilities() -> Value {
    json!({"streaming": true, "toolCalling": true, "modelList": false, "vision": true})
}

fn model(provider_id: &str, model_id: &str) -> Value {
    json!({
        "providerId": provider_id,
        "modelId": model_id,
        "fetchedAt": "2026-01-01T00:00:00.000Z",
        // Present-and-null is valid; absent is not (`provider_catalog::model`
        // reads it with `is_some_and`).
        "displayName": model_id,
    })
}

fn summary(id: &str, session: &str, created_at: &str) -> Value {
    json!({
        "summaryId": id, "sessionId": session,
        "coversThroughMessageId": "m1", "firstKeptMessageId": "m2",
        "summary": "text", "sourceHash": "h", "model": "m1",
        "summarizerRevision": "r1", "createdAt": created_at,
        "tokensBefore": 100, "tokensAfter": 10,
        "details": {"readFiles": [], "modifiedFiles": []},
    })
}

fn receipt(execution_id: &str, run_id: &str, chat_id: &str) -> ExecutionReceiptRecord {
    ExecutionReceiptRecord {
        execution_id: execution_id.into(),
        run_id: run_id.into(),
        chat_id: chat_id.into(),
        tool_call_id: format!("call-{execution_id}"),
        tool_name: "write_file".into(),
        args_sha256: "a".repeat(64),
        args_hash_provenance: Default::default(),
        context_sha256: "c".repeat(64),
        attempt_id: format!("attempt-{execution_id}"),
        approval_request_id: None,
        approval_generation: None,
        status: ExecutionReceiptStatus::Admitted,
        result: None,
        resolution: None,
    }
}

fn resolution(outcome: ResolutionOutcome, result: Option<Value>) -> ResolveExecution {
    ResolveExecution {
        outcome,
        result,
        note: Some("closed by hand".into()),
        resolved_by: Some("operator".into()),
        now: NOW,
    }
}

/// The profile document the host resolver would seal, in store-side shape.
fn profile(agent_id: &str) -> Value {
    json!({
        "version": 1,
        "agentId": agent_id,
        "agentRevision": 1,
        "provider": {"providerId": "p1", "modelId": "m1", "localOnly": false, "fallbacks": []},
        "capability": {
            "toolCalling": true, "vision": true, "streaming": true,
            "continuationCompatible": true, "contextWindowTokens": 128_000,
        },
        "skills": [],
        "contextSources": [],
        "permissionCeiling": {"mode": "ask", "tools": ["read_file"]},
        "budgets": {"maxToolIterations": 8},
        "environment": {"vars": []},
        "approvals": [],
    })
}

fn agent_definition(name: &str, resources: &[&str]) -> agentkit_sessions::AgentDefinition {
    agentkit_sessions::AgentDefinition {
        name: name.into(),
        purpose: format!("purpose of {name}"),
        instructions: format!("instructions of {name}"),
        permission_mode: "ask".into(),
        allowed_tools: vec!["read_file".into()],
        resources: resources
            .iter()
            .map(|resource| (*resource).to_owned())
            .collect(),
        ..Default::default()
    }
}

/// A grant the store's standing-approval reader accepts.
fn grant(grant_id: &str, digest: &str) -> Value {
    json!({
        "grantId": grant_id, "label": "run tools", "toolName": "run_cmd",
        "sourceRequestId": "r1", "answerId": "a1",
        "grantedAt": "2026-01-01T00:00:00.000Z", "digest": digest,
    })
}

fn metadata_with_grant(grant_id: &str, digest: &str) -> Value {
    json!({
        SESSION_METADATA_KEY: descriptor(),
        agentkit_sessions::STANDING_APPROVALS_METADATA_KEY: {
            "version": 1, "grants": [grant(grant_id, digest)]
        },
    })
}

/// One task's status through the port's own listing.
async fn status_of<T: SessionFixture>(fixture: &T, task_id: &str) -> Option<TaskStatus> {
    port(repo(fixture).list_tasks(None).await)
        .into_iter()
        .find(|task| task.task_id == task_id)
        .map(|task| task.status)
}

/// A message listing's ids, in listing order.
fn ids(messages: &[agentkit_sessions::MessageRecord]) -> Vec<String> {
    messages.iter().map(|message| message.id.clone()).collect()
}

// ---- the session aggregate --------------------------------------------------

async fn create_chat_get_chat_roundtrip<T: SessionFixture>(fixture: T) {
    let created = port(repo(&fixture).create_chat("c1", None, json!({}), NOW).await);
    assert_eq!(created.id, "c1");
    assert_eq!(created.title, None);
    assert!(!created.archived);
    let read = port(repo(&fixture).get_chat("c1").await);
    assert_eq!(read, Some(created));
    assert!(port(repo(&fixture).get_chat("nope").await).is_none());
}

async fn create_chat_duplicate_is_duplicate<T: SessionFixture>(fixture: T) {
    port(repo(&fixture).create_chat("c1", None, json!({}), NOW).await);
    let error = repo(&fixture)
        .create_chat("c1", Some("again".into()), json!({}), NOW)
        .await
        .expect_err("a second chat under one id is a duplicate");
    assert!(error.is_duplicate(), "{error:?}");
    assert_eq!(error.input_conflict(), Some("c1"));
}

async fn get_chat_missing_is_none<T: SessionFixture>(fixture: T) {
    assert_eq!(port(repo(&fixture).get_chat("absent").await), None);
}

/// A `serde_json::Map` from string keys, for a mutation's patches.
fn patch(items: &[(&str, Value)]) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    for (key, value) in items {
        map.insert((*key).to_owned(), value.clone());
    }
    map
}

fn replace(items: &[(&str, Value)], invalid_message: &str) -> SessionMutation {
    SessionMutation::PatchDescriptor {
        replace: patch(items),
        merge: patch(&[]),
        remove: vec![],
        invalid_message: invalid_message.into(),
    }
}

async fn patch_session_patches_descriptor<T: SessionFixture>(fixture: T) {
    session_chat(&fixture, "c1").await;
    let chat = port(
        repo(&fixture)
            .patch_session(
                "c1",
                replace(
                    &[("model", json!("m2")), ("reasoningEffort", json!("low"))],
                    "set_model rejected the descriptor",
                ),
                NOW,
            )
            .await,
    );
    let stamped = &chat.metadata[SESSION_METADATA_KEY];
    assert_eq!(stamped["model"], json!("m2"));
    assert_eq!(stamped["reasoningEffort"], json!("low"));
    assert_eq!(
        stamped["profile"],
        json!({"id": "profile-1"}),
        "a patch never invents a digest"
    );
    assert_eq!(
        chat.updated_at,
        agentkit_kernel::ConversationTimestamp::from(NOW)
    );

    // A patched descriptor that does not read refuses with the caller's own
    // message, and writes nothing.
    let error = repo(&fixture)
        .patch_session(
            "c1",
            replace(
                &[("reasoningEffort", json!("sideways"))],
                "configure rejected the descriptor",
            ),
            NOW + 5,
        )
        .await
        .expect_err("an unreadable descriptor cannot land");
    assert_eq!(
        error.invalid_input(),
        Some("configure rejected the descriptor")
    );
    let stored = port(repo(&fixture).get_chat("c1").await).expect("chat");
    assert_eq!(stored.metadata[SESSION_METADATA_KEY]["model"], json!("m2"));
    assert_eq!(
        stored.updated_at,
        agentkit_kernel::ConversationTimestamp::from(NOW),
        "a refused patch restamps nothing"
    );

    // `remove` drops keys, `merge` extends an object the descriptor already
    // holds — and a key it does not hold is left alone.
    let chat = port(
        repo(&fixture)
            .patch_session(
                "c1",
                SessionMutation::PatchDescriptor {
                    replace: patch(&[]),
                    merge: patch(&[
                        ("profile", json!({"digest": "d".repeat(64)})),
                        ("workspace", json!({"trusted": true})),
                    ]),
                    remove: vec!["model".into(), "reasoningEffort".into()],
                    invalid_message: "nope".into(),
                },
                NOW + 6,
            )
            .await,
    );
    let after = &chat.metadata[SESSION_METADATA_KEY];
    assert!(after.get("model").is_none(), "{after:?}");
    assert!(after.get("reasoningEffort").is_none(), "{after:?}");
    assert_eq!(after["profile"]["digest"], json!("d".repeat(64)));
    assert!(
        after.get("workspace").is_none(),
        "an unknown key is never added"
    );
}

async fn patch_session_refuses_non_agent_session<T: SessionFixture>(fixture: T) {
    // A plain chat, with no session descriptor under the session key.
    port(
        repo(&fixture)
            .create_chat("plain", None, json!({}), NOW)
            .await,
    );
    for mutation in [
        replace(&[("model", json!("m1"))], "nope"),
        SessionMutation::ExecutionMode { mode: "ask".into() },
    ] {
        let error = repo(&fixture)
            .patch_session("plain", mutation, NOW)
            .await
            .expect_err("a chat that is not an agent session cannot be patched");
        assert!(error.is_not_found(), "{error:?}");
        assert!(
            error.not_found().is_some_and(
                |message| message.contains("not an agent session") && message.contains("plain")
            ),
            "{error:?}"
        );
    }
    // An unknown session is the same refusal, because its chat is absent.
    let error = repo(&fixture)
        .patch_session(
            "absent",
            SessionMutation::ExecutionMode { mode: "ask".into() },
            NOW,
        )
        .await
        .expect_err("no chat");
    assert!(error.is_not_found(), "{error:?}");
}

async fn patch_session_fork_descriptor<T: SessionFixture>(fixture: T) {
    // A forked chat whose copied descriptor does not read refuses before the
    // stamp lands.
    port(
        repo(&fixture)
            .create_chat("fork", None, json!({}), NOW)
            .await,
    );
    let error = repo(&fixture)
        .patch_session(
            "fork",
            SessionMutation::ForkDescriptor {
                forked_from: json!({"sessionId": "c1", "messageId": "m1"}),
            },
            NOW,
        )
        .await
        .expect_err("an unreadable fork copy is refused");
    assert_eq!(error.invalid_input(), Some("forked descriptor unreadable"));

    // With a readable descriptor, the stamp writes its own instant and
    // provenance and nothing else changes.
    session_chat(&fixture, "fork2").await;
    let chat = port(
        repo(&fixture)
            .patch_session(
                "fork2",
                SessionMutation::ForkDescriptor {
                    forked_from: json!({"sessionId": "c1", "messageId": "m1"}),
                },
                NOW,
            )
            .await,
    );
    let stamped = &chat.metadata[SESSION_METADATA_KEY];
    assert_eq!(stamped["profile"], json!({"id": "profile-1"}));
    assert_eq!(stamped["model"], json!("m1"));
    assert_eq!(stamped["createdAt"], json!(wire(NOW)));
    assert_eq!(
        stamped["forkedFrom"],
        json!({"sessionId": "c1", "messageId": "m1"})
    );
}

async fn patch_session_execution_mode_rejects_empty<T: SessionFixture>(fixture: T) {
    session_chat(&fixture, "c1").await;
    let error = repo(&fixture)
        .patch_session(
            "c1",
            SessionMutation::ExecutionMode {
                mode: String::new(),
            },
            NOW,
        )
        .await
        .expect_err("an empty mode is invalid input");
    assert_eq!(error.invalid_input(), Some("mode must be non-empty."));
}

async fn patch_session_execution_mode_roundtrip<T: SessionFixture>(fixture: T) {
    session_chat(&fixture, "c1").await;
    let chat = port(
        repo(&fixture)
            .patch_session(
                "c1",
                SessionMutation::ExecutionMode { mode: "ask".into() },
                NOW,
            )
            .await,
    );
    let state = agentkit_sessions::read_execution_state(&chat.metadata).expect("state written");
    assert_eq!(state["mode"], json!("ask"));
    assert_eq!(state["version"], json!(1));

    // Entering `plan` never drops the mode write; the state carries `plan`.
    let chat = port(
        repo(&fixture)
            .patch_session(
                "c1",
                SessionMutation::ExecutionMode {
                    mode: "plan".into(),
                },
                NOW,
            )
            .await,
    );
    assert_eq!(
        agentkit_sessions::read_execution_state(&chat.metadata).unwrap()["mode"],
        json!("plan")
    );
}

// ---- messages, forks, listings ----------------------------------------------

async fn fork_copies_history_up_to_the_message<T: SessionFixture>(fixture: T) {
    let chat = session_chat(&fixture, "src").await;
    assert_eq!(chat.title.as_deref(), Some("chat src"));
    // A root and two children: root -> a -> b, plus an inactive sibling.
    let root = MessageInput {
        id: Some("root".into()),
        chat_id: "src".into(),
        run_id: None,
        role: "user".into(),
        content: json!("root"),
        activate: true,
        explicit_root: true,
        ..Default::default()
    };
    port(
        repo(&fixture)
            .submit_turn(turn("t-src", "src"), vec![root])
            .await,
    );
    let a = MessageInput {
        id: Some("a".into()),
        parent_message_id: Some("root".into()),
        ..user_message("a", "src", "first")
    };
    let b = MessageInput {
        id: Some("b".into()),
        parent_message_id: Some("a".into()),
        ..user_message("b", "src", "second")
    };
    // `b` lands on a fresh turn of the same chat, then the fork point is `a`.
    port(
        repo(&fixture)
            .submit_turn(turn("t-src2", "src"), vec![a, b])
            .await,
    );

    let (forked, copied) = port(repo(&fixture).fork_chat("src", "a", "dst", NOW).await);
    assert_eq!(forked.id, "dst");
    assert_eq!(forked.title.as_deref(), Some("Fork of chat src"));
    // The copied path is the ancestry through the fork point, and nothing
    // beyond it. The copies are NEW records — the backend owns their ids —
    // so what is pinned is their content and their parent chain.
    let texts: Vec<&str> = copied
        .iter()
        .map(|message| message.content.as_str().expect("string content"))
        .collect();
    assert_eq!(texts, ["root", "first"]);
    assert_eq!(copied[0].parent_message_id, None);
    assert_eq!(
        copied[1].parent_message_id.as_deref(),
        Some(copied[0].id.as_str())
    );
    assert!(copied.iter().all(|message| message.chat_id == "dst"));
    assert!(copied.iter().all(|message| message.run_id.is_none()));
    // The source keeps its own history, including the child past the point.
    let source = port(repo(&fixture).messages("src", false).await);
    assert!(ids(&source).contains(&"b".to_owned()));
    assert_eq!(port(repo(&fixture).messages("dst", false).await).len(), 2);
    // Metadata travels, so the fork is an agent session too.
    assert_eq!(forked.metadata, metadata_with_descriptor());
}

async fn fork_refuses_a_foreign_or_inactive_fork_point<T: SessionFixture>(fixture: T) {
    session_chat(&fixture, "src").await;
    session_chat(&fixture, "other").await;
    port(
        repo(&fixture)
            .submit_turn(
                turn("t-src", "src"),
                vec![
                    user_message("root", "src", "root"),
                    user_message("kid", "src", "kid"),
                ],
            )
            .await,
    );
    // An id that names nothing.
    let error = repo(&fixture)
        .fork_chat("src", "ghost", "dst", NOW)
        .await
        .expect_err("a fork point must exist");
    assert_eq!(
        error.task_input_reason(),
        Some(TaskInputReason::InvalidForkPoint)
    );
    // A message of another chat is no fork point either.
    port(
        repo(&fixture)
            .submit_turn(
                turn("t-other", "other"),
                vec![user_message("elsewhere", "other", "x")],
            )
            .await,
    );
    let error = repo(&fixture)
        .fork_chat("src", "elsewhere", "dst2", NOW)
        .await
        .expect_err("a foreign message is no fork point");
    assert_eq!(
        error.task_input_reason(),
        Some(TaskInputReason::InvalidForkPoint)
    );
    // A source chat that does not exist is a plain not-found.
    let error = repo(&fixture)
        .fork_chat("absent", "root", "dst3", NOW)
        .await
        .expect_err("no source chat");
    assert!(error.is_not_found(), "{error:?}");
}

async fn messages_hide_inactive_without_the_flag<T: SessionFixture>(fixture: T) {
    session_chat(&fixture, "c1").await;
    // Two branches off one root: the second activation retires the first.
    let root = MessageInput {
        id: Some("root".into()),
        chat_id: "c1".into(),
        run_id: None,
        role: "user".into(),
        content: json!("root"),
        activate: true,
        explicit_root: true,
        ..Default::default()
    };
    port(
        repo(&fixture)
            .submit_turn(turn("t1", "c1"), vec![root])
            .await,
    );
    let branch_a = MessageInput {
        id: Some("branch-a".into()),
        parent_message_id: Some("root".into()),
        ..user_message("branch-a", "c1", "a")
    };
    port(
        repo(&fixture)
            .submit_turn(turn("t2", "c1"), vec![branch_a])
            .await,
    );
    let branch_b = MessageInput {
        id: Some("branch-b".into()),
        parent_message_id: Some("root".into()),
        activate: true,
        ..user_message("branch-b", "c1", "b")
    };
    port(
        repo(&fixture)
            .submit_turn(turn("t3", "c1"), vec![branch_b])
            .await,
    );

    let active = port(repo(&fixture).messages("c1", false).await);
    let all = port(repo(&fixture).messages("c1", true).await);
    assert!(all.len() > active.len(), "inactive history must be hidden");
    assert!(
        !active.iter().any(|message| message.id == "branch-a"),
        "a retired branch is not active"
    );
    assert!(all.iter().any(|message| message.id == "branch-a"));
    // Listing order is stored order, and a chat that never was has nothing.
    let mut keys: Vec<u64> = active.iter().map(|message| message.order_key).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        active
            .iter()
            .map(|message| message.order_key)
            .collect::<Vec<_>>()
    );
    assert!(port(repo(&fixture).messages("absent", true).await).is_empty());
}

async fn list_context_summaries_newest_first<T: SessionFixture>(fixture: T) {
    // Two valid summaries of one session; the newest is written first, so
    // only the sort can put them in order.
    let older = summary("sum-old", "s1", "2026-01-01T00:00:00.000Z");
    let newer = summary("sum-new", "s1", "2026-02-01T00:00:00.000Z");
    seed!(fixture.put_setting(CONTEXT_SUMMARIES_NAMESPACE, "sum-new", newer));
    seed!(fixture.put_setting(CONTEXT_SUMMARIES_NAMESPACE, "sum-old", older));
    let listed = port(repo(&fixture).list_context_summaries("s1").await);
    assert_eq!(
        listed
            .iter()
            .map(|value| value["summaryId"].clone())
            .collect::<Vec<_>>(),
        [json!("sum-new"), json!("sum-old")]
    );
    assert!(port(repo(&fixture).list_context_summaries("s2").await).is_empty());
}

async fn get_setting_absent_and_present<T: SessionFixture>(fixture: T) {
    assert!(port(repo(&fixture).get_setting("ns", "missing").await).is_none());
    seed!(fixture.put_setting("ns", "key", json!({"a": 1})));
    assert_eq!(
        port(repo(&fixture).get_setting("ns", "key").await),
        Some(json!({"a": 1}))
    );
    // One namespace's key never reads in another.
    assert!(port(repo(&fixture).get_setting("ns2", "key").await).is_none());
}

async fn events_page_after_a_cursor<T: SessionFixture>(fixture: T) {
    port(
        repo(&fixture)
            .submit_turn(turn("t1", "scope"), vec![])
            .await,
    );
    let events: Vec<Value> = (1..=4).map(|seq| event(seq, &format!("e{seq}"))).collect();
    seed!(fixture.put_events("t1", events, NOW));

    let all = port(repo(&fixture).events("t1", None, 10).await);
    assert_eq!(all.len(), 4);
    assert_eq!(
        all.iter().map(|event| event.seq).collect::<Vec<_>>(),
        [1.0, 2.0, 3.0, 4.0]
    );
    let page = port(repo(&fixture).events("t1", Some(2), 2).await);
    assert_eq!(
        page.iter().map(|event| event.seq).collect::<Vec<_>>(),
        [3.0, 4.0]
    );
    assert!(port(repo(&fixture).events("t1", Some(4), 10).await).is_empty());
    assert!(port(repo(&fixture).events("t1", None, 0).await).is_empty());
    // A task that never was has no events.
    let error = repo(&fixture)
        .events("absent", None, 10)
        .await
        .expect_err("no task, no events");
    assert!(error.is_not_found(), "{error:?}");
}

fn event(seq: u64, id: &str) -> Value {
    json!({"type":"run.test","seq":seq,"eventId":id,"timestamp":"2026-10-22T12:00:00.000Z","contractVersion":"0.7.0","data":{"nested":true}})
}

async fn list_tasks_by_scope_and_all<T: SessionFixture>(fixture: T) {
    port(repo(&fixture).submit_turn(turn("t1", "a"), vec![]).await);
    port(repo(&fixture).submit_turn(turn("t2", "b"), vec![]).await);
    let one = port(repo(&fixture).list_tasks(Some("a".into())).await);
    assert_eq!(
        one.iter()
            .map(|task| task.task_id.clone())
            .collect::<Vec<_>>(),
        ["t1"]
    );
    let all = port(repo(&fixture).list_tasks(None).await);
    assert_eq!(all.len(), 2);
    assert!(port(repo(&fixture).list_tasks(Some("none".into())).await).is_empty());
}

// ---- the steering input queue ------------------------------------------------

async fn create_input_replays_identical_bytes<T: SessionFixture>(fixture: T) {
    let input = steer("i1", "s1", "hello");
    let (first, created) = port(repo(&fixture).create_input(input.clone(), NOW).await);
    assert!(created);
    assert_eq!(first.status, AgentInputStatus::Pending);
    assert_eq!(first.order, 1, "the store assigns the FIFO key");
    let (again, created) = port(repo(&fixture).create_input(input, NOW + 10).await);
    assert!(!created, "the same id and bytes replay");
    assert_eq!(again, first, "the replay is the original record");
}

async fn create_input_conflicts_on_different_bytes<T: SessionFixture>(fixture: T) {
    port(
        repo(&fixture)
            .create_input(steer("i1", "s1", "hello"), NOW)
            .await,
    );
    let mut changed = steer("i1", "s1", "hello");
    changed.content_hash = "different".into();
    let error = repo(&fixture)
        .create_input(changed, NOW)
        .await
        .expect_err("same id, different content");
    assert!(error.input_conflict().is_some(), "{error:?}");
    assert!(
        error
            .input_conflict()
            .is_some_and(|m| m.starts_with("input_conflict:"))
    );
    // A different kind under one id is the same refusal.
    let mut other_kind = steer("i1", "s1", "hello");
    other_kind.kind = AgentInputKind::FollowUp;
    let error = repo(&fixture)
        .create_input(other_kind, NOW)
        .await
        .expect_err("kind is part of the identity");
    assert!(error.input_conflict().is_some(), "{error:?}");
    // And so is a different session.
    let mut other_session = steer("i1", "s2", "hello");
    other_session.content_hash = "hash-i1".into();
    let error = repo(&fixture)
        .create_input(other_session, NOW)
        .await
        .expect_err("session is part of the identity");
    assert!(error.input_conflict().is_some(), "{error:?}");
}

async fn get_input_missing_is_none<T: SessionFixture>(fixture: T) {
    assert!(port(repo(&fixture).get_input("nope".into()).await).is_none());
    port(
        repo(&fixture)
            .create_input(steer("i1", "s1", "hi"), NOW)
            .await,
    );
    assert!(port(repo(&fixture).get_input("i1".into()).await).is_some());
}

async fn list_inputs_filters_by_kind_status_and_target<T: SessionFixture>(fixture: T) {
    port(
        repo(&fixture)
            .create_input(steer("s-a", "s1", "a"), NOW)
            .await,
    );
    port(
        repo(&fixture)
            .create_input(follow_up("f-b", "s1", "b"), NOW)
            .await,
    );
    let mut targeted = follow_up("f-c", "s1", "c");
    targeted.target_task_id = Some("turn-1".into());
    port(repo(&fixture).create_input(targeted, NOW).await);
    // Another session's input never appears.
    port(
        repo(&fixture)
            .create_input(steer("s-d", "s2", "d"), NOW)
            .await,
    );

    let base = AgentInputQuery {
        session_id: "s1".into(),
        kinds: None,
        statuses: None,
        target_task_id: None,
    };
    let all = port(repo(&fixture).list_inputs(base.clone()).await);
    assert_eq!(
        all.iter()
            .map(|record| record.input_id.clone())
            .collect::<Vec<_>>(),
        ["s-a", "f-b", "f-c"],
        "ordered by the store's FIFO key"
    );
    let steers = port(
        repo(&fixture)
            .list_inputs(AgentInputQuery {
                kinds: Some(vec![AgentInputKind::Steer]),
                ..base.clone()
            })
            .await,
    );
    assert_eq!(steers.len(), 1);
    let pending = port(
        repo(&fixture)
            .list_inputs(AgentInputQuery {
                statuses: Some(vec![AgentInputStatus::Pending]),
                ..base.clone()
            })
            .await,
    );
    assert_eq!(pending.len(), 3);
    let answered = port(
        repo(&fixture)
            .list_inputs(AgentInputQuery {
                statuses: Some(vec![AgentInputStatus::Answered]),
                ..base.clone()
            })
            .await,
    );
    assert!(answered.is_empty());
    let targeted = port(
        repo(&fixture)
            .list_inputs(AgentInputQuery {
                target_task_id: Some("turn-1".into()),
                ..base
            })
            .await,
    );
    assert_eq!(targeted.len(), 1);
    assert_eq!(targeted[0].input_id, "f-c");
}

async fn transition_input_refuses_a_wrong_from<T: SessionFixture>(fixture: T) {
    port(
        repo(&fixture)
            .create_input(steer("i1", "s1", "hi"), NOW)
            .await,
    );
    // The record is Pending; a `from` that does not name Pending refuses.
    let error = repo(&fixture)
        .transition_input(
            "i1".into(),
            vec![AgentInputStatus::Applied],
            AgentInputStatus::Applied,
            AgentInputPatch::default(),
            NOW,
        )
        .await
        .expect_err("the CAS must see the current status");
    assert!(error.is_invalid_transition(), "{error:?}");
    // A transition the per-kind machine forbids refuses too: a steer can
    // never be answered.
    let error = repo(&fixture)
        .transition_input(
            "i1".into(),
            vec![AgentInputStatus::Pending],
            AgentInputStatus::Answered,
            AgentInputPatch::default(),
            NOW,
        )
        .await
        .expect_err("answered is not a steer target");
    assert!(error.is_invalid_transition(), "{error:?}");
    assert!(error.invalid_transition().is_some(), "{error:?}");
    // An unknown input is a not-found.
    let error = repo(&fixture)
        .transition_input(
            "ghost".into(),
            vec![AgentInputStatus::Pending],
            AgentInputStatus::Cancelled,
            AgentInputPatch::default(),
            NOW,
        )
        .await
        .expect_err("no such input");
    assert!(error.is_not_found(), "{error:?}");
}

async fn transition_input_applies_the_patch<T: SessionFixture>(fixture: T) {
    port(
        repo(&fixture)
            .create_input(steer("i1", "s1", "hi"), NOW)
            .await,
    );
    let moved = port(
        repo(&fixture)
            .transition_input(
                "i1".into(),
                vec![AgentInputStatus::Pending],
                AgentInputStatus::Applied,
                AgentInputPatch {
                    applied_task_id: Some("t1".into()),
                    applied_message_id: Some("m1".into()),
                    answer_id: None,
                },
                NOW + 5,
            )
            .await,
    );
    assert_eq!(moved.status, AgentInputStatus::Applied);
    assert_eq!(moved.applied_task_id.as_deref(), Some("t1"));
    assert_eq!(moved.applied_message_id.as_deref(), Some("m1"));
    assert_ne!(moved.updated_at, moved.created_at, "the move restamps it");
    assert_eq!(
        moved.updated_at,
        agentkit_kernel::ConversationTimestamp::from_epoch_millis(NOW + 5)
            .unwrap()
            .wire_text()
            .unwrap()
    );
    // Non-pending statuses are terminal: a second move refuses.
    let error = repo(&fixture)
        .transition_input(
            "i1".into(),
            vec![AgentInputStatus::Applied],
            AgentInputStatus::Converted,
            AgentInputPatch::default(),
            NOW,
        )
        .await
        .expect_err("applied is terminal");
    assert!(error.is_invalid_transition(), "{error:?}");
    // A patch field that is `None` keeps what the record has.
    port(
        repo(&fixture)
            .create_input(follow_up("i2", "s1", "again"), NOW)
            .await,
    );
    let kept = port(
        repo(&fixture)
            .transition_input(
                "i2".into(),
                vec![AgentInputStatus::Pending],
                AgentInputStatus::Converted,
                AgentInputPatch {
                    applied_task_id: Some("t2".into()),
                    ..Default::default()
                },
                NOW,
            )
            .await,
    );
    assert_eq!(kept.applied_task_id.as_deref(), Some("t2"));
    assert_eq!(kept.applied_message_id, None);
    assert_eq!(kept.answer_id, None);
}

async fn queue_or_stage_queues_behind_a_live_turn<T: SessionFixture>(fixture: T) {
    let session = "s1";
    session_chat(&fixture, session).await;
    queued_turn(&fixture, "live", session).await;
    let admission = port(
        repo(&fixture)
            .queue_or_stage_turn(admission("i1", session, "later"))
            .await,
    );
    let record = match admission {
        QueueAdmission::Queued(record) => record,
        QueueAdmission::Staged(task) => panic!("a live turn must queue, staged {task:?}"),
    };
    assert_eq!(record.status, AgentInputStatus::Pending);
    // `target_the_live_turn` fills the turn that actually holds the scope.
    assert_eq!(record.target_task_id.as_deref(), Some("live"));
    // Nothing was staged: no task named for the candidate exists.
    assert!(port(repo(&fixture).list_tasks(None).await).len() == 1);
    assert!(port(repo(&fixture).messages(session, false).await).len() == 1);
}

async fn queue_or_stage_stages_when_the_scope_is_free<T: SessionFixture>(fixture: T) {
    let session = "s1";
    session_chat(&fixture, session).await;
    let admission = port(
        repo(&fixture)
            .queue_or_stage_turn(admission("i1", session, "now"))
            .await,
    );
    let task = match admission {
        QueueAdmission::Staged(task) => task,
        QueueAdmission::Queued(record) => {
            panic!("a free scope stages the input, queued {record:?}")
        }
    };
    assert_eq!(task.task_id, "task-i1");
    assert_eq!(task.status, TaskStatus::Queued);
    let record = port(repo(&fixture).get_input("i1".into()).await).expect("stored");
    assert_eq!(record.status, AgentInputStatus::Converted);
    assert_eq!(record.applied_task_id.as_deref(), Some("task-i1"));
    // The turn's messages landed with the task.
    assert_eq!(
        ids(&port(repo(&fixture).messages(session, false).await)),
        ["msg-i1"]
    );
}

async fn stage_queued_turn_busy_then_free<T: SessionFixture>(fixture: T) {
    let session = "s1";
    session_chat(&fixture, session).await;
    // Queue one input behind a live turn (no candidate staging: the input
    // stays Pending and visible).
    queued_turn(&fixture, "live", session).await;
    let admission = port(
        repo(&fixture)
            .queue_or_stage_turn(QueueOrStage {
                candidate: None,
                ..admission("i1", session, "queued")
            })
            .await,
    );
    assert!(matches!(admission, QueueAdmission::Queued(_)));
    // A merely-queued turn of the same kind is NOT busy — its own settlement
    // owns the promotion, so a flush is allowed to convert right now.
    // `park_turn` is what makes the scope genuinely busy.
    seed!(fixture.park_turn(
        "live",
        human_request("parked", session, "live", json!({"kind": "question"})),
        NOW,
    ));
    assert_eq!(
        status_of(&fixture, "live").await,
        Some(TaskStatus::WaitingApproval)
    );
    assert_eq!(
        port(
            repo(&fixture)
                .stage_queued_turn(flush("i1", session, "queued"))
                .await
        ),
        None,
        "a parked turn holds the scope"
    );
    assert_eq!(
        port(repo(&fixture).get_input("i1".into()).await)
            .expect("stored")
            .status,
        AgentInputStatus::Pending,
        "the refusal wrote nothing"
    );

    // The settlement's own flush excludes the settling turn: free now.
    let flushed = port(
        repo(&fixture)
            .stage_queued_turn(StageQueuedTurn {
                exclude_task_id: Some("live".into()),
                ..flush("i1", session, "queued")
            })
            .await,
    );
    assert_eq!(flushed.as_deref(), Some("task-i1"));
    assert_eq!(
        port(repo(&fixture).get_input("i1".into()).await)
            .expect("stored")
            .status,
        AgentInputStatus::Converted
    );
    assert_eq!(
        ids(&port(repo(&fixture).messages(session, false).await)),
        ["live-user", "msg-i1"]
    );
}

async fn answer_human_request_records_the_answer<T: SessionFixture>(fixture: T) {
    let session = "s1";
    session_chat(&fixture, session).await;
    // A running turn, a tool record for it, then a non-approval request.
    port(
        repo(&fixture)
            .submit_turn(
                turn("t1", session),
                vec![
                    user_message("t1-user", session, "go"),
                    assistant_message("t1-asst", session, "t1"),
                ],
            )
            .await,
    );
    let content = json!({"kind": "question", "toolName": "request_user_input"});
    seed!(fixture.park_turn("t1", human_request("req", session, "t1", content), NOW));
    assert_eq!(
        status_of(&fixture, "t1").await,
        Some(TaskStatus::WaitingApproval),
        "the fixture parked the turn"
    );

    let outcome = port(
        repo(&fixture)
            .answer_human_request(answer("req", "ans", json!({"text": "yes"})))
            .await,
    );
    assert!(!outcome.receipt.duplicate);
    assert_eq!(outcome.receipt.turn_id, "t1");
    assert_eq!(outcome.receipt.message_id, "ans-msg");
    assert_eq!(outcome.enqueue, Some(("t1".to_owned(), session.to_owned())));
    // The answer's tool record is durable, and the request is answered.
    let messages = port(repo(&fixture).messages(session, false).await);
    assert!(ids(&messages).contains(&"ans-msg".to_owned()));
    let record = port(repo(&fixture).get_input("req".into()).await).unwrap();
    assert_eq!(record.status, AgentInputStatus::Answered);
    assert_eq!(record.answer_id.as_deref(), Some("ans"));

    // The same answer again is the idempotent duplicate, not a new write.
    let again = port(
        repo(&fixture)
            .answer_human_request(answer("req", "ans", json!({"text": "yes"})))
            .await,
    );
    assert!(again.receipt.duplicate);
    assert_eq!(again.receipt.message_id, "ans-msg");
    assert_eq!(
        port(repo(&fixture).messages(session, false).await).len(),
        messages.len()
    );
}

async fn answer_human_request_refuses_a_second_answer<T: SessionFixture>(fixture: T) {
    let session = "s1";
    session_chat(&fixture, session).await;
    port(
        repo(&fixture)
            .submit_turn(
                turn("t1", session),
                vec![user_message("t1-user", session, "go")],
            )
            .await,
    );
    seed!(fixture.park_turn(
        "t1",
        human_request("req", session, "t1", approval_content(false)),
        NOW,
    ));
    // An approve answer records the marker, not a tool row.
    let first = port(
        repo(&fixture)
            .answer_human_request(answer("req", "ans", json!({"decision": "approve"})))
            .await,
    );
    assert!(!first.receipt.duplicate);
    assert_eq!(first.receipt.message_id, "");

    // A different answer id on the same request is an answer conflict.
    let error = repo(&fixture)
        .answer_human_request(answer("req", "other", json!({"decision": "approve"})))
        .await
        .expect_err("one request, one answer");
    assert!(error.answer_conflict().is_some(), "{error:?}");
    // The same answer id but different bytes too: the marker carries the
    // response hash.
    let error = repo(&fixture)
        .answer_human_request(AnswerHumanRequest {
            response_hash: "different-hash".into(),
            ..answer("req", "ans", json!({"decision": "approve"}))
        })
        .await
        .expect_err("the hash is part of the answer identity");
    assert!(error.answer_conflict().is_some(), "{error:?}");
    // The identical answer replays.
    let replay = port(
        repo(&fixture)
            .answer_human_request(answer("req", "ans", json!({"decision": "approve"})))
            .await,
    );
    assert!(replay.receipt.duplicate);
}

async fn answer_human_request_refuses_a_stale_answer<T: SessionFixture>(fixture: T) {
    let session = "s1";
    session_chat(&fixture, session).await;
    // A request whose turn never parked: it is merely queued.
    port(
        repo(&fixture)
            .submit_turn(
                turn("t1", session),
                vec![user_message("t1-user", session, "go")],
            )
            .await,
    );
    port(
        repo(&fixture)
            .create_input(
                human_request("req", session, "t1", approval_content(false)),
                NOW,
            )
            .await,
    );
    let error = repo(&fixture)
        .answer_human_request(answer("req", "ans", json!({"decision": "approve"})))
        .await
        .expect_err("the turn is not waiting for this answer");
    assert!(error.answer_stale().is_some(), "{error:?}");
    assert!(error.is_invalid_transition(), "stale is a state refusal");

    // A request that names no turn at all is stale for the same reason.
    let mut orphan = human_request("orphan", session, "t1", approval_content(false));
    orphan.target_task_id = None;
    port(repo(&fixture).create_input(orphan, NOW).await);
    let error = repo(&fixture)
        .answer_human_request(answer("orphan", "ans", json!({"decision": "approve"})))
        .await
        .expect_err("no turn to resume");
    assert!(error.answer_stale().is_some(), "{error:?}");

    // An unknown request id, and a non-human-request id, are not-found.
    let error = repo(&fixture)
        .answer_human_request(answer("ghost", "ans", json!({"decision": "approve"})))
        .await
        .expect_err("no such request");
    assert!(error.is_not_found(), "{error:?}");
    port(
        repo(&fixture)
            .create_input(steer("steer-1", session, "not a request"), NOW)
            .await,
    );
    let error = repo(&fixture)
        .answer_human_request(answer("steer-1", "ans", json!({"decision": "approve"})))
        .await
        .expect_err("a steer is no request");
    assert!(error.is_not_found(), "{error:?}");

    // An approval answer that does not read as a decision is invalid input,
    // refused before any staleness gate speaks.
    let error = repo(&fixture)
        .answer_human_request(answer("req", "ans", json!({"answer": "yes"})))
        .await
        .expect_err("an approval answer must be a decision");
    assert!(error.invalid_input().is_some(), "{error:?}");
}

// ---- interrupts, compaction, revocation, submission --------------------------

async fn request_interrupt_marks_a_running_turn<T: SessionFixture>(fixture: T) {
    let session = "s1";
    session_chat(&fixture, session).await;
    // A queued task parks directly: nothing is running to notice.
    port(
        repo(&fixture)
            .submit_turn(
                turn("queued", session),
                vec![user_message("q", session, "go")],
            )
            .await,
    );
    port(repo(&fixture).request_interrupt("queued".into(), NOW).await);
    assert_eq!(
        status_of(&fixture, "queued").await,
        Some(TaskStatus::Interrupted)
    );

    // A terminal task is left alone, and an unknown one is a not-found.
    let error = repo(&fixture)
        .request_interrupt("ghost".into(), NOW)
        .await
        .expect_err("no such task");
    assert!(error.is_not_found(), "{error:?}");
    assert!(
        error
            .not_found()
            .is_some_and(|message| message.contains("does not exist"))
    );
}

async fn request_compact_refuses_a_stale_descriptor<T: SessionFixture>(fixture: T) {
    let session = "s1";
    session_chat(&fixture, session).await;
    let error = repo(&fixture)
        .request_compact(
            session.into(),
            "session.turn".into(),
            json!({"version": 1}),
            NOW,
        )
        .await
        .expect_err("the descriptor moved");
    assert_eq!(error.session_conflict(), Some(session));

    // A live turn of that kind refuses too, and names the busy scope.
    port(
        repo(&fixture)
            .request_compact(session.into(), "session.turn".into(), descriptor(), NOW)
            .await,
    );
    queued_turn(&fixture, "live", session).await;
    let error = repo(&fixture)
        .request_compact(session.into(), "session.turn".into(), descriptor(), NOW)
        .await
        .expect_err("a turn is assembling its history");
    assert_eq!(error.scope_busy(), Some(session));
    // A chat that never was is a not-found.
    let error = repo(&fixture)
        .request_compact("absent".into(), "session.turn".into(), descriptor(), NOW)
        .await
        .expect_err("no chat");
    assert!(error.is_not_found(), "{error:?}");
}

async fn request_compact_stamps_a_current_descriptor<T: SessionFixture>(fixture: T) {
    let session = "s1";
    session_chat(&fixture, session).await;
    port(
        repo(&fixture)
            .request_compact(session.into(), "session.turn".into(), descriptor(), NOW)
            .await,
    );
    let chat = port(repo(&fixture).get_chat(session).await).expect("chat");
    assert!(
        chat.metadata
            .get(agentkit_sessions::COMPACT_REQUESTED_METADATA_KEY)
            .is_some_and(|value| value.is_string()),
        "the request is stamped with the instant: {:?}",
        chat.metadata
    );
    // Another kind's live turn does not block this kind's compaction.
    port(
        repo(&fixture)
            .submit_turn(
                SubmitTask {
                    kind: "other.kind".into(),
                    ..turn("other", session)
                },
                vec![user_message("o", session, "go")],
            )
            .await,
    );
    port(
        repo(&fixture)
            .request_compact(session.into(), "session.turn".into(), descriptor(), NOW + 1)
            .await,
    );
}

async fn revoke_standing_approval_is_idempotent<T: SessionFixture>(fixture: T) {
    let session = "s1";
    let digest = "d".repeat(64);
    port(
        repo(&fixture)
            .create_chat(session, None, metadata_with_grant("g1", &digest), NOW)
            .await,
    );
    let revoked = port(
        repo(&fixture)
            .revoke_standing_approval(session.into(), "g1".into(), NOW)
            .await,
    );
    assert_eq!(revoked["grantId"], json!("g1"));
    assert_eq!(
        revoked["revokedAt"],
        json!(wire(NOW)),
        "the returned grant carries the revocation instant"
    );
    // A second revoke is the same answer, and does not restamp it.
    let again = port(
        repo(&fixture)
            .revoke_standing_approval(session.into(), "g1".into(), NOW + 5_000)
            .await,
    );
    assert_eq!(again, revoked);
    let chat = port(repo(&fixture).get_chat(session).await).expect("chat");
    let grants = agentkit_sessions::standing_grants(&chat.metadata);
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0]["revokedAt"], json!(wire(NOW)));
    // The revocation stops the grant matching.
    assert!(
        agentkit_sessions::find_active_standing_grant(&chat.metadata, &digest).is_none(),
        "a revoked grant matches nothing"
    );
    // An unknown grant id is a not-found naming both ids.
    let error = repo(&fixture)
        .revoke_standing_approval(session.into(), "ghost".into(), NOW)
        .await
        .expect_err("no such grant");
    assert!(error.is_not_found(), "{error:?}");
    assert!(
        error
            .not_found()
            .is_some_and(|message| message.contains("ghost") && message.contains(session))
    );
    // A chat with no readable grants refuses rather than overwrite.
    port(
        repo(&fixture)
            .create_chat(
                "broken",
                None,
                json!({
                    SESSION_METADATA_KEY: descriptor(),
                    agentkit_sessions::STANDING_APPROVALS_METADATA_KEY: {"version": 2},
                }),
                NOW,
            )
            .await,
    );
    let error = repo(&fixture)
        .revoke_standing_approval("broken".into(), "g1".into(), NOW)
        .await
        .expect_err("unreadable permission data is never overwritten");
    assert_eq!(
        error.invalid_input(),
        Some("The session's standing approvals are unreadable and were not overwritten. (broken)")
    );
}

/// The wire text of one instant, the shape both twins stamp.
fn wire(now: i64) -> String {
    agentkit_kernel::ConversationTimestamp::from_epoch_millis(now)
        .expect("in range")
        .wire_text()
        .expect("in range")
        .into_owned()
}

async fn submit_turn_writes_the_task_and_its_messages<T: SessionFixture>(fixture: T) {
    let session = "s1";
    session_chat(&fixture, session).await;
    let task = port(
        repo(&fixture)
            .submit_turn(
                turn("t1", session),
                vec![
                    user_message("t1-user", session, "hello"),
                    assistant_message("t1-asst", session, "t1"),
                ],
            )
            .await,
    );
    assert_eq!(task.status, TaskStatus::Queued);
    assert_eq!(task.scope_id, session);
    assert_eq!(
        ids(&port(repo(&fixture).messages(session, false).await)),
        ["t1-user", "t1-asst"]
    );
    // The replay of the same identity returns the same record, writes nothing.
    let replay = port(
        repo(&fixture)
            .submit_turn(
                turn("t1", session),
                vec![user_message("dup", session, "again")],
            )
            .await,
    );
    assert_eq!(replay, task);
    assert_eq!(port(repo(&fixture).messages(session, false).await).len(), 2);
    // A replay naming different facts is refused.
    let error = repo(&fixture)
        .submit_turn(
            SubmitTask {
                payload: json!({"chatId": session, "model": "m2"}),
                ..turn("t1", session)
            },
            vec![],
        )
        .await
        .expect_err("a replay must name the same identity");
    assert_eq!(
        error.task_input_reason(),
        Some(TaskInputReason::ReplayIdentityMismatch)
    );
    // A user message aimed at another chat is refused, and the task lands
    // with nothing (the whole call is one commit).
    let error = repo(&fixture)
        .submit_turn(
            turn("t2", session),
            vec![user_message("wrong", "elsewhere", "nope")],
        )
        .await
        .expect_err("a turn may not write another chat");
    assert_eq!(
        error.invalid_input(),
        Some("message scope does not match task")
    );
    assert!(port(repo(&fixture).get_chat("t2").await).is_none());
    assert_eq!(
        port(repo(&fixture).list_tasks(None).await).len(),
        1,
        "a refused submit writes no task"
    );
}

// ---- catalogs, managed content, dispatch -------------------------------------

async fn catalog_reads_and_absences<T: SessionFixture>(fixture: T) {
    seed!(fixture.put_provider(provider("p1"), capabilities()));
    seed!(fixture.put_provider_models("p1", vec![model("p1", "m2"), model("p1", "m1")]));
    let skill = seed!(fixture.put_skill("docs", "SKILL.md", SKILL_BODY));
    let digest = "e".repeat(64);
    let _ = digest;
    seed!(fixture.put_agent("alice", agent_definition("alice", &[]), NOW));

    assert_eq!(
        port(repo(&fixture).get_provider("p1").await),
        Some(provider("p1"))
    );
    assert!(port(repo(&fixture).get_provider("p2").await).is_none());
    assert_eq!(
        port(repo(&fixture).get_provider_capabilities("p1").await),
        Some(capabilities())
    );
    assert!(port(repo(&fixture).get_provider_capabilities("p2").await).is_none());
    // Catalog order is by modelId, not insertion order.
    assert_eq!(
        port(repo(&fixture).list_provider_models("p1").await)
            .iter()
            .map(|value| value["modelId"].clone())
            .collect::<Vec<_>>(),
        [json!("m1"), json!("m2")]
    );
    assert!(port(repo(&fixture).list_provider_models("nobody").await).is_empty());

    let entry = port(repo(&fixture).get_skill("docs:SKILL.md".into()).await).expect("skill");
    assert_eq!(entry["id"], json!("docs:SKILL.md"));
    assert_eq!(entry["revision"], skill["revision"]);
    // The catalog projection carries no document body.
    assert!(entry.get("content").is_none(), "{entry:?}");
    assert!(port(repo(&fixture).get_skill("no-colon".into()).await).is_none());
    assert!(port(repo(&fixture).get_skill("docs:OTHER.md".into()).await).is_none());

    let agent = port(repo(&fixture).get_agent("alice".into()).await).expect("agent");
    assert_eq!(agent.agent_id, "alice");
    assert_eq!(agent.current_revision, 1);
    assert_eq!(agent.lifecycle, agentkit_sessions::AgentLifecycle::Active);
    assert!(port(repo(&fixture).get_agent("bob".into()).await).is_none());
}

const SKILL_BODY: &str = "---\nname: docs\ndescription: pinned skill\n---\n# skill body";

async fn resolve_run_content_reports_unavailable_skills<T: SessionFixture>(fixture: T) {
    let skill = seed!(fixture.put_skill("docs", "SKILL.md", SKILL_BODY));
    let revision = skill["revision"].as_str().expect("hashed").to_owned();
    let selection = json!({"skills": [{"id": "docs:SKILL.md", "revision": revision}]});

    let resolved = port(
        repo(&fixture)
            .resolve_run_content(selection.clone(), vec![], false)
            .await,
    );
    assert_eq!(resolved["blocks"].as_array().unwrap().len(), 1);
    assert!(resolved["unavailable"].as_array().unwrap().is_empty());
    assert!(resolved["messages"].as_array().unwrap().is_empty());

    // A stale revision is a hard refusal without tolerance, and a reported
    // absence with it.
    let stale = json!({"skills": [{"id": "docs:SKILL.md", "revision": "0".repeat(64)}]});
    let error = repo(&fixture)
        .resolve_run_content(stale.clone(), vec![], false)
        .await
        .expect_err("a changed revision must refuse");
    assert_eq!(
        error.invalid_input(),
        Some("Selected skill revision changed or disappeared.")
    );
    let tolerated = port(
        repo(&fixture)
            .resolve_run_content(stale, vec![], true)
            .await,
    );
    let unavailable = tolerated["unavailable"].as_array().unwrap();
    assert_eq!(unavailable.len(), 1);
    assert_eq!(unavailable[0]["id"], json!("docs:SKILL.md"));
    assert!(tolerated["blocks"].as_array().unwrap().is_empty());

    // A skill needing a connector the caller did not authorize is the
    // connector refusal, and it tolerates into `unavailable` too.
    let needs = seed!(fixture.put_skill(
        "needs",
        "SKILL.md",
        "---\nname: needs\ndescription: d\nmetadata:\n  requires-connectors: con-1\n---\nbody"
    ));
    let need_selection = json!({
        "skills": [{"id": "needs:SKILL.md", "revision": needs["revision"].as_str().unwrap()}]
    });
    let error = repo(&fixture)
        .resolve_run_content(need_selection.clone(), vec![], false)
        .await
        .expect_err("the connector grant is required");
    assert!(error.selected_connector_required(), "{error:?}");
    assert_eq!(
        error.invalid_input(),
        Some("Skill requires a selected authorized connector.")
    );
    let granted = port(
        repo(&fixture)
            .resolve_run_content(need_selection.clone(), vec!["con-1".into()], false)
            .await,
    );
    assert_eq!(granted["authorizedConnectors"], json!(["con-1"]));
    let refused = port(
        repo(&fixture)
            .resolve_run_content(need_selection, vec![], true)
            .await,
    );
    assert_eq!(refused["unavailable"].as_array().unwrap().len(), 1);

    // A malformed selection is invalid input whatever the tolerance.
    for selection in [
        json!({"skills": "not-an-array"}),
        json!({"skills": []}),
        json!({"skills": [{"id": "", "revision": "r"}]}),
        json!({"skills": [{"id": "docs:SKILL.md", "revision": revision}, {"id": "docs:SKILL.md", "revision": revision}]}),
    ] {
        if selection["skills"].is_array() && selection["skills"].as_array().unwrap().is_empty() {
            // An empty selection is legal: it resolves to nothing.
            let empty = port(
                repo(&fixture)
                    .resolve_run_content(selection, vec![], false)
                    .await,
            );
            assert!(empty["blocks"].as_array().unwrap().is_empty());
            continue;
        }
        let error = repo(&fixture)
            .resolve_run_content(selection, vec![], false)
            .await
            .expect_err("a malformed selection refuses");
        assert_eq!(error.invalid_input(), Some("Invalid skill selection."));
    }
}

async fn run_profile_dispatch_refusal_reads_the_pinned_facts<T: SessionFixture>(fixture: T) {
    seed!(fixture.put_provider(provider("p1"), capabilities()));
    seed!(fixture.put_agent("alice", agent_definition("alice", &[]), NOW));
    session_chat(&fixture, "s1").await;
    // An un-pinned turn is never fenced.
    port(
        repo(&fixture)
            .submit_turn(turn("plain", "s1"), vec![user_message("p", "s1", "go")])
            .await,
    );
    assert_eq!(
        port(
            repo(&fixture)
                .run_profile_dispatch_refusal("plain".into())
                .await
        ),
        None
    );

    seed!(fixture.put_pinned_turn("pinned", "alice", profile("alice"), NOW));
    assert_eq!(
        port(
            repo(&fixture)
                .run_profile_dispatch_refusal("pinned".into())
                .await
        ),
        None,
        "every pinned fact still holds"
    );

    // Deleting the provider fences the pin and names it.
    seed!(fixture.delete_provider("p1"));
    let reason = port(
        repo(&fixture)
            .run_profile_dispatch_refusal("pinned".into())
            .await,
    );
    assert!(
        reason
            .as_ref()
            .is_some_and(|reason| reason.contains("provider_deleted:p1")),
        "{reason:?}"
    );
    // A task that never was is a not-found, not a refusal.
    let error = repo(&fixture)
        .run_profile_dispatch_refusal("ghost".into())
        .await
        .expect_err("no such task");
    assert!(error.is_not_found(), "{error:?}");
}

// ---- execution receipts ------------------------------------------------------

async fn list_executions_by_chat_and_resolve<T: SessionFixture>(fixture: T) {
    session_chat(&fixture, "c1").await;
    port(
        repo(&fixture)
            .submit_turn(turn("run-1", "c1"), vec![user_message("u", "c1", "go")])
            .await,
    );
    seed!(fixture.admit_receipt(receipt("x1", "run-1", "c1"), true, NOW + 1));
    let listed = port(repo(&fixture).list_executions_by_chat("c1".into()).await);
    assert_eq!(
        listed
            .iter()
            .map(|record| record.execution_id.clone())
            .collect::<Vec<_>>(),
        ["x1"]
    );
    assert_eq!(listed[0].status, ExecutionReceiptStatus::Admitted);
    assert!(port(repo(&fixture).list_executions_by_chat("other".into()).await).is_empty());

    // `not_started` closes it with no result and audit the operator's word.
    let closed = port(
        repo(&fixture)
            .resolve_execution("x1".into(), resolution(ResolutionOutcome::NotStarted, None))
            .await,
    );
    assert_eq!(closed.status, ExecutionReceiptStatus::Resolved);
    assert_eq!(closed.result, None);
    let audit = closed.resolution.clone().expect("audit written");
    assert_eq!(audit.outcome, ResolutionOutcome::NotStarted);
    assert_eq!(audit.note.as_deref(), Some("closed by hand"));
    assert_eq!(audit.resolved_by.as_deref(), Some("operator"));
    assert_eq!(audit.resolved_at, NOW);
    // Repeating the same outcome is the stored record, unchanged.
    let again = port(
        repo(&fixture)
            .resolve_execution("x1".into(), resolution(ResolutionOutcome::NotStarted, None))
            .await,
    );
    assert_eq!(again, closed);
    // A different outcome on a closed receipt is refused.
    let error = repo(&fixture)
        .resolve_execution(
            "x1".into(),
            resolution(ResolutionOutcome::Acknowledged, None),
        )
        .await
        .expect_err("one decision per receipt");
    assert!(
        error
            .receipt_not_uncertain()
            .is_some_and(|message| message.starts_with(RECEIPT_NOT_UNCERTAIN))
    );
    assert!(error.is_invalid_transition(), "{error:?}");
}

async fn resolve_execution_refuses_a_finished_receipt<T: SessionFixture>(fixture: T) {
    session_chat(&fixture, "c1").await;
    port(
        repo(&fixture)
            .submit_turn(turn("run-1", "c1"), vec![user_message("u", "c1", "go")])
            .await,
    );
    seed!(fixture.admit_receipt(receipt("x1", "run-1", "c1"), true, NOW + 1));

    // A completed resolution needs a tool-result object.
    let error = repo(&fixture)
        .resolve_execution("x1".into(), resolution(ResolutionOutcome::Completed, None))
        .await
        .expect_err("no result named");
    assert_eq!(
        error.invalid_input(),
        Some("a completed resolution requires a tool-result object")
    );
    let error = repo(&fixture)
        .resolve_execution(
            "x1".into(),
            resolution(ResolutionOutcome::NotStarted, Some(json!({"ok": true}))),
        )
        .await
        .expect_err("only a completed outcome may carry a result");
    assert_eq!(
        error.invalid_input(),
        Some("only a completed resolution may carry a result")
    );
    let error = repo(&fixture)
        .resolve_execution(
            "x1".into(),
            ResolveExecution {
                resolved_by: Some(String::new()),
                ..resolution(ResolutionOutcome::NotStarted, None)
            },
        )
        .await
        .expect_err("an empty actor is refused");
    assert_eq!(
        error.invalid_input(),
        Some("resolvedBy must be 1-200 characters when present")
    );

    // An unknown receipt is a not-found naming it.
    let error = repo(&fixture)
        .resolve_execution(
            "ghost".into(),
            resolution(ResolutionOutcome::NotStarted, None),
        )
        .await
        .expect_err("no such receipt");
    assert!(error.is_not_found(), "{error:?}");
    assert!(
        error
            .not_found()
            .is_some_and(|message| message.contains("ghost"))
    );

    // Completing an uncertain receipt records the operator's result, and a
    // repeat must name the same result.
    let done = port(
        repo(&fixture)
            .resolve_execution(
                "x1".into(),
                resolution(ResolutionOutcome::Completed, Some(json!({"ok": true}))),
            )
            .await,
    );
    assert_eq!(done.status, ExecutionReceiptStatus::Completed);
    assert_eq!(done.result, Some(json!({"ok": true})));
    let same = port(
        repo(&fixture)
            .resolve_execution(
                "x1".into(),
                resolution(ResolutionOutcome::Completed, Some(json!({"ok": true}))),
            )
            .await,
    );
    assert_eq!(same, done);
    let error = repo(&fixture)
        .resolve_execution(
            "x1".into(),
            resolution(ResolutionOutcome::Completed, Some(json!({"ok": false}))),
        )
        .await
        .expect_err("a different result is a different decision");
    assert!(error.receipt_not_uncertain().is_some(), "{error:?}");
}

async fn resolve_execution_refuses_an_active_run<T: SessionFixture>(fixture: T) {
    session_chat(&fixture, "c1").await;
    // The run is still live: it may yet record its own result.
    port(
        repo(&fixture)
            .submit_turn(turn("run-1", "c1"), vec![user_message("u", "c1", "go")])
            .await,
    );
    seed!(fixture.admit_receipt(receipt("x1", "run-1", "c1"), false, NOW));
    let error = repo(&fixture)
        .resolve_execution("x1".into(), resolution(ResolutionOutcome::NotStarted, None))
        .await
        .expect_err("resolving would erase real uncertainty");
    assert!(
        error
            .receipt_run_active()
            .is_some_and(|message| message.starts_with(RECEIPT_RUN_ACTIVE))
    );
    assert!(
        error
            .receipt_run_active()
            .is_some_and(|message| message.contains("run-1"))
    );
    assert_eq!(
        port(repo(&fixture).list_executions_by_chat("c1".into()).await)[0].status,
        ExecutionReceiptStatus::Admitted,
        "the refusal writes nothing"
    );
}
