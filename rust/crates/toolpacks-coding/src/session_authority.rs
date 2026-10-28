//! The session-authority bridge (M2-8): the coding pack's permission modes
//! are session state, not process configuration. The session service owns
//! the durable execution state (explicit switches and approved plan grants);
//! this adapter answers `modeOf` from it so a turn always consults what the
//! human authorized NOW — never what a previous turn negotiated away.

use crate::types::{CodingMode, CodingPermissionAuthority, PermissionContext};
use agentkit_host::{AgentSessionService, HostError, HostResult};
use async_trait::async_trait;
use serde_json::Value;

/// Reads the session's durable execution state through the session service.
/// Absent state means "no session-level decision yet": the guard falls back
/// to its configured mode. An unreadable or malformed state is an ERROR,
/// not an absence — the guard then fails closed instead of guessing.
pub struct SessionExecutionAuthority {
    service: AgentSessionService,
}

impl SessionExecutionAuthority {
    pub fn new(service: AgentSessionService) -> Self {
        Self { service }
    }
}

#[async_trait]
impl CodingPermissionAuthority for SessionExecutionAuthority {
    async fn mode_of(&self, ctx: &PermissionContext) -> HostResult<Option<CodingMode>> {
        let Some(session_id) = ctx.chat_id.as_deref() else {
            return Ok(None); // a chat that is not a session has no session mode
        };
        let state = self
            .service
            .get_execution(session_id)
            .await
            .map_err(|error| {
                HostError::Execution(format!(
                    "Cannot read the session's execution state; permissions stay with the guard. ({error:?})"
                ))
            })?;
        let Some(state) = state else {
            return Ok(None);
        };
        let mode = state
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                HostError::Execution(
                "The session's execution state carries no mode; permissions stay with the guard."
                    .into(),
            )
            })?;
        CodingMode::parse(mode)
            .map(Some)
            .ok_or_else(|| HostError::Execution(format!("unknown session execution mode {mode}")))
    }

    /// `Some(true)` only when the session's durable state carries a plan
    /// grant for THIS exact digest; `Some(false)` when the session exists
    /// but no such approval is recorded (with or without any execution
    /// state yet — an unanswered plan is an unanswered plan). A malformed
    /// grant is an error, never an absence: the plan tool must not read a
    /// corrupt record as "no authority wired" and silently report the plan
    /// as merely recorded.
    async fn plan_approved(
        &self,
        chat_id: Option<&str>,
        plan_digest: &str,
    ) -> HostResult<Option<bool>> {
        let Some(session_id) = chat_id else {
            return Ok(None); // not a session: nothing can have approved this plan
        };
        let state = self
            .service
            .get_execution(session_id)
            .await
            .map_err(|error| {
                HostError::Execution(format!(
                    "Cannot read the session's execution state; the plan stays unjudged. ({error:?})"
                ))
            })?;
        let Some(state) = state else {
            return Ok(Some(false)); // a session with no execution state has no approval
        };
        match state.get("grantedPlan") {
            None => Ok(Some(false)),
            Some(Value::Null) => Ok(Some(false)),
            Some(grant) => {
                let recorded =
                    grant
                        .get("planDigest")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            HostError::Execution(
                            "The session's plan grant carries no planDigest; refusing to judge."
                                .into(),
                        )
                        })?;
                Ok(Some(recorded == plan_digest))
            }
        }
    }
}
