//! Durable session mechanics: parking, answering, settling, interrupting
//! (ADR 0016 §2/§5, ADR 0019).

mod support;
use agentkit_store::*;
use serde_json::{Value, json};
use support::*;

const DIGEST: &str = "a5ffc907b0a2d9c0f8b9d1647a3e5cb13d24f7bd0f9a41e2c3d5e7f8091a2b3c";
const PLAN: &str = "0b8c9d2e4f60718293a4b5c6d7e8f90123456789aabbccddeeff00112233445566";

fn store() -> SqliteStore {
    SqliteStore::memory(8).unwrap()
}

fn approval_content(extra: Option<Value>) -> Value {
    let mut content = json!({
        "kind": "approval",
        "toolName": "write_file",
        "argsSha256": DIGEST,
        "prompt": "write x",
    });
    if let Some(extra) = extra {
        for (key, value) in extra.as_object().unwrap() {
            content[key] = value.clone();
        }
    }
    content
}

fn human_request(
    input_id: &str,
    turn: &str,
    content: Value,
    tool_call_id: Option<&str>,
    expires_at: Option<&str>,
) -> CreateAgentInput {
    CreateAgentInput {
        input_id: input_id.into(),
        session_id: "chat".into(),
        kind: AgentInputKind::HumanRequest,
        content_hash: format!("hash-{input_id}"),
        content,
        target_task_id: Some(turn.into()),
        on_settled: None,
        attempt_id: Some("attempt-1".into()),
        tool_call_id: tool_call_id.map(str::to_owned),
        expires_at: expires_at.map(str::to_owned),
    }
}

async fn running_turn(store: &SqliteStore, turn: &str) -> Lease {
    if store.get_chat("chat").await.unwrap().is_none() {
        store
            .create_chat("chat", None, json!({}), 1000)
            .await
            .unwrap();
    }
    let message = MessageInput {
        id: Some(format!("{turn}-user")),
        chat_id: "chat".into(),
        run_id: None,
        role: "user".into(),
        content: json!({"role":"user","content":"do it"}),
        ..MessageInput::default()
    };
    let assistant = MessageInput {
        id: Some(format!("{turn}-assistant")),
        chat_id: "chat".into(),
        run_id: Some(turn.into()),
        role: "assistant".into(),
        content: json!({"role":"assistant","content":"working"}),
        ..MessageInput::default()
    };
    store
        .submit_turn(task(turn, "chat"), vec![message, assistant])
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
        .unwrap();
    let claimed = match claimed {
        Some(claimed) => claimed,
        None => panic!(
            "claim found nothing for {turn}: {:?}",
            store.list_tasks(None).await.unwrap()
        ),
    };
    assert_eq!(claimed.task.task_id, turn);
    claimed.lease
}

async fn park(
    store: &SqliteStore,
    lease: &Lease,
    request: CreateAgentInput,
    now: i64,
) -> Option<String> {
    store
        .park_turn(ParkTurn {
            fence: lease.fence(),
            assistant_message: None,
            events: vec![event(1, "evt-waiting")],
            request: Some(request),
            now,
        })
        .await
        .unwrap()
}

fn answer(request_id: &str, answer_id: &str, response: Value) -> AnswerHumanRequest {
    AnswerHumanRequest {
        request_id: request_id.into(),
        answer_id: answer_id.into(),
        response,
        response_hash: format!("sha-{answer_id}"),
        now: 3000,
        message_id: format!("{answer_id}-msg"),
        plan_tool_names: vec![],
        policy_denial: None,
        granted_mode: None,
    }
}

#[tokio::test]
async fn park_parks_and_approve_resumes_the_turn() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    park(
        &store,
        &lease,
        human_request("req", "run", approval_content(None), Some("call-1"), None),
        2000,
    )
    .await;
    let parked = store.get_task("run").await.unwrap().unwrap();
    assert_eq!(parked.status, TaskStatus::WaitingApproval);
    assert!(
        store
            .claim_with_concurrency(claim(2050), 1)
            .await
            .unwrap()
            .is_none(),
        "a waiting turn never holds the scope"
    );
    let record = store.get_input("req".into()).await.unwrap().unwrap();
    assert_eq!(record.status, AgentInputStatus::Pending);

    let outcome = store
        .answer_human_request(answer("req", "ans", json!({"decision":"approve"})))
        .await
        .unwrap();
    assert!(!outcome.receipt.duplicate);
    assert_eq!(outcome.receipt.message_id, "");
    assert_eq!(outcome.enqueue, Some(("run".to_owned(), "chat".to_owned())));
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Queued
    );
    let record = store.get_input("req".into()).await.unwrap().unwrap();
    assert_eq!(record.status, AgentInputStatus::Answered);
    assert_eq!(
        record.applied_message_id.as_deref(),
        Some(format!("{APPROVE_MARKER}sha-ans").as_str())
    );
    assert_eq!(record.answer_id.as_deref(), Some("ans"));
    // An approval appends no tool record.
    assert!(
        store
            .messages("chat", false)
            .await
            .unwrap()
            .iter()
            .all(|message| message.role != "tool")
    );
}

#[tokio::test]
async fn deny_appends_one_tool_record_and_remembers_the_note() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    park(
        &store,
        &lease,
        human_request("req", "run", approval_content(None), Some("call-1"), None),
        2000,
    )
    .await;
    let outcome = store
        .answer_human_request(answer(
            "req",
            "ans",
            json!({"decision":"deny","note":"not this path"}),
        ))
        .await
        .unwrap();
    assert_eq!(outcome.receipt.message_id, "ans-msg");
    let tool = store
        .messages("chat", false)
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.role == "tool")
        .unwrap();
    let envelope: Value = serde_json::from_str(tool.content.as_str().unwrap()).unwrap();
    assert_eq!(envelope["data"]["errorCode"], "approval_denied");
    assert_eq!(envelope["data"]["errorMessage"], "not this path");
    assert_eq!(tool.metadata["responseHash"], "sha-ans");
    assert_eq!(tool.metadata["requestId"], "req");
    assert_eq!(tool.tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Queued
    );
}

#[tokio::test]
async fn the_same_answer_replays_and_a_different_one_conflicts() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    park(
        &store,
        &lease,
        human_request(
            "req",
            "run",
            json!({"kind":"question","prompt":"which?"}),
            Some("call-1"),
            None,
        ),
        2000,
    )
    .await;
    let first = store
        .answer_human_request(answer("req", "ans", json!({"answer":"left"})))
        .await
        .unwrap();
    assert!(!first.receipt.duplicate);
    // Exact replay of the same answer: the same receipt, no second record.
    let replay = store
        .answer_human_request(answer("req", "ans", json!({"answer":"left"})))
        .await
        .unwrap();
    assert!(replay.receipt.duplicate);
    assert_eq!(replay.receipt.message_id, "ans-msg");
    assert_eq!(
        store
            .messages("chat", false)
            .await
            .unwrap()
            .iter()
            .filter(|message| message.role == "tool")
            .count(),
        1
    );
    // A different response to an answered request never overwrites.
    let error = store
        .answer_human_request(answer("req", "other", json!({"answer":"right"})))
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Duplicate(_)));
    assert!(error.to_string().contains("answer_conflict"));
}

#[tokio::test]
async fn a_half_committed_answer_is_repaired_not_conflicted() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    park(
        &store,
        &lease,
        human_request(
            "req",
            "run",
            json!({"kind":"question","prompt":"which?"}),
            Some("call-1"),
            None,
        ),
        2000,
    )
    .await;
    // Simulate the half-committed crash state: the receipt names a tool
    // record that never landed.
    store
        .transition_input(
            "req".into(),
            vec![AgentInputStatus::Pending],
            AgentInputStatus::Answered,
            AgentInputPatch {
                answer_id: Some("ans".into()),
                applied_message_id: Some("missing-msg".into()),
                ..Default::default()
            },
            2500,
        )
        .await
        .unwrap();
    let outcome = store
        .answer_human_request(answer("req", "ans", json!({"answer":"left"})))
        .await
        .unwrap();
    // Same answer id, same hash: the record it was owed is written.
    assert!(!outcome.receipt.duplicate);
    assert_eq!(outcome.receipt.message_id, "missing-msg");
    assert!(
        store
            .messages("chat", false)
            .await
            .unwrap()
            .iter()
            .any(|message| message.id == "missing-msg")
    );
}

#[tokio::test]
async fn expired_cancelled_and_wrong_turn_requests_are_stale() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    park(
        &store,
        &lease,
        human_request(
            "expired",
            "run",
            json!({"kind":"question"}),
            Some("call-1"),
            Some("1970-01-01T00:00:01.000Z"),
        ),
        2000,
    )
    .await;
    let error = store
        .answer_human_request(answer("expired", "a", json!({"answer":"x"})))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("it expired"), "{error}");

    store
        .create_input(
            human_request(
                "gone",
                "run",
                json!({"kind":"question"}),
                Some("call-1"),
                None,
            ),
            2000,
        )
        .await
        .unwrap();
    store
        .transition_input(
            "gone".into(),
            vec![AgentInputStatus::Pending],
            AgentInputStatus::Cancelled,
            AgentInputPatch::default(),
            2100,
        )
        .await
        .unwrap();
    let error = store
        .answer_human_request(answer("gone", "a", json!({"answer":"x"})))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("it was cancelled"), "{error}");

    // The request names a turn that is not waiting.
    let error = store
        .answer_human_request({
            let mut input = answer("req-x", "a", json!({"answer":"x"}));
            input.request_id = "ghost".into();
            input
        })
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::NotFound(_)), "{error}");
    // A waiting-approval task whose request was answered is no longer
    // answerable a second way: after the approve, a deny attempt is stale.
    // Cancelling the still-parked first turn is terminal (no lease, no
    // answerer) and takes its unanswered request down with it.
    store.cancel_task("run", 2300).await.unwrap();
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Cancelled
    );
    assert_eq!(
        store
            .get_input("expired".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Cancelled
    );
    let lease2 = running_turn(&store, "run2").await;
    park(
        &store,
        &lease2,
        human_request("twice", "run2", approval_content(None), Some("c"), None),
        2000,
    )
    .await;
    store
        .answer_human_request(answer("twice", "a1", json!({"decision":"approve"})))
        .await
        .unwrap();
    let error = store
        .answer_human_request(answer("twice", "a2", json!({"decision":"deny"})))
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Duplicate(_)), "{error}");
}

#[tokio::test]
async fn settle_converts_the_first_follow_up_and_cancels_cancel_steers() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    let follow_up = |id: &str| CreateAgentInput {
        input_id: id.into(),
        session_id: "chat".into(),
        kind: AgentInputKind::FollowUp,
        content: json!({"text": id}),
        content_hash: format!("hash-{id}"),
        target_task_id: None,
        on_settled: None,
        attempt_id: None,
        tool_call_id: None,
        expires_at: None,
    };
    store.create_input(follow_up("fu1"), 1600).await.unwrap();
    store.create_input(follow_up("fu2"), 1601).await.unwrap();
    store
        .create_input(
            CreateAgentInput {
                input_id: "s-cancel".into(),
                session_id: "chat".into(),
                kind: AgentInputKind::Steer,
                content: json!({"text":"stop","onSettled":"cancel"}),
                content_hash: "hash-sc".into(),
                target_task_id: Some("run".into()),
                on_settled: Some(AgentInputSettlePolicy::Cancel),
                attempt_id: None,
                tool_call_id: None,
                expires_at: None,
            },
            1602,
        )
        .await
        .unwrap();
    store
        .create_input(
            CreateAgentInput {
                input_id: "s-keep".into(),
                session_id: "chat".into(),
                kind: AgentInputKind::Steer,
                content: json!({"text":"also","onSettled":"follow_up"}),
                content_hash: "hash-sk".into(),
                target_task_id: Some("run".into()),
                on_settled: Some(AgentInputSettlePolicy::FollowUp),
                attempt_id: None,
                tool_call_id: None,
                expires_at: None,
            },
            1603,
        )
        .await
        .unwrap();

    let candidate = |id: &str| StagedCandidate {
        input_id: id.into(),
        task: SubmitTask {
            task_id: format!("{id}-turn"),
            kind: "test".into(),
            scope_id: "chat".into(),
            payload: json!({"chatId":"chat","text":id}),
            priority: 0,
            now: 1900,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        },
        messages: vec![MessageInput {
            id: Some(format!("{id}-message")),
            chat_id: "chat".into(),
            run_id: Some(format!("{id}-turn")),
            role: "user".into(),
            content: json!(id),
            ..MessageInput::default()
        }],
    };
    let settled = store
        .settle_session(
            SettleTask {
                fence: lease.fence(),
                now: 2000,
                status: TaskStatus::Completed,
                error: None,
                events: vec![],
                messages: vec![],
            },
            SessionSettle {
                convert: vec![candidate("fu1"), candidate("fu2")],
            },
        )
        .await
        .unwrap();
    assert_eq!(settled.status, TaskStatus::Completed);
    // The settling turn releases the scope and exactly one follow-up
    // converts; the rest wait for the next settle.
    let fu1 = store.get_input("fu1".into()).await.unwrap().unwrap();
    assert_eq!(fu1.status, AgentInputStatus::Converted);
    assert_eq!(fu1.applied_task_id.as_deref(), Some("fu1-turn"));
    assert_eq!(
        store.get_input("fu2".into()).await.unwrap().unwrap().status,
        AgentInputStatus::Pending
    );
    assert!(store.get_task("fu1-turn").await.unwrap().is_some());
    assert!(
        store
            .messages("chat", false)
            .await
            .unwrap()
            .iter()
            .any(|message| message.id == "fu1-message")
    );
    // Cancel-marked steers die with the turn; the others stay durable.
    assert_eq!(
        store
            .get_input("s-cancel".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Cancelled
    );
    assert_eq!(
        store
            .get_input("s-keep".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Pending
    );
}

#[tokio::test]
async fn conversion_refuses_a_busy_scope_and_failure_cancels_everything() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("busy.db");
    let store = SqliteStore::open(&path, 8).unwrap();
    let lease = running_turn(&store, "run").await;
    let follow_up = |id: &str| CreateAgentInput {
        input_id: id.into(),
        session_id: "chat".into(),
        kind: AgentInputKind::FollowUp,
        content: json!({"text": id}),
        content_hash: format!("hash-{id}"),
        target_task_id: None,
        on_settled: None,
        attempt_id: None,
        tool_call_id: None,
        expires_at: None,
    };
    store.create_input(follow_up("fu1"), 1600).await.unwrap();
    // A sibling turn in durable retry backoff reserves the scope exactly the
    // way the claim guard reads it. (Claim itself cannot create a second
    // live turn in one scope, so the state is stamped directly.)
    store.submit(task("sib", "chat")).await.unwrap();
    {
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute(
            "UPDATE tasks SET record=json_patch(record,'{\"retryPending\":true}') WHERE id='sib'",
            [],
        )
        .unwrap();
    }
    let settled = store
        .settle_session(
            SettleTask {
                fence: lease.fence(),
                now: 2000,
                status: TaskStatus::Completed,
                error: None,
                events: vec![],
                messages: vec![],
            },
            SessionSettle {
                convert: vec![StagedCandidate {
                    input_id: "fu1".into(),
                    task: SubmitTask {
                        task_id: "fu1-turn".into(),
                        ..task("fu1-turn", "chat")
                    },
                    messages: vec![],
                }],
            },
        )
        .await
        .unwrap();
    assert_eq!(settled.status, TaskStatus::Completed);
    assert_eq!(
        store.get_input("fu1".into()).await.unwrap().unwrap().status,
        AgentInputStatus::Pending,
        "a live sibling turn keeps the conversion refused"
    );
    assert!(store.get_task("fu1-turn").await.unwrap().is_none());
    // The sibling itself is untouched.
    assert_eq!(
        store.get_task("sib").await.unwrap().unwrap().status,
        TaskStatus::Queued
    );

    // A failed turn settles nothing: all pending inputs die with it.
    // The retry sibling is claimable; failing it cancels every pending input.
    let failed_lease = store
        .claim_with_concurrency(
            ClaimInput {
                owner_id: "worker".into(),
                now: 2100,
                ttl_ms: 1_000_000,
                kinds: vec![],
                busy_scopes: vec![],
                max_attempts: 3,
            },
            1,
        )
        .await
        .unwrap()
        .unwrap()
        .lease;
    assert_eq!(failed_lease.task_id, "sib");
    store
        .settle_session(
            SettleTask {
                fence: failed_lease.fence(),
                now: 2200,
                status: TaskStatus::Failed,
                error: Some("boom".into()),
                events: vec![],
                messages: vec![],
            },
            SessionSettle { convert: vec![] },
        )
        .await
        .unwrap();
    assert_eq!(
        store.get_input("fu1".into()).await.unwrap().unwrap().status,
        AgentInputStatus::Cancelled
    );
}

#[tokio::test]
async fn interrupt_and_park_and_resume() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    store.submit(task("queued", "other")).await.unwrap();
    store
        .request_interrupt("queued".into(), 1100)
        .await
        .unwrap();
    assert_eq!(
        store.get_task("queued").await.unwrap().unwrap().status,
        TaskStatus::Interrupted
    );
    store.resume_task("queued", 1200).await.unwrap();
    assert_eq!(
        store.get_task("queued").await.unwrap().unwrap().status,
        TaskStatus::Queued
    );
    store.request_interrupt("run".into(), 1600).await.unwrap();
    let task = store.get_task("run").await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Running);
    assert!(task.interrupt_requested);
    store.park_interrupted(lease.fence(), 1700).await.unwrap();
    let parked = store.get_task("run").await.unwrap().unwrap();
    assert_eq!(parked.status, TaskStatus::Interrupted);
    assert!(!parked.interrupt_requested);
    assert_eq!(store.attempts("run").await.unwrap().len(), 1);
    assert_eq!(
        store.attempts("run").await.unwrap()[0].status,
        "interrupted"
    );
    // A stale fence can no longer park.
    let error = store
        .park_interrupted(lease.fence(), 1800)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::LeaseLost(_)), "{error}");
}

#[tokio::test]
async fn a_remembered_approval_grants_and_revocation_withdraws_it() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    let content = approval_content(Some(json!({
        "standing": {"digest": DIGEST, "label": "write src/x"},
    })));
    park(
        &store,
        &lease,
        human_request("req", "run", content, Some("call-1"), None),
        2000,
    )
    .await;
    store
        .answer_human_request(answer(
            "req",
            "ans",
            json!({"decision":"approve","remember":true}),
        ))
        .await
        .unwrap();
    let chat = store.get_chat("chat").await.unwrap().unwrap();
    let grants = standing_grants(&chat.metadata);
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0]["digest"], DIGEST);
    assert_eq!(grants[0]["toolName"], "write_file");
    assert_eq!(grants[0]["sourceRequestId"], "req");
    assert!(find_active_standing_grant(&chat.metadata, DIGEST).is_some());

    let revoked = store
        .revoke_standing_approval("chat".into(), "grant:req".into(), 4000)
        .await
        .unwrap();
    assert_eq!(
        revoked["revokedAt"],
        ConversationTimestamp::from_epoch_millis(4000)
            .unwrap()
            .wire_text()
            .unwrap()
            .as_ref()
    );
    let chat = store.get_chat("chat").await.unwrap().unwrap();
    assert!(find_active_standing_grant(&chat.metadata, DIGEST).is_none());
    // Revoking twice is the idempotent same answer; unknown ids error.
    store
        .revoke_standing_approval("chat".into(), "grant:req".into(), 5000)
        .await
        .unwrap();
    assert!(matches!(
        store
            .revoke_standing_approval("chat".into(), "grant:nope".into(), 5000)
            .await
            .unwrap_err(),
        StoreError::NotFound(_)
    ));
}

#[tokio::test]
async fn remember_requires_eligibility_and_malformed_grant_data_refuses_writes() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    // No `standing` key: nothing can be remembered, and the whole answer
    // (task transition included) rolls back.
    park(
        &store,
        &lease,
        human_request("req", "run", approval_content(None), Some("call-1"), None),
        2000,
    )
    .await;
    let error = store
        .answer_human_request(answer(
            "req",
            "ans",
            json!({"decision":"approve","remember":true}),
        ))
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::InvalidInput(_)), "{error}");
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::WaitingApproval,
        "the refused answer left the park intact"
    );
    assert_eq!(
        store.get_input("req".into()).await.unwrap().unwrap().status,
        AgentInputStatus::Pending
    );
    // A readable request can still be answered after the refusal.
    store
        .answer_human_request(answer("req", "ans", json!({"decision":"approve"})))
        .await
        .unwrap();

    // Malformed standing approvals are never overwritten.
    store
        .update_chat(
            "chat",
            None,
            Some(json!({ STANDING_APPROVALS_METADATA_KEY: {"version": 1, "grants": [{"grantId": "g"}]} })),
            None,
            4100,
        )
        .await
        .unwrap();
    let error = store
        .revoke_standing_approval("chat".into(), "g".into(), 4200)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unreadable"), "{error}");
    // Matching fails closed: an unreadable list grants nothing.
    assert!(standing_grants(&json!({ STANDING_APPROVALS_METADATA_KEY: "junk" })).is_empty());
}

#[tokio::test]
async fn a_plan_approval_writes_the_grant_and_replay_repairs_it() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    let content = json!({
        "kind": "approval",
        "toolName": "plan",
        "argsSha256": PLAN,
        "prompt": "the plan",
        "requestedMode": "act",
    });
    park(
        &store,
        &lease,
        human_request("req", "run", content.clone(), Some("call-1"), None),
        2000,
    )
    .await;
    let mut input = answer("req", "ans", json!({"decision":"approve"}));
    input.plan_tool_names = vec!["plan".into()];
    input.granted_mode = Some("act".into());
    store.answer_human_request(input.clone()).await.unwrap();
    let chat = store.get_chat("chat").await.unwrap().unwrap();
    let state = read_execution_state(&chat.metadata).unwrap();
    assert_eq!(state["mode"], "act");
    assert_eq!(state["grantedPlan"]["planDigest"], PLAN);
    assert_eq!(state["grantedPlan"]["turnId"], "run");

    // The half-committed crash state: the marker committed, the grant did
    // not. The exact replay repairs it.
    store
        .update_chat("chat", None, Some(json!({})), None, 3500)
        .await
        .unwrap();
    let replay = store.answer_human_request(input).await.unwrap();
    assert!(replay.receipt.duplicate);
    let chat = store.get_chat("chat").await.unwrap().unwrap();
    let state = read_execution_state(&chat.metadata).unwrap();
    assert_eq!(state["grantedPlan"]["planDigest"], PLAN);

    // Plan mode is never grantable: the answer fails whole.
    let lease2 = running_turn(&store, "run2").await;
    park(
        &store,
        &lease2,
        human_request("req2", "run2", content, Some("call-2"), None),
        2000,
    )
    .await;
    let mut poisoned = answer("req2", "ans2", json!({"decision":"approve"}));
    poisoned.plan_tool_names = vec!["plan".into()];
    poisoned.granted_mode = Some("plan".into());
    let error = store.answer_human_request(poisoned).await.unwrap_err();
    assert!(matches!(error, StoreError::InvalidInput(_)), "{error}");
    assert_eq!(
        store
            .get_input("req2".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Pending
    );
    // A policy veto resolved by the host is recorded AS a denial.
    let mut vetoed = answer("req2", "ans3", json!({"decision":"approve"}));
    vetoed.plan_tool_names = vec!["plan".into()];
    vetoed.policy_denial = Some("plan digest unknown".into());
    let outcome = store.answer_human_request(vetoed).await.unwrap();
    let tool = store
        .messages("chat", false)
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.id == outcome.receipt.message_id)
        .unwrap();
    let envelope: Value = serde_json::from_str(tool.content.as_str().unwrap()).unwrap();
    assert_eq!(envelope["data"]["errorCode"], "approval_denied");
    assert!(
        envelope["data"]["errorMessage"]
            .as_str()
            .unwrap()
            .starts_with("plan_not_authorized:")
    );
}

#[tokio::test]
async fn projection_replays_commit_nothing_twice() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    let create = CreateAgentInput {
        input_id: "steer-1".into(),
        session_id: "chat".into(),
        kind: AgentInputKind::Steer,
        content: json!({"text": "mid-run steer"}),
        content_hash: "hash-s1".into(),
        target_task_id: Some("run".into()),
        on_settled: Some(AgentInputSettlePolicy::FollowUp),
        attempt_id: None,
        tool_call_id: None,
        expires_at: None,
    };
    let tool_record = MessageInput {
        id: Some("tool-1".into()),
        chat_id: "chat".into(),
        run_id: Some("run".into()),
        role: "tool".into(),
        content: json!("result"),
        tool_call_id: Some("call-1".into()),
        ..MessageInput::default()
    };
    let created = store
        .append_projected_inputs(AppendProjectedInputs {
            applies: vec![],
            fence: lease.fence(),
            events: vec![event(1, "evt-tool-result")],
            messages: vec![tool_record.clone()],
            creates: vec![create.clone()],
            task_id: "run".into(),
            now: 2100,
        })
        .await
        .unwrap();
    assert_eq!(created, vec!["steer-1".to_owned()]);
    let record = store.get_input("steer-1".into()).await.unwrap().unwrap();
    assert_eq!(record.status, AgentInputStatus::Applied);
    assert_eq!(record.applied_task_id.as_deref(), Some("run"));

    // Exact replay: no new input, the message id collision is what the
    // caller must avoid by reusing ids.
    let replay = store
        .append_projected_inputs(AppendProjectedInputs {
            applies: vec![],
            fence: lease.fence(),
            events: vec![],
            messages: vec![],
            creates: vec![create],
            task_id: "run".into(),
            now: 2150,
        })
        .await
        .unwrap();
    assert!(replay.is_empty());

    // A conflicting replay of the same id is refused.
    let error = store
        .append_projected_inputs(AppendProjectedInputs {
            applies: vec![],
            fence: lease.fence(),
            events: vec![],
            messages: vec![],
            creates: vec![CreateAgentInput {
                content_hash: "different".into(),
                ..create_warmup()
            }],
            task_id: "run".into(),
            now: 2200,
        })
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Duplicate(_)), "{error}");
}

fn create_warmup() -> CreateAgentInput {
    CreateAgentInput {
        input_id: "steer-1".into(),
        session_id: "chat".into(),
        kind: AgentInputKind::Steer,
        content: json!({"text": "mid-run steer"}),
        content_hash: "hash-s1".into(),
        target_task_id: Some("run".into()),
        on_settled: Some(AgentInputSettlePolicy::FollowUp),
        attempt_id: None,
        tool_call_id: None,
        expires_at: None,
    }
}

#[tokio::test]
async fn a_park_and_its_request_survive_reopening_the_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.db");
    {
        let store = SqliteStore::open(&path, 8).unwrap();
        let lease = running_turn(&store, "run").await;
        park(
            &store,
            &lease,
            human_request("req", "run", approval_content(None), Some("call-1"), None),
            2000,
        )
        .await;
    }
    // Process death between the park commit and any answer: the reopened
    // store still shows a parked turn with its pending request, and the
    // answer path completes the workflow.
    let store = SqliteStore::open(&path, 8).unwrap();
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::WaitingApproval
    );
    let record = store.get_input("req".into()).await.unwrap().unwrap();
    assert_eq!(record.status, AgentInputStatus::Pending);
    assert!(record.applied_message_id.is_none());
    store
        .answer_human_request(answer("req", "ans", json!({"decision":"approve"})))
        .await
        .unwrap();
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Queued
    );
}
