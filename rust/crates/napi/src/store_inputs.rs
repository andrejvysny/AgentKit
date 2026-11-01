//! The `inputs.*` port surface: the `AgentInputStore` half of the assistant
//! store (ADR 0016 §2). Idempotent creation, the per-kind transition machine,
//! and FIFO ordering are owned by `agentkit-store`.

use super::ports::{
    deserialized, instant_or_now, opt_text, optional_record, record, records, text,
};
use crate::errors;
use agentkit_store::{
    AgentInputKind, AgentInputPatch, AgentInputQuery, AgentInputStatus, CreateAgentInput,
    SqliteStore,
};
use napi::bindgen_prelude::*;
use serde_json::{Value, json};

pub(super) async fn execute(store: &SqliteStore, method: &str, input: Value) -> Result<Value> {
    match method {
        "createInput" => {
            let source = &input["input"];
            let content = match source.get("content") {
                None | Some(Value::Null) => {
                    return Err(errors::invalid("input.content must be provided"));
                }
                Some(value) => value.clone(),
            };
            let (created_record, created) = store
                .create_input(
                    CreateAgentInput {
                        input_id: text(source, "inputId")?.to_owned(),
                        session_id: text(source, "sessionId")?.to_owned(),
                        kind: deserialized(source, "kind")?,
                        content,
                        content_hash: text(source, "contentHash")?.to_owned(),
                        target_task_id: opt_text(source, "targetTaskId")?.map(str::to_owned),
                        on_settled: match source.get("onSettled") {
                            None | Some(Value::Null) => None,
                            Some(_) => Some(deserialized(source, "onSettled")?),
                        },
                        attempt_id: opt_text(source, "attemptId")?.map(str::to_owned),
                        tool_call_id: opt_text(source, "toolCallId")?.map(str::to_owned),
                        expires_at: opt_text(source, "expiresAt")?.map(str::to_owned),
                    },
                    instant_or_now(&input, "now")?,
                )
                .await
                .map_err(errors::store)?;
            Ok(json!({"record": record(created_record), "created": created}))
        }
        "getInput" => Ok(optional_record(
            store
                .get_input(text(&input, "inputId")?.to_owned())
                .await
                .map_err(errors::store)?,
        )),
        "listInputs" => {
            let query = &input["query"];
            let kinds: Option<Vec<AgentInputKind>> = match query.get("kinds") {
                None | Some(Value::Null) => None,
                Some(_) => Some(deserialized(query, "kinds")?),
            };
            let statuses: Option<Vec<AgentInputStatus>> = match query.get("statuses") {
                None | Some(Value::Null) => None,
                Some(_) => Some(deserialized(query, "statuses")?),
            };
            Ok(records(
                store
                    .list_inputs(AgentInputQuery {
                        session_id: text(query, "sessionId")?.to_owned(),
                        kinds,
                        statuses,
                        target_task_id: opt_text(query, "targetTaskId")?.map(str::to_owned),
                    })
                    .await
                    .map_err(errors::store)?,
            ))
        }
        "transitionInput" => {
            let from: Vec<AgentInputStatus> = deserialized(&input, "from")?;
            let to: AgentInputStatus = deserialized(&input, "to")?;
            let patch = input.get("patch").cloned().unwrap_or(Value::Null);
            Ok(record(
                store
                    .transition_input(
                        text(&input, "inputId")?.to_owned(),
                        from,
                        to,
                        AgentInputPatch {
                            applied_task_id: opt_text(&patch, "appliedTaskId")?.map(str::to_owned),
                            applied_message_id: opt_text(&patch, "appliedMessageId")?
                                .map(str::to_owned),
                            answer_id: opt_text(&patch, "answerId")?.map(str::to_owned),
                        },
                        instant_or_now(&input, "now")?,
                    )
                    .await
                    .map_err(|error| {
                        crate::errors::store_port(
                            error,
                            "input_transition_invalid",
                            "input_conflict",
                        )
                    })?,
            ))
        }
        "deleteSessionInputs" => Ok(json!(
            store
                .delete_session_inputs(text(&input, "sessionId")?.to_owned())
                .await
                .map_err(errors::store)?
        )),
        _ => Err(errors::stable(
            "not_implemented",
            "Unsupported agent input store operation",
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::store::ports::testkit::{NOW, begin, call, code, store};
    use serde_json::json;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn input_payload(id: &str) -> serde_json::Value {
        json!({"input":{"inputId":id,"sessionId":"s1","kind":"follow_up",
            "content":{"text":"do the next thing"},"contentHash":HASH},
            "now":NOW})
    }

    #[tokio::test]
    async fn agent_input_flow_through_the_port_surface() {
        let store = store();
        let created = call(&store, "inputs.createInput", input_payload("i1"))
            .await
            .expect("input created");
        assert_eq!(created["created"], true);
        assert_eq!(created["record"]["inputId"], "i1");
        assert_eq!(created["record"]["status"], "pending");

        let replay = call(&store, "inputs.createInput", input_payload("i1"))
            .await
            .expect("idempotent replay");
        assert_eq!(
            replay["created"], false,
            "same id and hash replays, never duplicates"
        );

        assert_eq!(
            call(&store, "inputs.getInput", json!({"inputId":"i1"}))
                .await
                .unwrap()["inputId"],
            "i1"
        );
        assert!(
            call(&store, "inputs.getInput", json!({"inputId":"ghost"}))
                .await
                .unwrap()
                .is_null()
        );
        assert_eq!(
            call(
                &store,
                "inputs.listInputs",
                json!({"query":{"sessionId":"s1"}})
            )
            .await
            .unwrap()
            .as_array()
            .expect("list")
            .len(),
            1
        );

        let cancelled = call(
            &store,
            "inputs.transitionInput",
            json!({"inputId":"i1","from":["pending"],"to":"cancelled","now":NOW}),
        )
        .await
        .expect("cancelled");
        assert_eq!(cancelled["status"], "cancelled");

        assert_eq!(
            call(
                &store,
                "inputs.deleteSessionInputs",
                json!({"sessionId":"s1"})
            )
            .await
            .unwrap(),
            1
        );
        assert!(
            call(&store, "inputs.getInput", json!({"inputId":"i1"}))
                .await
                .unwrap()
                .is_null()
        );
    }

    #[tokio::test]
    async fn malformed_input_is_a_typed_error() {
        let store = store();
        assert_eq!(
            code(
                call(
                    &store,
                    "inputs.createInput",
                    json!({"input":{"inputId":"i1","sessionId":"s1","kind":"follow_up",
                        "contentHash":HASH}})
                )
                .await
                .unwrap_err()
            ),
            "invalid_input",
            "content is mandatory"
        );
        assert_eq!(
            code(
                call(
                    &store,
                    "inputs.createInput",
                    json!({"input":{"inputId":"i2","sessionId":"s1","kind":"follow_up",
                        "content":{},"contentHash":HASH,"onSettled":"cancel"},"now":NOW})
                )
                .await
                .unwrap_err()
            ),
            "invalid_input",
            "the store owns the settle-policy rule"
        );
        assert_eq!(
            code(
                call(
                    &store,
                    "inputs.createInput",
                    json!({"input":{"inputId":"i3","sessionId":"s1","kind":"nonsense",
                        "content":{},"contentHash":HASH},"now":NOW})
                )
                .await
                .unwrap_err()
            ),
            "invalid_input"
        );
        assert_eq!(
            code(
                call(&store, "inputs.consumeLater", json!({}))
                    .await
                    .unwrap_err()
            ),
            "not_implemented"
        );
    }

    #[tokio::test]
    async fn committed_input_writes_become_visible() {
        let store = store();
        let transaction = begin(&store).await;
        call(
            &transaction.view(),
            "inputs.createInput",
            input_payload("i9"),
        )
        .await
        .expect("create inside the transaction");
        transaction.commit().await.expect("commit");
        assert_eq!(
            call(&store, "inputs.getInput", json!({"inputId":"i9"}))
                .await
                .unwrap()["inputId"],
            "i9"
        );
    }

    #[tokio::test]
    async fn rolled_back_input_writes_leave_no_trace() {
        let store = store();
        let transaction = begin(&store).await;
        call(
            &transaction.view(),
            "inputs.createInput",
            input_payload("i9"),
        )
        .await
        .expect("create inside the transaction");
        transaction.rollback().await.expect("rollback");
        assert!(
            call(&store, "inputs.getInput", json!({"inputId":"i9"}))
                .await
                .unwrap()
                .is_null()
        );
    }
}
