use super::*;
use agentkit_sessions::SessionsError as S;
use agentkit_store::{ManagedInputReason as R, ReceiptTransitionReason};

#[test]
fn context_specific_codes_and_prefix_spoofs_stay_distinct() {
    let general = session_error(S::InvalidOverrides("bad".into()));
    assert_eq!(
        (general.status, general.code.as_str()),
        (500, "internal_error")
    );
    let teammate = teammate_problem(S::InvalidOverrides("bad".into()));
    assert_eq!(
        (
            teammate.status,
            teammate.code.as_str(),
            teammate.detail.as_str()
        ),
        (400, "invalid_request", "invalid_overrides: bad")
    );
    for message in ["invalid_overrides: secret", "run_profile_refused: secret"] {
        let error = teammate_problem(S::ContentAuthorityCallback(message.into()));
        assert_eq!((error.status, error.code.as_str()), (500, "internal_error"));
    }
    let typed = managed_store_error(StoreError::ManagedInput {
        reason: R::InvalidAlias,
        message: "bad alias".into(),
    });
    assert_eq!(
        (typed.status, typed.code.as_str()),
        (400, "mcp_invalid_config")
    );
    let spoof = managed_store_error(StoreError::InvalidInput(
        "MCP server alias \"spoof\"".into(),
    ));
    assert_eq!(
        (spoof.status, spoof.code.as_str()),
        (400, "invalid_request")
    );
}

#[test]
fn proposal_transition_siblings_keep_proposal_context() {
    for error in [
        StoreError::InvalidTransition("ordinary".into()),
        StoreError::AgentInputTransition("input".into()),
        StoreError::AnswerStale("stale".into()),
        StoreError::ReceiptTransition {
            reason: ReceiptTransitionReason::RunActive,
            message: "active".into(),
        },
    ] {
        let expected = match &error {
            StoreError::InvalidTransition(message)
            | StoreError::AgentInputTransition(message)
            | StoreError::AnswerStale(message)
            | StoreError::ReceiptTransition { message, .. } => message.clone(),
            _ => unreachable!(),
        };
        let problem = proposal_error(agentkit_governance::GovernanceError::Store(error));
        assert_eq!(
            (
                problem.status,
                problem.code.as_str(),
                problem.detail.as_str()
            ),
            (409, "invalid_proposal_transition", expected.as_str())
        );
    }
}
