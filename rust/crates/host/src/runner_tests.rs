use super::*;
use agentkit_core::HumanRequestSpec;
use agentkit_store::{AgentInputKind, AnswerHumanRequest, CreateAgentInput, ParkTurn};
use std::sync::atomic::{AtomicI64, Ordering};

struct Clock(AtomicI64);
impl Runtime for Clock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    fn id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

async fn fixture() -> (AgentHost, ExecutionContext, Arc<Clock>) {
    let store = SqliteStore::memory(16).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1_000)));
    store
        .create_chat("chat", None, json!({}), 1_000)
        .await
        .unwrap();
    let host = AgentHost::new(
        store.clone(),
        HashMap::new(),
        clock.clone(),
        HostOptions::default(),
    )
    .unwrap();
    host.submit(SubmitTask {
        task_id: "turn".into(),
        kind: "chat.turn".into(),
        scope_id: "chat".into(),
        payload: json!({"chatId":"chat"}),
        priority: 0,
        now: 1_000,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: true,
    })
    .await
    .unwrap();
    let context = host.claim_context().await.unwrap().unwrap();
    (host, context, clock)
}

/// Recreates the precise park/heartbeat interleaving without sleeps: the
/// store has deleted the lease, but the parking executor has not published
/// its suspension yet. Renewal must skip this transition rather than report
/// a stolen lease, and must not wait on the mutex held by the executor that
/// the same select loop needs to keep polling.
#[tokio::test]
async fn heartbeat_during_committed_park_does_not_lose_the_worker_and_answer_resumes() {
    let (host, context, clock) = fixture().await;
    let request = HumanRequestSpec {
        request_id: "request".into(),
        content: json!({"kind":"approval", "toolName":"write_file", "argsSha256":"a".repeat(64), "prompt":"write"}),
        content_hash: "request-hash".into(),
        expires_at: None,
    };
    let mut suspension = context.suspension.lock().await;
    host.store
        .park_turn(ParkTurn {
            fence: context.fence.clone(),
            assistant_message: None,
            events: vec![],
            now: 1_000,
            request: Some(CreateAgentInput {
                input_id: request.request_id.clone(),
                session_id: "chat".into(),
                kind: AgentInputKind::HumanRequest,
                content: request.content.clone(),
                content_hash: request.content_hash.clone(),
                target_task_id: Some("turn".into()),
                on_settled: None,
                attempt_id: Some(context.attempt_id.clone()),
                tool_call_id: Some("call".into()),
                expires_at: None,
            }),
        })
        .await
        .unwrap();
    assert_eq!(
        host.store.get_task("turn").await.unwrap().unwrap().status,
        TaskStatus::WaitingApproval
    );
    tokio::time::timeout(Duration::from_secs(5), host.heartbeat(&context))
        .await
        .expect("heartbeat must not block the parked executor")
        .expect("a committed park is not lease theft");
    assert!(!context.cancellation.is_cancelled());
    *suspension = Some(request);
    drop(suspension);
    host.heartbeat(&context)
        .await
        .expect("a published suspension no longer owns a lease");

    host.store
        .answer_human_request(AnswerHumanRequest {
            request_id: "request".into(),
            answer_id: "answer".into(),
            response: json!({"decision":"approve"}),
            response_hash: "answer-hash".into(),
            now: 1_001,
            message_id: "answer-message".into(),
            plan_tool_names: vec![],
            policy_denial: None,
            granted_mode: None,
        })
        .await
        .unwrap();
    clock.0.store(1_001, Ordering::SeqCst);
    let resumed = host
        .claim_context()
        .await
        .unwrap()
        .expect("answer resumes the turn");
    assert_ne!(resumed.fence.lease_token, context.fence.lease_token);
    assert_ne!(resumed.attempt_id, context.attempt_id);
    host.heartbeat(&resumed).await.unwrap();
}

/// Coordination with an intentional park must not turn an actually expired
/// or replaced lease into success. Those cases still cancel the old context.
#[tokio::test]
async fn heartbeat_still_refuses_expired_and_replaced_leases() {
    for replace in [false, true] {
        let (host, context, clock) = fixture().await;
        if replace {
            host.store
                .acquire_lease("turn", "other-attempt", "other-worker", 1_000, 30_000)
                .await
                .unwrap();
        } else {
            clock.0.store(31_000, Ordering::SeqCst);
        }
        assert!(matches!(
            host.heartbeat(&context).await,
            Err(HostError::Store(StoreError::LeaseLost(_)))
        ));
        assert!(context.cancellation.is_cancelled());
    }
}
