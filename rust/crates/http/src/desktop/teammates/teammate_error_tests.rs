use super::*;
use agentkit_agents::AgentsError;
use agentkit_sessions::SessionsError;
use agentkit_store::StoreError;
use axum::response::IntoResponse;

#[test]
fn teammate_refusals_preserve_status_code_and_detail() {
    for (error, message) in [
        (
            SessionsError::InvalidOverrides("bad model".into()),
            "invalid_overrides: bad model",
        ),
        (
            SessionsError::RunProfileRefused(" missing".into()),
            "run_profile_refused: missing",
        ),
    ] {
        let problem = teammate_problem(error);
        assert_eq!(
            (
                problem.status,
                problem.code.as_str(),
                problem.detail.as_str()
            ),
            (400, "invalid_request", message)
        );
    }
    for (error, status, code, detail) in [
        (
            AgentsError::Store(StoreError::Conflict("rev 2".into())),
            409,
            "revision_conflict",
            "rev 2",
        ),
        (
            AgentsError::Store(StoreError::NotFound("task missing".into())),
            404,
            "not_found",
            "task missing",
        ),
        (
            AgentsError::SubagentNotFound("a".into()),
            404,
            "not_found",
            "subagent_not_found: a",
        ),
    ] {
        let problem = teammate_problem(error);
        assert_eq!(
            (
                problem.status,
                problem.code.as_str(),
                problem.detail.as_str()
            ),
            (status, code, detail)
        );
    }
}

#[tokio::test]
async fn teammate_internal_diagnostic_is_redacted_on_wire() {
    for message in ["invalid_overrides: secret", "run_profile_refused: secret"] {
        let problem = teammate_problem(SessionsError::ContentAuthorityCallback(message.into()));
        assert_eq!(
            (problem.status, problem.code.as_str()),
            (500, "internal_error")
        );
        let body = axum::body::to_bytes(problem.into_response().into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["detail"], "The server failed to handle the request.");
        assert!(!String::from_utf8_lossy(&body).contains("secret"));
    }
}
