use crate::{GovernanceResult, ToolGuardContext};
use async_trait::async_trait;

/// An answered approval bound to one exact call: the answered request id
/// (spent at admission, single-use via the store's UNIQUE index) and the
/// canonical arguments hash it was granted for (a changed call is not this
/// grant's to spend). `uncertainty_generation` is the execution id of the
/// unresolved admitted receipt an answer explicitly reauthorized: the
/// store's pending-generation gate only opens for exactly that value, so
/// the grant cannot spend itself on a different (later) uncertainty.
#[derive(Clone, Debug)]
pub struct ApprovalGrant {
    pub request_id: String,
    pub args_sha256: String,
    pub uncertainty_generation: Option<String>,
    pub context_sha256: Option<String>,
}

/// Which approvable refusals may be remembered, and under what digest. The
/// coding pack implements this for its shell tools: a policy that declines
/// to key a call (high-risk, plan, malformed) forces a fresh ask.
#[async_trait]
pub trait StandingApprovalPolicy: Send + Sync {
    /// `(digest, label)` for this exact call, or `None` when uncoverable.
    /// `reasons` are every approvable refusal the guards gave; a policy
    /// covers only the ordinary `approval_required:` surface.
    async fn key_for(
        &self,
        context: &ToolGuardContext,
        reasons: &[String],
    ) -> GovernanceResult<Option<(String, String)>>;
}
