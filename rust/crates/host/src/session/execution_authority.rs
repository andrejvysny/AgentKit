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

use super::AgentSessionService;
use crate::{HostError, HostResult};
use async_trait::async_trait;
use serde_json::Value;

/// Context passed to the permission authority; mirrors `{chatId?, runId?}`.
#[derive(Clone, Debug, Default)]
pub struct PermissionContext {
    pub chat_id: Option<String>,
    pub run_id: Option<String>,
}

/// The host's answer to "what is this session allowed to do RIGHT NOW".
/// Optional: without one the guard uses the mode it was constructed with,
/// which is what a single-mode CLI does.
///
/// The host owns it deliberately — a permission mode the model could set is
/// not a permission mode.
/// Host-supplied session policy source. The TypeScript port lets these
/// hooks reject (`roleOf`/`modeOf`/`planApproved` throw), and the host
/// staging then FAILS CLOSED: a guard that could not answer has not allowed
/// anything. The Rust port keeps that contract — an implementation that
/// cannot answer returns `Err`, the guard propagates it, and host staging
/// turns the failed guard call into a `guard error` refusal. Silently
/// returning `None` means "no opinion" (use the configured mode/role),
/// never "the lookup failed".
#[async_trait]
pub trait CodingPermissionAuthority: Send + Sync {
    /// The session's effective mode; `None` means "use the configured mode".
    async fn mode_of(&self, _ctx: &PermissionContext) -> HostResult<Option<CodingMode>> {
        Ok(None)
    }
    /// The role a session runs under; `None` means "the configured role".
    async fn role_of(&self, _ctx: &PermissionContext) -> HostResult<Option<CodingRole>> {
        Ok(None)
    }
    /// True when THIS plan was approved by a human for this session.
    /// `None` means the host never wired a plan authority at all — the TS
    /// `authority?.planApproved === undefined` check; the plan is then only
    /// recorded, never judged.
    async fn plan_approved(
        &self,
        _chat_id: Option<&str>,
        _plan_digest: &str,
    ) -> HostResult<Option<bool>> {
        Ok(None)
    }
}

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
