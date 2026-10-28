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
    running_turn_payload(store, turn, json!({"chatId":"chat"})).await
}

/// A claimed turn whose payload is whatever a submission made at some earlier
/// moment carried — the fixture for "this turn was staged before the session
/// changed", which the settlement must never template off.
async fn running_turn_payload(store: &SqliteStore, turn: &str, payload: Value) -> Lease {
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
        .submit_turn(
            SubmitTask {
                payload,
                ..task(turn, "chat")
            },
            vec![message, assistant],
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

/// What the host hands a settlement as the authoritative staging config:
/// the payload keys a turn created NOW carries, for this fixture's `"test"`
/// task kind. `None` (the other tests) means "promote nothing the snapshot
/// missed" — the fail-closed half of the contract.
fn staging(config: Value) -> TurnStaging {
    TurnStaging {
        kind: "test".into(),
        config,
    }
}

fn follow_up(id: &str, content: Value) -> CreateAgentInput {
    CreateAgentInput {
        input_id: id.into(),
        session_id: "chat".into(),
        kind: AgentInputKind::FollowUp,
        content,
        content_hash: format!("hash-{id}"),
        target_task_id: None,
        on_settled: None,
        attempt_id: None,
        tool_call_id: None,
        expires_at: None,
    }
}

/// The admission write the service issues: the input record and the turn it
/// becomes, in the shapes the staging contract defines (`turn:<inputId>`,
/// `msg_q:`/`msg_a:`, `steer:<inputId>`).
fn admission(
    id: &str,
    kind: AgentInputKind,
    on_settled: Option<AgentInputSettlePolicy>,
    target_the_live_turn: bool,
) -> QueueOrStage {
    let user_message_id = if kind == AgentInputKind::Steer {
        format!("steer:{id}")
    } else {
        format!("msg_q:{id}")
    };
    let assistant_message_id = format!("msg_a:{id}");
    let task_id = format!("turn:{id}");
    let parent = user_message_id.clone();
    QueueOrStage {
        session_id: "chat".into(),
        input: CreateAgentInput {
            input_id: id.into(),
            session_id: "chat".into(),
            kind,
            content: json!({"content":"and summarize"}),
            content_hash: format!("hash-{id}"),
            target_task_id: None,
            on_settled,
            attempt_id: None,
            tool_call_id: None,
            expires_at: None,
        },
        target_the_live_turn,
        candidate: Some(StagedCandidate {
            input_id: id.into(),
            task: SubmitTask {
                task_id: task_id.clone(),
                kind: "test".into(),
                scope_id: "chat".into(),
                payload: json!({"chatId":"chat","model":"new-model",
                                "userMessageId": user_message_id,
                                "assistantMessageId": assistant_message_id}),
                priority: 0,
                now: 2100,
                available_at: None,
                depends_on: vec![],
                parent_task_id: None,
                exclusive_scope: true,
            },
            messages: vec![
                MessageInput {
                    id: Some(user_message_id),
                    chat_id: "chat".into(),
                    run_id: None,
                    role: "user".into(),
                    content: json!("and summarize"),
                    parent_message_id: None,
                    ..Default::default()
                },
                MessageInput {
                    id: Some(assistant_message_id),
                    chat_id: "chat".into(),
                    run_id: Some(task_id),
                    role: "assistant".into(),
                    content: Value::String(String::new()),
                    parent_message_id: Some(parent),
                    metadata: json!({"placeholder": true}),
                    ..Default::default()
                },
            ],
        }),
        now: 2100,
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
async fn a_lost_answer_record_is_missing_evidence_never_a_fresh_replay() {
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
    // A receipt naming a tool record that never landed. The record and the
    // receipt commit in one transaction, so this state is corrupt evidence,
    // not a retryable half commit: the answer must refuse, never rewrite
    // itself as a fresh (duplicate:false) commit.
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
    let error = store
        .answer_human_request(answer("req", "ans", json!({"answer":"left"})))
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Duplicate(_)), "{error}");
    assert!(error.to_string().contains("answer_conflict"), "{error}");
    assert!(
        !store
            .messages("chat", false)
            .await
            .unwrap()
            .iter()
            .any(|message| message.id == "missing-msg"),
        "the missing record was not rewritten"
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
                content: json!({"text":"stop"}),
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
                content: json!({"text":"also"}),
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
                staging: Some(staging(json!({"chatId":"chat"}))),
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

/// Eligibility is the settlement's own decision: an input the host's
/// snapshot could not have named — it arrived after the service read the
/// queue — is promoted by THIS commit, staged from the settling turn,
/// instead of staying Pending with no next task and an idle worker. Unusable
/// content and `cancel` steers keep their existing treatment.
#[tokio::test]
async fn a_settlement_promotes_an_input_the_snapshot_missed() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    let queued = |id: &str, kind: AgentInputKind, content: Value, on_settled| CreateAgentInput {
        input_id: id.into(),
        session_id: "chat".into(),
        kind,
        content,
        content_hash: format!("hash-{id}"),
        target_task_id: (kind == AgentInputKind::Steer).then(|| "run".into()),
        on_settled,
        attempt_id: None,
        tool_call_id: None,
        expires_at: None,
    };
    store
        .create_input(
            queued(
                "late",
                AgentInputKind::FollowUp,
                json!({"content":"and summarize"}),
                None,
            ),
            1700,
        )
        .await
        .unwrap();
    store
        .create_input(
            queued(
                "poison",
                AgentInputKind::FollowUp,
                json!({"content":""}),
                None,
            ),
            1701,
        )
        .await
        .unwrap();
    store
        .create_input(
            queued(
                "s-cancel",
                AgentInputKind::Steer,
                json!({"content":"stop"}),
                Some(AgentInputSettlePolicy::Cancel),
            ),
            1702,
        )
        .await
        .unwrap();

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
                convert: vec![],
                staging: Some(staging(json!({"chatId":"chat"}))),
            },
        )
        .await
        .unwrap();
    assert_eq!(settled.status, TaskStatus::Completed);

    let late = store.get_input("late".into()).await.unwrap().unwrap();
    assert_eq!(
        late.status,
        AgentInputStatus::Converted,
        "the settlement promotes what its snapshot missed"
    );
    assert_eq!(late.applied_task_id.as_deref(), Some("turn:late"));
    let promoted = store.get_task("turn:late").await.unwrap().unwrap();
    assert_eq!(promoted.kind, "test");
    assert_eq!(promoted.scope_id, "chat");
    assert_eq!(
        promoted.payload,
        json!({"chatId":"chat","userMessageId":"msg_q:late","assistantMessageId":"msg_a:late"}),
        "the promoted turn carries the session's staging contract, not the \
         settling turn's payload"
    );
    let messages = store.messages("chat", false).await.unwrap();
    assert!(
        messages.iter().any(|message| message.id == "msg_q:late"
            && message.role == "user"
            && message.content == json!("and summarize")),
        "the user record commits with the task"
    );
    assert!(
        messages.iter().any(|message| message.id == "msg_a:late"
            && message.role == "assistant"
            && message.run_id.as_deref() == Some("turn:late")),
        "and so does the assistant placeholder"
    );
    assert_eq!(
        store
            .get_input("poison".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Pending,
        "unusable content is never staged, and never aborts the settle"
    );
    assert!(store.get_task("turn:poison").await.unwrap().is_none());
    assert_eq!(
        store
            .get_input("s-cancel".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Cancelled,
        "a cancel-policy steer is cancelled, never converted"
    );
}

/// The settling turn is NOT the staging template. A model change that lands
/// while a turn runs must reach the turn ITS settlement promotes: the
/// promotion stamps the config the host read from the descriptor at settle
/// time, so the next provider request carries the new model instead of the
/// model the settling turn was created with.
#[tokio::test]
async fn a_promoted_turn_carries_the_current_config_not_the_settling_turns() {
    let store = store();
    // The settling turn was staged while the session still used `old-model`.
    let lease =
        running_turn_payload(&store, "run", json!({"chatId":"chat","model":"old-model"})).await;
    store
        .create_input(follow_up("late", json!({"content":"and summarize"})), 1700)
        .await
        .unwrap();

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
                convert: vec![],
                staging: Some(staging(
                    json!({"chatId":"chat","model":"new-model","providerId":"p2"}),
                )),
            },
        )
        .await
        .unwrap();
    assert_eq!(settled.status, TaskStatus::Completed);
    let promoted = store.get_task("turn:late").await.unwrap().unwrap();
    assert_eq!(
        promoted.payload,
        json!({"chatId":"chat","model":"new-model","providerId":"p2",
               "userMessageId":"msg_q:late","assistantMessageId":"msg_a:late"}),
        "the promoted turn is what a submission made AFTER the model change stages"
    );
    assert_eq!(
        promoted.kind, "test",
        "the kind comes from the contract too"
    );
    let messages = store.messages("chat", false).await.unwrap();
    assert!(
        messages
            .iter()
            .any(|message| message.id == "msg_q:late" && message.content == json!("and summarize")),
        "the user record commits with the current-config task"
    );
}

/// No current config, no promotion. Stamping a new turn from another turn's
/// payload is what handed a session a model it had already replaced, so
/// without the host's snapshot the settlement leaves the input Pending and
/// visible for an explicit flush instead of guessing.
#[tokio::test]
async fn a_missed_input_stays_pending_without_the_current_config() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    store
        .create_input(follow_up("late", json!({"content":"and summarize"})), 1700)
        .await
        .unwrap();
    store
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
                convert: vec![],
                staging: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .get_input("late".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Pending,
        "un-stammable input stays visible rather than running a stale config"
    );
    assert!(store.get_task("turn:late").await.unwrap().is_none());
}

/// The follow-up that used to strand: the settlement had already committed
/// (re-reading a queue that did not yet contain the input) when the input's
/// admission ran. Because admission IS the busy check, the same write that
/// inserts it also stages the next turn — no Pending-forever residue, and no
/// second settlement needed to wake the worker.
#[tokio::test]
async fn an_input_admitted_after_the_settlement_committed_still_becomes_a_turn() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    // The settlement commits with an empty queue: nothing to promote, and no
    // later settlement exists — the old code's dead end.
    store
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
                convert: vec![],
                staging: None,
            },
        )
        .await
        .unwrap();

    let admission = store
        .queue_or_stage_turn(admission("late", AgentInputKind::FollowUp, None, false))
        .await
        .unwrap();
    let QueueAdmission::Staged(task) = admission else {
        panic!("a free scope admits the input as the next turn");
    };
    assert_eq!(task.task_id, "turn:late");
    assert_eq!(
        task.status,
        TaskStatus::Queued,
        "the worker has something to claim the moment admission commits"
    );
    let input = store.get_input("late".into()).await.unwrap().unwrap();
    assert_eq!(input.status, AgentInputStatus::Converted);
    assert_eq!(input.applied_task_id.as_deref(), Some("turn:late"));
    let messages = store.messages("chat", false).await.unwrap();
    assert!(
        messages
            .iter()
            .any(|message| message.id == "msg_q:late" && message.content == json!("and summarize")),
        "the task, its user record and its receipt are one commit"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.id == "msg_a:late"
                && message.run_id.as_deref() == Some("turn:late")),
        "and so is the assistant placeholder"
    );
}

/// The other half of the same write: while a turn holds the scope, admission
/// only stores the input, and the live turn's settlement — which re-reads the
/// pending set against its own commit — owns the promotion.
#[tokio::test]
async fn admission_queues_behind_a_live_turn_for_its_settlement_to_promote() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    let admission = store
        .queue_or_stage_turn(admission("fu1", AgentInputKind::FollowUp, None, false))
        .await
        .unwrap();
    assert!(
        matches!(admission, QueueAdmission::Queued(_)),
        "a live scope is never staged over"
    );
    assert_eq!(
        store.get_input("fu1".into()).await.unwrap().unwrap().status,
        AgentInputStatus::Pending
    );
    assert!(store.get_task("turn:fu1").await.unwrap().is_none());

    store
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
                convert: vec![],
                staging: Some(staging(json!({"chatId":"chat"}))),
            },
        )
        .await
        .unwrap();
    let input = store.get_input("fu1".into()).await.unwrap().unwrap();
    assert_eq!(
        input.status,
        AgentInputStatus::Converted,
        "the input admitted behind a live turn is promoted by its settlement"
    );
    assert_eq!(input.applied_task_id.as_deref(), Some("turn:fu1"));
}

/// Admission never invents work the human did not ask for: a `cancel`-policy
/// steer and unusable content are stored Pending and visible, never started
/// as a turn, and never an error that would refuse the caller's send.
#[tokio::test]
async fn admission_never_starts_a_cancel_policy_steer_or_unusable_content_as_a_turn() {
    let store = store();
    if store.get_chat("chat").await.unwrap().is_none() {
        store
            .create_chat("chat", None, json!({}), 1000)
            .await
            .unwrap();
    }
    let steer = store
        .queue_or_stage_turn(admission(
            "s-cancel",
            AgentInputKind::Steer,
            Some(AgentInputSettlePolicy::Cancel),
            true,
        ))
        .await
        .unwrap();
    assert!(
        matches!(steer, QueueAdmission::Queued(_)),
        "a steer the human marked `cancel` is dropped, never run"
    );
    assert!(store.get_task("turn:s-cancel").await.unwrap().is_none());

    let mut poison = admission("poison", AgentInputKind::FollowUp, None, false);
    poison.input.content = json!({"content":""});
    poison.candidate = None;
    let poison = store.queue_or_stage_turn(poison).await.unwrap();
    assert!(
        matches!(poison, QueueAdmission::Queued(_)),
        "a poison record stays Pending and visible"
    );
    assert_eq!(
        store
            .get_input("poison".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Pending
    );
    assert!(store.get_task("turn:poison").await.unwrap().is_none());
}

/// `content.onSettled` is user data, not policy: only the top-level record
/// field the service writes governs what a settle does with a steer.
#[tokio::test]
async fn only_the_top_level_settle_policy_governs_content_markers_are_inert() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    for (input_id, content, top) in [
        (
            "top-cancel",
            json!({"text":"x","onSettled":"follow_up"}),
            Some(AgentInputSettlePolicy::Cancel),
        ),
        (
            "content-only",
            json!({"text":"y","onSettled":"cancel"}),
            None,
        ),
    ] {
        store
            .create_input(
                CreateAgentInput {
                    input_id: input_id.into(),
                    session_id: "chat".into(),
                    kind: AgentInputKind::Steer,
                    content,
                    content_hash: format!("hash-{input_id}"),
                    target_task_id: Some("run".into()),
                    on_settled: top,
                    attempt_id: None,
                    tool_call_id: None,
                    expires_at: None,
                },
                1602,
            )
            .await
            .unwrap();
    }

    store
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
                convert: vec![],
                staging: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        store
            .get_input("top-cancel".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Cancelled,
        "the top-level field cancels even when content claims otherwise"
    );
    assert_eq!(
        store
            .get_input("content-only".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Pending,
        "a content marker alone never cancels anything"
    );
}

/// A refused conversion writes NOTHING: no task without its messages, no
/// receipt without its task, and — inside a settle — no terminal state
/// either. The whole promotion is one transaction or none.
#[tokio::test]
async fn a_refused_conversion_writes_nothing() {
    let store = store();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store
        .create_input(
            CreateAgentInput {
                input_id: "bad".into(),
                session_id: "chat".into(),
                kind: AgentInputKind::FollowUp,
                content: json!({"content": ""}),
                content_hash: "hash-bad".into(),
                target_task_id: None,
                on_settled: None,
                attempt_id: None,
                tool_call_id: None,
                expires_at: None,
            },
            1600,
        )
        .await
        .unwrap();

    // (a) The explicit flush path: unusable user content is refused and
    // the input keeps waiting, unclaimed.
    let error = store
        .stage_queued_turn(StageQueuedTurn {
            session_id: "chat".into(),
            task_kind: "test".into(),
            exclude_task_id: None,
            input_id: "bad".into(),
            expected_kind: AgentInputKind::FollowUp,
            task: task("bad-turn", "chat"),
            messages: vec![MessageInput {
                id: Some("bad-user".into()),
                chat_id: "chat".into(),
                run_id: Some("bad-turn".into()),
                role: "user".into(),
                content: json!(""),
                ..MessageInput::default()
            }],
            now: 2100,
        })
        .await;
    assert!(
        matches!(error, Err(StoreError::InvalidInput(_))),
        "unusable content must be refused: {error:?}"
    );
    assert!(store.get_task("bad-turn").await.unwrap().is_none());
    assert_eq!(
        store.get_input("bad".into()).await.unwrap().unwrap().status,
        AgentInputStatus::Pending,
        "the CAS rolled back with the refusal"
    );
    assert!(
        store
            .messages("chat", false)
            .await
            .unwrap()
            .iter()
            .all(|message| message.id != "bad-user")
    );
}

/// The settle half of the zero-residue guarantee: a poisoned candidate
/// rejects the ENTIRE commit — the turn stays Running with its lease
/// intact, ready to settle cleanly on the next attempt.
#[tokio::test]
async fn a_refused_settle_conversion_leaves_the_turn_running() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    store
        .create_input(
            CreateAgentInput {
                input_id: "bad2".into(),
                session_id: "chat".into(),
                kind: AgentInputKind::FollowUp,
                content: json!({"content": ""}),
                content_hash: "hash-bad2".into(),
                target_task_id: None,
                on_settled: None,
                attempt_id: None,
                tool_call_id: None,
                expires_at: None,
            },
            1600,
        )
        .await
        .unwrap();
    let poisoned = StagedCandidate {
        input_id: "bad2".into(),
        task: task("bad2-turn", "chat"),
        messages: vec![MessageInput {
            id: Some("bad2-user".into()),
            chat_id: "chat".into(),
            run_id: Some("bad2-turn".into()),
            role: "user".into(),
            content: json!(""),
            ..MessageInput::default()
        }],
    };
    let error = store
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
                convert: vec![poisoned],
                staging: None,
            },
        )
        .await;
    assert!(
        matches!(error, Err(StoreError::InvalidInput(_))),
        "the poisoned candidate must reject the whole settle: {error:?}"
    );
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Running,
        "nothing settled — no half-promoted terminal turn"
    );
    assert_eq!(
        store
            .get_input("bad2".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Pending
    );
    assert!(store.get_task("bad2-turn").await.unwrap().is_none());
    // The lease fence is untouched: the retry settles cleanly once the
    // candidate list is right.
    store
        .settle_session(
            SettleTask {
                fence: lease.fence(),
                now: 2100,
                status: TaskStatus::Completed,
                error: None,
                events: vec![],
                messages: vec![],
            },
            SessionSettle {
                convert: vec![],
                staging: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
}

#[tokio::test]
async fn conversion_refuses_a_busy_scope_and_failure_keeps_the_queue_pending() {
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
                staging: None,
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

    // A failed turn promotes nothing (ADR 0016 §4): queued inputs stay
    // pending and visible, and even a passed conversion candidate must not
    // be taken — a user who cancelled (or a provider that failed) did not
    // ask for the queued turn to start on its own.
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
            SessionSettle {
                convert: vec![StagedCandidate {
                    input_id: "fu1".into(),
                    task: SubmitTask {
                        task_id: "fu1-late".into(),
                        ..task("fu1-late", "chat")
                    },
                    messages: vec![],
                }],
                staging: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.get_input("fu1".into()).await.unwrap().unwrap().status,
        AgentInputStatus::Pending,
        "a failed turn leaves the queue pending for an explicit flush"
    );
    assert!(store.get_task("fu1-late").await.unwrap().is_none());

    // The explicit release is what advances the queue after a failure.
    store
        .stage_queued_turn(StageQueuedTurn {
            session_id: "chat".into(),
            task_kind: "test".into(),
            exclude_task_id: None,
            input_id: "fu1".into(),
            expected_kind: AgentInputKind::FollowUp,
            task: SubmitTask {
                task_id: "fu1-flushed".into(),
                ..task("fu1-flushed", "chat")
            },
            messages: vec![MessageInput {
                id: Some("fu1-flushed-message".into()),
                chat_id: "chat".into(),
                run_id: Some("fu1-flushed".into()),
                role: "user".into(),
                content: json!("fu1"),
                ..MessageInput::default()
            }],
            now: 2300,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store.get_input("fu1".into()).await.unwrap().unwrap().status,
        AgentInputStatus::Converted
    );
}

#[tokio::test]
async fn an_invalid_conversion_writes_neither_task_nor_receipt() {
    // Reproduction: invalid queued user content returned an error but left
    // a Queued task, a Pending input and no messages. The conversion is
    // CAS-first and transactional: a refused candidate writes NOTHING.
    let store = store();
    store
        .create_input(
            CreateAgentInput {
                input_id: "fu-bad".into(),
                session_id: "chat".into(),
                kind: AgentInputKind::FollowUp,
                content: json!({"text": "fine"}),
                content_hash: "hash-bad".into(),
                target_task_id: None,
                on_settled: None,
                attempt_id: None,
                tool_call_id: None,
                expires_at: None,
            },
            1600,
        )
        .await
        .unwrap();
    // The chat must exist for the conversion's scope checks.
    if store.get_chat("chat").await.unwrap().is_none() {
        store
            .create_chat("chat", None, json!({}), 1000)
            .await
            .unwrap();
    }
    let error = store
        .stage_queued_turn(StageQueuedTurn {
            session_id: "chat".into(),
            task_kind: "test".into(),
            exclude_task_id: None,
            input_id: "fu-bad".into(),
            expected_kind: AgentInputKind::FollowUp,
            task: SubmitTask {
                task_id: "fu-bad-turn".into(),
                ..task("fu-bad-turn", "chat")
            },
            messages: vec![MessageInput {
                id: Some("fu-bad-message".into()),
                chat_id: "chat".into(),
                run_id: Some("fu-bad-turn".into()),
                role: "user".into(),
                content: json!(42), // not a string, not content blocks
                ..MessageInput::default()
            }],
            now: 1700,
        })
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::InvalidInput(_)), "{error}");
    assert!(
        store.get_task("fu-bad-turn").await.unwrap().is_none(),
        "no half-committed queued task survives the refusal"
    );
    assert_eq!(
        store
            .get_input("fu-bad".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Pending,
        "the input keeps its Pending evidence — no CAS was burned"
    );
    assert!(
        !store
            .messages("chat", false)
            .await
            .unwrap()
            .iter()
            .any(|message| message.id == "fu-bad-message")
    );

    // The same input converts cleanly once the content is usable.
    store
        .stage_queued_turn(StageQueuedTurn {
            session_id: "chat".into(),
            task_kind: "test".into(),
            exclude_task_id: None,
            input_id: "fu-bad".into(),
            expected_kind: AgentInputKind::FollowUp,
            task: SubmitTask {
                task_id: "fu-bad-turn".into(),
                ..task("fu-bad-turn", "chat")
            },
            messages: vec![MessageInput {
                id: Some("fu-bad-message".into()),
                chat_id: "chat".into(),
                run_id: Some("fu-bad-turn".into()),
                role: "user".into(),
                content: json!("usable"),
                ..MessageInput::default()
            }],
            now: 1800,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .get_input("fu-bad".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Converted
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
async fn a_plan_approval_writes_the_grant_and_replay_never_restores_it() {
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

    // The grant commits with the marker in ONE transaction, so an answered
    // request is never owed a grant. Wipe the execution state (a later
    // revocation) and replay the exact answer: it is observation only —
    // duplicate:true, and it must NOT resurrect the revoked grant.
    store
        .update_chat("chat", None, Some(json!({})), None, 3500)
        .await
        .unwrap();
    let replay = store.answer_human_request(input).await.unwrap();
    assert!(replay.receipt.duplicate);
    let chat = store.get_chat("chat").await.unwrap().unwrap();
    assert!(
        read_execution_state(&chat.metadata).is_none(),
        "the replay did not re-apply the grant: {:?}",
        chat.metadata
    );

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
async fn a_replayed_answer_never_undoes_a_later_mode_change() {
    // Handoff reproduction: approve a plan to auto_edit, explicitly set the
    // session back to plan, replay the old answer. The replay must not
    // silently re-grant auto_edit.
    let store = store();
    let lease = running_turn(&store, "run").await;
    let content = json!({
        "kind": "approval",
        "toolName": "plan",
        "argsSha256": PLAN,
        "prompt": "the plan",
    });
    park(
        &store,
        &lease,
        human_request("req", "run", content, Some("call-1"), None),
        2000,
    )
    .await;
    let mut input = answer("req", "ans", json!({"decision":"approve"}));
    input.plan_tool_names = vec!["plan".into()];
    input.granted_mode = Some("auto_edit".into());
    store.answer_human_request(input.clone()).await.unwrap();
    assert_eq!(
        read_execution_state(&store.get_chat("chat").await.unwrap().unwrap().metadata).unwrap()["mode"],
        "auto_edit"
    );

    // An explicit mode switch by the host/user: plan, grant dropped.
    store
        .update_chat(
            "chat",
            None,
            Some(json!({ EXECUTION_METADATA_KEY: {
                "version": 1,
                "mode": "plan",
                "updatedAt": ConversationTimestamp::from_epoch_millis(3500)
                    .unwrap()
                    .wire_text()
                    .unwrap()
                    .as_ref(),
            } })),
            None,
            3500,
        )
        .await
        .unwrap();

    let replay = store.answer_human_request(input).await.unwrap();
    assert!(
        replay.receipt.duplicate,
        "the replay is observed, not re-applied"
    );
    let state =
        read_execution_state(&store.get_chat("chat").await.unwrap().unwrap().metadata).unwrap();
    assert_eq!(
        state["mode"], "plan",
        "the later restriction stands: {state}"
    );
    assert!(
        state.get("grantedPlan").is_none_or(Value::is_null),
        "no grant resurrected: {state}"
    );
}

#[tokio::test]
async fn a_replayed_remembered_answer_never_resurrects_a_revoked_grant() {
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
    let input = answer("req", "ans", json!({"decision":"approve","remember":true}));
    store.answer_human_request(input.clone()).await.unwrap();
    assert!(
        find_active_standing_grant(
            &store.get_chat("chat").await.unwrap().unwrap().metadata,
            DIGEST
        )
        .is_some()
    );
    store
        .revoke_standing_approval("chat".into(), "grant:req".into(), 4000)
        .await
        .unwrap();

    let replay = store.answer_human_request(input).await.unwrap();
    assert!(replay.receipt.duplicate);
    assert!(
        find_active_standing_grant(
            &store.get_chat("chat").await.unwrap().unwrap().metadata,
            DIGEST
        )
        .is_none(),
        "the replay did not re-write the revoked standing grant"
    );
}

#[tokio::test]
async fn replaying_the_first_plan_approval_keeps_the_newer_grant() {
    let store = store();
    let plan_request = |input_id: &str, turn: &str, digest: &str| {
        human_request(
            input_id,
            turn,
            json!({
                "kind": "approval",
                "toolName": "plan",
                "argsSha256": digest,
                "prompt": "the plan",
            }),
            Some("call-1"),
            None,
        )
    };
    let lease = running_turn(&store, "run").await;
    park(&store, &lease, plan_request("req-a", "run", PLAN), 2000).await;
    let mut first = answer("req-a", "ans-a", json!({"decision":"approve"}));
    first.plan_tool_names = vec!["plan".into()];
    first.granted_mode = Some("act".into());
    store.answer_human_request(first.clone()).await.unwrap();

    let lease2 = running_turn(&store, "run2").await;
    park(&store, &lease2, plan_request("req-b", "run2", DIGEST), 2100).await;
    let mut second = answer("req-b", "ans-b", json!({"decision":"approve"}));
    second.plan_tool_names = vec!["plan".into()];
    second.granted_mode = Some("auto_edit".into());
    store.answer_human_request(second).await.unwrap();
    let state =
        read_execution_state(&store.get_chat("chat").await.unwrap().unwrap().metadata).unwrap();
    assert_eq!(state["grantedPlan"]["planDigest"], DIGEST);

    // Replaying the OLDER approval must leave the newer grant alone.
    let replay = store.answer_human_request(first).await.unwrap();
    assert!(replay.receipt.duplicate);
    let state =
        read_execution_state(&store.get_chat("chat").await.unwrap().unwrap().metadata).unwrap();
    assert_eq!(
        state["grantedPlan"]["planDigest"], DIGEST,
        "the newer grant stands: {state}"
    );
    assert_eq!(state["mode"], "auto_edit");
}

#[tokio::test]
async fn racing_identical_answers_commit_once() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    park(
        &store,
        &lease,
        human_request("req", "run", approval_content(None), Some("call-1"), None),
        2000,
    )
    .await;
    let answer_a = answer("req", "ans", json!({"decision":"approve"}));
    let answer_b = answer_a.clone();
    let (a, b) = tokio::join!(
        store.answer_human_request(answer_a),
        store.answer_human_request(answer_b)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let fresh = [a.receipt.duplicate, b.receipt.duplicate]
        .iter()
        .filter(|d| !**d)
        .count();
    assert_eq!(fresh, 1, "exactly one commit, one observation");
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Queued
    );
}

#[tokio::test]
async fn a_replayed_answer_stays_duplicate_after_reopening_the_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reopen.db");
    let input = answer("req", "ans", json!({"answer":"left"}));
    {
        let store = SqliteStore::open(&path, 8).unwrap();
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
        let outcome = store.answer_human_request(input.clone()).await.unwrap();
        assert!(!outcome.receipt.duplicate);
    }
    // Process death and reopen: the replay is still an observation, and it
    // must not append a second answer record.
    let store = SqliteStore::open(&path, 8).unwrap();
    let replay = store.answer_human_request(input).await.unwrap();
    assert!(replay.receipt.duplicate);
    let answers = store
        .messages("chat", false)
        .await
        .unwrap()
        .into_iter()
        .filter(|message| {
            message.metadata.get("humanAnswer").and_then(Value::as_bool) == Some(true)
        })
        .count();
    assert_eq!(answers, 1, "exactly one durable answer record");
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
    assert_eq!(created.created, vec!["steer-1".to_owned()]);
    assert!(created.applied.is_empty());
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
    assert!(replay.created.is_empty());
    assert!(replay.applied.is_empty());

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

fn steer_cas_input() -> CreateAgentInput {
    CreateAgentInput {
        input_id: "steer-cas".into(),
        session_id: "chat".into(),
        kind: AgentInputKind::Steer,
        content: json!({"content": "place this"}),
        content_hash: "hash-cas".into(),
        target_task_id: Some("run".into()),
        on_settled: None,
        attempt_id: None,
        tool_call_id: None,
        expires_at: None,
    }
}

fn steer_placement() -> AppliedSteer {
    AppliedSteer {
        input_id: "steer-cas".into(),
        message: MessageInput {
            id: Some("steer:steer-cas".into()),
            chat_id: "chat".into(),
            run_id: Some("run".into()),
            role: "user".into(),
            content: json!("place this"),
            metadata: json!({"steering": true, "inputId": "steer-cas"}),
            ..MessageInput::default()
        },
    }
}

fn projection_for(lease: &Lease) -> AppendProjectedInputs {
    AppendProjectedInputs {
        fence: lease.fence(),
        events: vec![],
        messages: vec![],
        creates: vec![],
        applies: vec![],
        task_id: "run".into(),
        now: 2100,
    }
}

#[tokio::test]
async fn a_steer_placement_writes_message_and_receipt_together() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    store.create_input(steer_cas_input(), 2000).await.unwrap();
    let commit = store
        .append_projected_inputs(AppendProjectedInputs {
            applies: vec![steer_placement()],
            ..projection_for(&lease)
        })
        .await
        .unwrap();
    assert_eq!(commit.applied, vec!["steer-cas".to_owned()]);
    let record = store.get_input("steer-cas".into()).await.unwrap().unwrap();
    assert_eq!(record.status, AgentInputStatus::Applied);
    assert_eq!(
        record.applied_message_id.as_deref(),
        Some("steer:steer-cas")
    );
    assert!(
        store
            .messages("chat", false)
            .await
            .unwrap()
            .iter()
            .any(|message| message.id == "steer:steer-cas")
    );

    // An identical replay of the same receipt is idempotent: applied stays
    // reported, and the record is not written twice.
    let replay = store
        .append_projected_inputs(AppendProjectedInputs {
            applies: vec![steer_placement()],
            ..projection_for(&lease)
        })
        .await
        .unwrap();
    assert_eq!(replay.applied, vec!["steer-cas".to_owned()]);
    assert_eq!(
        store
            .messages("chat", false)
            .await
            .unwrap()
            .iter()
            .filter(|message| message.id == "steer:steer-cas")
            .count(),
        1
    );
}

#[tokio::test]
async fn a_cancelled_steer_placement_writes_no_message_and_no_receipt() {
    let store = store();
    let lease = running_turn(&store, "run").await;
    store.create_input(steer_cas_input(), 2000).await.unwrap();
    // The cancellation wins the race before the projection even starts.
    store
        .transition_input(
            "steer-cas".into(),
            vec![AgentInputStatus::Pending],
            AgentInputStatus::Cancelled,
            AgentInputPatch::default(),
            2050,
        )
        .await
        .unwrap();
    let commit = store
        .append_projected_inputs(AppendProjectedInputs {
            applies: vec![steer_placement()],
            ..projection_for(&lease)
        })
        .await
        .unwrap();
    assert!(commit.applied.is_empty(), "the winner was the cancellation");
    let record = store.get_input("steer-cas".into()).await.unwrap().unwrap();
    assert_eq!(record.status, AgentInputStatus::Cancelled);
    assert!(record.applied_message_id.is_none());
    assert!(
        !store
            .messages("chat", false)
            .await
            .unwrap()
            .iter()
            .any(|message| message.id == "steer:steer-cas"),
        "the cancelled steer left no applied message behind"
    );
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
