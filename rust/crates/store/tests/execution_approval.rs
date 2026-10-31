mod support;
use agentkit_store::*;
use serde_json::{Value, json};
use support::*;

fn request(id: &str) -> CreateAgentInput {
    CreateAgentInput {
        input_id: id.into(),
        session_id: "chat".into(),
        kind: AgentInputKind::HumanRequest,
        content: json!({"kind":"approval", "toolName":"write", "argsSha256":"a".repeat(64), "contextSha256":"b".repeat(64), "approvalGeneration":"generation"}),
        content_hash: "hash".into(),
        target_task_id: Some("run".into()),
        on_settled: None,
        attempt_id: Some("old-attempt".into()),
        tool_call_id: Some("call".into()),
        expires_at: None,
    }
}
fn admission(id: &str) -> ExecutionReceiptAdmission {
    ExecutionReceiptAdmission {
        execution_id: format!("execution-{id}"),
        run_id: "run".into(),
        chat_id: "chat".into(),
        tool_call_id: "call".into(),
        tool_name: "write".into(),
        args_sha256: "a".repeat(64),
        context_sha256: "b".repeat(64),
        attempt_id: "attempt".into(),
        approval_request_id: Some(id.into()),
        approval_generation: Some("generation".into()),
        now: 1002,
    }
}
async fn answered(store: &SqliteStore, input: CreateAgentInput, marker: &str) {
    let id = input.input_id.clone();
    store.create_input(input, 1000).await.unwrap();
    store
        .transition_input(
            id,
            vec![AgentInputStatus::Pending],
            AgentInputStatus::Answered,
            AgentInputPatch {
                answer_id: Some("answer".into()),
                applied_message_id: Some(marker.into()),
                ..Default::default()
            },
            1001,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn approval_is_revalidated_in_the_receipt_transaction() {
    let store = SqliteStore::memory(8).unwrap();
    store.submit(task("run", "chat")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    let mut cases: Vec<(&str, CreateAgentInput, &str)> = vec![];
    let mut wrong_session = request("session");
    wrong_session.session_id = "other".into();
    cases.push(("session", wrong_session, "approval:ok"));
    let mut wrong_run = request("run");
    wrong_run.target_task_id = Some("other".into());
    cases.push(("run", wrong_run, "approval:ok"));
    let mut wrong_call = request("call");
    wrong_call.tool_call_id = Some("other".into());
    cases.push(("call", wrong_call, "approval:ok"));
    for (id, field, value) in [
        ("kind", "kind", json!("input")),
        ("tool", "toolName", json!("other")),
        ("args", "argsSha256", json!("c".repeat(64))),
        ("context", "contextSha256", json!("c".repeat(64))),
        ("generation", "approvalGeneration", json!("old")),
        ("malformed", "contextSha256", Value::Null),
    ] {
        let mut wrong = request(id);
        wrong.content[field] = value;
        cases.push((id, wrong, "approval:ok"));
    }
    cases.push(("denied", request("denied"), "denied-message"));
    for (id, input, marker) in cases {
        answered(&store, input, marker).await;
        assert!(
            store
                .admit_execution(lease.fence(), admission(id))
                .await
                .is_err(),
            "{id}"
        );
        assert!(
            store
                .list_executions("run".into())
                .await
                .unwrap()
                .is_empty(),
            "{id}"
        );
    }
    store.create_input(request("pending"), 1000).await.unwrap();
    assert!(
        store
            .admit_execution(lease.fence(), admission("pending"))
            .await
            .is_err()
    );
    store
        .transition_input(
            "pending".into(),
            vec![AgentInputStatus::Pending],
            AgentInputStatus::Cancelled,
            AgentInputPatch::default(),
            1001,
        )
        .await
        .unwrap();
    assert!(
        store
            .admit_execution(lease.fence(), admission("pending"))
            .await
            .is_err()
    );
    assert!(
        store
            .admit_execution(lease.fence(), admission("missing"))
            .await
            .is_err()
    );
    assert!(
        store
            .list_executions("run".into())
            .await
            .unwrap()
            .is_empty()
    );
    let mut legacy = request("legacy");
    legacy
        .content
        .as_object_mut()
        .unwrap()
        .remove("approvalGeneration");
    legacy
        .content
        .as_object_mut()
        .unwrap()
        .remove("contextSha256");
    legacy.content["uncertaintyGeneration"] = json!("generation");
    answered(&store, legacy, "approval:ok").await;
    assert!(
        store
            .admit_execution(lease.fence(), admission("legacy"))
            .await
            .unwrap()
    );
    store
        .complete_execution(
            lease.fence(),
            "execution-legacy".into(),
            json!({"ok": true}),
            1003,
        )
        .await
        .unwrap();
    let mut repeated = admission("legacy");
    repeated.execution_id = "different".into();
    assert!(
        !store
            .admit_execution(lease.fence(), repeated)
            .await
            .unwrap()
    );
    assert_eq!(store.list_executions("run".into()).await.unwrap().len(), 1);
    answered(&store, request("current"), "approval:ok").await;
    assert!(
        store
            .admit_execution(lease.fence(), admission("current"))
            .await
            .unwrap()
    );
    assert_eq!(store.list_executions("run".into()).await.unwrap().len(), 2);
}
