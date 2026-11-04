//! The `proposals.*` port surface: the `ProposalStore` half of the assistant
//! store. Arms parse the port's parameters, call the engine, and serialize
//! the store's native records; the transition machine, apply-claim binding,
//! and receipt uniqueness are owned by `agentkit-store`.

use super::ports::{
    deserialized, instant, instant_or_now, nonnegative_i64, opt_text, optional_record, record,
    records, text,
};
use crate::errors;
use agentkit_store::{ApplyOutcome, ProposalPatch, ProposalRecord, ProposalStatus, SqliteStore};
use napi::bindgen_prelude::*;
use serde_json::{Value, json};

pub(super) async fn execute(store: &SqliteStore, method: &str, input: Value) -> Result<Value> {
    match method {
        "create" => {
            let source = &input["input"];
            let operations = source["operations"]
                .as_array()
                .ok_or_else(|| errors::invalid("input.operations must be an array"))?
                .clone();
            let warnings = super::ports::string_array(source, "warnings")?;
            let truncated = match source.get("truncated") {
                Some(Value::Bool(value)) => *value,
                _ => return Err(errors::invalid("input.truncated must be a boolean")),
            };
            let proposal = ProposalRecord {
                id: text(source, "id")?.to_owned(),
                chat_id: text(source, "chatId")?.to_owned(),
                run_id: opt_text(source, "runId")?.map(str::to_owned),
                scope_key: text(source, "scopeKey")?.to_owned(),
                action_id: opt_text(source, "actionId")?.map(str::to_owned),
                tool_name: text(source, "toolName")?.to_owned(),
                kind: text(source, "kind")?.to_owned(),
                risk: text(source, "risk")?.to_owned(),
                status: ProposalStatus::Pending,
                envelope: match source.get("envelope") {
                    None | Some(Value::Null) => {
                        return Err(errors::invalid("input.envelope must be provided"));
                    }
                    Some(value) => value.clone(),
                },
                operations,
                warnings,
                truncated,
                revision_at_create: opt_text(source, "revisionAtCreate")?.map(str::to_owned),
                operation_id: None,
                decision: None,
                reason: None,
                created_at: instant(source, "createdAt")?,
                decided_at: None,
                claimed_at: None,
                applied_at: None,
            };
            Ok(record(
                store.create_proposal(proposal).await.map_err(port_error)?,
            ))
        }
        "get" => Ok(optional_record(
            store
                .get_proposal(text(&input, "proposalId")?)
                .await
                .map_err(errors::store)?,
        )),
        "getByActionId" => Ok(optional_record(
            store
                .proposal_by_action(text(&input, "scopeKey")?, text(&input, "actionId")?)
                .await
                .map_err(errors::store)?,
        )),
        "listByChat" => {
            let opts = input.get("opts").cloned().unwrap_or(Value::Null);
            let status = match opts.get("status") {
                None | Some(Value::Null) => None,
                Some(_) => Some(deserialized(&opts, "status")?),
            };
            let mut proposals = store
                .proposals(Some(text(&input, "chatId")?.to_owned()), status)
                .await
                .map_err(errors::store)?;
            if let Some(limit) = opt_limit(&opts)? {
                proposals.truncate(limit);
            }
            Ok(records(proposals))
        }
        "listByStatus" => {
            let status: ProposalStatus = deserialized(&input, "status")?;
            let opts = input.get("opts").cloned().unwrap_or(Value::Null);
            let mut proposals = store
                .proposals(None, Some(status))
                .await
                .map_err(errors::store)?;
            if let Some(limit) = opt_limit(&opts)? {
                proposals.truncate(limit);
            }
            Ok(records(proposals))
        }
        "transition" => {
            let from: Vec<ProposalStatus> = deserialized(&input, "from")?;
            let to: ProposalStatus = deserialized(&input, "to")?;
            let patch = input.get("patch").cloned().unwrap_or(Value::Null);
            // `decidedAt`/`claimedAt`/`appliedAt` are accepted for wire
            // compatibility; the engine stamps those instants itself (the
            // port adapter routes them through `now`).
            let proposal = store
                .transition_proposal(
                    text(&input, "proposalId")?,
                    from,
                    to,
                    ProposalPatch {
                        operation_id: opt_text(&patch, "operationId")?.map(str::to_owned),
                        decision: match patch.get("decision") {
                            None | Some(Value::Null) => None,
                            Some(value) => Some(value.clone()),
                        },
                        reason: opt_text(&patch, "reason")?.map(str::to_owned),
                    },
                    instant_or_now(&input, "now")?,
                )
                .await
                .map_err(port_error)?;
            Ok(record(proposal))
        }
        "complete" => {
            // The receipt-bound terminal apply behind the port's
            // `transition(…, "applied" | "failed")`.
            let outcome: ApplyOutcome = deserialized(&input, "outcome")?;
            let receipt = store
                .complete_proposal(
                    text(&input, "proposalId")?,
                    text(&input, "operationId")?,
                    outcome,
                    instant_or_now(&input, "now")?,
                )
                .await
                .map_err(port_error)?;
            Ok(record(receipt))
        }
        "finalize" => {
            // The receipt-bound terminal apply WITH the CAS's expected-state
            // comparison: `from`/`to` are re-checked against the proposal at
            // write time inside the engine transaction, so a competing
            // terminal request that raced past the facade's read is refused
            // (`invalid_proposal_transition`) instead of silently succeeding,
            // and the patch's `reason`/`decision` land with the stamp.
            let from: Vec<ProposalStatus> = deserialized(&input, "from")?;
            let to: ProposalStatus = deserialized(&input, "to")?;
            let outcome: ApplyOutcome = deserialized(&input, "outcome")?;
            let patch = input.get("patch").cloned().unwrap_or(Value::Null);
            let receipt = store
                .finalize_apply(agentkit_store::FinalizeApply {
                    proposal_id: text(&input, "proposalId")?.to_owned(),
                    operation_id: text(&input, "operationId")?.to_owned(),
                    outcome,
                    from,
                    to,
                    patch: ProposalPatch {
                        operation_id: opt_text(&patch, "operationId")?.map(str::to_owned),
                        decision: match patch.get("decision") {
                            None | Some(Value::Null) => None,
                            Some(value) => Some(value.clone()),
                        },
                        reason: opt_text(&patch, "reason")?.map(str::to_owned),
                    },
                    now: instant_or_now(&input, "now")?,
                })
                .await
                .map_err(port_error)?;
            Ok(record(receipt))
        }
        "recordOutcome" => {
            let outcome: ApplyOutcome = deserialized(&input, "outcome")?;
            Ok(record(
                store
                    .record_outcome(
                        text(&input, "operationId")?,
                        outcome,
                        instant_or_now(&input, "now")?,
                    )
                    .await
                    .map_err(errors::store)?,
            ))
        }
        "getOutcome" => Ok(optional_record(
            store
                .receipt(text(&input, "operationId")?)
                .await
                .map_err(errors::store)?,
        )),
        "invalidatePendingForRevision" => Ok(json!(
            store
                .invalidate_pending(text(&input, "scopeKey")?, text(&input, "newRevision")?)
                .await
                .map_err(errors::store)?
        )),
        "deleteByChat" => Ok(json!(
            store
                .delete_proposals_by_chat(text(&input, "chatId")?)
                .await
                .map_err(errors::store)?
        )),
        _ => Err(errors::stable(
            "not_implemented",
            "Unsupported proposal store operation",
            None,
        )),
    }
}

/// Port-surface error mapping: the proposal machine's own refusal codes —
/// transition legality is `invalid_proposal_transition`, and a held
/// (scopeKey, actionId) reservation is the port's public `duplicate_action_id`
/// (the same refusal the reference adapters raise).
fn port_error(error: agentkit_store::StoreError) -> napi::Error {
    crate::errors::store_port(error, "invalid_proposal_transition", "duplicate_action_id")
}

fn opt_limit(opts: &Value) -> Result<Option<usize>> {
    match opts.get("limit") {
        None | Some(Value::Null) => Ok(None),
        Some(_) => Ok(Some(nonnegative_i64(opts, "limit")? as usize)),
    }
}

#[cfg(test)]
mod tests {
    use crate::store::ports::testkit::{NOW, begin, call, code, store};
    use serde_json::json;

    async fn seed_chat(store: &agentkit_store::SqliteStore) {
        call(
            store,
            "conversations.createChat",
            json!({"input":{"id":"c1"},"now":NOW}),
        )
        .await
        .expect("chat created");
    }

    fn proposal_input(id: &str) -> serde_json::Value {
        json!({"input":{"id":id,"chatId":"c1","scopeKey":"rev-1","actionId":"act-1",
            "toolName":"write_file","kind":"write","risk":"low",
            "envelope":{"path":"notes.md"},"operations":[{"op":"write"}],
            "warnings":[],"truncated":false,"revisionAtCreate":"r1","createdAt":NOW},"now":NOW})
    }

    #[tokio::test]
    async fn proposal_flow_through_the_port_surface() {
        let store = store();
        seed_chat(&store).await;
        let created = call(&store, "proposals.create", proposal_input("p1"))
            .await
            .expect("proposal created");
        assert_eq!(created["id"], "p1");
        assert_eq!(created["status"], "pending");
        assert_eq!(
            call(&store, "proposals.get", json!({"proposalId":"p1"}))
                .await
                .unwrap()["id"],
            "p1"
        );
        assert!(
            call(&store, "proposals.get", json!({"proposalId":"ghost"}))
                .await
                .unwrap()
                .is_null()
        );
        assert_eq!(
            call(
                &store,
                "proposals.getByActionId",
                json!({"scopeKey":"rev-1","actionId":"act-1"})
            )
            .await
            .unwrap()["id"],
            "p1"
        );
        assert_eq!(
            call(&store, "proposals.listByChat", json!({"chatId":"c1"}))
                .await
                .unwrap()
                .as_array()
                .expect("list")
                .len(),
            1
        );
        assert_eq!(
            call(
                &store,
                "proposals.listByStatus",
                json!({"status":"pending","opts":{"limit":5}})
            )
            .await
            .unwrap()
            .as_array()
            .expect("list")
            .len(),
            1
        );

        let transitioned = call(
            &store,
            "proposals.transition",
            json!({"proposalId":"p1","from":["pending"],"to":"approved",
                "patch":{"decision":{"approved":true},"reason":"ok"},"now":NOW}),
        )
        .await
        .expect("approved");
        assert_eq!(transitioned["status"], "approved");
        let claimed = call(
            &store,
            "proposals.transition",
            json!({"proposalId":"p1","from":["approved"],"to":"applying",
                "patch":{"operationId":"op1"},"now":NOW}),
        )
        .await
        .expect("apply claimed");
        assert_eq!(claimed["status"], "applying");
        assert_eq!(claimed["operationId"], "op1");
        assert!(
            call(&store, "proposals.getOutcome", json!({"operationId":"op1"}))
                .await
                .unwrap()
                .is_null(),
            "no outcome has been recorded yet"
        );
        let receipt = call(
            &store,
            "proposals.recordOutcome",
            json!({"operationId":"op1","outcome":{"status":"applied","appliedOps":1,
                "failedOps":[],"resultJson":null,"revision":null},"now":NOW}),
        )
        .await
        .expect("outcome recorded");
        assert_eq!(receipt["operationId"], "op1");
        assert_eq!(receipt["outcome"]["status"], "applied");
        assert_eq!(
            call(&store, "proposals.getOutcome", json!({"operationId":"op1"}))
                .await
                .unwrap()["outcome"]["status"],
            "applied"
        );

        let mut second = proposal_input("p2");
        second["input"]["actionId"] = json!("act-2");
        call(&store, "proposals.create", second)
            .await
            .expect("second pending proposal");
        assert_eq!(
            call(
                &store,
                "proposals.invalidatePendingForRevision",
                json!({"scopeKey":"rev-1","newRevision":"r2"})
            )
            .await
            .unwrap(),
            1,
            "the newer revision invalidates the remaining pending proposal"
        );
        assert_eq!(
            call(&store, "proposals.deleteByChat", json!({"chatId":"c1"}))
                .await
                .unwrap(),
            2,
            "both proposals leave with the chat"
        );
        assert!(
            call(&store, "proposals.get", json!({"proposalId":"p2"}))
                .await
                .unwrap()
                .is_null()
        );
    }

    #[tokio::test]
    async fn malformed_input_and_missing_parents_are_typed_errors() {
        let store = store();
        // The port treats `chatId` as a free-form association, not a foreign
        // key: a proposal for a chat the store has never seen is a legitimate
        // write (the shared conformance suite creates exactly this shape).
        let orphan = call(&store, "proposals.create", proposal_input("p1"))
            .await
            .expect("a proposal may reference an unknown chat");
        assert_eq!(orphan["id"], "p1");
        seed_chat(&store).await;
        let mut malformed = proposal_input("p3");
        malformed["input"]
            .as_object_mut()
            .expect("object")
            .remove("envelope");
        assert_eq!(
            code(
                call(&store, "proposals.create", malformed)
                    .await
                    .unwrap_err()
            ),
            "invalid_input"
        );
        assert_eq!(
            code(
                call(
                    &store,
                    "proposals.create",
                    json!({"input":{"id":"p4","chatId":"c1","scopeKey":"k","toolName":"t",
                        "kind":"write","risk":"imminent","envelope":{},"operations":[],
                        "warnings":[],"truncated":false,"createdAt":NOW}})
                )
                .await
                .unwrap_err()
            ),
            "invalid_input",
            "the risk vocabulary is enforced by the store"
        );
        assert_eq!(
            code(
                call(&store, "proposals.circulate", json!({}))
                    .await
                    .unwrap_err()
            ),
            "not_implemented"
        );
    }

    #[tokio::test]
    async fn duplicate_action_id_and_competing_terminals_carry_stable_port_codes() {
        // (External review, section 3.) Through the port surface — with no
        // TypeScript facade pre-check in front — a held (scopeKey, actionId)
        // is refused with the port's public `duplicate_action_id`, never an
        // opaque internal error, and the loser adds no row. And the
        // receipt-bound terminal apply carries the CAS's expected-state
        // comparison: the second competing request is refused
        // `invalid_proposal_transition`, the committed receipt is untouched,
        // and the winner's patch reason lands with the stamp.
        let store = store();
        seed_chat(&store).await;
        call(&store, "proposals.create", proposal_input("p1"))
            .await
            .expect("first proposal");
        assert_eq!(
            code(
                call(&store, "proposals.create", proposal_input("p2"))
                    .await
                    .unwrap_err()
            ),
            "duplicate_action_id",
            "the reservation refusal is a public code, not internal_error"
        );
        assert_eq!(
            call(&store, "proposals.listByChat", json!({"chatId":"c1"}))
                .await
                .unwrap()
                .as_array()
                .expect("list")
                .len(),
            1,
            "the refused create left no second row"
        );

        call(
            &store,
            "proposals.transition",
            json!({"proposalId":"p1","from":["pending"],"to":"approved","now":NOW}),
        )
        .await
        .expect("approved");
        call(
            &store,
            "proposals.transition",
            json!({"proposalId":"p1","from":["approved"],"to":"applying",
                "patch":{"operationId":"op1"},"now":NOW}),
        )
        .await
        .expect("apply claimed");
        call(
            &store,
            "proposals.recordOutcome",
            json!({"operationId":"op1","outcome":{"status":"applied","appliedOps":1,
                "failedOps":[],"resultJson":null,"revision":null},"now":NOW}),
        )
        .await
        .expect("outcome recorded");
        let receipt = call(
            &store,
            "proposals.finalize",
            json!({"proposalId":"p1","operationId":"op1",
                "outcome":{"status":"applied","appliedOps":1,"failedOps":[]},
                "from":["applying"],"to":"applied","patch":{},"now":NOW}),
        )
        .await
        .expect("winner finalized");
        assert_eq!(receipt["outcome"]["status"], "applied");
        assert_eq!(
            code(
                call(
                    &store,
                    "proposals.finalize",
                    json!({"proposalId":"p1","operationId":"op1",
                        "outcome":{"status":"applied","appliedOps":1,"failedOps":[]},
                        "from":["applying"],"to":"failed",
                        "patch":{"reason":"late rival"},"now":NOW}),
                )
                .await
                .unwrap_err()
            ),
            "invalid_proposal_transition",
            "the loser of two competing terminal transitions is refused"
        );
        let done = call(&store, "proposals.get", json!({"proposalId":"p1"}))
            .await
            .unwrap();
        assert_eq!(done["status"], "applied", "only the winner stamped");
        assert!(done["reason"].is_null(), "the loser wrote no patch");
        assert_eq!(
            call(&store, "proposals.getOutcome", json!({"operationId":"op1"}))
                .await
                .unwrap()["outcome"]["status"],
            "applied",
            "the committed receipt is untouched by the refused rival"
        );

        // The winner's patch reason lands with the stamp on a fresh apply.
        let mut other = proposal_input("p9");
        other["input"]["actionId"] = json!("act-9");
        call(&store, "proposals.create", other)
            .await
            .expect("second proposal");
        call(
            &store,
            "proposals.transition",
            json!({"proposalId":"p9","from":["pending"],"to":"approved","now":NOW}),
        )
        .await
        .expect("approved");
        call(
            &store,
            "proposals.transition",
            json!({"proposalId":"p9","from":["approved"],"to":"applying",
                "patch":{"operationId":"op9"},"now":NOW}),
        )
        .await
        .expect("apply claimed");
        let failed_outcome = json!({"status":"failed","appliedOps":0,
            "failedOps":[{"opIndex":0,"error":"apply failed"}]});
        call(
            &store,
            "proposals.recordOutcome",
            json!({"operationId":"op9","outcome":failed_outcome,"now":NOW}),
        )
        .await
        .expect("failure recorded");
        call(
            &store,
            "proposals.finalize",
            json!({"proposalId":"p9","operationId":"op9",
                "outcome":failed_outcome,"from":["applying"],"to":"failed",
                "patch":{"reason":"applier said so"},"now":NOW}),
        )
        .await
        .expect("failed finalized with its reason");
        assert_eq!(
            call(&store, "proposals.get", json!({"proposalId":"p9"}))
                .await
                .unwrap()["reason"],
            "applier said so"
        );
    }

    #[tokio::test]
    async fn committed_proposal_writes_become_visible() {
        let store = store();
        seed_chat(&store).await;
        let transaction = begin(&store).await;
        let view = transaction.view();
        call(&view, "proposals.create", proposal_input("p7"))
            .await
            .expect("create inside the transaction");
        call(
            &view,
            "proposals.transition",
            json!({"proposalId":"p7","from":["pending"],"to":"rejected","now":NOW}),
        )
        .await
        .expect("reject inside the transaction");
        transaction.commit().await.expect("commit");
        assert_eq!(
            call(&store, "proposals.get", json!({"proposalId":"p7"}))
                .await
                .unwrap()["status"],
            "rejected"
        );
    }

    #[tokio::test]
    async fn rolled_back_proposal_writes_leave_no_trace() {
        let store = store();
        seed_chat(&store).await;
        let transaction = begin(&store).await;
        call(
            &transaction.view(),
            "proposals.create",
            proposal_input("p8"),
        )
        .await
        .expect("create inside the transaction");
        transaction.rollback().await.expect("rollback");
        assert!(
            call(&store, "proposals.get", json!({"proposalId":"p8"}))
                .await
                .unwrap()
                .is_null()
        );
        assert!(
            call(&store, "proposals.listByChat", json!({"chatId":"c1"}))
                .await
                .unwrap()
                .as_array()
                .expect("list")
                .is_empty()
        );
    }
}
