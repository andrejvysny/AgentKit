//! The `tasks.*` port surface: the `TaskStore` half of the assistant store.
//! Arms are parse → engine call → serialize; transition legality, lease
//! fencing, poison counting, and the busy-scope refusal are owned by
//! `agentkit-store`.

use super::ports::{
    deserialized, fence_proof, flag, instant, instant_or_now, nonnegative_i64, opt_i64,
    opt_instant, opt_lease_token, opt_text, optional_record, record, records, required_i64,
    required_lease_token, text,
};
use crate::errors;
use agentkit_store::{ClaimInput, SqliteStore, SubmitTask, TaskStatus, TaskTransitionPatch};
use napi::bindgen_prelude::*;
use serde_json::{Value, json};

const DEFAULT_LEASE_TTL_MS: i64 = 30_000;
const DEFAULT_MAX_ATTEMPTS: i64 = 10;

pub(super) async fn execute(store: &SqliteStore, method: &str, input: Value) -> Result<Value> {
    match method {
        "createTask" => {
            let source = &input["input"];
            let payload = match source.get("payload") {
                None | Some(Value::Null) => {
                    return Err(errors::invalid("input.payload must be provided"));
                }
                Some(value) => value.clone(),
            };
            let depends_on = match source.get("dependsOn") {
                None | Some(Value::Null) => Vec::new(),
                Some(_) => super::ports::string_array(source, "dependsOn")?,
            };
            let now = instant_or_now(&input, "now")?;
            let task = store
                .submit(SubmitTask {
                    task_id: text(source, "taskId")?.to_owned(),
                    kind: text(source, "kind")?.to_owned(),
                    scope_id: text(source, "scopeId")?.to_owned(),
                    payload,
                    priority: opt_i32(source, "priority", 0)?,
                    now,
                    available_at: opt_instant(source, "availableAt")?,
                    depends_on,
                    parent_task_id: opt_text(source, "parentTaskId")?.map(str::to_owned),
                    exclusive_scope: flag(source, "exclusiveScope", false)?,
                })
                .await
                .map_err(port_error)?;
            Ok(record(task))
        }
        "getTask" => Ok(optional_record(
            store
                .get_task(text(&input, "taskId")?)
                .await
                .map_err(port_error)?,
        )),
        "interruptQueued" => Ok(json!(
            store
                .interrupt_queued(instant(&input, "before")?)
                .await
                .map_err(port_error)?
        )),
        "interruptTask" => {
            let lease_token = opt_lease_token(&input).await?;
            Ok(Value::Bool(
                store
                    .interrupt_task(
                        text(&input, "taskId")?,
                        text(&input, "reason")?,
                        lease_token,
                        instant_or_now(&input, "now")?,
                    )
                    .await
                    .map_err(port_error)?,
            ))
        }
        "listChildren" => Ok(records(
            store
                .list_children(text(&input, "taskId")?)
                .await
                .map_err(port_error)?,
        )),
        "listByScope" => Ok(records(
            store
                .list_tasks(Some(text(&input, "scopeId")?.to_owned()))
                .await
                .map_err(port_error)?,
        )),
        "deleteByScope" => {
            // The port's refusal names what holds the scope: this check only
            // builds a helpful error — the engine's `delete_by_scope` stays
            // the atomic all-or-nothing guard.
            let scope_id = text(&input, "scopeId")?;
            let busy: Vec<(String, String)> = store
                .list_tasks(Some(scope_id.to_owned()))
                .await
                .map_err(port_error)?
                .into_iter()
                .filter(|task| {
                    matches!(
                        task.status,
                        TaskStatus::Running | TaskStatus::Interrupted | TaskStatus::WaitingApproval
                    )
                })
                .map(|task| {
                    let status = match serde_json::to_value(task.status) {
                        Ok(Value::String(status)) => status,
                        _ => "running".to_owned(),
                    };
                    (task.task_id, status)
                })
                .collect();
            if !busy.is_empty() {
                return Err(errors::stable(
                    "chat_busy",
                    format!("scope busy: {scope_id}"),
                    Some(json!({
                        "taskIds": busy.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
                        "statuses": busy.iter().map(|(_, status)| status.clone()).collect::<Vec<_>>(),
                    })),
                ));
            }
            Ok(json!(
                store.delete_by_scope(scope_id).await.map_err(port_error)?
            ))
        }
        "transitionTask" => {
            let from: Vec<TaskStatus> = deserialized(&input, "from")?;
            let to: TaskStatus = deserialized(&input, "to")?;
            let patch = input.get("patch").cloned().unwrap_or(Value::Null);
            let lease_token = opt_lease_token(&input).await?;
            let task = store
                .transition_task(
                    text(&input, "taskId")?,
                    from,
                    to,
                    TaskTransitionPatch {
                        started_at: opt_instant(&patch, "startedAt")?,
                        finished_at: opt_instant(&patch, "finishedAt")?,
                        error: opt_text(&patch, "error")?.map(str::to_owned),
                        available_at: opt_instant(&patch, "availableAt")?,
                        priority: match patch.get("priority") {
                            None | Some(Value::Null) => None,
                            Some(_) => {
                                Some(i32::try_from(required_i64(&patch, "priority")?).map_err(
                                    |_| errors::invalid("patch.priority is out of range"),
                                )?)
                            }
                        },
                        poison_count: match patch.get("poisonCount") {
                            None | Some(Value::Null) => None,
                            Some(_) => Some(
                                u32::try_from(nonnegative_i64(&patch, "poisonCount")?).map_err(
                                    |_| errors::invalid("patch.poisonCount is out of range"),
                                )?,
                            ),
                        },
                        payload: match patch.get("payload") {
                            None | Some(Value::Null) => None,
                            Some(value) => Some(value.clone()),
                        },
                    },
                    lease_token,
                    instant_or_now(&input, "now")?,
                )
                .await
                .map_err(port_error)?;
            Ok(record(task))
        }
        "createAttempt" => {
            let source = &input["input"];
            Ok(record(
                store
                    .create_attempt(
                        text(source, "taskId")?,
                        text(source, "attemptId")?,
                        text(source, "ownerId")?,
                        instant_or_now(&input, "now")?,
                    )
                    .await
                    .map_err(port_error)?,
            ))
        }
        "endAttempt" => {
            let source = &input["input"];
            Ok(record(
                store
                    .end_attempt(
                        text(source, "attemptId")?,
                        text(source, "status")?,
                        opt_text(source, "error")?.map(str::to_owned),
                        opt_text(source, "leaseToken")?.map(str::to_owned),
                        instant_or_now(&input, "now")?,
                    )
                    .await
                    .map_err(port_error)?,
            ))
        }
        "acquireLease" => {
            let source = &input["input"];
            Ok(record(
                store
                    .acquire_lease(
                        text(source, "taskId")?,
                        text(source, "attemptId")?,
                        text(source, "ownerId")?,
                        instant_or_now(&input, "now")?,
                        nonnegative_i64(source, "ttlMs")?,
                    )
                    .await
                    .map_err(port_error)?,
            ))
        }
        "renewLease" => Ok(record(
            store
                .renew_lease_by_token(
                    text(&input, "leaseToken")?,
                    instant_or_now(&input, "now")?,
                    nonnegative_i64(&input, "ttlMs")?,
                )
                .await
                .map_err(port_error)?,
        )),
        "releaseLease" => {
            store
                .release_lease(text(&input, "leaseToken")?)
                .await
                .map_err(port_error)?;
            Ok(Value::Null)
        }
        "expireStaleLeases" => Ok(records(
            store
                .expire_stale_leases(instant(&input, "now")?)
                .await
                .map_err(port_error)?,
        )),
        "appendEvents" => {
            let events = input["events"]
                .as_array()
                .ok_or_else(|| errors::invalid("events must be an array"))?
                .clone();
            let lease_token = required_lease_token(&input).await?;
            let proof = fence_proof(store, text(&input, "taskId")?, &lease_token).await?;
            store
                .append_events(proof, events, instant_or_now(&input, "now")?)
                .await
                .map_err(port_error)?;
            Ok(Value::Null)
        }
        "listEvents" => {
            let opts = input.get("opts").cloned().unwrap_or(Value::Null);
            let after_seq = match opts.get("afterSeq") {
                None | Some(Value::Null) => None,
                Some(_) => Some(
                    opts["afterSeq"]
                        .as_f64()
                        .filter(|number| number.is_finite())
                        .ok_or_else(|| errors::invalid("opts.afterSeq must be a number"))?,
                ),
            };
            let limit = match opts.get("limit") {
                None | Some(Value::Null) => usize::MAX,
                Some(_) => usize::try_from(nonnegative_i64(&opts, "limit")?)
                    .map_err(|_| errors::invalid("opts.limit is out of range"))?,
            };
            match store
                .events_after_number(text(&input, "taskId")?, after_seq, limit)
                .await
            {
                // A task that is gone has no log; the reference answers with
                // the empty listing rather than NotFound.
                Ok(rows) => Ok(records(rows)),
                Err(agentkit_store::StoreError::NotFound(_)) => Ok(json!([])),
                Err(error) => Err(port_error(error)),
            }
        }
        "updateProgress" => {
            let progress = match input.get("progress") {
                Some(Value::Object(_)) => input["progress"].clone(),
                _ => return Err(errors::invalid("progress must be an object")),
            };
            let task_id = text(&input, "taskId")?.to_owned();
            let lease_token = required_lease_token(&input).await?;
            let proof = fence_proof(store, &task_id, &lease_token).await?;
            store
                .update_progress(proof, progress, instant_or_now(&input, "now")?)
                .await
                .map_err(port_error)?;
            Ok(optional_record(
                store.get_task(&task_id).await.map_err(port_error)?,
            ))
        }
        "nextSeq" => match store.next_sequence(text(&input, "taskId")?).await {
            // The port's numbering of a task that is gone restarts at 0;
            // a task that was just deleted (cascade) must not surface NotFound.
            Ok(sequence) => Ok(json!(sequence)),
            Err(agentkit_store::StoreError::NotFound(_)) => Ok(json!(0)),
            Err(error) => Err(port_error(error)),
        },
        "claimNext" => {
            let source = &input["input"];
            let kinds = match source.get("kinds") {
                None | Some(Value::Null) => Vec::new(),
                Some(_) => super::ports::string_array(source, "kinds")?,
            };
            let busy_scopes = match source.get("scopesBusy") {
                None | Some(Value::Null) => Vec::new(),
                Some(_) => super::ports::string_array(source, "scopesBusy")?,
            };
            let claimed = store
                .claim(ClaimInput {
                    owner_id: text(source, "ownerId")?.to_owned(),
                    now: instant(source, "now")?,
                    ttl_ms: opt_i64(source, "ttlMs")?.unwrap_or(DEFAULT_LEASE_TTL_MS),
                    kinds,
                    busy_scopes,
                    max_attempts: opt_i64(source, "maxAttempts")?
                        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
                        .try_into()
                        .map_err(|_| errors::invalid("input.maxAttempts must be positive"))?,
                })
                .await
                .map_err(port_error)?;
            let Some(claimed) = claimed else {
                return Ok(Value::Null);
            };
            // The claim minted an attempt alongside its lease; attach the
            // record the lease names so the port's `{task, attempt, lease}`
            // shape arrives whole.
            let attempt_id = claimed.lease.attempt_id.clone();
            let attempt = store
                .attempts(&claimed.task.task_id)
                .await
                .map_err(port_error)?
                .into_iter()
                .find(|attempt| attempt.attempt_id == attempt_id);
            Ok(json!({
                "task": record(claimed.task),
                "attempt": attempt.map(record).unwrap_or(Value::Null),
                "lease": record(claimed.lease),
            }))
        }
        "markDeadLettered" => {
            let lease_token = opt_lease_token(&input).await?;
            Ok(record(
                store
                    .mark_dead_lettered(
                        text(&input, "taskId")?,
                        text(&input, "reason")?,
                        lease_token,
                        instant_or_now(&input, "now")?,
                    )
                    .await
                    .map_err(port_error)?,
            ))
        }
        _ => Err(errors::stable(
            "not_implemented",
            "Unsupported task store operation",
            None,
        )),
    }
}

/// Port-surface error mapping: transition legality and duplicate task ids
/// carry the `TaskStore`'s own codes; everything else maps as `errors::store`.
fn port_error(error: agentkit_store::StoreError) -> napi::Error {
    crate::errors::store_port(error, "invalid_task_transition", "duplicate_task")
}

fn opt_i32(input: &Value, field: &str, default: i32) -> Result<i32> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(_) => i32::try_from(required_i64(input, field)?)
            .map_err(|_| errors::invalid(format!("{field} is out of range"))),
    }
}

#[cfg(test)]
mod tests {
    use crate::store::ports::testkit::{NOW, begin, call, code, store};
    use serde_json::json;

    async fn submit(store: &agentkit_store::SqliteStore, task_id: &str, scope_id: &str) {
        call(
            store,
            "tasks.createTask",
            json!({"input":{"taskId":task_id,"kind":"chat_turn","scopeId":scope_id,
                "payload":{"chatId":"c1"},"availableAt":NOW},"now":NOW}),
        )
        .await
        .expect("task submitted");
    }

    #[tokio::test]
    async fn task_lifecycle_through_the_port_surface() {
        let store = store();
        submit(&store, "t1", "s1").await;
        let task = call(&store, "tasks.getTask", json!({"taskId":"t1"}))
            .await
            .unwrap();
        assert_eq!(task["taskId"], "t1");
        assert_eq!(task["status"], "queued");
        assert!(
            call(&store, "tasks.getTask", json!({"taskId":"ghost"}))
                .await
                .unwrap()
                .is_null()
        );

        let claimed = call(
            &store,
            "tasks.claimNext",
            json!({"input":{"ownerId":"w1","now":NOW}}),
        )
        .await
        .expect("claimed");
        assert_eq!(claimed["task"]["status"], "running");
        assert_eq!(claimed["attempt"]["status"], "running");
        let token = claimed["lease"]["leaseToken"]
            .as_str()
            .expect("claim minted a lease")
            .to_owned();

        call(
            &store,
            "tasks.appendEvents",
            json!({"taskId":"t1","events":[{"type":"progress","seq":1,"eventId":"ev1",
                "timestamp":"2026-11-01T00:00:00.000Z","contractVersion":"1"}],
                "opts":{"leaseToken":token},"now":NOW}),
        )
        .await
        .expect("events appended");
        let events = call(&store, "tasks.listEvents", json!({"taskId":"t1","opts":{}}))
            .await
            .unwrap();
        assert_eq!(events.as_array().expect("events").len(), 1);
        assert_eq!(events[0]["event"]["eventId"], "ev1");
        assert_eq!(
            call(&store, "tasks.nextSeq", json!({"taskId":"t1"}))
                .await
                .unwrap()
                .as_f64(),
            Some(2.0)
        );

        let progressed = call(
            &store,
            "tasks.updateProgress",
            json!({"taskId":"t1","progress":{"percent":50},"opts":{"leaseToken":token},"now":NOW}),
        )
        .await
        .expect("progress stored");
        assert_eq!(progressed["progress"]["percent"], 50);

        call(
            &store,
            "tasks.renewLease",
            json!({"leaseToken":token,"ttlMs":60_000,"now":NOW}),
        )
        .await
        .expect("lease renewed");
        call(&store, "tasks.releaseLease", json!({"leaseToken":token}))
            .await
            .expect("lease released");

        assert_eq!(
            call(&store, "tasks.listByScope", json!({"scopeId":"s1"}))
                .await
                .unwrap()
                .as_array()
                .expect("scope listing")
                .len(),
            1
        );
        assert_eq!(
            call(
                &store,
                "tasks.interruptQueued",
                json!({"before":NOW + 10_000})
            )
            .await
            .unwrap(),
            0,
            "the only task is running, not queued"
        );
    }

    #[tokio::test]
    async fn malformed_input_and_missing_records_are_typed_errors() {
        let store = store();
        assert_eq!(
            code(
                call(
                    &store,
                    "tasks.createTask",
                    json!({"input":{"taskId":"t","kind":"k","scopeId":"s"}})
                )
                .await
                .unwrap_err()
            ),
            "invalid_input",
            "a task without a payload is malformed input"
        );
        assert_eq!(
            code(
                call(
                    &store,
                    "tasks.claimNext",
                    json!({"input":{"ownerId":"w","now":NOW,"maxAttempts":0}})
                )
                .await
                .unwrap_err()
            ),
            "invalid_input"
        );
        assert_eq!(
            code(
                call(
                    &store,
                    "tasks.transitionTask",
                    json!({"taskId":"ghost","from":["queued"],"to":"running","now":NOW})
                )
                .await
                .unwrap_err()
            ),
            "not_found"
        );
        submit(&store, "t2", "s2").await;
        assert_eq!(
            code(
                call(
                    &store,
                    "tasks.appendEvents",
                    json!({"taskId":"t2","events":[],"opts":{}})
                )
                .await
                .unwrap_err()
            ),
            "invalid_input",
            "fenced event writes demand the lease token"
        );
        assert_eq!(
            code(
                call(&store, "tasks.frobnicate", json!({}))
                    .await
                    .unwrap_err()
            ),
            "not_implemented"
        );
    }

    #[tokio::test]
    async fn claimed_tasks_and_leases_survive_commit() {
        let store = store();
        let transaction = begin(&store).await;
        let view = transaction.view();
        submit(&view, "t5", "s5").await;
        let claimed = call(
            &view,
            "tasks.claimNext",
            json!({"input":{"ownerId":"w1","now":NOW,"kinds":["chat_turn"]}}),
        )
        .await
        .expect("claim inside the transaction");
        assert_eq!(claimed["task"]["status"], "running");
        transaction.commit().await.expect("commit");
        let task = call(&store, "tasks.getTask", json!({"taskId":"t5"}))
            .await
            .unwrap();
        assert_eq!(task["status"], "running", "the claim persisted atomically");
    }

    #[tokio::test]
    async fn rolled_back_task_writes_leave_no_trace() {
        let store = store();
        let transaction = begin(&store).await;
        let view = transaction.view();
        submit(&view, "t6", "s6").await;
        call(
            &view,
            "tasks.createAttempt",
            json!({"input":{"taskId":"t6","attemptId":"a6","ownerId":"w"},"now":NOW}),
        )
        .await
        .expect("attempt inside the transaction");
        transaction.rollback().await.expect("rollback");
        assert!(
            call(&store, "tasks.getTask", json!({"taskId":"t6"}))
                .await
                .unwrap()
                .is_null()
        );
        assert!(
            call(&store, "tasks.listByScope", json!({"scopeId":"s6"}))
                .await
                .unwrap()
                .as_array()
                .expect("scope listing")
                .is_empty()
        );
    }
}
