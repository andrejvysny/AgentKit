//! The session-authority bridge (M2-8): the coding pack's permission modes
//! are session state, not process configuration. The session service owns
//! the durable execution state (explicit switches and approved plan grants);
//! this adapter answers `modeOf` from it so a turn always consults what the
//! human authorized NOW — never what a previous turn negotiated away.

use crate::types::{CodingMode, CodingPermissionAuthority, PermissionContext};
use agentkit_host::{AgentSessionService, HostError, HostResult};
use async_trait::async_trait;

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
}
