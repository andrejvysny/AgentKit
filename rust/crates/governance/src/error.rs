/// Failure of a governance port. In-crate failures use the typed variants
/// below; `Port` is the documented escape hatch for host implementations of
/// the port traits (mirrors `ChatError::External`), where the message text
/// is the stable, user-visible payload hosts convert verbatim.
#[derive(Debug, thiserror::Error)]
pub enum GovernanceError {
    #[error("{0}")]
    Core(#[from] agentkit_core::CoreError),
    #[error("{0}")]
    Port(String),
    #[error("{0}")]
    Store(#[from] agentkit_store::StoreError),
    #[error("actor_allowance_requires_exact_scope_and_payload")]
    ActorAllowanceRequiresExactScopeAndPayload,
    #[error("applying proposal has no operation identity")]
    ApplyingProposalHasNoOperationIdentity,
    #[error("dispatch_fence_lost")]
    DispatchFenceLost,
    #[error("invalid domain apply outcome; reconciliation required")]
    InvalidDomainApplyOutcome,
    #[error("invalid_proposal_build")]
    InvalidProposalBuild,
    #[error("invalid_write_query")]
    InvalidWriteQuery,
    #[error("proposal_operation_mismatch")]
    ProposalOperationMismatch,
    #[error("proposal_outcome_unresolved")]
    ProposalOutcomeUnresolved,
    #[error("revision_conflict")]
    RevisionConflict,
    #[error("tool_effect_or_actor_mismatch")]
    ToolEffectOrActorMismatch,
    #[error("tool_name_collision")]
    ToolNameCollision,
    #[error("tool_namespace_invalid")]
    ToolNamespaceInvalid,
    #[error("tool_namespace_reserved")]
    ToolNamespaceReserved,
    #[error("write_policy_revoked")]
    WritePolicyRevoked,
    #[error("write_policy_unavailable")]
    WritePolicyUnavailable,
    #[error("write_tool_requires_chat")]
    WriteToolRequiresChat,
    #[error("write_tool_requires_write_effect_and_trusted_actor")]
    WriteToolRequiresWriteEffectAndTrustedActor,
}

pub type GovernanceResult<T> = Result<T, GovernanceError>;

impl GovernanceError {
    /// Classification the host records when this error is wrapped into a
    /// context failure. Proposal/query refusals are rejections; everything
    /// else stays unclassified.
    pub fn context_classification(&self) -> agentkit_engine::ContextClassification {
        use agentkit_engine::ContextClassification::{Rejected, Unclassified};
        match self {
            Self::InvalidProposalBuild | Self::InvalidWriteQuery => Rejected,
            _ => Unclassified,
        }
    }
}
