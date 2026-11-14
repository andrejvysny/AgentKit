//! Turn control of [`AgentSessionService`]: cancel, interrupt, resume, request
//! cancellation, and the shared session/turn guards.

use crate::{
    AgentInputKind, AgentInputPatch, AgentInputQuery, AgentInputStatus, ChatRecord, SessionBackend,
    SessionFault, SessionsError, SessionsResult,
};
use agentkit_engine::{TaskRecord, TaskStatus};
use serde_json::Value;

use super::{AgentSessionService, read_session_descriptor};

impl<R: SessionBackend> AgentSessionService<R> {
    // -- turn lifecycle -------------------------------------------------------

    pub async fn cancel(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> SessionsResult<(), SessionFault<R>> {
        self.require_turn(session_id, turn_id).await?;
        self.store().cancel_task(turn_id, self.now_ms()).await?;
        // Unconditional: a turn can park between any pre-cancel status read
        // and the cancel itself, and a Pending request left behind keeps a
        // dead turn answerable. Cancelling zero requests is a no-op.
        self.cancel_requests(session_id, turn_id).await?;
        // Promptness, not correctness: the durable flag above is what the
        // worker settles on. Rouse an idle worker (queued targets) and the
        // in-flight attempt (heartbeat check now instead of next tick).
        if let Some(wake) = &self.deps.wake {
            wake.wake();
            wake.wake_active(turn_id).await;
        }
        Ok(())
    }

    /// Stop a turn RESUMABLY (distinct from [`Self::cancel`]): the turn parks
    /// `interrupted` with its id, events and records intact, and
    /// [`Self::resume`] continues it with a new attempt.
    pub async fn interrupt(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> SessionsResult<(), SessionFault<R>> {
        self.require_turn(session_id, turn_id).await?;
        self.store()
            .request_interrupt(turn_id.to_owned(), self.now_ms())
            .await?;
        self.wake();
        Ok(())
    }

    /// Continue an interrupted turn; any other status is `invalid_state`.
    pub async fn resume(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> SessionsResult<(), SessionFault<R>> {
        let task = self.require_turn(session_id, turn_id).await?;
        if task.status != TaskStatus::Interrupted {
            return Err(SessionsError::invalid_state(format!(
                "Turn {turn_id} is {:?}; only an interrupted turn can resume.",
                task.status
            )));
        }
        self.store().resume_task(turn_id, self.now_ms()).await?;
        self.wake();
        Ok(())
    }

    // -- internals -----------------------------------------------------------

    pub(super) async fn require_session(
        &self,
        session_id: &str,
    ) -> SessionsResult<(ChatRecord, Value), SessionFault<R>> {
        let chat = self.store().get_chat(session_id).await?;
        let descriptor = chat
            .as_ref()
            .and_then(|chat| read_session_descriptor(&chat.metadata));
        match (chat, descriptor) {
            (Some(chat), Some(descriptor)) => Ok((chat, descriptor)),
            _ => Err(SessionsError::session_not_found(format!(
                "Session not found: {session_id}"
            ))),
        }
    }

    async fn require_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> SessionsResult<TaskRecord, SessionFault<R>> {
        self.require_session(session_id).await?;
        self.store()
            .get_task(turn_id)
            .await?
            .filter(|task| task.scope_id == session_id)
            .ok_or_else(|| {
                SessionsError::TurnNotFound(format!("Turn {turn_id} not found in {session_id}."))
            })
    }

    pub(super) async fn active_turn(
        &self,
        session_id: &str,
        turn_kind: &str,
    ) -> SessionsResult<Option<TaskRecord>, SessionFault<R>> {
        Ok(self
            .store()
            .list_tasks(Some(session_id.to_owned()))
            .await?
            .into_iter()
            .find(|task| task.kind == turn_kind && !task.status.is_terminal()))
    }

    async fn cancel_requests(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> SessionsResult<(), SessionFault<R>> {
        let requests = self
            .store()
            .list_inputs(AgentInputQuery {
                session_id: session_id.to_owned(),
                kinds: Some(vec![AgentInputKind::HumanRequest]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: Some(turn_id.to_owned()),
            })
            .await?;
        for request in requests {
            let _ = self
                .store()
                .transition_input(
                    request.input_id,
                    vec![AgentInputStatus::Pending],
                    AgentInputStatus::Cancelled,
                    AgentInputPatch::default(),
                    self.now_ms(),
                )
                .await;
        }
        Ok(())
    }
}
