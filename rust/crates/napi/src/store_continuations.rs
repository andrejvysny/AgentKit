//! The `continuations.*` port surface: provider-continuation bindings.
//! Scope validation, anchor checks, the fenced-writer requirement, and the
//! state-required flag are owned by `agentkit-store`.

use super::ports::{instant_or_now, optional_record, record, records, string_array, text};
use crate::errors;
use agentkit_store::{ProviderContinuationRecord, SqliteStore};
use napi::bindgen_prelude::*;
use serde_json::Value;

pub(super) async fn execute(store: &SqliteStore, method: &str, input: Value) -> Result<Value> {
    match method {
        "bindRun" => {
            let scope = match input.get("scope") {
                Some(Value::Object(_)) => input["scope"].clone(),
                _ => return Err(errors::invalid("scope must be an object")),
            };
            let chat_id = scope
                .get("chatId")
                .and_then(Value::as_str)
                .ok_or_else(|| errors::invalid("scope.chatId must be a string"))?
                .to_owned();
            let proof = super::ports::optional_fence(store, &input, "fence")
                .await?
                .ok_or_else(|| errors::invalid("fence must be provided"))?;
            store
                .bind_run(
                    text(&input, "runId")?,
                    &chat_id,
                    scope,
                    proof,
                    instant_or_now(&input, "now")?,
                )
                .await
                .map_err(errors::store)?;
            Ok(Value::Null)
        }
        "getRunScope" => Ok(store
            .get_run_scope(text(&input, "runId")?)
            .await
            .map_err(errors::store)?
            .unwrap_or(Value::Null)),
        "put" => {
            let continuation: ProviderContinuationRecord =
                super::ports::deserialized(&input, "input")?;
            let proof = super::ports::optional_fence(store, &input, "fence")
                .await?
                .ok_or_else(|| errors::invalid("fence must be provided"))?;
            Ok(record(
                store
                    .put_continuation(continuation, proof, instant_or_now(&input, "now")?)
                    .await
                    .map_err(errors::store)?,
            ))
        }
        "getByRun" => Ok(optional_record(
            store
                .get_continuation(text(&input, "runId")?)
                .await
                .map_err(errors::store)?,
        )),
        "findByAnchors" => Ok(records(
            store
                .continuations_by_anchors(
                    text(&input, "chatId")?,
                    string_array(&input, "anchorMessageIds")?,
                )
                .await
                .map_err(errors::store)?,
        )),
        _ => Err(errors::stable(
            "not_implemented",
            "Unsupported provider continuation store operation",
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::store::ports::testkit::{NOW, begin, call, code, store};
    use agentkit_store::SqliteStore;
    use serde_json::{Value, json};

    fn scope(chat_id: &str) -> Value {
        json!({"providerId":"openai","protocol":"responses","connectionId":"conn-1",
            "generation":1,"chatId":chat_id,"branchId":"b1","model":"gpt-x"})
    }

    fn continuation_state(chat_id: &str) -> Value {
        json!({"version":1,"scope":scope(chat_id),"messageCount":1,
            "messageDigest":"ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34ab12cd34",
            "inputItems":[]})
    }

    /// A fenced writer for `runId`: the chat, its anchor message, a task
    /// named after the run, and the lease the fence proves.
    async fn bind_writer(store: &SqliteStore, chat_id: &str, run_id: &str) -> Value {
        call(
            store,
            "conversations.createChat",
            json!({"input":{"id":chat_id},"now":NOW}),
        )
        .await
        .expect("chat");
        call(
            store,
            "tasks.createTask",
            json!({"input":{"taskId":run_id,"kind":"chat_turn","scopeId":chat_id,
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
        let fence = json!({"taskId":run_id,"leaseToken":lease["leaseToken"]});
        call(
            store,
            "conversations.appendMessage",
            json!({"input":{"id":"m-anchor","chatId":chat_id,"runId":run_id,"role":"user",
                "content":"anchor"},
                "fence":fence,"now":NOW}),
        )
        .await
        .expect("run-owned anchor message");
        fence
    }

    #[tokio::test]
    async fn continuation_flow_through_the_port_surface() {
        let store = store();
        let fence = bind_writer(&store, "c1", "r1").await;
        assert!(
            call(
                &store,
                "continuations.bindRun",
                json!({"runId":"r1","scope":scope("c1"),"now":NOW})
            )
            .await
            .is_err(),
            "binding without the writer fence is refused"
        );
        call(
            &store,
            "continuations.bindRun",
            json!({"runId":"r1","scope":scope("c1"),"fence":fence,"now":NOW}),
        )
        .await
        .expect("bound");
        assert_eq!(
            call(&store, "continuations.getRunScope", json!({"runId":"r1"}))
                .await
                .unwrap()["chatId"],
            "c1"
        );
        assert!(
            call(
                &store,
                "continuations.getRunScope",
                json!({"runId":"ghost"})
            )
            .await
            .unwrap()
            .is_null()
        );

        let put = call(
            &store,
            "continuations.put",
            json!({"input":{"runId":"r1","anchorMessageId":"m-anchor",
                "state":continuation_state("c1")},"fence":fence,"now":NOW}),
        )
        .await
        .expect("continuation stored");
        assert_eq!(put["runId"], "r1");
        assert_eq!(put["anchorMessageId"], "m-anchor");
        assert_eq!(
            call(&store, "continuations.getByRun", json!({"runId":"r1"}))
                .await
                .unwrap()["runId"],
            "r1"
        );
        let found = call(
            &store,
            "continuations.findByAnchors",
            json!({"chatId":"c1","anchorMessageIds":["m-anchor","other"]}),
        )
        .await
        .unwrap();
        assert_eq!(found.as_array().expect("hits").len(), 1);
    }

    #[tokio::test]
    async fn malformed_fences_and_scopes_are_typed_errors() {
        let store = store();
        let fence = bind_writer(&store, "c2", "r2").await;
        assert_eq!(
            code(
                call(
                    &store,
                    "continuations.put",
                    json!({"input":{"runId":"r2","anchorMessageId":"m",
                        "state":continuation_state("c2")},"now":NOW})
                )
                .await
                .unwrap_err()
            ),
            "invalid_input",
            "continuation writes require the writer fence"
        );
        let mut broken_scope = scope("c2");
        broken_scope["protocol"] = json!("chat");
        assert_eq!(
            code(
                call(
                    &store,
                    "continuations.bindRun",
                    json!({"runId":"r2","scope":broken_scope,"fence":fence,"now":NOW})
                )
                .await
                .unwrap_err()
            ),
            "invalid_input",
            "the store owns scope validation"
        );
        assert_eq!(
            code(
                call(&store, "continuations.rehydrate", json!({}))
                    .await
                    .unwrap_err()
            ),
            "not_implemented"
        );
    }

    #[tokio::test]
    async fn committed_binding_and_continuation_become_visible() {
        let store = store();
        let fence = bind_writer(&store, "c3", "r3").await;
        let transaction = begin(&store).await;
        let view = transaction.view();
        call(
            &view,
            "continuations.bindRun",
            json!({"runId":"r3","scope":scope("c3"),"fence":fence,"now":NOW}),
        )
        .await
        .expect("bind inside the transaction");
        call(
            &view,
            "continuations.put",
            json!({"input":{"runId":"r3","anchorMessageId":"m-anchor",
                "state":continuation_state("c3")},"fence":fence,"now":NOW}),
        )
        .await
        .expect("put inside the transaction");
        transaction.commit().await.expect("commit");
        assert_eq!(
            call(&store, "continuations.getByRun", json!({"runId":"r3"}))
                .await
                .unwrap()["runId"],
            "r3"
        );
    }

    #[tokio::test]
    async fn rolled_back_binding_and_continuation_leave_no_trace() {
        let store = store();
        let fence = bind_writer(&store, "c4", "r4").await;
        let transaction = begin(&store).await;
        let view = transaction.view();
        call(
            &view,
            "continuations.bindRun",
            json!({"runId":"r4","scope":scope("c4"),"fence":fence,"now":NOW}),
        )
        .await
        .expect("bind inside the transaction");
        call(
            &view,
            "continuations.put",
            json!({"input":{"runId":"r4","anchorMessageId":"m-anchor",
                "state":continuation_state("c4")},"fence":fence,"now":NOW}),
        )
        .await
        .expect("put inside the transaction");
        transaction.rollback().await.expect("rollback");
        assert!(
            call(&store, "continuations.getByRun", json!({"runId":"r4"}))
                .await
                .unwrap()
                .is_null()
        );
        assert!(
            call(&store, "continuations.getRunScope", json!({"runId":"r4"}))
                .await
                .unwrap()
                .is_null()
        );
    }
}
