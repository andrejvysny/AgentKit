//! The portable session-authority surface (moved out of the coding pack so
//! bindings and native tools share ONE authority — and so no platform-bound
//! execution code sits on the permission path).
//!
//! The coding pack's permission modes are session state, not process
//! configuration. The session service owns the durable execution state
//! (explicit switches and approved plan grants); this adapter answers
//! `mode_of` from it so a turn always consults what the human authorized
//! NOW — never what a previous turn negotiated away.

use agentkit_core::permission::CodingMode;
use agentkit_core::permission::CodingRole;

use agentkit_governance::{
    CodingPermissionAuthority, GovernanceError, GovernanceResult, PermissionContext,
};
use agentkit_sessions::{AgentSessionService, LegacySessionDebug, SessionBackend, SessionsError};
use async_trait::async_trait;
use serde_json::Value;

/// Reads the session's durable execution state through the session service.
/// Absent state means "no session-level decision yet": the guard falls back
/// to its configured mode. An unreadable or malformed state is an ERROR,
/// not an absence — the guard then fails closed instead of guessing.
pub struct SessionExecutionAuthority<R: SessionBackend> {
    service: AgentSessionService<R>,
}

impl<R: SessionBackend> SessionExecutionAuthority<R> {
    pub fn new(service: AgentSessionService<R>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl<R: SessionBackend> CodingPermissionAuthority for SessionExecutionAuthority<R> {
    async fn mode_of(&self, ctx: &PermissionContext) -> GovernanceResult<Option<CodingMode>> {
        let Some(session_id) = ctx.chat_id.as_deref() else {
            return Ok(None); // a chat that is not a session has no session mode
        };
        let state = self
            .service
            .get_execution(session_id)
            .await
            // A chat that is not an agent session has no session-level
            // decision — the same absence as a session that never set
            // execution state, and the configured mode applies (a REST-minted
            // chat must be able to run its tools). Only a real read failure
            // stays an error; the guard then fails closed.
            .or_else(|error| {
                if matches!(error, SessionsError::SessionNotFound(_)) {
                    Ok(None)
                } else {
                    Err(error)
                }
            })
            .map_err(|error| {
                GovernanceError::Port(format!(
                    "Cannot read the session's execution state; permissions stay with the guard. ({:?})", LegacySessionDebug(&error)
                ))
            })?;
        let Some(state) = state else {
            return Ok(None);
        };
        let mode = state
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                GovernanceError::Port(
                "The session's execution state carries no mode; permissions stay with the guard."
                    .into(),
            )
            })?;
        CodingMode::parse(mode)
            .map(Some)
            .ok_or_else(|| GovernanceError::Port(format!("unknown session execution mode {mode}")))
    }

    async fn role_of(&self, ctx: &PermissionContext) -> GovernanceResult<Option<CodingRole>> {
        let Some(session_id) = ctx.chat_id.as_deref() else {
            return Ok(None);
        };
        let state = match self.service.get_state(session_id).await {
            Ok(state) => state,
            Err(SessionsError::SessionNotFound(_)) => {
                return Ok(None);
            }
            Err(error) => return Err(GovernanceError::Port(error.to_string())),
        };
        match state.descriptor["profileSnapshot"]["role"].as_str() {
            Some("implementer") => Ok(Some(CodingRole::Implementer)),
            Some("reviewer") => Ok(Some(CodingRole::Reviewer)),
            Some("supervisor") => Ok(Some(CodingRole::Supervisor)),
            None => Ok(None),
            Some(_) => Err(GovernanceError::Port("invalid_profile_role".into())),
        }
    }

    /// Some(true) only when the session's durable state carries a plan
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
    ) -> GovernanceResult<Option<bool>> {
        let Some(session_id) = chat_id else {
            return Ok(None); // not a session: nothing can have approved this plan
        };
        let state = match self.service.get_execution(session_id).await {
            Ok(state) => state,
            // Not an agent session: nothing can have approved this plan —
            // the same answer as a chat without a session at all, and never
            // a silent pass.
            Err(SessionsError::SessionNotFound(_)) => {
                return Ok(None);
            }
            Err(error) => {
                return Err(GovernanceError::Port(format!(
                    "Cannot read the session's execution state; the plan stays unjudged. ({:?})",
                    LegacySessionDebug(&error)
                )));
            }
        };
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
                            GovernanceError::Port(
                            "The session's plan grant carries no planDigest; refusing to judge."
                                .into(),
                        )
                        })?;
                Ok(Some(recorded == plan_digest))
            }
        }
    }
}
