use crate::GovernanceResult;
use agentkit_core::permission::{CodingMode, CodingRole};
use async_trait::async_trait;

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
    async fn mode_of(&self, _ctx: &PermissionContext) -> GovernanceResult<Option<CodingMode>> {
        Ok(None)
    }
    /// The role a session runs under; `None` means "the configured role".
    async fn role_of(&self, _ctx: &PermissionContext) -> GovernanceResult<Option<CodingRole>> {
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
    ) -> GovernanceResult<Option<bool>> {
        Ok(None)
    }
}
