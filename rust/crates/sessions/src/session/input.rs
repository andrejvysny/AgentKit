//! Input surface of [`AgentSessionService`]: send, follow-ups, steering,
//! queueing, cancellation of queued inputs and the explicit flush.

use crate::{
    AgentInputKind, AgentInputPatch, AgentInputQuery, AgentInputSettlePolicy, AgentInputStatus,
    CreateAgentInput, QueueAdmission, QueueOrStage, SessionBackend, SessionFault, SessionsError,
    SessionsResult, StageQueuedTurn,
};
use serde_json::{Map, Value, json};

use super::{
    AgentSendResult, AgentSessionService, FollowUpInput, QueueIntent, QueueKind, SendInput,
    SteerInput, WhenBusy, session_turn_kind, turn_id_for, turn_result,
};

impl<R: SessionBackend> AgentSessionService<R> {
    // -- sending -------------------------------------------------------------

    pub async fn send(
        &self,
        session_id: &str,
        input: &SendInput,
    ) -> SessionsResult<AgentSendResult, SessionFault<R>> {
        if input.when_busy != WhenBusy::Reject && input.input_id.is_none() {
            return Err(SessionsError::invalid_input(format!(
                "inputId is required when whenBusy is \"{}\".",
                match input.when_busy {
                    WhenBusy::Reject => "reject",
                    WhenBusy::Queue => "queue",
                    WhenBusy::Steer => "steer",
                }
            )));
        }
        let (_, descriptor) = self.require_session(session_id).await?;
        if input.when_busy == WhenBusy::Reject {
            return self
                .submit_turn(
                    session_id,
                    &descriptor,
                    &input.content,
                    input.input_id.as_deref(),
                    input.frozen_payload.as_ref(),
                    // A direct (reject) turn is a follow-up in admission
                    // terms: the replay digest must be computed under the
                    // same kind a follow-up replay would use (F8-a).
                    QueueKind::FollowUp,
                )
                .await;
        }
        let kind = if input.when_busy == WhenBusy::Steer {
            QueueKind::Steer
        } else {
            QueueKind::FollowUp
        };
        self.submit_or_queue(
            session_id,
            &descriptor,
            kind,
            input.content.clone(),
            QueueIntent {
                input_id: input.input_id.clone(),
                turn_id: input.turn_id.clone(),
                on_settled: input.on_settled,
            },
            input.frozen_payload.as_ref(),
        )
        .await
    }

    pub async fn follow_up(
        &self,
        session_id: &str,
        input: &FollowUpInput,
    ) -> SessionsResult<AgentSendResult, SessionFault<R>> {
        let (_, descriptor) = self.require_session(session_id).await?;
        self.submit_or_queue(
            session_id,
            &descriptor,
            QueueKind::FollowUp,
            input.content.clone(),
            QueueIntent {
                input_id: input.input_id.clone(),
                turn_id: None,
                on_settled: None,
            },
            input.frozen_payload.as_ref(),
        )
        .await
    }

    /// On an idle session a steer is just a new turn.
    pub async fn steer(
        &self,
        session_id: &str,
        input: &SteerInput,
    ) -> SessionsResult<AgentSendResult, SessionFault<R>> {
        let (_, descriptor) = self.require_session(session_id).await?;
        self.submit_or_queue(
            session_id,
            &descriptor,
            QueueKind::Steer,
            input.content.clone(),
            QueueIntent {
                input_id: input.input_id.clone(),
                turn_id: input.turn_id.clone(),
                on_settled: input.on_settled,
            },
            input.frozen_payload.as_ref(),
        )
        .await
    }

    /// A steer that is delivered ONLY to a live turn and can never start
    /// one: the store admission runs with no staged candidate, so an idle
    /// session (even one that settles between a caller's check and this
    /// call) cancels the input and answers `chat_idle`. For machine-authored
    /// notes (advisor escalation) that must not drive an unattended turn.
    pub async fn steer_live(
        &self,
        session_id: &str,
        input: &SteerInput,
    ) -> SessionsResult<AgentSendResult, SessionFault<R>> {
        let (_, descriptor) = self.require_session(session_id).await?;
        let input_id = input
            .input_id
            .clone()
            .ok_or_else(|| SessionsError::invalid_input("inputId is required to steer."))?;
        if let Some(prior) = self
            .prior_receipt(
                session_id,
                QueueKind::Steer,
                &input_id,
                &input.content,
                None,
            )
            .await?
        {
            return Ok(prior);
        }
        let envelope = json!({"content": input.content});
        let admission = self
            .store()
            .queue_or_stage_turn(QueueOrStage {
                session_id: session_id.to_owned(),
                task_kind: session_turn_kind(&descriptor),
                input: CreateAgentInput {
                    input_id: input_id.clone(),
                    session_id: session_id.to_owned(),
                    kind: AgentInputKind::Steer,
                    content: envelope.clone(),
                    content_hash: self.hash_of(QueueKind::Steer, &envelope),
                    target_task_id: None,
                    on_settled: Some(AgentInputSettlePolicy::Cancel),
                    attempt_id: None,
                    tool_call_id: None,
                    expires_at: None,
                },
                target_the_live_turn: true,
                candidate: None,
                now: self.now_ms(),
            })
            .await?;
        match admission {
            QueueAdmission::Queued(record) if record.target_task_id.is_some() => {
                self.receipt_of(record).await
            }
            QueueAdmission::Queued(record) => {
                // No live turn held the scope: nothing will ever consume it.
                let _ = self
                    .store()
                    .transition_input(
                        record.input_id,
                        vec![AgentInputStatus::Pending],
                        AgentInputStatus::Cancelled,
                        AgentInputPatch::default(),
                        self.now_ms(),
                    )
                    .await;
                Err(SessionsError::chat_idle(format!(
                    "Session {session_id} has no live turn to steer."
                )))
            }
            QueueAdmission::Staged(_) => Err(SessionsError::chat_idle(
                "unexpected staged turn from a live-only steer",
            )),
        }
    }

    /// Convert the oldest queued input into a turn. Refused with `chat_busy`
    /// while the session has unfinished work; `None` when nothing is queued.
    pub async fn flush_queue(
        &self,
        session_id: &str,
    ) -> SessionsResult<Option<AgentSendResult>, SessionFault<R>> {
        let (_, descriptor) = self.require_session(session_id).await?;
        let turn_kind = session_turn_kind(&descriptor);
        self.require_executable(&turn_kind)?;
        let pending = self
            .store()
            .list_inputs(AgentInputQuery {
                session_id: session_id.to_owned(),
                kinds: Some(vec![AgentInputKind::FollowUp, AgentInputKind::Steer]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: None,
            })
            .await?;
        if pending.is_empty() {
            return Ok(None);
        }
        // Keeps a refused flush from cancelling steers meant for the live
        // turn; the staged conversion's busy check stays the authoritative
        // guard.
        if self.active_turn(session_id, &turn_kind).await?.is_some() {
            return Err(SessionsError::chat_busy(session_id));
        }
        for candidate in self
            .queue_candidates(session_id, &descriptor, &turn_kind, None)
            .await?
        {
            let input_id = candidate.input_id.clone();
            let user_message_id = candidate.messages[0].id.clone().unwrap_or_default();
            let assistant_message_id = candidate.messages[1].id.clone().unwrap_or_default();
            if self.store().stage_queued_turn(candidate).await?.is_some() {
                self.wake();
                return Ok(Some(AgentSendResult::Turn {
                    turn_id: turn_id_for(&input_id),
                    user_message_id,
                    assistant_message_id,
                }));
            }
            // Lost the race for this input (a concurrent flush settled it);
            // keep scanning.
        }
        Ok(None)
    }

    /// Every queued input that may take the scope, in arrival order, with
    /// its task and messages fully staged. `on_settled: cancel` steers whose
    /// target has settled are cancelled on an explicit flush, never converted.
    /// Settlement plans leave that cancellation to their transaction. `settling`
    /// names a turn that is about to become terminal (the settle path):
    /// steers aimed at it are treated as target-terminal.
    pub(super) async fn queue_candidates(
        &self,
        session_id: &str,
        descriptor: &Value,
        turn_kind: &str,
        settling: Option<&str>,
    ) -> SessionsResult<Vec<StageQueuedTurn>, SessionFault<R>> {
        let pending = self
            .store()
            .list_inputs(AgentInputQuery {
                session_id: session_id.to_owned(),
                kinds: Some(vec![AgentInputKind::FollowUp, AgentInputKind::Steer]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: None,
            })
            .await?;
        let mut out = Vec::new();
        for record in &pending {
            if record.kind == AgentInputKind::Steer {
                if let Some(target) = &record.target_task_id
                    && Some(target.as_str()) != settling
                {
                    let target = self.store().get_task(target).await?;
                    if target
                        .as_ref()
                        .is_some_and(|task| !task.status.is_terminal())
                    {
                        continue; // aimed at a live turn: not this flush's candidate
                    }
                }
                if record.on_settled == Some(AgentInputSettlePolicy::Cancel) {
                    // Settlement owns this cancellation in its transaction.
                    // Planning may be retried or the settlement may fail, so
                    // taking its snapshot must have no durable side effects.
                    if settling.is_none() {
                        let _ = self
                            .store()
                            .transition_input(
                                record.input_id.clone(),
                                vec![AgentInputStatus::Pending],
                                AgentInputStatus::Cancelled,
                                AgentInputPatch::default(),
                                self.now_ms(),
                            )
                            .await;
                    }
                    continue;
                }
            }
            // A steer converts under the SAME deterministic record id the
            // in-run projection uses, so every placement path for one input
            // is the same receipt — never two records for one steering.
            let Some(candidate) = self
                .stage_candidate(
                    session_id,
                    descriptor,
                    &record.input_id,
                    record.kind,
                    &record.content,
                )
                .await?
            else {
                continue; // malformed content is skipped, like the TS logger path
            };
            out.push(StageQueuedTurn {
                session_id: session_id.to_owned(),
                task_kind: turn_kind.to_owned(),
                exclude_task_id: None,
                input_id: record.input_id.clone(),
                expected_kind: record.kind,
                task: candidate.task,
                messages: candidate.messages,
                now: self.now_ms(),
            });
        }
        Ok(out)
    }

    // -- deferred compaction, queue maintenance, configuration -------------------

    /// Cancel one pending queued follow-up or steer of the session before it
    /// is converted or placed. False when it is no longer pending; unknown or
    /// foreign ids are not found.
    pub async fn cancel_queued_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> SessionsResult<bool, SessionFault<R>> {
        self.require_session(session_id).await?;
        let record = self
            .store()
            .get_input(input_id.to_owned())
            .await?
            .filter(|record| {
                record.session_id == session_id
                    && matches!(
                        record.kind,
                        AgentInputKind::FollowUp | AgentInputKind::Steer
                    )
            })
            .ok_or_else(|| {
                SessionsError::InputNotFound(format!(
                    "Queued input {input_id} not found in {session_id}."
                ))
            })?;
        if record.status != AgentInputStatus::Pending {
            return Ok(false);
        }
        Ok(self
            .store()
            .transition_input(
                input_id.to_owned(),
                vec![AgentInputStatus::Pending],
                AgentInputStatus::Cancelled,
                AgentInputPatch::default(),
                self.now_ms(),
            )
            .await
            .is_ok())
    }

    /// Cancel every pending queued follow-up and steer of the session. Human
    /// requests are untouched. Returns how many inputs were cancelled.
    pub async fn clear_queue(&self, session_id: &str) -> SessionsResult<usize, SessionFault<R>> {
        self.require_session(session_id).await?;
        let pending = self
            .store()
            .list_inputs(AgentInputQuery {
                session_id: session_id.to_owned(),
                kinds: Some(vec![AgentInputKind::FollowUp, AgentInputKind::Steer]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: None,
            })
            .await?;
        let mut cleared = 0;
        for record in pending {
            // A concurrent flush or cancel won this input; it is no longer queued.
            if self
                .store()
                .transition_input(
                    record.input_id,
                    vec![AgentInputStatus::Pending],
                    AgentInputStatus::Cancelled,
                    AgentInputPatch::default(),
                    self.now_ms(),
                )
                .await
                .is_ok()
            {
                cleared += 1;
            }
        }
        Ok(cleared)
    }

    // -- internals -----------------------------------------------------------

    /// Try to start a turn; if the session is busy, admit the input instead.
    /// The submit attempt IS the idle check (the store refuses a concurrent
    /// turn), and the queueing half is ONE store write that re-checks the
    /// active turn as it inserts — so there is no check-then-write window
    /// anywhere for a settlement to slip between. An input can never be left
    /// Pending behind a turn that settled after a read and before an insert.
    async fn submit_or_queue(
        &self,
        session_id: &str,
        descriptor: &Value,
        kind: QueueKind,
        content: Value,
        intent: QueueIntent,
        frozen: Option<&Map<String, Value>>,
    ) -> SessionsResult<AgentSendResult, SessionFault<R>> {
        self.check_frozen(frozen, descriptor)?;
        if let Some(existing) = &intent.input_id
            && let Some(prior) = self
                .prior_receipt(session_id, kind, existing, &content, frozen)
                .await?
        {
            return Ok(prior);
        }
        match self
            .submit_turn(
                session_id,
                descriptor,
                &content,
                intent.input_id.as_deref(),
                frozen,
                kind,
            )
            .await
        {
            Ok(result) => {
                self.wake();
                Ok(result)
            }
            Err(error) if error.is_scope_busy() => {
                self.enqueue_input(session_id, descriptor, kind, content, intent, frozen)
                    .await
            }
            Err(error) => Err(error),
        }
    }

    /// Admit a queued input. `submit_turn`'s `chat_busy` is what routes here,
    /// so the session WAS busy moments ago; the store's own check inside the
    /// admission transaction — not a read this call cannot keep true —
    /// decides whether the input waits behind the live turn or becomes the
    /// next turn itself, and the answer commits with the insertion. The
    /// reread-then-insert pair this replaces is exactly how a follow-up that
    /// landed after a settlement's commit stayed Pending forever with no
    /// active turn and an idle worker.
    async fn enqueue_input(
        &self,
        session_id: &str,
        descriptor: &Value,
        kind: QueueKind,
        content: Value,
        intent: QueueIntent,
        frozen: Option<&Map<String, Value>>,
    ) -> SessionsResult<AgentSendResult, SessionFault<R>> {
        // The queue-or-stage transaction admits input+task+messages as ONE
        // batch, so the registry check runs before it: a refused admission
        // leaves no input row behind either.
        self.require_executable(&session_turn_kind(descriptor))?;
        let QueueIntent {
            input_id,
            turn_id,
            on_settled,
        } = intent;
        let input_id = input_id
            .ok_or_else(|| SessionsError::invalid_input("inputId is required to queue."))?;
        // An explicit target is validated up front (a steer aimed at a turn
        // that never existed is `not_found`); with none, the store names the
        // live turn it actually finds, so the running turn sees its steer at
        // the next boundary instead of promoting it blind.
        let target_task_id = match (&turn_id, kind) {
            (Some(target), QueueKind::Steer) => {
                self.store()
                    .get_task(target)
                    .await?
                    .filter(|task| task.scope_id == session_id)
                    .ok_or_else(|| {
                        SessionsError::TurnNotFound(format!(
                            "Turn {target} not found in {session_id}."
                        ))
                    })?;
                Some(target.clone())
            }
            _ => None,
        };
        // The durable envelope is what the staging filter reads too, so the
        // turn an admission can start is staged from exactly the bytes the
        // input stores (`queue_content` / `read_input_content` shape). The
        // frozen native stanza rides the same durable envelope: a queued
        // input flushes with the identical admission bytes it was sent with.
        let envelope = self.admission_envelope(&content, frozen);
        let candidate = self
            .stage_candidate(
                session_id,
                descriptor,
                &input_id,
                kind.input_kind(),
                &envelope,
            )
            .await?;
        let content_hash = self.hash_of(kind, &envelope);
        let admission = self
            .store()
            .queue_or_stage_turn(QueueOrStage {
                session_id: session_id.to_owned(),
                task_kind: session_turn_kind(descriptor),
                input: CreateAgentInput {
                    input_id,
                    session_id: session_id.to_owned(),
                    kind: kind.input_kind(),
                    content: envelope,
                    content_hash,
                    target_task_id,
                    on_settled: on_settled
                        .map(AgentInputSettlePolicy::from)
                        .filter(|_| kind == QueueKind::Steer),
                    attempt_id: None,
                    tool_call_id: None,
                    expires_at: None,
                },
                target_the_live_turn: kind == QueueKind::Steer && turn_id.is_none(),
                candidate,
                now: self.now_ms(),
            })
            .await?;
        match admission {
            QueueAdmission::Staged(task) => {
                // This write created the next turn: rouse a worker for it.
                self.wake();
                Ok(turn_result(task))
            }
            QueueAdmission::Queued(record) => {
                // Durable behind a live turn whose settlement transaction
                // re-reads the queue and owns the promotion.
                self.receipt_of(record).await
            }
        }
    }
}
