//! The `outbox.*` port surface: the `OutboxStore` half of the assistant
//! store. Durability rules — claim windows, attempt caps, prune eligibility —
//! are owned by `agentkit-store`; the port-shaped publish/fail arms match the
//! reference adapter's `markPublished`/`markFailed`, which settle a record by
//! id without a claim token.

use super::ports::{
    instant, instant_or_now, nonnegative_i64, opt_i64, opt_instant, opt_text, record, records, text,
};
use crate::errors;
use agentkit_store::{OutboxInput, SqliteStore};
use napi::bindgen_prelude::*;
use serde_json::Value;

const DEFAULT_CLAIM_VISIBILITY_MS: i64 = 30_000;
const DEFAULT_MAX_ATTEMPTS: i64 = 10;

pub(super) async fn execute(store: &SqliteStore, method: &str, input: Value) -> Result<Value> {
    match method {
        "enqueue" => {
            let source = &input["input"];
            let now = instant_or_now(&input, "now")?;
            let payload = match source.get("payload") {
                None | Some(Value::Null) => {
                    return Err(errors::invalid("input.payload must be provided"));
                }
                Some(value) => value.clone(),
            };
            let enqueued = store
                .enqueue_outbox(
                    OutboxInput {
                        id: opt_text(source, "id")?.map(str::to_owned),
                        topic: text(source, "topic")?.to_owned(),
                        run_id: opt_text(source, "runId")?.map(str::to_owned),
                        payload,
                        available_at: opt_instant(source, "availableAt")?.unwrap_or(now),
                    },
                    now,
                )
                .await
                .map_err(errors::store)?;
            Ok(record(enqueued))
        }
        "claimBatch" => {
            let source = &input["input"];
            let claimed = store
                .claim_outbox(
                    opt_text(source, "ownerId")?.unwrap_or(""),
                    instant(source, "now")?,
                    opt_i64(source, "ttlMs")?.unwrap_or(DEFAULT_CLAIM_VISIBILITY_MS),
                    nonnegative_i64(source, "limit")? as usize,
                    opt_i64(source, "maxAttempts")?
                        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
                        .try_into()
                        .map_err(|_| errors::invalid("input.maxAttempts must be positive"))?,
                )
                .await
                .map_err(errors::store)?;
            Ok(records(claimed))
        }
        "markPublished" => {
            store
                .mark_outbox_published(text(&input, "id")?, instant(&input, "at")?)
                .await
                .map_err(errors::store)?;
            Ok(Value::Null)
        }
        "markFailed" => {
            store
                .mark_outbox_failed(
                    text(&input, "id")?,
                    text(&input, "error")?.to_owned(),
                    instant(&input, "retryAt")?,
                )
                .await
                .map_err(errors::store)?;
            Ok(Value::Null)
        }
        "prune" => Ok(serde_json::json!(
            store
                .prune_outbox(
                    instant(&input, "before")?,
                    opt_i64(&input, "maxAttempts")?
                        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
                        .try_into()
                        .map_err(|_| errors::invalid("maxAttempts must be positive"))?,
                )
                .await
                .map_err(errors::store)?
        )),
        _ => Err(errors::stable(
            "not_implemented",
            "Unsupported outbox store operation",
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::store::ports::testkit::{NOW, begin, call, code, store};
    use serde_json::json;

    fn enqueue_input(id: &str) -> serde_json::Value {
        json!({"input":{"id":id,"topic":"run.finished","runId":null,"payload":{"run":"r1"}},"now":NOW})
    }

    fn claim_input() -> serde_json::Value {
        json!({"input":{"ownerId":"publisher","now":NOW,"limit":5}})
    }

    #[tokio::test]
    async fn outbox_publish_flow_through_the_port_surface() {
        let store = store();
        let enqueued = call(&store, "outbox.enqueue", enqueue_input("o1"))
            .await
            .expect("enqueued");
        assert_eq!(enqueued["id"], "o1");
        assert!(enqueued["publishedAt"].is_null());

        let batch = call(&store, "outbox.claimBatch", claim_input())
            .await
            .expect("claimed");
        assert_eq!(batch.as_array().expect("batch").len(), 1);
        assert_eq!(batch[0]["id"], "o1");

        assert!(
            call(
                &store,
                "outbox.markPublished",
                json!({"id":"o1","at":NOW + 1})
            )
            .await
            .unwrap()
            .is_null()
        );
        assert!(
            call(&store, "outbox.claimBatch", claim_input())
                .await
                .unwrap()
                .as_array()
                .expect("batch")
                .is_empty(),
            "a published record is never handed out again"
        );

        call(&store, "outbox.enqueue", enqueue_input("o2"))
            .await
            .expect("second enqueue");
        call(
            &store,
            "outbox.markFailed",
            json!({"id":"o2","error":"bus down","retryAt":NOW + 1000}),
        )
        .await
        .expect("failed");

        let pruned = call(
            &store,
            "outbox.prune",
            json!({"before":NOW + 10_000,"maxAttempts":1}),
        )
        .await
        .expect("pruned");
        assert!(
            pruned.as_i64().expect("count") >= 1,
            "the published record is prunable; the failed one is retried or pruned"
        );
    }

    #[tokio::test]
    async fn malformed_input_is_a_typed_error() {
        let store = store();
        assert_eq!(
            code(
                call(
                    &store,
                    "outbox.claimBatch",
                    json!({"input":{"ownerId":"w","now":NOW}})
                )
                .await
                .unwrap_err()
            ),
            "invalid_input",
            "a batch claim without a limit is malformed"
        );
        assert_eq!(
            code(
                call(
                    &store,
                    "outbox.enqueue",
                    json!({"input":{"topic":"t","now":NOW}})
                )
                .await
                .unwrap_err()
            ),
            "invalid_input",
            "payload is mandatory"
        );
        assert_eq!(
            code(
                call(&store, "outbox.flushAll", json!({}))
                    .await
                    .unwrap_err()
            ),
            "not_implemented"
        );
    }

    #[tokio::test]
    async fn committed_enqueue_becomes_claimable() {
        let store = store();
        let transaction = begin(&store).await;
        call(&transaction.view(), "outbox.enqueue", enqueue_input("o9"))
            .await
            .expect("enqueue inside the transaction");
        transaction.commit().await.expect("commit");
        let batch = call(&store, "outbox.claimBatch", claim_input())
            .await
            .expect("claimed");
        assert_eq!(batch.as_array().expect("batch").len(), 1);
        assert_eq!(batch[0]["id"], "o9");
    }

    #[tokio::test]
    async fn rolled_back_enqueue_leaves_no_trace() {
        let store = store();
        let transaction = begin(&store).await;
        call(&transaction.view(), "outbox.enqueue", enqueue_input("o9"))
            .await
            .expect("enqueue inside the transaction");
        transaction.rollback().await.expect("rollback");
        assert!(
            call(&store, "outbox.claimBatch", claim_input())
                .await
                .unwrap()
                .as_array()
                .expect("batch")
                .is_empty()
        );
    }
}
