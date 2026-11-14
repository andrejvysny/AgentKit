//! Idempotent admission of [`AgentSessionService`]: prior-receipt lookup,
//! admission envelopes and digests, frozen native-stanza validation, task
//! submission, and the host's settlement flush planner.

use crate::{
    AgentInputRecord, AgentInputStatus, CreateAgentInput, MessageInput, SESSION_METADATA_KEY,
    SessionBackend, SessionFault, SessionSettle, SessionsError, SessionsResult, StagedCandidate,
};
use agentkit_engine::{SubmitTask, TaskRecord};
use agentkit_kernel::sha256_hex;
use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::{
    AgentSendResult, AgentSessionService, QueueKind, session_turn_kind, turn_id_for, turn_result,
};

impl<R: SessionBackend> AgentSessionService<R> {
    // -- internals -----------------------------------------------------------

    /// The earlier outcome of this `inputId`, or `None` when it is new.
    pub(super) async fn prior_receipt(
        &self,
        session_id: &str,
        kind: QueueKind,
        input_id: &str,
        content: &Value,
        frozen: Option<&Map<String, Value>>,
    ) -> SessionsResult<Option<AgentSendResult>, SessionFault<R>> {
        if let Some(existing) = self.store().get_input(input_id.to_owned()).await? {
            // createInput is the idempotency check: same input (content AND
            // frozen stanza) replays, a different one is `input_conflict`.
            let envelope = self.admission_envelope(content, frozen);
            let (record, _) = self
                .store()
                .create_input(
                    CreateAgentInput {
                        input_id: input_id.to_owned(),
                        session_id: session_id.to_owned(),
                        kind: kind.input_kind(),
                        content: envelope.clone(),
                        content_hash: self.hash_of(kind, &envelope),
                        target_task_id: existing.target_task_id.clone(),
                        on_settled: existing.on_settled,
                        attempt_id: None,
                        tool_call_id: None,
                        expires_at: None,
                    },
                    self.now_ms(),
                )
                .await?;
            return Ok(Some(self.receipt_of(record).await?));
        }
        // No input row: an IMMEDIATE admission is its own idempotency
        // record — the turn itself (review round-4 F8-a). The digest
        // persisted at submit is the SAME queued-path digest over the
        // SAME envelope; a replay under one input id with swapped content
        // or frozen bytes conflicts, never silently returns the old turn.
        let turn = self
            .store()
            .get_task(&turn_id_for(input_id))
            .await?
            .filter(|task| task.scope_id == session_id);
        match turn {
            Some(task) => {
                if let Some(recorded) = task.payload.get("admissionDigest").and_then(Value::as_str)
                {
                    let digest = self.admission_digest(kind, content, frozen);
                    if digest != recorded {
                        return Err(SessionsError::InputConflict(format!(
                            "input {input_id} is already admitted under different content or frozen bytes"
                        )));
                    }
                }
                Ok(Some(turn_result(task)))
            }
            None => Ok(None),
        }
    }

    pub(super) async fn receipt_of(
        &self,
        record: AgentInputRecord,
    ) -> SessionsResult<AgentSendResult, SessionFault<R>> {
        if record.status == AgentInputStatus::Converted
            && let Some(applied) = &record.applied_task_id
            && let Some(task) = self.store().get_task(applied).await?
        {
            return Ok(turn_result(task));
        }
        Ok(AgentSendResult::Queued {
            input: Box::new(record),
        })
    }

    /// The idempotency digest covers the WHOLE durable envelope, including
    /// any frozen native stanza: a replay that silently swaps the execution
    /// target or turn input under the same `inputId` is an
    /// `input_conflict`, not a silent replay of the original bytes
    /// (AGENTKIT-112).
    pub(super) fn hash_of(&self, kind: QueueKind, envelope: &Value) -> String {
        let hashed = json!({"kind": kind.as_str(), "envelope": envelope});
        sha256_hex(serde_json::to_string(&hashed).unwrap_or_else(|_| "{}".into()))
    }

    /// The admission envelope — the ONE field set idempotency digests:
    /// the input content plus the frozen native stanza (review round-4
    /// F8-a). The queued path stores it on the input row; the immediate
    /// turn path digests it into the task payload. Both compare through
    /// `hash_of` over this same builder — one field list, no drift
    /// between the two admission shapes.
    pub(super) fn admission_envelope(
        &self,
        content: &Value,
        frozen: Option<&Map<String, Value>>,
    ) -> Value {
        let mut envelope = json!({"content": content.clone()});
        if let Some(frozen) = frozen {
            envelope["frozen"] = Value::Object(frozen.clone());
        }
        envelope
    }

    /// The idempotency digest of one admission, shared by the queued and
    /// the immediate path (F8-a): same input id, different bytes (content
    /// OR frozen target) is a conflict under both shapes.
    fn admission_digest(
        &self,
        kind: QueueKind,
        content: &Value,
        frozen: Option<&Map<String, Value>>,
    ) -> String {
        self.hash_of(kind, &self.admission_envelope(content, frozen))
    }

    /// Validate native admission BEFORE it becomes durable (AGENTKIT-112,
    /// review R9): the contract is checked on the MERGED view the
    /// executor will actually see — the per-send frozen stanza plus the
    /// session-frozen `nativeExecution` block it inherits — so an
    /// inherited target cannot hide a missing per-turn input, and a
    /// per-turn input cannot hide a missing target. An admission that is
    /// native at all (either half present) must carry the COMPLETE
    /// executable identity: `executionTarget` and `turnInput` somewhere
    /// in the merge, and `continuationScope` whenever the turn asks for
    /// an EXACT resume (R3 retired the unscoped fallback at dispatch —
    /// admission must not queue an attempt that can only fail there).
    pub(super) fn check_frozen(
        &self,
        frozen: Option<&Map<String, Value>>,
        descriptor: &Value,
    ) -> SessionsResult<(), SessionFault<R>> {
        let native = descriptor.get("nativeExecution").and_then(Value::as_object);
        if frozen.is_none() && native.is_none() {
            return Ok(());
        }
        let merged_has = |key: &str| {
            frozen.is_some_and(|stanza| stanza.contains_key(key))
                || native.is_some_and(|block| block.contains_key(key))
        };
        let mut detail: Vec<String> = ["executionTarget", "turnInput"]
            .into_iter()
            .filter(|key| !merged_has(key))
            .map(|key| {
                format!("native admission is missing required key '{key}' in the merged stanza")
            })
            .collect();
        // Exact resume without its complete scoped binding is refused at
        // the door (mirror of the executor's `continuation_scope_missing`).
        let exact_resume = frozen
            .as_ref()
            .and_then(|stanza| stanza.get("turnInput"))
            .or_else(|| native.and_then(|block| block.get("turnInput")))
            .and_then(|input| input.get("resume"))
            .and_then(|resume| resume.get("mode"))
            .and_then(Value::as_str)
            == Some("exact");
        if exact_resume && !merged_has("continuationScope") {
            detail.push(
                "exact resume requires a frozen 'continuationScope' in the merged stanza"
                    .to_owned(),
            );
        }
        {
            // Review round-3 F5: the typed contract is checked on the
            // MERGED stanza the executor will actually see — an inherited
            // descriptor half that merely PRESENTS `executionTarget` must
            // still parse against the executor's own types, or a garbage
            // target queues a task+messages pair that only dies at claim
            // (the exact AGENTKIT-112 outcome this gate exists to stop).
            let mut merged = native.cloned().unwrap_or_default();
            if let Some(stanza) = frozen {
                for (key, value) in stanza {
                    merged.insert(key.clone(), value.clone());
                }
            }
            if !merged.is_empty() {
                detail.extend(
                    agentkit_native::validate_frozen_stanza(&merged)
                        .err()
                        .map(|e| vec![e])
                        .unwrap_or_default(),
                );
            }
        }
        if detail.is_empty() {
            Ok(())
        } else {
            Err(SessionsError::invalid_input(detail.join("; ")))
        }
    }

    pub(super) async fn submit_turn(
        &self,
        session_id: &str,
        descriptor: &Value,
        content: &Value,
        input_id: Option<&str>,
        frozen: Option<&Map<String, Value>>,
        kind: QueueKind,
    ) -> SessionsResult<AgentSendResult, SessionFault<R>> {
        // Native admission is validated BEFORE the first mint: a malformed
        // or incomplete stanza is refused at send time, never discovered
        // after the task was queued and claimed (AGENTKIT-112).
        self.check_frozen(frozen, descriptor)?;
        // Refused before the first mint: a turn whose kind no executor
        // handles would be a task+messages batch the worker can never
        // claim. Nothing above this line writes.
        self.require_executable(&session_turn_kind(descriptor))?;
        let user_message_id = self.mint("msg_");
        let assistant_message_id = self.mint("msg_");
        let task_id = input_id
            .map(turn_id_for)
            .unwrap_or_else(|| self.mint("task_"));
        let mut payload = self
            .turn_payload(
                session_id,
                descriptor,
                &user_message_id,
                &assistant_message_id,
                None,
            )
            .await?;
        if let (Some(frozen), Some(map)) = (frozen, payload.as_object_mut()) {
            for (key, value) in frozen {
                map.insert(key.clone(), value.clone());
            }
        }
        // The immediate path creates no input row, so the turn itself
        // carries the admission digest — the SAME digest the queued path
        // hashes (F8-a): `prior_receipt` compares against it on replay.
        if input_id.is_some()
            && let Some(map) = payload.as_object_mut()
        {
            map.insert(
                "admissionDigest".into(),
                self.admission_digest(kind, content, frozen).into(),
            );
        }
        let task = SubmitTask {
            task_id: task_id.clone(),
            kind: session_turn_kind(descriptor),
            scope_id: session_id.to_owned(),
            payload,
            priority: 0,
            now: self.now_ms(),
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: true,
        };
        let messages = vec![
            MessageInput {
                id: Some(user_message_id.clone()),
                chat_id: session_id.to_owned(),
                role: "user".into(),
                content: content.clone(),
                ..MessageInput::default()
            },
            MessageInput {
                id: Some(assistant_message_id.clone()),
                chat_id: session_id.to_owned(),
                run_id: Some(task_id.clone()),
                role: "assistant".into(),
                content: Value::String(String::new()),
                parent_message_id: Some(user_message_id.clone()),
                metadata: json!({"placeholder": true}),
                ..MessageInput::default()
            },
        ];
        // The task goes in FIRST inside the store: it is the only write that
        // can reject, and a resubmitted key replays (identity-checked) rather
        // than answering the caller twice.
        let task = self.store().submit_turn(task, messages).await?;
        Ok(turn_result(task))
    }
}

#[async_trait]
impl<R> agentkit_engine::SessionFlushPlanner<R> for AgentSessionService<R>
where
    R: SessionBackend + agentkit_engine::TaskRepository<SessionSettle = SessionSettle>,
{
    async fn settle_plan(
        &self,
        settling: &TaskRecord,
        completed: bool,
    ) -> agentkit_engine::EngineResult<SessionSettle, SessionFault<R>> {
        let none = || SessionSettle {
            expected_descriptor: None,
            staging: None,
            convert: vec![],
        };
        // A failed or cancelled turn promotes nothing: its queued inputs
        // stay pending and visible until an explicit flushQueue
        // (ADR 0016 §4).
        if !completed {
            return Ok(none());
        }
        // Only the session's own turn kind drives its queue; anything else
        // sharing the scope (or a non-session chat) is nobody's queue.
        let Ok((chat, descriptor)) = self.require_session(&settling.scope_id).await else {
            return Ok(none());
        };
        let turn_kind = session_turn_kind(&descriptor);
        if turn_kind != settling.kind {
            return Ok(none());
        }
        // THE authoritative config for anything this snapshot cannot name,
        // read from the descriptor NOW. A `setModel` that landed while this
        // turn ran must reach the turn THIS settle promotes; templating the
        // missed staging off the settling task's payload handed it the model
        // an earlier submission was made with. A descriptor the native
        // dispatch path cannot honor stages nothing (fail-closed, exactly
        // like the candidate filter below) rather than half-stamping a turn.
        let staging = self
            .turn_staging(&settling.scope_id, &descriptor)
            .await
            .ok();
        // At settling time the settling turn itself counts as terminal: a
        // steer aimed at it that was never placed is a promotion candidate
        // (after the follow-ups). The whole eligible queue goes to the store
        // in arrival order; the settlement transaction CAS-validates each
        // against the state it commits and converts the first winner, so a
        // candidate cancelled after this snapshot cannot strand the ones
        // behind it.
        let candidates = self
            .queue_candidates(
                &settling.scope_id,
                &descriptor,
                &turn_kind,
                Some(&settling.task_id),
            )
            .await
            .unwrap_or_default();
        Ok(SessionSettle {
            expected_descriptor: Some(chat.metadata[SESSION_METADATA_KEY].clone()),
            staging,
            convert: candidates
                .into_iter()
                .map(|candidate| StagedCandidate {
                    input_id: candidate.input_id,
                    task: candidate.task,
                    messages: candidate.messages,
                })
                .collect(),
        })
    }
}
