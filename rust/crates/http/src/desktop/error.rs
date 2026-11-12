use crate::Problem;
use agentkit_agents::AgentsError;
use agentkit_chat::ChatError;
use agentkit_engine::EngineError as E;
use agentkit_sessions::SessionsError as S;
use agentkit_store::StoreError;
type EngineStoreError = E<StoreError>;

pub(super) fn store_error(error: StoreError) -> Problem {
    let code = match &error {
        StoreError::NotFound(_) => "not_found",
        StoreError::Duplicate(_) | StoreError::AnswerConflict(_) => "duplicate_task",
        StoreError::ScopeBusy(_) => "chat_busy",
        StoreError::LeaseLost(_) => "lease_lost",
        StoreError::InvalidTransition(_)
        | StoreError::AgentInputTransition(_)
        | StoreError::AnswerStale(_)
        | StoreError::ReceiptTransition { .. } => "invalid_task_transition",
        StoreError::TaskInput {
            reason: agentkit_engine::TaskInputReason::InvalidForkPoint,
            ..
        } => "invalid_fork_point",
        StoreError::InvalidInput(_)
        | StoreError::TaskInput { .. }
        | StoreError::ManagedInput { .. } => "invalid_request",
        _ => "internal_error",
    };
    if code == "invalid_request" {
        Problem::new(400, code, error.to_string())
    } else {
        Problem::host(code, error.to_string())
    }
}
pub(super) fn host_error(error: EngineStoreError) -> Problem {
    match error {
        EngineStoreError::Store(error) => store_error(error),
        EngineStoreError::UnknownKind(kind) => Problem::host("executor_not_found", kind),
        EngineStoreError::NoExecutorForKind(kind) => Problem::host("no_executor_for_kind", kind),
        _ => Problem::host("internal_error", error.to_string()),
    }
}

pub(super) fn session_error(error: S) -> Problem {
    match error {
        S::Engine(error) => host_error(error),
        S::Store(error) => store_error(error),
        other => Problem::host("internal_error", other.to_string()),
    }
}
pub(super) fn session_service_error(error: S) -> Problem {
    match error {
        S::InvalidInput(message) => Problem::new(400, "invalid_request", message),
        S::InvalidInputContext(source) => Problem::new(400, "invalid_request", source.to_string()),
        S::SessionStore(agentkit_sessions::SessionStoreError(
            StoreError::InvalidInput(message)
            | StoreError::TaskInput { message, .. }
            | StoreError::ManagedInput { message, .. },
        )) => Problem::new(400, "invalid_request", message),
        other => Problem::host("internal_error", other.to_string()),
    }
}
pub(super) fn chat_error(error: ChatError) -> Problem {
    match error {
        ChatError::Engine(error) => host_error(error),
        ChatError::Store(error) => store_error(error),
        ChatError::Sessions(error) => session_error(error),
        ChatError::Governance(agentkit_governance::GovernanceError::Store(error)) => {
            store_error(error)
        }
        other => Problem::host("internal_error", other.to_string()),
    }
}
pub(super) fn proposal_error(error: agentkit_governance::GovernanceError) -> Problem {
    match error {
        agentkit_governance::GovernanceError::Store(
            StoreError::InvalidTransition(detail)
            | StoreError::AgentInputTransition(detail)
            | StoreError::AnswerStale(detail)
            | StoreError::ReceiptTransition {
                message: detail, ..
            },
        ) => Problem::host("invalid_proposal_transition", detail),
        other => chat_error(other.into()),
    }
}
pub(super) fn teammate_problem(error: impl Into<AgentsError>) -> Problem {
    match error.into() {
        refusal @ (AgentsError::InvalidAgent(_) | AgentsError::InvalidAdmission(_)) => {
            Problem::new(400, "invalid_request", refusal.to_string())
        }
        AgentsError::AgentNotFound(id) => {
            Problem::new(404, "not_found", format!("agent_not_found: {id}"))
        }
        AgentsError::SubagentNotFound(id) => {
            Problem::new(404, "not_found", format!("subagent_not_found: {id}"))
        }
        AgentsError::Sessions(S::InvalidOverrides(message)) => Problem::new(
            400,
            "invalid_request",
            format!("invalid_overrides: {message}"),
        ),
        AgentsError::Sessions(S::RunProfileRefused(message)) => Problem::new(
            400,
            "invalid_request",
            format!("run_profile_refused:{message}"),
        ),
        AgentsError::Sessions(S::AgentNotFound(id)) => {
            Problem::new(404, "not_found", format!("agent {id}"))
        }
        AgentsError::Sessions(S::TaskNotFound(id)) => {
            Problem::new(404, "not_found", format!("task {id}"))
        }
        AgentsError::Sessions(S::Store(error)) | AgentsError::Store(error) => teammate_store(error),
        AgentsError::Engine(E::Store(error)) => teammate_store(error),
        AgentsError::Engine(error) => host_error(error),
        other => Problem::host("internal_error", other.to_string()),
    }
}
fn teammate_store(error: StoreError) -> Problem {
    match error {
        StoreError::Conflict(message) => Problem::host("revision_conflict", message),
        StoreError::InvalidTransition(message)
        | StoreError::AgentInputTransition(message)
        | StoreError::AnswerStale(message)
        | StoreError::ReceiptTransition { message, .. } => {
            Problem::host("invalid_task_transition", message)
        }
        StoreError::NotFound(message) => Problem::new(404, "not_found", message),
        other => store_error(other),
    }
}

/// Managed-content store refusals, mapped the way the TypeScript transport
/// maps this surface: a lost compare-and-swap is the connection's stale
/// conflict (409); identity/alias refusals are the reference's invalid
/// connection configuration (400 `mcp_invalid_config`); everything else
/// keeps the generic mapping.
pub(super) fn managed_store_error(error: agentkit_store::StoreError) -> Problem {
    use agentkit_store::StoreError as E;
    match &error {
        E::InvalidTransition(detail)
        | E::AgentInputTransition(detail)
        | E::AnswerStale(detail)
        | E::ReceiptTransition {
            message: detail, ..
        } => Problem::new(409, "mcp_stale_connection", detail.clone()),
        E::ManagedInput { reason, message } => {
            use agentkit_store::ManagedInputReason as R;
            let code = match reason {
                R::SecretReferencesUnsupported => "mcp_secret_missing",
                R::InvalidAlias | R::IdentityRequired | R::ImmutableIdentity | R::AliasConflict => {
                    "mcp_invalid_config"
                }
                R::SelectedConnectorRequired => "invalid_request",
            };
            Problem::new(400, code, message.clone())
        }
        E::InvalidInput(detail)
        | E::TaskInput {
            message: detail, ..
        } => Problem::new(400, "invalid_request", detail.clone()),
        _ => store_error(error),
    }
}

pub(super) fn managed_validation_error(error: StoreError) -> Problem {
    match error {
        StoreError::ManagedInput {
            reason: agentkit_store::ManagedInputReason::SecretReferencesUnsupported,
            message,
        } => Problem::new(400, "mcp_secret_missing", message),
        StoreError::InvalidInput(message) => Problem::new(400, "invalid_request", message),
        other => managed_store_error(other),
    }
}
pub(super) fn provider_snapshot_error(error: S) -> Problem {
    match error {
        S::UnknownProvider => Problem::new(
            400,
            "provider_not_bound",
            "Requested provider is not bound to this host executor.",
        ),
        other => session_error(other),
    }
}

#[cfg(test)]
mod tests;
