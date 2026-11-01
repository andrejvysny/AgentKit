//! The `toolExecutions.*` port surface: exactly-once tool execution receipts.
//! Admission uniqueness and result persistence are owned by `agentkit-store`.

use super::ports::{instant_or_now, opt_text, optional_fence, records, text};
use crate::errors;
use agentkit_store::{ExecutionReceiptAdmission, SqliteStore};
use napi::bindgen_prelude::*;
use serde_json::{Value, json};

pub(super) async fn execute(store: &SqliteStore, method: &str, input: Value) -> Result<Value> {
    match method {
        "admit" => {
            let source = &input["input"];
            let proof = optional_fence(store, &input, "fence")
                .await?
                .ok_or_else(|| errors::invalid("fence must be provided"))?;
            let run_id = text(source, "runId")?.to_owned();
            let chat_id = match source.get("chatId") {
                None | Some(Value::Null) => store
                    .get_run_scope(&run_id)
                    .await
                    .map_err(errors::store)?
                    .and_then(|scope| {
                        scope
                            .get("chatId")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .ok_or_else(|| errors::invalid("cannot derive chatId: the run is not bound"))?,
                Some(_) => text(source, "chatId")?.to_owned(),
            };
            let admitted = store
                .admit_execution(
                    proof,
                    ExecutionReceiptAdmission {
                        execution_id: text(source, "executionId")?.to_owned(),
                        run_id,
                        chat_id,
                        tool_call_id: text(source, "toolCallId")?.to_owned(),
                        tool_name: text(source, "toolName")?.to_owned(),
                        args_sha256: text(source, "argsSha256")?.to_owned(),
                        context_sha256: text(source, "contextSha256")?.to_owned(),
                        attempt_id: text(source, "attemptId")?.to_owned(),
                        approval_request_id: opt_text(source, "approvalRequestId")?
                            .map(str::to_owned),
                        approval_generation: opt_text(source, "approvalGeneration")?
                            .map(str::to_owned),
                        now: instant_or_now(&input, "now")?,
                    },
                )
                .await
                .map_err(errors::store)?;
            Ok(json!(admitted))
        }
        "complete" => {
            let proof = optional_fence(store, &input, "fence")
                .await?
                .ok_or_else(|| errors::invalid("fence must be provided"))?;
            let result = match input.get("result") {
                None | Some(Value::Null) => {
                    return Err(errors::invalid("result must be provided"));
                }
                Some(value) => value.clone(),
            };
            store
                .complete_execution(
                    proof,
                    text(&input, "executionId")?.to_owned(),
                    result,
                    instant_or_now(&input, "now")?,
                )
                .await
                .map_err(errors::store)?;
            Ok(Value::Null)
        }
        "listByRun" => Ok(records(
            store
                .list_executions(text(&input, "runId")?.to_owned())
                .await
                .map_err(errors::store)?,
        )),
        _ => Err(errors::stable(
            "not_implemented",
            "Unsupported tool execution store operation",
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::store::ports::testkit::{NOW, begin, call, code, store};
    use agentkit_store::SqliteStore;
    use serde_json::{Value, json};

    const HEX64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A fenced writer whose task id doubles as the run id the receipt names.
    async fn writer(store: &SqliteStore, run_id: &str) -> Value {
        call(
            store,
            "tasks.createTask",
            json!({"input":{"taskId":run_id,"kind":"tool_run","scopeId":"s1",
                "payload":{},"availableAt":NOW},"now":NOW}),
        )
        .await
        .expect("run task");
        let lease = call(
            store,
            "tasks.acquireLease",
            json!({"input":{"taskId":run_id,"attemptId":"a1","ownerId":"w1",
                "ttlMs":600_000},"now":NOW}),
        )
        .await
        .expect("lease");
        json!({"taskId":run_id,"leaseToken":lease["leaseToken"]})
    }

    fn admit_input(execution_id: &str, run_id: &str) -> Value {
        json!({"input":{"executionId":execution_id,"runId":run_id,"chatId":"c1",
            "toolCallId":"call-1","toolName":"read_file","argsSha256":HEX64,
            "contextSha256":HEX64,"attemptId":"a1"},
            "now":NOW})
    }

    fn admit_call(execution_id: &str, run_id: &str, fence: &Value) -> Value {
        let mut input = admit_input(execution_id, run_id);
        input["fence"] = fence.clone();
        input
    }

    #[tokio::test]
    async fn receipt_flow_through_the_port_surface() {
        let store = store();
        let fence = writer(&store, "r1").await;
        assert_eq!(
            call(
                &store,
                "toolExecutions.admit",
                admit_call("e1", "r1", &fence)
            )
            .await
            .unwrap(),
            true,
            "a fresh execution is admitted"
        );
        assert_eq!(
            call(
                &store,
                "toolExecutions.admit",
                admit_call("e1b", "r1", &fence)
            )
            .await
            .unwrap(),
            false,
            "the same operation cannot admit twice while pending"
        );
        call(
            &store,
            "toolExecutions.complete",
            json!({"executionId":"e1","result":{"content":"file body"},"fence":fence,"now":NOW}),
        )
        .await
        .expect("result persisted before delivery");
        let listed = call(&store, "toolExecutions.listByRun", json!({"runId":"r1"}))
            .await
            .unwrap();
        let receipts = listed.as_array().expect("receipts");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0]["executionId"], "e1");
        assert_eq!(receipts[0]["status"], "completed");
    }

    #[tokio::test]
    async fn malformed_proofs_and_missing_receipts_are_typed_errors() {
        let store = store();
        let fence = writer(&store, "r2").await;
        assert_eq!(
            code(
                call(&store, "toolExecutions.admit", admit_input("e1", "r2"))
                    .await
                    .unwrap_err()
            ),
            "invalid_input",
            "admission requires the writer fence"
        );
        let mut bad = admit_call("e2", "r2", &fence);
        bad["input"]["argsSha256"] = json!("not-a-digest");
        assert_eq!(
            code(call(&store, "toolExecutions.admit", bad).await.unwrap_err()),
            "invalid_input",
            "the store enforces the digest vocabulary"
        );
        assert_eq!(
            code(
                call(
                    &store,
                    "toolExecutions.complete",
                    json!({"executionId":"ghost","result":{},"fence":fence,"now":NOW})
                )
                .await
                .unwrap_err()
            ),
            "not_found",
            "completing an unadmitted receipt is reported, never ignored"
        );
        assert_eq!(
            code(
                call(&store, "toolExecutions.replay", json!({}))
                    .await
                    .unwrap_err()
            ),
            "not_implemented"
        );
    }

    #[tokio::test]
    async fn committed_receipt_becomes_visible() {
        let store = store();
        let fence = writer(&store, "r3").await;
        let transaction = begin(&store).await;
        call(
            &transaction.view(),
            "toolExecutions.admit",
            admit_call("e9", "r3", &fence),
        )
        .await
        .expect("admit inside the transaction");
        transaction.commit().await.expect("commit");
        assert_eq!(
            call(&store, "toolExecutions.listByRun", json!({"runId":"r3"}))
                .await
                .unwrap()
                .as_array()
                .expect("receipts")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn rolled_back_admission_leaves_no_trace() {
        let store = store();
        let fence = writer(&store, "r4").await;
        let transaction = begin(&store).await;
        call(
            &transaction.view(),
            "toolExecutions.admit",
            admit_call("e9", "r4", &fence),
        )
        .await
        .expect("admit inside the transaction");
        transaction.rollback().await.expect("rollback");
        assert!(
            call(&store, "toolExecutions.listByRun", json!({"runId":"r4"}))
                .await
                .unwrap()
                .as_array()
                .expect("receipts")
                .is_empty()
        );
    }
}
