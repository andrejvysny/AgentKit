//! M0 fault matrix — approval persistence and the dispatch boundary.
//!
//! An approval authorizes exactly one execution of exactly the call it names.
//! Two gate properties are asserted here by name: an approval is durable
//! before any dispatch happens (so a restart neither loses it nor spends it
//! twice), and a stale — unanswered, expired, cancelled or already-consumed —
//! approval can never authorize a dispatch. Every assertion is on stored rows:
//! the agent-input record, the execution receipt, the attempt and the event
//! stream.

#[path = "runner_conformance/scenarios.rs"]
mod scenarios;
#[path = "runner_conformance/support.rs"]
mod support;

use agentkit_host::{args_sha256, execution_id};
use agentkit_store::{
    AgentInputKind, AgentInputPatch, AgentInputRecord, AgentInputStatus, ArgsHashProvenance,
    CreateAgentInput, ExecutionReceiptAdmission, ExecutionReceiptStatus, SqliteStore, StoreError,
};
use serde_json::{Value, json};
use support::{Directory, Environment, snapshot};

const CALL: &str = "call-1";
const ATTEMPT: &str = "attempt-2";
const CHAT: &str = "chat-1";
const TOOL: &str = "write_file";

fn vector(name: &str) -> support::Profile {
    let profile = support::profile(name);
    assert!(!profile.schedule.is_empty(), "{name} documents no schedule");
    assert!(
        profile.expected.typescript.is_empty(),
        "profile {name} is executed by the Rust host only"
    );
    profile
}

fn arguments() -> Value {
    json!({"path": "a.txt"})
}

fn digest() -> String {
    args_sha256(&arguments())
}

fn request(id: &str, expires_at: Option<&str>, generation: Option<&str>) -> CreateAgentInput {
    let mut content = json!({
        "kind": "approval",
        "toolName": TOOL,
        "argsSha256": digest(),
        "prompt": "write a.txt",
    });
    if let Some(generation) = generation {
        content["approvalGeneration"] = json!(generation);
    }
    CreateAgentInput {
        input_id: id.into(),
        session_id: CHAT.into(),
        kind: AgentInputKind::HumanRequest,
        content,
        content_hash: format!("hash-{id}"),
        target_task_id: Some("run".into()),
        on_settled: None,
        attempt_id: Some(ATTEMPT.into()),
        tool_call_id: Some(CALL.into()),
        expires_at: expires_at.map(str::to_owned),
    }
}

fn admission(execution: &str, approval: Option<&str>, now: i64) -> ExecutionReceiptAdmission {
    admission_opening(execution, approval, None, now)
}

fn admission_opening(
    execution: &str,
    approval: Option<&str>,
    generation: Option<&str>,
    now: i64,
) -> ExecutionReceiptAdmission {
    ExecutionReceiptAdmission {
        execution_id: execution.into(),
        run_id: "run".into(),
        chat_id: CHAT.into(),
        tool_call_id: CALL.into(),
        tool_name: TOOL.into(),
        args_sha256: digest(),
        ts_args_sha256: None,
        args_hash_provenance: ArgsHashProvenance::Canonical,
        context_sha256: "0".repeat(64),
        attempt_id: ATTEMPT.into(),
        approval_request_id: approval.map(str::to_owned),
        approval_generation: generation.map(str::to_owned),
        now,
    }
}

async fn answered(store: &SqliteStore, id: &str, now: i64) -> AgentInputRecord {
    store
        .transition_input(
            id.into(),
            vec![AgentInputStatus::Pending],
            AgentInputStatus::Answered,
            AgentInputPatch {
                applied_task_id: Some("run".into()),
                applied_message_id: Some(format!("approval:{id}")),
                answer_id: Some(format!("ans-{id}")),
            },
            now,
        )
        .await
        .unwrap()
}

/// Gate property: a stale approval can never authorize a dispatch. A pending,
/// an expired, a cancelled, an already-consumed and a generation-mismatched
/// authorization each fail at the store's dispatch gate, and none of them
/// leaves a receipt behind or spends the effect.
#[tokio::test]
async fn a_stale_approval_can_never_authorize_a_dispatch() {
    let vector = vector("m0_approval_stale_dispatch");
    let directory = Directory::new();
    let store = SqliteStore::open(directory.0.join("m0-approval-stale.db"), 16).unwrap();
    let env = Environment::new(store, "m0_approval_stale_dispatch", 1000).await;
    env.submit().await;
    assert!(env.host.run_once().await.unwrap());
    let store = env.host.store();

    for id in ["hreq-pending", "hreq-expired"] {
        store
            .create_input(request(id, Some("2020-01-01T00:00:00.000Z"), None), 1500)
            .await
            .unwrap();
    }
    store
        .create_input(request("hreq-cancelled", None, None), 1500)
        .await
        .unwrap();
    store
        .transition_input(
            "hreq-cancelled".into(),
            vec![AgentInputStatus::Pending],
            AgentInputStatus::Cancelled,
            AgentInputPatch::default(),
            1500,
        )
        .await
        .unwrap();
    store
        .create_input(request("hreq-answered", None, None), 1500)
        .await
        .unwrap();
    answered(store, "hreq-answered", 1500).await;

    // A dispatch fence for the run, as the executor would hold one.
    env.at(2000);
    let fence = store
        .acquire_lease("run", ATTEMPT, "approver", 2000, 1000)
        .await
        .unwrap()
        .fence();

    let mut refused = 0;
    for stale in ["hreq-pending", "hreq-expired", "hreq-cancelled"] {
        let error = store
            .admit_execution(
                fence.clone(),
                admission(&execution_id("run", CALL, Some(stale)), Some(stale), 2000),
            )
            .await;
        assert!(
            matches!(
                &error,
                Err(StoreError::InvalidInput(reason)) if reason.starts_with("approval_mismatch:")
            ),
            "{stale} must be refused as an authorization, got {error:?}"
        );
        refused += 1;
    }

    // The answered authorization spends exactly once.
    let spent = execution_id("run", CALL, Some("hreq-answered"));
    assert!(
        store
            .admit_execution(
                fence.clone(),
                admission(&spent, Some("hreq-answered"), 2000)
            )
            .await
            .unwrap(),
        "the answered approval admits its own execution"
    );
    assert!(
        !store
            .admit_execution(
                fence.clone(),
                admission(
                    &execution_id("run", CALL, Some("hreq-answered-replay")),
                    Some("hreq-answered"),
                    2000
                )
            )
            .await
            .unwrap(),
        "a consumed approval cannot authorize a second execution"
    );
    // Not admitted is a refusal too: it is counted, so the checkpoint says how
    // many authorizations this profile proved the dispatch gate can refuse.
    refused += 1;
    // Naming a generation the authorization does not carry is refused too.
    store
        .create_input(request("hreq-generation", None, Some("gen-42")), 1500)
        .await
        .unwrap();
    answered(store, "hreq-generation", 1500).await;
    let unbound = store
        .admit_execution(
            fence.clone(),
            admission(
                &execution_id("run", CALL, Some("hreq-generation-replay")),
                Some("hreq-generation"),
                2000,
            ),
        )
        .await;
    assert!(
        matches!(
            &unbound,
            Err(StoreError::InvalidInput(reason)) if reason.starts_with("approval_mismatch:")
        ),
        "an execution that does not name the approval's generation is refused"
    );
    refused += 1;

    let receipts = store.list_executions("run".into()).await.unwrap();
    assert_eq!(receipts.len(), 1, "exactly one effect was admitted");
    assert_eq!(receipts[0].status, ExecutionReceiptStatus::Admitted);
    assert_eq!(
        receipts[0].approval_request_id.as_deref(),
        Some("hreq-answered")
    );
    assert_eq!(
        store.attempts("run").await.unwrap().len(),
        1,
        "no extra attempt row"
    );
    assert_eq!(
        env.witness
            .effects
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no unauthorized mutation and no duplicate side effect"
    );
    let extra = json!({
        "admitted": receipts.len(),
        "refused": refused,
        "receiptStatuses": receipts.iter().map(|record| format!("{:?}", record.status).to_lowercase()).collect::<Vec<_>>(),
    });
    assert_eq!(
        vec![snapshot(&env, "settled", extra).await],
        vector.expected.rust
    );
}

/// An approval answered before any dispatch is durable: reopening the host
/// neither spends it nor starts the queued work; an explicit resume dispatches
/// it once, and only once.
#[tokio::test]
async fn an_approval_answered_before_dispatch_survives_the_restart() {
    let vector = vector("m0_approval_persisted_replay");
    let directory = Directory::new();
    let path = directory.0.join("m0-approval-replay.db");
    let first = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "m0_approval_persisted_replay",
        1000,
    )
    .await;
    first.submit().await;
    first
        .host
        .store()
        .create_input(request("hreq-live", None, None), 1100)
        .await
        .unwrap();
    answered(first.host.store(), "hreq-live", 1200).await;
    async fn approval_status(store: &SqliteStore) -> String {
        format!(
            "{:?}",
            store
                .get_input("hreq-live".into())
                .await
                .unwrap()
                .unwrap()
                .status
        )
        .to_lowercase()
    }
    assert_eq!(approval_status(first.host.store()).await, "answered");
    let transcript_first = snapshot(
        &first,
        "persisted",
        json!({"approvalStatus": "answered", "attemptRows": 0}),
    )
    .await;
    drop(first);

    let reopened = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "m0_approval_persisted_replay",
        2000,
    )
    .await;
    assert_eq!(approval_status(reopened.host.store()).await, "answered");
    assert!(
        !reopened.host.run_once().await.unwrap(),
        "restart must not dispatch work behind an approval it merely recovered"
    );
    assert_eq!(
        reopened
            .witness
            .effects
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the restarting host performed no effect"
    );
    let mut transcript = vec![
        transcript_first,
        snapshot(
            &reopened,
            "after_restart",
            json!({"approvalStatus": "answered", "attemptRows": 0}),
        )
        .await,
    ];

    reopened.host.resume("run").await.unwrap();
    assert!(reopened.host.run_once().await.unwrap());
    assert!(
        !reopened.host.run_once().await.unwrap(),
        "the approved work dispatches exactly once"
    );
    assert_eq!(
        reopened
            .witness
            .effects
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert!(
        reopened
            .host
            .store()
            .get_input("hreq-live".into())
            .await
            .unwrap()
            .is_some(),
        "the spent approval stays on the record"
    );
    transcript.push(snapshot(
        &reopened,
        "dispatched",
        json!({"approvalStatus": "answered", "attemptRows": reopened.host.store().attempts("run").await.unwrap().len()}),
    )
    .await);
    assert_eq!(transcript, vector.expected.rust);
}
